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
            pending_focus: Some(Focus::Editor),
            #[cfg(windows)]
            maximize_on_startup: true,
        };
        app.bindings = app.config.bindings(&app.config_path).unwrap_or_default();
        ctx.set_visuals(app.config.editor.theme.visuals());
        let _ = app.reload_config(ctx);
        app
    }

    fn reload_config(&mut self, ctx: &egui::Context) -> Reply {
        let result = Config::load(&self.config_path).map(|(config, bindings)| {
            ctx.set_visuals(config.editor.theme.visuals());
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

    fn find(&mut self, query: Option<String>) -> Reply {
        let reply = self.documents.current_mut().find(query)?;
        self.pending_focus = Some(Focus::Editor);
        Ok(reply)
    }

    fn shortcut_commands(&self, ctx: &egui::Context) -> Vec<Command> {
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
                    && let Some((_, command)) = self.bindings.iter().find(|(shortcut, _)| {
                        *key == shortcut.logical_key && modifiers.matches_exact(shortcut.modifiers)
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

        let width = ctx.available_rect().width() * self.config.terminal.width_fraction;
        let panel = match self.config.terminal.side {
            Side::Left => egui::SidePanel::left("terminal"),
            Side::Right => egui::SidePanel::right("terminal"),
        };
        let background = self.config.editor.theme.terminal_background();
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
                        self.terminal_open && self.pending_focus.take() == Some(Focus::Terminal),
                        self.terminal_open,
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
        if ctx.input(|input| input.viewport().close_requested())
            && self.documents.items.iter().any(document::Document::dirty)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.notice = Some("Save or discard changed documents before closing Synth.".into());
        }

        if let Some(index) = editor::tabs(ctx, &self.documents, &self.config) {
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
                for error in [&self.config_error, &self.notice].into_iter().flatten() {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                self.editor
                    .show(ui, self.documents.current_mut(), &self.config);
            });
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
    if !args.is_empty() {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
        let installation = config::installation_path()?;
        if args == ["--help"] || args == ["-h"] {
            println!("{}", command::USAGE);
            return Ok(());
        }
        let config_path = Config::path(&installation)?;
        let command = Command::parse(&args, &env::current_dir()?)?;
        let reply = match command {
            Command::ConfigPath => Ok(config_path.display().to_string()),
            Command::ConfigCheck => check_config(&config_path),
            command => command::dispatch(&installation, &command),
        }?;
        if !reply.is_empty() {
            println!("{reply}");
        }
        return Ok(());
    }
    let installation = config::installation_path()?;

    let config_path = Config::path(&installation)?;
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
            Ok(Box::new(Synth::new(
                &cc.egui_ctx,
                installation,
                config_path,
            )))
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
