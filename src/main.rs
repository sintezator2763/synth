#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod animation;
mod command;
mod config;
mod cursor_trail;
mod document;
mod editor;
mod graphics;
mod recovery;
mod terminal;

use std::{
    collections::HashMap,
    env, io,
    path::{Path, PathBuf},
    process::{ExitCode, Stdio},
    sync::mpsc,
};

use command::{Command, Reply, Request, Zoom};
use config::{Bindings, Config, FileWatch, ShortcutAction, Side};
use document::{DiskChange, Documents};
use editor::{Editor, SearchBar};
use eframe::egui::{self, FontId};
use recovery::Recovery;
use terminal::Terminal;

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Editor,
    Terminal,
}

enum Confirmation {
    SaveCopy(String),
    Reload(u64),
    Close(HashMap<u64, String>),
}

struct WatchedDocument {
    id: u64,
    path: PathBuf,
    watcher: Option<FileWatch>,
    change: Option<DiskChange>,
}

struct Synth {
    config: Config,
    config_path: PathBuf,
    bindings: Bindings,
    watcher: Option<FileWatch>,
    watched_document: Option<WatchedDocument>,
    commands: mpsc::Receiver<Request>,
    shell_errors: (mpsc::Sender<String>, mpsc::Receiver<String>),
    documents: Documents,
    editor: Editor,
    search_bar: SearchBar,
    recovery: Option<Recovery>,
    recovery_error: Option<String>,
    terminal: Option<Terminal>,
    terminal_open: bool,
    terminal_font_override: Option<f32>,
    terminal_error: Option<String>,
    config_error: Option<String>,
    notice: Option<String>,
    confirmation: Option<Confirmation>,
    closing: bool,
    pending_focus: Option<Focus>,
    #[cfg(windows)]
    maximize_on_startup: bool,
}

