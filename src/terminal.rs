use std::{
    env, io,
    sync::{Arc, Mutex, mpsc},
};

use alacritty_terminal::{
    event::{Event, EventListener, WindowSize},
    event_loop::{EventLoop, EventLoopSender, Msg},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    sync::FairMutex,
    term::{self, Term, TermMode, cell::Flags},
    tty,
    vte::ansi::{Color, CursorShape, Handler, NamedColor, Rgb},
};
use eframe::egui::{self, Align2, Color32, FontId, Key, Modifiers, Rect, Sense, Stroke, Vec2};

use crate::{
    config::Terminal as Settings,
    cursor_trail::{self, Cursor, Target, Trail},
    graphics::{self, Graphics, GraphicsPty, PendingTransfer, Position},
};

enum TerminalEvent {
    Terminal(Event),
    Graphics(PendingTransfer, (usize, usize)),
}

struct GraphicsEvents {
    packets: mpsc::Receiver<PendingTransfer>,
    awaiting_position: bool,
}

#[derive(Clone)]
struct EventProxy {
    events: mpsc::Sender<TerminalEvent>,
    ctx: egui::Context,
    graphics: Arc<Mutex<GraphicsEvents>>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        if let Ok(mut graphics) = self.graphics.lock() {
            match &event {
                Event::Title(title) if title == graphics::MARKER => {
                    graphics.awaiting_position = true;
                    return;
                }
                Event::PtyWrite(text) if graphics.awaiting_position => {
                    graphics.awaiting_position = false;
                    if let Some(position) = graphics::reported_position(text)
                        && let Ok(packet) = graphics.packets.try_recv()
                    {
                        let _ = self.events.send(TerminalEvent::Graphics(packet, position));
                        self.ctx.request_repaint();
                        return;
                    }
                }
                _ => {}
            }
        }
        if !matches!(event, Event::Wakeup | Event::MouseCursorDirty) {
            let _ = self.events.send(TerminalEvent::Terminal(event));
        }
        self.ctx.request_repaint();
    }
}

#[derive(Clone, Copy)]
struct GridSize {
    columns: u16,
    lines: u16,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.lines.into()
    }

    fn columns(&self) -> usize {
        self.columns.into()
    }
}

fn grid_size(rect: Rect, cell: Vec2) -> GridSize {
    GridSize {
        columns: ((rect.width() / cell.x) as u16).max(2),
        lines: ((rect.height() / cell.y) as u16).max(2),
    }
}

fn window_size(grid: GridSize, cell: Vec2) -> WindowSize {
    WindowSize {
        num_lines: grid.lines,
        num_cols: grid.columns,
        cell_width: cell.x as u16,
        cell_height: cell.y as u16,
    }
}

pub struct Terminal {
    term: Arc<FairMutex<Term<EventProxy>>>,
    sender: EventLoopSender,
    events: mpsc::Receiver<TerminalEvent>,
    graphics: Graphics,
    size: WindowSize,
    font: FontId,
    cursor_trail: Trail,
}

