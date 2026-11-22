#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod animation;
mod command;
mod config;
mod cursor_trail;
mod document;
mod editor;
mod graphics;
mod terminal;

use std::{
    collections::HashMap,
    env, io,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::mpsc,
};

use command::{Command, Reply, Request};
use config::{Bindings, Config, ConfigWatch, Side};
use document::Documents;
use editor::Editor;
use eframe::egui::{self, FontId};
use terminal::Terminal;

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Editor,
    Terminal,
}

enum Confirmation {
    Reload(u64),
    Close(HashMap<u64, String>),
}

struct Synth {
    config: Config,
    config_path: PathBuf,
    bindings: Bindings,
    watcher: Option<ConfigWatch>,
    commands: Option<mpsc::Receiver<Request>>,
    documents: Documents,
    editor: Editor,
    terminal: Option<Terminal>,
    terminal_open: bool,
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
        let setup_error = Config::ensure_file(&config_path, &installation)
            .err()
            .map(|error| {
                format!(
                    "Could not create {}: {}",
                    config_path.display(),
                    io_message(&error)
                )
            });
        let (watcher, watch_error) = match ConfigWatch::new(&config_path, ctx.clone()) {
            Ok(watcher) => (Some(watcher), None),
            Err(error) => (None, Some(format!("Config watcher: {error}"))),
        };
        let commands = command::listen(&installation, ctx.clone()).map_err(|error| {
            let message = io_message(&error);
            eprintln!("Synth command listener: {message}");
            format!("Synth command listener: {message}")
        });
        let mut app = Self {
            config: Config::default(),
            config_path,
            bindings: Vec::new(),
            watcher,
            commands: commands.ok(),
            documents: Documents::default(),
            editor: Editor::default(),
            terminal: None,
            terminal_open: false,
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
            self.bindings = bindings;
            self.config = config;
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
        if self.confirmation.is_some() {
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
        if canceled || result.should_close() {
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
        let directory = env::current_dir().map_err(|error| io_message(&error))?;
        for document in self
            .documents
            .items
            .iter_mut()
            .filter(|document| document.dirty())
        {
            let destination = paths.get(&document.id).map(|path| directory.join(path));
            document.save(destination.as_deref())?;
        }
        Ok(())
    }

    fn find(&mut self, query: Option<String>) -> Reply {
        let reply = self.documents.current_mut().find(query)?;
        self.pending_focus = Some(Focus::Editor);
        Ok(reply)
    }

    fn shortcut_commands(&self, ctx: &egui::Context) -> Vec<Command> {
        if self.confirmation.is_some() {
            return Vec::new();
        }
        let terminal_focused = self.focus(ctx) == Focus::Terminal;
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
                            && !(terminal_focused
                                && *key == egui::Key::Escape
                                && modifiers.is_none()
                                && matches!(command, Command::ToggleTerminal))
                    })
                {
                    if *pressed && !*repeat {
                        commands.push(command.clone());
                    }
                    discard_text = *pressed && key.name().len() == 1;
                    return false;
                }
                true
            });
            commands
        })
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
            }
            return;
        }

        let interactive = self.terminal_open && self.confirmation.is_none();
        let width = ctx.available_rect().width() * self.config.terminal.width_fraction;
        let panel = match self.config.terminal.side {
            Side::Left => egui::SidePanel::left("terminal"),
            Side::Right => egui::SidePanel::right("terminal"),
        };
        let background = self.config.palette().terminal_background;
        panel
            .resizable(false)
            .show_separator_line(false)
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
                    terminal.set_font_size(self.config.terminal.font_size);
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
                        FontId::monospace(self.config.terminal.font_size),
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
                Err(error) => self.config_error = Some(error),
            }
        }
        while let Some(request) = self
            .commands
            .as_ref()
            .and_then(|queue| queue.try_recv().ok())
        {
            let reply = self.execute(ctx, request.command);
            let _ = request.respond.send(reply);
        }
        for command in self.shortcut_commands(ctx) {
            if let Err(error) = self.execute(ctx, command) {
                self.notice = Some(error);
            }
        }
        if let Some(index) = editor::tabs(ctx, &self.documents, &self.config)
            && self.confirmation.is_none()
        {
            self.documents.active = index;
            self.pending_focus = Some(Focus::Editor);
        }
        editor::status(ctx, self.documents.current(), &self.config);
        if self.pending_focus == Some(Focus::Editor) {
            let id = Editor::id(self.documents.current());
            ctx.memory_mut(|memory| memory.request_focus(id));
            self.pending_focus = None;
        }
        self.terminal_panel(ctx);
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(self.config.interface.editor_padding))
            .show(ctx, |ui| {
                let modal_open = self.confirmation.is_some();
                let notice = if modal_open {
                    None
                } else {
                    self.notice.as_ref()
                };
                for error in [self.config_error.as_ref(), notice].into_iter().flatten() {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                ui.add_enabled_ui(!modal_open, |ui| {
                    self.editor
                        .show(ui, self.documents.current_mut(), &self.config);
                });
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
    }
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
