#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod command;
mod config;
mod terminal;

use std::{env, path::PathBuf, process::ExitCode, sync::mpsc};

use config::{Config, Side};
use eframe::egui::{self, FontId};
use terminal::Terminal;

struct Synth {
    config: Config,
    shortcut: Option<egui::KeyboardShortcut>,
    commands: Option<mpsc::Receiver<()>>,
    editor: String,
    terminal: Option<Terminal>,
    terminal_open: bool,
    terminal_error: Option<String>,
    config_error: Option<String>,
    focus_editor: bool,
    focus_terminal: bool,
}

impl Synth {
    fn new(ctx: &egui::Context, installation: PathBuf) -> Self {
        let (config, config_error) = match Config::load(&installation) {
            Ok(config) => (config, None),
            Err(error) => (Config::default(), Some(error)),
        };
        let commands = command::listen(&installation, ctx.clone()).map_err(|error| {
            eprintln!("Synth command listener: {error}");
        });
        ctx.set_visuals(config.editor.theme.visuals());
        Self {
            shortcut: config.terminal.shortcut().ok().flatten(),
            config,
            commands: commands.ok(),
            editor: String::new(),
            terminal: None,
            terminal_open: false,
            terminal_error: None,
            config_error,
            focus_editor: true,
            focus_terminal: false,
        }
    }

    fn toggle_terminal(&mut self) {
        self.terminal_open = !self.terminal_open;
        self.focus_terminal = self.terminal_open;
        self.focus_editor = !self.terminal_open;
    }

    fn terminal_panel(&mut self, ctx: &egui::Context) {
        let progress = ctx.animate_bool_with_time(
            egui::Id::new("terminal animation"),
            self.terminal_open,
            f32::from(self.config.terminal.animation_ms) / 1000.0,
        );
        if progress == 0.0 {
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
                        Err(error) => self.terminal_error = Some(error.to_string()),
                    }
                }
                if let Some(terminal) = &mut self.terminal {
                    let foreground = ui.visuals().text_color();
                    let exited = terminal.show(
                        ui,
                        rect,
                        self.terminal_open && std::mem::take(&mut self.focus_terminal),
                        self.terminal_open,
                        foreground,
                        background,
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
        let commands = self
            .commands
            .as_ref()
            .map_or(0, |queue| queue.try_iter().count());
        let shortcut = self.shortcut.is_some_and(|shortcut| {
            ctx.input_mut(|input| {
                let mut toggle = false;
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
                        && *key == shortcut.logical_key
                        && modifiers.matches_exact(shortcut.modifiers)
                    {
                        toggle ^= *pressed && !*repeat;
                        discard_text = *pressed && key.name().len() == 1;
                        return false;
                    }
                    true
                });
                toggle
            })
        });
        if (commands % 2 == 1) ^ shortcut {
            self.toggle_terminal();
        }

        self.terminal_panel(ctx);
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(18))
            .show(ctx, |ui| {
                if let Some(error) = &self.config_error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }

                egui::ScrollArea::vertical().show(ui, |ui| {
                    let edit = egui::TextEdit::multiline(&mut self.editor)
                        .font(FontId::monospace(self.config.editor.font_size))
                        .frame(false)
                        .desired_width(f32::INFINITY)
                        .min_size(ui.available_size());
                    let response = ui.add_sized(ui.available_size(), edit);
                    if std::mem::take(&mut self.focus_editor) {
                        response.request_focus();
                    }
                });
            });
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let installation = config::installation_path()?;
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args == ["terminal", "toggle"] {
        command::toggle(&installation)?;
        return Ok(());
    }
    if !args.is_empty() {
        return Err("Usage: synth [terminal toggle]".into());
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
        Box::new(move |cc| Ok(Box::new(Synth::new(&cc.egui_ctx, installation)))),
    )?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Synth: {error}");
            ExitCode::FAILURE
        }
    }
}