impl Terminal {
    pub fn start(ui: &egui::Ui, rect: Rect, settings: &Settings) -> io::Result<Self> {
        let font = FontId::monospace(settings.font_size);
        let cell = cell_size(ui, &font);
        let grid = grid_size(rect, cell);
        let size = window_size(grid, cell);

        let mut options = tty::Options::default();
        options.env.insert("TERM".into(), "xterm-256color".into());
        options.env.insert("COLORTERM".into(), "truecolor".into());
        options.env.insert("TERM_PROGRAM".into(), "Synth".into());
        let target = match settings.command_target {
            crate::config::CommandTarget::Window => std::process::id().to_string(),
            crate::config::CommandTarget::Shared => String::new(),
        };
        options.env.insert(crate::command::WINDOW_ENV.into(), target);
        for name in [
            "WT_SESSION",
            "KITTY_WINDOW_ID",
            "WEZTERM_EXECUTABLE",
            "ITERM_SESSION_ID",
            "TMUX",
            "STY",
            "ZELLIJ",
        ] {
            if env::var_os(name).is_some() {
                options.env.insert(name.into(), String::new());
            }
        }
        let executable = env::current_exe()?;
        if let Some(dir) = executable.parent() {
            let paths = std::iter::once(dir.to_path_buf())
                .chain(env::var_os("PATH").iter().flat_map(env::split_paths))
                .collect::<Vec<_>>();
            if let Ok(path) = env::join_paths(paths)
                && let Ok(path) = path.into_string()
            {
                options.env.insert("PATH".into(), path);
            }
        }
        if let Some(shell) = &settings.shell {
            options.shell = Some(tty::Shell::new(shell.clone(), settings.args.clone()));
        }
        #[cfg(windows)]
        {
            options.escape_args = true;
            let executable_path = executable.to_str();
            if let Some(path) = executable_path {
                options.env.insert("SYNTH_EXE".into(), path.into());
            }
            if options.shell.is_none() {
                let mut args = vec!["/D".into()];
                if executable_path.is_some() {
                    options.escape_args = false;
                    options.env.insert(
                        "SYNTH_CMD".into(),
                        r#"start "" /b /wait "%SYNTH_EXE%""#.into(),
                    );
                    args.extend(["/K", "doskey synth=%SYNTH_CMD% $*"].map(str::to_owned));
                }
                options.shell = Some(tty::Shell::new("cmd.exe".into(), args));
            }
        }
        #[cfg(not(windows))]
        {
            if options.shell.is_none()
                && let Some(shell) = env::var("SHELL").ok().filter(|shell| !shell.is_empty())
            {
                options.shell = Some(tty::Shell::new(shell, Vec::new()));
            }
        }

        let (tx, events) = mpsc::channel();
        let (packets_sender, packets) = mpsc::channel();
        let proxy = EventProxy {
            events: tx,
            ctx: ui.ctx().clone(),
            graphics: Arc::new(Mutex::new(GraphicsEvents {
                packets,
                awaiting_position: false,
            })),
        };
        let term = Arc::new(FairMutex::new(Term::new(
            term::Config {
                kitty_keyboard: true,
                ..Default::default()
            },
            &grid,
            proxy.clone(),
        )));
        let pty = tty::new(&options, size, 0)?;
        let pty = GraphicsPty::new(pty, packets_sender, size);
        let event_loop = EventLoop::new(term.clone(), proxy, pty, false, false)?;
        let sender = event_loop.channel();
        event_loop.spawn();
        Ok(Self {
            term,
            sender,
            events,
            graphics: Graphics::default(),
            size,
            font,
            cursor_trail: Trail::default(),
        })
    }

    pub fn show_image(
        &mut self,
        ctx: &egui::Context,
        path: &std::path::Path,
    ) -> Result<String, String> {
        let cell = egui::vec2(
            f32::from(self.size.cell_width),
            f32::from(self.size.cell_height),
        );
        let mut term = self.term.lock();
        let cursor = term.grid().cursor.point;
        let position = Position {
            row: term.grid().history_size() as i64 + i64::from(cursor.line.0),
            column: cursor.column.0,
            alternate: term.mode().contains(TermMode::ALT_SCREEN),
        };
        let room = egui::vec2(
            (term.columns().saturating_sub(position.column)).max(1) as f32,
            term.screen_lines().saturating_sub(1).max(1) as f32,
        );
        let rows = self.graphics.show_file(ctx, path, position, cell, room)?;
        for _ in 0..rows {
            term.linefeed();
        }
        term.carriage_return();
        ctx.request_repaint();
        Ok(format!("Displayed {}", path.display()))
    }

    pub fn set_font_size(&mut self, size: f32) {
        self.font.size = size;
    }

    pub fn clear_cursor_trail(&mut self) {
        self.cursor_trail = Trail::default();
    }

    pub fn focus_id() -> egui::Id {
        egui::Id::new("terminal input")
    }