impl Synth {
    fn new(ctx: &egui::Context, installation: PathBuf, config_path: PathBuf) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        for (name, family, data) in [
            (
                "IBM Plex Mono",
                egui::FontFamily::Monospace,
                include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf").as_slice(),
            ),
            (
                "IBM Plex Sans",
                egui::FontFamily::Proportional,
                include_bytes!("../assets/fonts/IBMPlexSans.ttf").as_slice(),
            ),
        ] {
            fonts
                .font_data
                .insert(name.into(), egui::FontData::from_static(data).into());
            fonts
                .families
                .entry(family)
                .or_default()
                .insert(0, name.into());
        }
        ctx.set_fonts(fonts);
        ctx.options_mut(|options| options.zoom_with_keyboard = false);
        let setup_error = Config::ensure_file(&config_path, &installation)
            .err()
            .map(|error| {
                format!(
                    "Could not create {}: {}",
                    config_path.display(),
                    io_message(&error)
                )
            });
        let (watcher, watch_error) = match FileWatch::watch_settings(&config_path, ctx.clone()) {
            Ok(watcher) => (Some(watcher), None),
            Err(error) => (None, Some(format!("Config watcher: {error}"))),
        };
        let commands = command::listen(&installation, ctx);
        let mut app = Self {
            config: Config::default(),
            config_path,
            bindings: Vec::new(),
            watcher,
            watched_document: None,
            commands,
            shell_errors: mpsc::channel(),
            documents: Documents::default(),
            editor: Editor::default(),
            search_bar: SearchBar::default(),
            recovery: None,
            recovery_error: None,
            terminal: None,
            terminal_open: false,
            terminal_font_override: None,
            terminal_error: None,
            config_error: None,
            notice: setup_error.or(watch_error),
            confirmation: None,
            closing: false,
            pending_focus: Some(Focus::Editor),
            #[cfg(windows)]
            maximize_on_startup: true,
        };
        app.bindings = app.config.bindings(&app.config_path).unwrap_or_default();
        ctx.set_visuals(app.config.palette().visuals(app.config.editor.theme));
        let _ = app.reload_config(ctx);
        app
    }

    fn reload_config(&mut self, ctx: &egui::Context) -> Reply {
        let result = Config::load(&self.config_path).map(|(config, bindings)| {
            ctx.set_visuals(config.palette().visuals(config.editor.theme));
            if self.config.palette() != config.palette() {
                self.editor.reset_highlighting();
            }
            self.bindings = bindings;
            if self.config.terminal.font_size != config.terminal.font_size {
                self.terminal_font_override = None;
            }
            self.config = config;
            if !self.config.interface.enable_search_and_replace {
                self.search_bar.close();
            }
            if self.config.editor.recover_unsaved_documents {
                if self.recovery.is_none() {
                    let root = self.config_path.with_file_name("recovery");
                    match Recovery::new(&root) {
                        Ok(recovery) => self.recovery = Some(recovery),
                        Err(error) => {
                            self.recovery_error = Some(format!("Draft recovery: {error}"))
                        }
                    }
                }
            } else {
                self.recovery = None;
                self.recovery_error = None;
            }
            self.terminal_error = None;
            format!("Reloaded {}", self.config_path.display())
        });
        self.config_error = result.as_ref().err().cloned();
        result
    }

    fn toggle_terminal(&mut self) {
        self.terminal_open = !self.terminal_open;
        if self.terminal_open {
            self.terminal_error = None;
        }
        self.pending_focus = Some(if self.terminal_open {
            Focus::Terminal
        } else {
            Focus::Editor
        });
    }

    fn focus(&self, ctx: &egui::Context) -> Focus {
        self.pending_focus.unwrap_or_else(|| {
            if self.terminal_open && ctx.memory(|memory| memory.has_focus(Terminal::focus_id())) {
                Focus::Terminal
            } else {
                Focus::Editor
            }
        })
    }

    fn execute(&mut self, ctx: &egui::Context, command: Command) -> Reply {
        if matches!(command, Command::CancelDialog) {
            if self.confirmation.take().is_none() {
                return Err("No confirmation dialog is open".into());
            }
            self.notice = None;
            self.pending_focus = Some(Focus::Editor);
            return Ok(String::new());
        }
        if self.confirmation.is_some() && !matches!(command, Command::InterfaceZoom(_)) {
            return Err("Finish the confirmation in the Synth window first".into());
        }
        self.notice = None;
        let previous = self.documents.current().id;
        let focus = self.focus(ctx);
        let reply = match command {
            Command::Open(path) => self.documents.open(Path::new(&path)),
            Command::New => {
                self.documents.create();
                Ok("Created a new document".into())
            }
            Command::Save => self.documents.save(None),
            Command::SaveAs(path) => self.documents.save(Some(Path::new(&path))),
            Command::Reload => self.reload_document(ctx),
            Command::Goto { line, column } => {
                let reply = self.documents.current_mut().goto(line, column)?;
                self.pending_focus = Some(Focus::Editor);
                Ok(reply)
            }
            Command::Close { discard } => self.documents.close(discard),
            Command::Properties => Ok(self.documents.properties()),
            Command::Tabs => Ok(self.documents.list()),
            Command::NextTab => Ok(self.documents.switch(true)),
            Command::PreviousTab => Ok(self.documents.switch(false)),
            Command::Find(query) => self.find(Some(query)),
            Command::FindNext => self.find(None),
            Command::ToggleSearch => {
                if !self.config.interface.enable_search_and_replace {
                    return Err(
                        "Enable interface.enable_search_and_replace to show the search bar".into(),
                    );
                }
                self.search_bar.toggle();
                self.pending_focus = Some(Focus::Editor);
                Ok(String::new())
            }
            Command::CloseSearch => {
                self.search_bar.close();
                self.pending_focus = Some(Focus::Editor);
                Ok(String::new())
            }
            Command::SearchNext => {
                if !self.search_bar.is_open() {
                    return Err("Open the search bar with synth search toggle".into());
                }
                self.search_bar.request_next();
                Ok(String::new())
            }
            Command::CancelDialog => unreachable!(),
            Command::ToggleWrap => {
                let document = self.documents.current_mut();
                let wrap = !document.wrap_lines.unwrap_or(self.config.editor.wrap_lines);
                document.wrap_lines = Some(wrap);
                Ok(if wrap {
                    "Line wrapping enabled"
                } else {
                    "Line wrapping disabled"
                }
                .into())
            }
            Command::TerminalImage(path) => {
                let terminal = self
                    .terminal
                    .as_mut()
                    .filter(|_| self.terminal_open)
                    .ok_or("Open the terminal first with synth terminal toggle")?;
                terminal.show_image(ctx, Path::new(&path))
            }
            Command::ToggleTerminal => {
                self.toggle_terminal();
                Ok(String::new())
            }
            Command::ToggleEditorTerminal => {
                if focus == Focus::Editor {
                    self.toggle_terminal();
                }
                Ok(String::new())
            }
            Command::CopyTerminalSelection => {
                let terminal = self
                    .terminal
                    .as_ref()
                    .filter(|_| self.terminal_open)
                    .ok_or("No terminal panel is open")?;
                terminal.copy_selection(ctx);
                Ok(String::new())
            }
            Command::TerminalZoom(zoom) => {
                let size = self.terminal_font_size();
                let range = config::FONT_SIZE_RANGE;
                self.terminal_font_override = match zoom {
                    Zoom::In => Some((size + 1.0).min(*range.end())),
                    Zoom::Out => Some((size - 1.0).max(*range.start())),
                    Zoom::Reset => None,
                };
                Ok(format!("Terminal font size: {}", self.terminal_font_size()))
            }
            Command::InterfaceZoom(zoom) => {
                match zoom {
                    Zoom::In => egui::gui_zoom::zoom_in(ctx),
                    Zoom::Out => egui::gui_zoom::zoom_out(ctx),
                    Zoom::Reset => ctx.set_zoom_factor(1.0),
                }
                Ok(String::new())
            }
            Command::HideTerminal => {
                if self.terminal_open {
                    self.toggle_terminal();
                }
                Ok(String::new())
            }
            Command::ToggleFocus => {
                if !self.terminal_open {
                    self.toggle_terminal();
                } else {
                    self.pending_focus = Some(if focus == Focus::Editor {
                        Focus::Terminal
                    } else {
                        Focus::Editor
                    });
                }
                Ok(String::new())
            }
            Command::ConfigPath => Ok(self.config_path.display().to_string()),
            Command::ConfigCheck => check_config(&self.config_path),
            Command::ConfigReload => self.reload_config(ctx),
        };
        if self.documents.current().id != previous && focus == Focus::Editor {
            self.pending_focus = Some(Focus::Editor);
        }
        reply
    }

    fn check_document_on_disk(&mut self, ctx: &egui::Context) {
        let document = self.documents.current_mut();
        let Some(path) = document
            .path
            .as_ref()
            .filter(|_| self.config.interface.show_external_file_changes)
        else {
            self.watched_document = None;
            return;
        };
        if let Some(watched) = &mut self.watched_document
            && watched.id == document.id
            && watched.path == *path
        {
            let Some(watcher) = &mut watched.watcher else {
                return;
            };
            match watcher.ready(ctx) {
                Ok(false) => {}
                Ok(true) => {
                    watched.change = document.disk_change();
                    if watched.change.is_none() {
                        document.dismissed_disk_change = None;
                    }
                }
                Err(error) => self.notice = Some(format!("File watcher: {error}")),
            }
            return;
        }
        let watcher = FileWatch::new(path, ctx.clone())
            .map_err(|error| self.notice = Some(format!("File watcher: {error}")))
            .ok();
        self.watched_document = Some(WatchedDocument {
            id: document.id,
            path: path.clone(),
            watcher,
            change: document.disk_change(),
        });
    }

    fn reload_document(&mut self, ctx: &egui::Context) -> Reply {
        let document = self.documents.current();
        if document.path.is_none() {
            return Err("This document has no file to reload".into());
        }
        let id = document.id;
        if document.dirty() {
            self.confirmation = Some(Confirmation::Reload(id));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            return Ok("Confirm reload in the Synth window.".into());
        }
        let reply = self.documents.reload(id)?;
        self.pending_focus = Some(Focus::Editor);
        Ok(reply)
    }

    fn confirm(&mut self, ctx: &egui::Context) {
        let Some(mut confirmation) = self.confirmation.take() else {
            return;
        };
        let mut canceled = false;
        let mut accepted = false;
        let mut discard = false;
        let result = egui::Modal::new(egui::Id::new("confirm document changes")).show(ctx, |ui| {
            match &mut confirmation {
                Confirmation::SaveCopy(path) => {
                    ui.heading("Save a copy");
                    ui.add(egui::TextEdit::singleline(path).hint_text("Destination file path"));
                    ui.horizontal(|ui| {
                        canceled = ui.button("Cancel").clicked();
                        accepted = ui
                            .add_enabled(!path.is_empty(), egui::Button::new("Save copy"))
                            .clicked();
                    });
                }
                Confirmation::Reload(_) => {
                    ui.heading("Reload from disk?");
                    ui.label("This will discard your edits to the active document.");
                    ui.horizontal(|ui| {
                        canceled = ui.button("Cancel").clicked();
                        accepted = ui.button("Discard edits and reload").clicked();
                    });
                }
                Confirmation::Close(paths) => {
                    ui.heading("Save changes before closing Synth?");
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .show(ui, |ui| {
                            for document in &self.documents.items {
                                if !document.dirty() {
                                    continue;
                                }
                                ui.push_id(document.id, |ui| {
                                    ui.label(document.display_path());
                                    if document.path.is_none() {
                                        ui.add(
                                            egui::TextEdit::singleline(
                                                paths.entry(document.id).or_default(),
                                            )
                                            .hint_text("File path for saving"),
                                        );
                                    }
                                });
                            }
                        });
                    ui.horizontal(|ui| {
                        canceled = ui.button("Cancel").clicked();
                        discard = ui.button("Discard all").clicked();
                        accepted = ui.button("Save all").clicked();
                    });
                }
            }
            if let Some(error) = &self.notice {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
        });
        if canceled || result.backdrop_response.clicked() {
            self.notice = None;
            self.pending_focus = Some(Focus::Editor);
            return;
        }
        if discard {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            self.closing = true;
            return;
        }
        if accepted {
            let result = match &confirmation {
                Confirmation::SaveCopy(path) => std::path::absolute(path)
                    .map_err(|error| io_message(&error))
                    .and_then(|destination| self.documents.save(Some(&destination)))
                    .map(|_| ()),
                Confirmation::Reload(id) => self.documents.reload(*id).map(|_| ()),
                Confirmation::Close(paths) => self.save_changed(paths),
            };
            match result {
                Ok(()) => {
                    self.notice = None;
                    if matches!(confirmation, Confirmation::Close(_)) {
                        self.closing = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    } else {
                        self.pending_focus = Some(Focus::Editor);
                    }
                    return;
                }
                Err(error) => self.notice = Some(error),
            }
        }
        self.confirmation = Some(confirmation);
    }

    fn save_changed(&mut self, paths: &HashMap<u64, String>) -> Result<(), String> {
        for document in self
            .documents
            .items
            .iter()
            .filter(|document| document.dirty())
        {
            if document.path.is_none() && paths.get(&document.id).is_none_or(|path| path.is_empty())
            {
                return Err("Enter a file path for each untitled document.".into());
            }
        }
        for document in self
            .documents
            .items
            .iter_mut()
            .filter(|document| document.dirty())
        {
            let destination = paths
                .get(&document.id)
                .map(std::path::absolute)
                .transpose()
                .map_err(|error| io_message(&error))?;
            document.save(destination.as_deref())?;
        }
        Ok(())
    }

    fn find(&mut self, query: Option<String>) -> Reply {
        let reply = self.documents.current_mut().find(query)?;
        self.pending_focus = Some(Focus::Editor);
        Ok(reply)
    }

    fn shortcut_commands(&self, ctx: &egui::Context) -> Vec<ShortcutAction> {
        let terminal_focused = self.focus(ctx) == Focus::Terminal;
        let search_open = self.search_bar.is_open();
        let query_focused = ctx.memory(|memory| memory.has_focus(SearchBar::query_id()));
        let dialog_open = self.confirmation.is_some();
        let available = |action: &ShortcutAction| {
            let ShortcutAction::Synth(command) = action else {
                return !dialog_open;
            };
            if dialog_open {
                return matches!(command, Command::CancelDialog | Command::InterfaceZoom(_));
            }
            match command {
                Command::CancelDialog => false,
                Command::ToggleSearch => {
                    self.config.interface.enable_search_and_replace && !terminal_focused
                }
                Command::CloseSearch => search_open && !terminal_focused,
                Command::SearchNext => search_open && query_focused && !terminal_focused,
                Command::CopyTerminalSelection | Command::TerminalZoom(_) => terminal_focused,
                Command::ToggleEditorTerminal => !terminal_focused,
                _ => true,
            }
        };
        ctx.input_mut(|input| {
            let mut commands = Vec::new();
            let mut discard_text = false;
            input.events.retain(|event| {
                if std::mem::take(&mut discard_text) && matches!(event, egui::Event::Text(_)) {
                    return false;
                }
                if let egui::Event::Key {
                    key,
                    pressed,
                    repeat,
                    modifiers,
                    ..
                } = event
                    && let Some((_, command)) = self.bindings.iter().find(|(shortcut, command)| {
                        *key == shortcut.logical_key
                            && modifiers.matches_exact(shortcut.modifiers)
                            && available(command)
                    })
                {
                    if *pressed && !*repeat {
                        commands.push(command.clone());
                    }
                    discard_text = *pressed && terminal::printable_key(*key).is_some();
                    return false;
                }
                true
            });
            commands
        })
    }

    fn run_shell_shortcut(&self, ctx: &egui::Context, script: String) -> Reply {
        let directory = self
            .documents
            .current()
            .path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_owned);
        let sender = self.shell_errors.0.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("synth-shell-shortcut".into())
            .spawn(move || {
                let result = run_shortcut_shell(&script, directory.as_deref());
                if let Err(error) = result {
                    let _ = sender.send(format!("Shortcut command: {error}"));
                    ctx.request_repaint();
                }
            })
            .map_err(|error| io_message(&error))?;
        Ok(String::new())
    }

    fn terminal_font_size(&self) -> f32 {
        self.terminal_font_override
            .unwrap_or(self.config.terminal.font_size)
    }

    fn terminal_panel(&mut self, ctx: &egui::Context) {
        let progress = self.config.terminal_animation(self.terminal_open).progress(
            ctx,
            "terminal animation",
            self.terminal_open,
            self.config.animations.enabled,
        );
        if progress == 0.0 {
            if let Some(terminal) = &mut self.terminal {
                terminal.clear_cursor_trail();
                let palette = self.config.palette();
                if terminal.process_events(ctx, palette.foreground, palette.terminal_background) {
                    self.terminal = None;
                    if self.terminal_open {
                        self.toggle_terminal();
                    }
                }
            }
            return;
        }

        let interactive = self.terminal_open && self.confirmation.is_none();
        let width = ctx.available_rect().width() * self.config.terminal.width_fraction;
        let panel = match self.config.terminal.side {
            Side::Left => egui::SidePanel::left("terminal"),
            Side::Right => egui::SidePanel::right("terminal"),
        };
        let font_size = self.terminal_font_size();
        let background = self.config.palette().terminal_background;
        panel
            .resizable(false)
            .show_separator_line(true)
            .exact_width(width * progress)
            .frame(egui::Frame::NONE.fill(background))
            .show(ctx, |ui| {
                let panel = ui.max_rect();
                let offset = width * (1.0 - progress);
                let origin = match self.config.terminal.side {
                    Side::Left => panel.min - egui::vec2(offset, 0.0),
                    Side::Right => panel.min,
                };
                let rect = egui::Rect::from_min_size(origin, egui::vec2(width, panel.height()))
                    .shrink(10.0);
                if self.terminal_open && self.terminal.is_none() && self.terminal_error.is_none() {
                    match Terminal::start(ui, rect, &self.config.terminal) {
                        Ok(terminal) => self.terminal = Some(terminal),
                        Err(error) => self.terminal_error = Some(io_message(&error)),
                    }
                }
                if let Some(terminal) = &mut self.terminal {
                    terminal.set_font_size(font_size);
                    let foreground = ui.visuals().text_color();
                    let exited = terminal.show(
                        ui,
                        rect,
                        interactive && self.pending_focus.take() == Some(Focus::Terminal),
                        interactive,
                        (foreground, background),
                        &self.config.cursor_trail,
                    );
                    if exited {
                        self.terminal = None;
                        if self.terminal_open {
                            self.toggle_terminal();
                        }
                    }
                } else if let Some(error) = &self.terminal_error {
                    ui.painter().with_clip_rect(panel).text(
                        rect.min,
                        egui::Align2::LEFT_TOP,
                        error,
                        FontId::monospace(font_size),
                        ui.visuals().error_fg_color,
                    );
                    if !self.terminal_open {
                        self.terminal_error = None;
                    }
                }
            });
    }
}