    pub fn process_events(&mut self, ctx: &egui::Context, foreground: Color32, background: Color32) -> bool {
        for event in self.events.try_iter() {
            let event = match event {
                TerminalEvent::Terminal(event) => event,
                TerminalEvent::Graphics(packet, (row, column)) => {
                    let position = {
                        let term = self.term.lock();
                        Position {
                            row: (term.grid().history_size() + row) as i64,
                            column,
                            alternate: term.mode().contains(TermMode::ALT_SCREEN),
                        }
                    };
                    if let Some(reply) = self.graphics.receive(ctx, packet, position) {
                        self.write(reply);
                    }
                    continue;
                }
            };
            match event {
                Event::Exit | Event::ChildExit(_) => return true,
                Event::PtyWrite(text) => self.write(text),
                Event::TextAreaSizeRequest(format) => self.write(format(self.size)),
                Event::ColorRequest(index, format) => {
                    let color = self.term.lock().colors()[index]
                        .map_or_else(|| palette(index, foreground, background), rgb_color);
                    self.write(format(Rgb {
                        r: color.r(),
                        g: color.g(),
                        b: color.b(),
                    }));
                }
                Event::ClipboardStore(_, text) => ctx.copy_text(text),
                _ => {}
            }
        }
        false
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        request_focus: bool,
        interactive: bool,
        colors: (Color32, Color32),
        trail: &cursor_trail::Settings,
    ) -> bool {
        let (foreground, background) = colors;
        let cell = cell_size(ui, &self.font);
        let grid = grid_size(rect, cell);
        let size = window_size(grid, cell);
        if (
            size.num_lines,
            size.num_cols,
            size.cell_width,
            size.cell_height,
        ) != (
            self.size.num_lines,
            self.size.num_cols,
            self.size.cell_width,
            self.size.cell_height,
        ) {
            self.term.lock().resize(grid);
            let _ = self.sender.send(Msg::Resize(size));
            self.size = size;
        }
        if self.process_events(ui.ctx(), foreground, background) { return true; }
        let response = ui.interact(
            rect.intersect(ui.clip_rect()),
            Self::focus_id(),
            Sense::click_and_drag(),
        );
        if interactive && (request_focus || response.clicked() || response.drag_started()) {
            response.request_focus();
        }
        if interactive {
            self.pointer_input(ui, &response, rect, cell, grid);
            if response.has_focus() {
                ui.memory_mut(|memory| {
                    memory.set_focus_lock_filter(
                        response.id,
                        egui::EventFilter {
                            tab: true,
                            horizontal_arrows: true,
                            vertical_arrows: true,
                            escape: true,
                        },
                    );
                });
                self.keyboard_input(ui);
            }
        } else {
            response.surrender_focus();
        }

        self.draw(ui, rect, cell, colors, response.has_focus(), trail);
        false
    }

    fn write(&self, bytes: impl Into<Vec<u8>>) {
        let _ = self.sender.send(Msg::Input(bytes.into().into()));
    }

    pub fn copy_selection(&self, ctx: &egui::Context) {
        if let Some(text) = self.term.lock().selection_to_string() {
            ctx.copy_text(text);
        }
    }

    fn keyboard_input(&self, ui: &egui::Ui) {
        let mode = *self.term.lock().mode();
        let mut events = ui
            .input(|input| input.events.clone())
            .into_iter()
            .peekable();
        while let Some(event) = events.next() {
            match event {
                egui::Event::Text(text) => self.write(text),
                egui::Event::Ime(egui::ImeEvent::Commit(text)) => self.write(text),
                egui::Event::Paste(text) => {
                    let text = text.replace("\r\n", "\n").replace('\n', "\r");
                    if mode.contains(TermMode::BRACKETED_PASTE) {
                        self.write(format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")));
                    } else {
                        self.write(text);
                    }
                }
                egui::Event::Copy => {
                    let modifiers = ui.input(|input| input.modifiers);
                    let selection = self.term.lock().selection_to_string();
                    if !cfg!(target_os = "macos") && modifiers.ctrl && !modifiers.shift {
                        self.write(b"\x03");
                    } else if !modifiers.ctrl || !modifiers.shift {
                        if let Some(text) = selection {
                            ui.ctx().copy_text(text);
                        } else if !cfg!(target_os = "macos") && !modifiers.shift {
                            self.write(b"\x03");
                        }
                    } else if let Some(bytes) = key_bytes(Key::C, modifiers, mode) {
                        self.write(bytes);
                    }
                }
                egui::Event::Cut if !cfg!(target_os = "macos") => self.write(b"\x18"),
                egui::Event::Key {
                    key,
                    physical_key,
                    pressed,
                    repeat,
                    modifiers,
                } => {
                    let associated = match events.peek() {
                        Some(egui::Event::Text(text)) if pressed => Some(text.as_str()),
                        _ => None,
                    };
                    if let Some(sequence) = kitty_key(
                        KeyStroke {
                            key,
                            physical_key,
                            pressed,
                            repeat,
                            modifiers,
                            associated,
                        },
                        mode,
                    ) {
                        if associated.is_some() {
                            events.next();
                        }
                        self.write(sequence);
                    } else if pressed && let Some(bytes) = key_bytes(key, modifiers, mode) {
                        self.write(bytes);
                    }
                }
                _ => {}
            }
        }
    }

    fn pointer_input(
        &self,
        ui: &egui::Ui,
        response: &egui::Response,
        rect: Rect,
        cell: Vec2,
        grid: GridSize,
    ) {
        let mut term = self.term.lock();
        if response.hovered() {
            let scroll = ui.input(|input| input.smooth_scroll_delta.y);
            if scroll.abs() > 0.0 {
                let lines = ((scroll / cell.y).abs().ceil() as i32) * scroll.signum() as i32;
                if term
                    .mode()
                    .contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
                {
                    let key = if lines > 0 {
                        Key::ArrowUp
                    } else {
                        Key::ArrowDown
                    };
                    if let Some(bytes) = key_bytes(key, Modifiers::NONE, *term.mode()) {
                        self.write(bytes.repeat(lines.unsigned_abs().min(64) as usize));
                    }
                } else {
                    term.scroll_display(Scroll::Delta(lines));
                }
            }
        }
        let Some(pos) = response.interact_pointer_pos() else {
            return;
        };
        let relative = pos - rect.min;
        let column = (relative.x / cell.x).clamp(0.0, f32::from(grid.columns - 1));
        let line = (relative.y / cell.y).clamp(0.0, f32::from(grid.lines - 1)) as i32;
        let point = Point::new(
            Line(line - term.grid().display_offset() as i32),
            Column(column as usize),
        );
        let side = if column.fract() < 0.5 {
            Side::Left
        } else {
            Side::Right
        };
        if response.drag_started() {
            term.selection = Some(Selection::new(SelectionType::Simple, point, side));
        } else if response.dragged() || response.drag_stopped() {
            if let Some(selection) = &mut term.selection {
                selection.update(point, side);
            }
        } else if response.double_clicked() {
            term.selection = Some(Selection::new(SelectionType::Semantic, point, side));
        } else if response.clicked() {
            term.selection = None;
        }
    }

    fn draw(
        &mut self,
        ui: &egui::Ui,
        rect: Rect,
        cell: Vec2,
        colors: (Color32, Color32),
        focused: bool,
        trail: &cursor_trail::Settings,
    ) {
        let (foreground, background) = colors;
        let term = self.term.lock();
        let content = term.renderable_content();
        let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
        let origin = Position {
            row: term.grid().history_size() as i64 - content.display_offset as i64,
            column: 0,
            alternate: term.mode().contains(TermMode::ALT_SCREEN),
        };
        self.graphics.draw(&painter, rect, cell, origin, true);
        for entry in content.display_iter {
            let flags = entry.flags;
            if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let x = entry.point.column.0 as f32 * cell.x;
            let y = (entry.point.line.0 + content.display_offset as i32) as f32 * cell.y;
            let width = if flags.contains(Flags::WIDE_CHAR) {
                cell.x * 2.0
            } else {
                cell.x
            };
            let bounds =
                Rect::from_min_size(rect.min + egui::vec2(x, y), egui::vec2(width, cell.y));
            let color = |color| match color {
                Color::Spec(rgb) => rgb_color(rgb),
                Color::Named(name) => {
                    let index = name as usize;
                    content.colors[index]
                        .map_or_else(|| palette(index, foreground, background), rgb_color)
                }
                Color::Indexed(index) => {
                    let index = usize::from(index);
                    content.colors[index]
                        .map_or_else(|| palette(index, foreground, background), rgb_color)
                }
            };
            let mut fg = color(entry.fg);
            let mut bg = color(entry.bg);
            let selected = content
                .selection
                .is_some_and(|selection| selection.contains(entry.point));
            if selected != flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if flags.contains(Flags::DIM) {
                fg = fg.gamma_multiply(0.7);
            }
            if bg != background {
                painter.rect_filled(bounds, 0.0, bg);
            }

            if entry.point == content.cursor.point && content.display_offset == 0 {
                match content.cursor.shape {
                    CursorShape::Hidden => {}
                    CursorShape::Block if focused => {
                        painter.rect_filled(bounds, 0.0, fg);
                        std::mem::swap(&mut fg, &mut bg);
                    }
                    CursorShape::Beam => {
                        painter.line_segment(
                            [bounds.left_top(), bounds.left_bottom()],
                            Stroke::new(1.5_f32, fg),
                        );
                    }
                    CursorShape::Underline => {
                        painter.hline(
                            bounds.x_range(),
                            bounds.bottom() - 1.0,
                            Stroke::new(1.5_f32, fg),
                        );
                    }
                    _ => {
                        painter.rect_stroke(
                            bounds,
                            0.0,
                            Stroke::new(1.0_f32, fg),
                            egui::StrokeKind::Inside,
                        );
                    }
                }
            }

            if entry.c != ' ' && !flags.contains(Flags::HIDDEN) {
                let mut text = String::from(entry.c);
                if let Some(chars) = entry.zerowidth() {
                    text.extend(chars);
                }
                painter.text(
                    bounds.left_top(),
                    Align2::LEFT_TOP,
                    text,
                    self.font.clone(),
                    fg,
                );
            }
            if flags.intersects(Flags::ALL_UNDERLINES | Flags::STRIKEOUT) {
                let y = if flags.contains(Flags::STRIKEOUT) {
                    bounds.center().y
                } else {
                    bounds.bottom() - 1.0
                };
                painter.hline(bounds.x_range(), y, Stroke::new(1.0_f32, fg));
            }
        }
        self.graphics.draw(&painter, rect, cell, origin, false);
        let cursor = if trail.enabled_for(Target::Terminal)
            && focused
            && content.display_offset == 0
            && content.cursor.shape != CursorShape::Hidden
        {
            let mut size = cell;
            let mut pos = egui::pos2(
                content.cursor.point.column.0 as f32 * cell.x,
                content.cursor.point.line.0 as f32 * cell.y,
            );
            match content.cursor.shape {
                CursorShape::Beam => size.x = 1.5,
                CursorShape::Underline => {
                    pos.y += cell.y - 2.0;
                    size.y = 2.0;
                }
                _ => {}
            }
            Some(Cursor {
                surface: egui::Id::new(("terminal cursor", origin.alternate)),
                rect: Rect::from_min_size(pos, size),
                origin: rect.min.to_vec2(),
                cell,
                color: content.colors[NamedColor::Cursor as usize].map_or(foreground, rgb_color),
            })
        } else {
            None
        };
        self.cursor_trail.draw(&painter, trail, cursor);
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}

fn cell_size(ui: &egui::Ui, font: &FontId) -> Vec2 {
    ui.fonts(|fonts| {
        egui::vec2(
            fonts.glyph_width(font, 'M').ceil(),
            fonts.row_height(font).ceil(),
        )
    })
}

fn rgb_color(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.r, rgb.g, rgb.b)
}

fn palette(index: usize, foreground: Color32, background: Color32) -> Color32 {
    const ANSI: [(u8, u8, u8); 16] = [
        (28, 34, 46),
        (218, 95, 104),
        (100, 168, 124),
        (198, 156, 73),
        (93, 149, 219),
        (167, 121, 213),
        (61, 164, 180),
        (197, 208, 224),
        (99, 112, 133),
        (243, 124, 134),
        (133, 197, 146),
        (230, 183, 93),
        (130, 176, 242),
        (199, 151, 238),
        (102, 201, 214),
        (235, 241, 250),
    ];
    match index {
        0..=15 => {
            let (r, g, b) = ANSI[index];
            Color32::from_rgb(r, g, b)
        }
        16..=231 => {
            let value = index - 16;
            let level = |component: usize| {
                if component == 0 {
                    0
                } else {
                    55 + 40 * component as u8
                }
            };
            Color32::from_rgb(level(value / 36), level(value / 6 % 6), level(value % 6))
        }
        232..=255 => Color32::from_gray(8 + 10 * (index as u8 - 232)),
        index if index == NamedColor::Background as usize => background,
        _ => foreground,
    }
}

struct KeyStroke<'a> {
    key: Key,
    physical_key: Option<Key>,
    pressed: bool,
    repeat: bool,
    modifiers: Modifiers,
    associated: Option<&'a str>,
}