impl eframe::App for Synth {
    fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        if self.focus(ctx) == Focus::Terminal {
            terminal::restore_modified_copy_key(input);
        }
    }

    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        #[cfg(windows)]
        if std::mem::take(&mut self.maximize_on_startup) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        }
        if let Some(watcher) = &mut self.watcher {
            match watcher.ready(ctx) {
                Ok(true) => {
                    let _ = self.reload_config(ctx);
                }
                Ok(false) => {}
                Err(error) => self.config_error = Some(format!("Config watcher: {error}")),
            }
        }
        while let Ok(request) = self.commands.try_recv() {
            let reply = self.execute(ctx, request.command);
            let _ = request.respond.send(reply);
        }
        for error in self.shell_errors.1.try_iter() {
            self.notice = Some(error);
        }
        for action in self.shortcut_commands(ctx) {
            let result = match action {
                ShortcutAction::Synth(command) => self.execute(ctx, command),
                ShortcutAction::Shell(script) => self.run_shell_shortcut(ctx, script),
            };
            if let Err(error) = result {
                self.notice = Some(error);
            }
        }
        if let Some(index) = editor::tabs(ctx, &self.documents, &self.config)
            && self.confirmation.is_none()
        {
            self.documents.active = index;
            self.pending_focus = Some(Focus::Editor);
        }
        self.check_document_on_disk(ctx);
        editor::status(ctx, self.documents.current(), &self.config);
        if self.pending_focus == Some(Focus::Editor) {
            let id = Editor::id(self.documents.current());
            ctx.memory_mut(|memory| memory.request_focus(id));
            self.pending_focus = None;
        }
        self.terminal_panel(ctx);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(&ctx.style())
                    .inner_margin(self.config.interface.editor_padding),
            )
            .show(ctx, |ui| {
                let modal_open = self.confirmation.is_some();
                let notice = if modal_open {
                    None
                } else {
                    self.notice.as_ref()
                };
                for error in [
                    self.config_error.as_ref(),
                    self.recovery_error.as_ref(),
                    notice,
                ]
                .into_iter()
                .flatten()
                {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                let mut reload = false;
                ui.add_enabled_ui(!modal_open, |ui| {
                    if let Some(recovery) = &mut self.recovery {
                        for warning in &recovery.warnings {
                            ui.colored_label(
                                ui.visuals().error_fg_color,
                                format!("Unreadable draft: {warning}"),
                            );
                        }
                        if recovery.choices().next().is_some() {
                            ui.label("Unsaved drafts from previous sessions");
                            egui::ScrollArea::vertical()
                                .id_salt("draft choices")
                                .max_height(120.0)
                                .show(ui, |ui| {
                                    for (label, selected) in recovery.choices() {
                                        ui.checkbox(selected, label);
                                    }
                                });
                            let selected = recovery.choices().any(|(_, selected)| *selected);
                            ui.add_enabled_ui(selected, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    if ui.button("Restore selected").clicked() {
                                        recovery.restore(&mut self.documents);
                                        self.pending_focus = Some(Focus::Editor);
                                    }
                                    if ui.button("Discard selected").clicked()
                                        && let Err(error) = recovery.discard()
                                    {
                                        self.notice = Some(format!("Draft recovery: {error}"));
                                    }
                                });
                            });
                            ui.separator();
                        }
                    }
                    let document = self.documents.current_mut();
                    if let Some(change) = self
                        .watched_document
                        .as_ref()
                        .filter(|watched| watched.id == document.id)
                        .and_then(|watched| watched.change)
                        && document.dismissed_disk_change != Some(change)
                    {
                        let message = match change {
                            DiskChange::Modified(_) => {
                                format!("{} changed on disk.", document.name())
                            }
                            DiskChange::Unavailable => {
                                format!("{} is unavailable on disk.", document.name())
                            }
                        };
                        ui.label(message);
                        ui.horizontal_wrapped(|ui| {
                            if matches!(change, DiskChange::Modified(_)) {
                                reload = ui.button("Reload from disk").clicked();
                            }
                            if ui.button("Save a copy…").clicked() {
                                self.confirmation = Some(Confirmation::SaveCopy(String::new()));
                            }
                            if ui.button("Keep editing").clicked() {
                                document.dismissed_disk_change = Some(change);
                                self.pending_focus = Some(Focus::Editor);
                            }
                        });
                        ui.separator();
                    }
                    self.search_bar.show(ui, document);
                    self.editor
                        .show(ui, document, &self.config, self.search_bar.highlight());
                });
                if reload && let Err(error) = self.execute(ctx, Command::Reload) {
                    self.notice = Some(error);
                }
            });
        self.editor.retain_documents(&self.documents);
        if !self.closing
            && ctx.input(|input| input.viewport().close_requested())
            && self.documents.items.iter().any(document::Document::dirty)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.confirmation.is_none() {
                self.notice = None;
                self.confirmation = Some(Confirmation::Close(HashMap::new()));
            }
        }

        self.confirm(ctx);
        if let Some(recovery) = &mut self.recovery {
            self.recovery_error = recovery
                .sync(ctx, &self.documents)
                .err()
                .map(|error| format!("Draft recovery: {error}"));
        }
    }
}