pub fn restore_modified_copy_key(input: &mut egui::RawInput) {
    for index in 0..input.events.len() {
        if !matches!(input.events[index], egui::Event::Copy) {
            continue;
        }
        let modifiers = input.events[index + 1..].iter()
            .take_while(|event| !matches!(event, egui::Event::Copy))
            .find_map(|event| match event {
                egui::Event::Key { key: Key::C, pressed: false, modifiers, .. } => Some(*modifiers),
                _ => None,
            }).unwrap_or(input.modifiers);
        if modifiers.ctrl && modifiers.shift {
            input.events[index] = egui::Event::Key {
                key: Key::C,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            };
        }
    }
}

fn printable_key(key: Key) -> Option<char> {
    if key == Key::Space {
        return Some(' ');
    }
    let mut chars = key.symbol_or_name().chars();
    let character = chars.next()?;
    chars
        .next()
        .is_none()
        .then_some(character.to_ascii_lowercase())
}

fn kitty_key(stroke: KeyStroke<'_>, mode: TermMode) -> Option<String> {
    if !mode.intersects(TermMode::KITTY_KEYBOARD_PROTOCOL) {
        return None;
    }
    let KeyStroke {
        key,
        physical_key,
        pressed,
        repeat,
        modifiers,
        associated,
    } = stroke;
    let all = mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    let report_events = mode.contains(TermMode::REPORT_EVENT_TYPES);
    if !pressed
        && (!report_events || (!all && matches!(key, Key::Enter | Key::Tab | Key::Backspace)))
    {
        return None;
    }
    let printable = printable_key(key);
    if !all && printable.is_some() && !modifiers.ctrl && !modifiers.alt && !modifiers.mac_cmd {
        return None;
    }
    let function = match key {
        Key::ArrowUp => Some((1, 'A')),
        Key::ArrowDown => Some((1, 'B')),
        Key::ArrowRight => Some((1, 'C')),
        Key::ArrowLeft => Some((1, 'D')),
        Key::Home => Some((1, 'H')),
        Key::End => Some((1, 'F')),
        Key::Insert => Some((2, '~')),
        Key::Delete => Some((3, '~')),
        Key::PageUp => Some((5, '~')),
        Key::PageDown => Some((6, '~')),
        Key::F1 => Some((11, '~')),
        Key::F2 => Some((12, '~')),
        Key::F3 => Some((13, '~')),
        Key::F4 => Some((14, '~')),
        Key::F5 => Some((15, '~')),
        Key::F6 => Some((17, '~')),
        Key::F7 => Some((18, '~')),
        Key::F8 => Some((19, '~')),
        Key::F9 => Some((20, '~')),
        Key::F10 => Some((21, '~')),
        Key::F11 => Some((23, '~')),
        Key::F12 => Some((24, '~')),
        _ => None,
    };
    let (code, terminator) = if let Some(value) = function {
        value
    } else {
        let code = match key {
            Key::Escape => 27,
            Key::Enter | Key::Tab | Key::Backspace if !all && modifiers.is_none() => return None,
            Key::Enter => 13,
            Key::Tab => 9,
            Key::Backspace => 127,
            _ => u32::from(printable?),
        };
        (code, 'u')
    };
    let modifier = 1
        + u8::from(modifiers.shift)
        + 2 * u8::from(modifiers.alt)
        + 4 * u8::from(modifiers.ctrl)
        + 8 * u8::from(modifiers.mac_cmd);
    let mut sequence = format!("\x1b[{code}");
    if printable.is_some() && mode.contains(TermMode::REPORT_ALTERNATE_KEYS) {
        let shifted = associated.and_then(|text| {
            let mut chars = text.chars();
            let character = chars.next()?;
            (modifiers.shift && chars.next().is_none()).then_some(character)
        });
        let physical = physical_key
            .and_then(printable_key)
            .filter(|key| Some(*key) != printable);
        if let Some(shifted) = shifted {
            sequence.push_str(&format!(":{}", u32::from(shifted)));
        } else if physical.is_some() {
            sequence.push(':');
        }
        if let Some(physical) = physical {
            sequence.push_str(&format!(":{}", u32::from(physical)));
        }
    }
    sequence.push_str(&format!(";{modifier}"));
    if report_events {
        let kind = if !pressed {
            3
        } else if repeat {
            2
        } else {
            1
        };
        sequence.push_str(&format!(":{kind}"));
    }
    if mode.contains(TermMode::REPORT_ASSOCIATED_TEXT)
        && let Some(text) = associated
    {
        sequence.push(';');
        sequence.push_str(
            &text
                .chars()
                .map(|character| u32::from(character).to_string())
                .collect::<Vec<_>>()
                .join(":"),
        );
    }
    sequence.push(terminator);
    Some(sequence)
}