fn run_shortcut_shell(script: &str, directory: Option<&Path>) -> Result<(), String> {
    #[cfg(unix)]
    let mut shell = {
        let mut shell = std::process::Command::new("/bin/sh");
        shell.args(["-c", script]);
        shell
    };
    #[cfg(windows)]
    let mut shell = {
        use std::os::windows::process::CommandExt;

        let executable = env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join("System32/cmd.exe"))
            .unwrap_or_else(|| PathBuf::from("cmd.exe"));
        let mut shell = std::process::Command::new(executable);
        shell
            .args(["/D", "/S", "/C"])
            .raw_arg(format!("\"{script}\""))
            .creation_flags(0x08000000);
        shell
    };
    if let Some(directory) = directory {
        shell.current_dir(directory);
    }
    let status = shell
        .env(command::WINDOW_ENV, std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| io_message(&error))?;
    if !status.success() {
        return Err(status.to_string());
    }
    Ok(())
}

fn check_config(path: &Path) -> Reply {
    if !path.try_exists().map_err(|error| io_message(&error))? {
        return Err(format!(
            "Config not found: {}. Launch Synth to create it.",
            path.display()
        ));
    }
    Config::load(path)?;
    Ok(format!("Config OK: {}", path.display()))
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    #[cfg(windows)]
    if !args.is_empty() {
        unsafe {
            use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
    if args.len() == 1 && matches!(args[0].to_str(), Some("help" | "--help" | "-h")) {
        print!("{}", command::help());
        return Ok(());
    }
    if args.len() == 1 && args[0] == "--licenses" {
        let mut notices = String::new();
        for license in two_face::acknowledgement::listing().for_syntaxes() {
            license.write_md(&mut notices);
        }
        print!(
            "{}\n{}\n{notices}",
            include_str!("../assets/fonts/Mono-OFL.txt"),
            include_str!("../assets/fonts/Sans-OFL.txt")
        );
        return Ok(());
    }
    let installation = config::installation_path()?;
    let config_path = Config::path(&installation)?;
    let mut initial_documents = Documents::default();
    if !args.is_empty() {
        let command = Command::parse_cli(&args, &env::current_dir()?)?;
        let reply = match &command {
            Command::ConfigPath => Ok(config_path.display().to_string()),
            Command::ConfigCheck => check_config(&config_path),
            Command::Open(path) => match command::dispatch_if_running(&installation, &command)? {
                Some(reply) => reply,
                None => {
                    initial_documents.open(Path::new(path))?;
                    Ok(String::new())
                }
            },
            _ => command::dispatch(&installation, &command),
        }?;
        if !reply.is_empty() {
            println!("{reply}");
        }
        if initial_documents.current().path.is_none() {
            return Ok(());
        }
    }
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../packaging/synth.png"))?;
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("synth")
            .with_icon(icon)
            .with_inner_size([1040.0, 680.0])
            .with_min_inner_size([480.0, 300.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Synth",
        native,
        Box::new(move |cc| {
            let mut app = Synth::new(&cc.egui_ctx, installation, config_path);
            app.documents = initial_documents;
            Ok(Box::new(app))
        }),
    )?;
    Ok(())
}

fn io_message(error: &io::Error) -> String {
    if let Some(code) = error.raw_os_error() {
        format!("{} (OS error {code})", io::Error::from(error.kind()))
    } else {
        error.to_string()
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(error) = error.downcast_ref::<io::Error>() {
                eprintln!("Synth: {}", io_message(error));
            } else {
                eprintln!("Synth: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_shortcuts_use_shell_syntax_and_the_document_directory() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let script = "printf '%s' 'hello world' | cat > shortcut.txt";
        #[cfg(windows)]
        let script = "echo hello world>shortcut.txt";
        run_shortcut_shell(script, Some(dir.path())).unwrap();
        let output = std::fs::read_to_string(dir.path().join("shortcut.txt")).unwrap();
        assert_eq!(output.trim_end(), "hello world");
        assert!(run_shortcut_shell("exit 7", Some(dir.path())).is_err());
    }
}