fn key_bytes(key: Key, modifiers: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    if modifiers.ctrl && !modifiers.shift && !modifiers.alt {
        let name = key.name().as_bytes();
        if name.len() == 1 && name[0].is_ascii_uppercase() {
            return Some(vec![name[0] - b'A' + 1]);
        }
        let byte = match key {
            Key::Space => Some(0),
            Key::OpenBracket => Some(27),
            Key::Backslash => Some(28),
            Key::CloseBracket => Some(29),
            _ => None,
        };
        if let Some(byte) = byte {
            return Some(vec![byte]);
        }
    }

    let modifiers_code =
        1 + u8::from(modifiers.shift) + 2 * u8::from(modifiers.alt) + 4 * u8::from(modifiers.ctrl);
    let cursor = match key {
        Key::ArrowUp => Some('A'),
        Key::ArrowDown => Some('B'),
        Key::ArrowRight => Some('C'),
        Key::ArrowLeft => Some('D'),
        Key::Home => Some('H'),
        Key::End => Some('F'),
        _ => None,
    };
    if let Some(cursor) = cursor {
        let sequence = if modifiers_code != 1 {
            format!("\x1b[1;{modifiers_code}{cursor}")
        } else if mode.contains(TermMode::APP_CURSOR) {
            format!("\x1bO{cursor}")
        } else {
            format!("\x1b[{cursor}")
        };
        return Some(sequence.into_bytes());
    }

    let tilde = match key {
        Key::Insert => Some(2),
        Key::Delete => Some(3),
        Key::PageUp => Some(5),
        Key::PageDown => Some(6),
        Key::F5 => Some(15),
        Key::F6 => Some(17),
        Key::F7 => Some(18),
        Key::F8 => Some(19),
        Key::F9 => Some(20),
        Key::F10 => Some(21),
        Key::F11 => Some(23),
        Key::F12 => Some(24),
        _ => None,
    };
    if let Some(code) = tilde {
        let sequence = if modifiers_code == 1 {
            format!("\x1b[{code}~")
        } else {
            format!("\x1b[{code};{modifiers_code}~")
        };
        return Some(sequence.into_bytes());
    }

    let bytes: &[u8] = match key {
        Key::Enter => b"\r",
        Key::Tab if modifiers.shift => b"\x1b[Z",
        Key::Tab => b"\t",
        Key::Backspace if modifiers.ctrl => b"\x08",
        Key::Backspace => b"\x7f",
        Key::Escape => b"\x1b",
        Key::F1 => b"\x1bOP",
        Key::F2 => b"\x1bOQ",
        Key::F3 => b"\x1bOR",
        Key::F4 => b"\x1bOS",
        _ => return None,
    };
    Some(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_keys_reach_the_terminal_as_bytes() {
        assert_eq!(
            key_bytes(Key::C, Modifiers::CTRL, TermMode::empty()),
            Some(vec![3])
        );
        assert_eq!(
            key_bytes(Key::Tab, Modifiers::NONE, TermMode::empty()),
            Some(vec![9])
        );
        assert_eq!(
            key_bytes(Key::Escape, Modifiers::NONE, TermMode::empty()),
            Some(vec![27])
        );
    }

    #[test]
    fn cursor_keys_respect_application_mode() {
        assert_eq!(
            key_bytes(Key::ArrowUp, Modifiers::NONE, TermMode::APP_CURSOR).as_deref(),
            Some(b"\x1bOA".as_slice())
        );
        assert_eq!(
            key_bytes(Key::ArrowUp, Modifiers::CTRL, TermMode::APP_CURSOR).as_deref(),
            Some(b"\x1b[1;5A".as_slice())
        );
    }
}
