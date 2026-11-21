use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;

use crate::{
    animation::{Animation, Direction, Style},
    command::Command,
};

pub type Bindings = Vec<(egui::KeyboardShortcut, Command)>;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub editor: Editor,
    pub terminal: Terminal,
    pub interface: Interface,
    pub animations: Animations,
    keybindings: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Editor {
    pub theme: Theme,
    pub font_size: f32,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            font_size: 16.0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Dark,
    Light,
}

impl Theme {
    pub fn visuals(self) -> egui::Visuals {
        let (mut visuals, background, foreground, selection) = match self {
            Self::Dark => (
                egui::Visuals::dark(),
                Color32::from_rgb(18, 23, 33),
                Color32::from_rgb(221, 231, 243),
                Color32::from_rgb(45, 68, 96),
            ),
            Self::Light => (
                egui::Visuals::light(),
                Color32::from_rgb(246, 249, 253),
                Color32::from_rgb(28, 43, 61),
                Color32::from_rgb(179, 214, 247),
            ),
        };
        visuals.panel_fill = background;
        visuals.override_text_color = Some(foreground);
        visuals.selection.bg_fill = selection;
        visuals
    }

    pub fn terminal_background(self) -> Color32 {
        match self {
            Self::Dark => Color32::from_rgb(13, 18, 27),
            Self::Light => Color32::from_rgb(234, 241, 249),
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Terminal {
    pub side: Side,
    pub width_fraction: f32,
    animation_ms: Option<u16>,
    pub font_size: f32,
    pub toggle_key: String,
    pub shell: Option<String>,
    pub args: Vec<String>,
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            side: Side::Right,
            width_fraction: 0.44,
            animation_ms: None,
            font_size: 14.0,
            toggle_key: "Esc".into(),
            shell: None,
            args: Vec::new(),
        }
    }
}

impl Terminal {
    pub fn shortcut(&self) -> Result<Option<egui::KeyboardShortcut>, String> {
        if self.toggle_key.trim().is_empty() {
            return Ok(None);
        }
        parse_shortcut(&self.toggle_key).map(Some)
    }
}

fn parse_shortcut(text: &str) -> Result<egui::KeyboardShortcut, String> {
    let mut parts = text.split('+').map(str::trim);
    let key = parts
        .next_back()
        .and_then(egui::Key::from_name)
        .ok_or_else(|| format!("Invalid keyboard shortcut: {text}"))?;
    let mut modifiers = egui::Modifiers::NONE;
    for part in parts {
        match part {
            "Ctrl" => modifiers.ctrl = true,
            "Alt" => modifiers.alt = true,
            "Shift" => modifiers.shift = true,
            "Cmd" => modifiers.mac_cmd = true,
            _ => return Err(format!("Invalid keyboard modifier: {part}")),
        }
    }
    Ok(egui::KeyboardShortcut::new(modifiers, key))
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Interface {
    pub enable_tabs: bool,
    pub enable_line_numbers: bool,
    pub enable_syntax_highlighting: bool,
    pub enable_status_bar: bool,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalAnimation {
    pub open_ms: u16,
    pub close_ms: u16,
    #[serde(alias = "easing")]
    pub direction: Direction,
    pub style: Style,
}

impl Default for TerminalAnimation {
    fn default() -> Self {
        Self {
            open_ms: 180,
            close_ms: 140,
            direction: Direction::Out,
            style: Style::Quadratic,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Animations {
    pub enabled: bool,
    pub terminal: Option<TerminalAnimation>,
    pub tabs: Animation,
    pub status_bar: Animation,
    pub line_numbers: Animation,
}

impl Default for Animations {
    fn default() -> Self {
        Self {
            enabled: true,
            terminal: None,
            tabs: Animation::default(),
            line_numbers: Animation::default(),
            status_bar: Animation {
                duration_ms: 100,
                direction: Direction::Out,
                style: Style::Linear,
            },
        }
    }
}

pub struct ConfigWatch {
    _watcher: RecommendedWatcher,
    changes: mpsc::Receiver<notify::Result<()>>,
    next_reload: Option<Instant>,
}

impl ConfigWatch {
    pub fn new(path: &Path, ctx: egui::Context) -> notify::Result<Self> {
        let directory = path
            .parent()
            .ok_or_else(|| notify::Error::generic("Config has no parent directory"))?;
        let filename = path.file_name().map(ToOwned::to_owned);
        let (sender, changes) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            let changed = result.map(|event| {
                event.need_rescan()
                    || (!matches!(event.kind, EventKind::Access(_))
                        && event
                            .paths
                            .iter()
                            .any(|path| path.file_name() == filename.as_deref()))
            });
            match changed {
                Ok(false) => return,
                Ok(true) => {
                    let _ = sender.send(Ok(()));
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                }
            }
            ctx.request_repaint();
        })?;
        watcher.watch(directory, RecursiveMode::NonRecursive)?;
        Ok(Self {
            _watcher: watcher,
            changes,
            next_reload: None,
        })
    }

    pub fn ready(&mut self, ctx: &egui::Context) -> Result<bool, String> {
        let now = Instant::now();
        for change in self.changes.try_iter() {
            change.map_err(|error| format!("Config watcher: {error}"))?;
            self.next_reload = Some(now + Duration::from_millis(120));
        }
        if let Some(deadline) = self.next_reload {
            if deadline <= now {
                self.next_reload = None;
                return Ok(true);
            }
            ctx.request_repaint_after(deadline.duration_since(now));
        }
        Ok(false)
    }
}

pub fn installation_path() -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(path) = env::var_os("APPIMAGE") {
        return Ok(path.into());
    }

    let executable = env::current_exe()?;
    #[cfg(target_os = "macos")]
    if let Some(bundle) = executable
        .ancestors()
        .find(|path| path.extension().is_some_and(|ext| ext == "app"))
    {
        return Ok(bundle.to_path_buf());
    }
    Ok(executable)
}

impl Config {
    pub fn path(installation: &Path) -> PathBuf {
        installation.with_file_name("config.toml")
    }

    pub fn load(path: &Path) -> Result<(Self, Bindings), String> {
        let config = match fs::read_to_string(path) {
            Ok(source) => {
                Self::parse(&source).map_err(|error| format!("{}: {error}", path.display()))?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let bindings = config
            .bindings(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok((config, bindings))
    }

    pub fn bindings(&self, path: &Path) -> Result<Bindings, String> {
        let Some(configured) = &self.keybindings else {
            return Ok(self
                .terminal
                .shortcut()?
                .into_iter()
                .map(|key| (key, Command::ToggleTerminal))
                .collect());
        };
        let directory = path.parent().ok_or("Config has no parent directory")?;
        let mut bindings = Vec::new();
        for (key, text) in configured {
            let shortcut = parse_shortcut(key)?;
            if bindings.iter().any(|(existing, _)| *existing == shortcut) {
                return Err(format!("Duplicate shortcut: {key}"));
            }
            let words = shell_words::split(text).map_err(|error| format!("{key}: {error}"))?;
            let (program, args) = words
                .split_first()
                .ok_or_else(|| format!("{key}: Expected synth command"))?;
            if program != "synth" {
                return Err(format!("{key}: Expected synth command"));
            }
            let args = args.iter().map(Into::into).collect::<Vec<_>>();
            let command =
                Command::parse(&args, directory).map_err(|error| format!("{key}: {error}"))?;
            bindings.push((shortcut, command));
        }
        Ok(bindings)
    }

    pub fn terminal_animation(&self, opening: bool) -> Animation {
        let configured = self.animations.terminal.unwrap_or_else(|| {
            if let Some(duration) = self.terminal.animation_ms {
                TerminalAnimation {
                    open_ms: duration,
                    close_ms: duration,
                    ..Default::default()
                }
            } else {
                TerminalAnimation::default()
            }
        });
        Animation {
            duration_ms: if opening {
                configured.open_ms
            } else {
                configured.close_ms
            },
            direction: configured.direction,
            style: configured.style,
        }
    }

    fn parse(source: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(source).map_err(|error| error.to_string())?;
        if !(8.0..=48.0).contains(&config.editor.font_size)
            || !(8.0..=48.0).contains(&config.terminal.font_size)
        {
            return Err("Font sizes must be between 8 and 48".into());
        }
        if !(0.15..=0.85).contains(&config.terminal.width_fraction) {
            return Err("terminal.width_fraction must be between 0.15 and 0.85".into());
        }
        if config
            .terminal
            .shell
            .as_ref()
            .is_some_and(|shell| shell.trim().is_empty())
        {
            return Err("terminal.shell must name an executable".into());
        }
        if config.terminal.shell.is_none() && !config.terminal.args.is_empty() {
            return Err("terminal.args requires terminal.shell".into());
        }
        config.terminal.shortcut()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_uses_default_shortcut_and_panel() {
        let config = Config::parse(include_str!("../config.toml")).unwrap();
        assert!(matches!(config.terminal.side, Side::Right));
        assert_eq!(
            config.terminal.shortcut().unwrap(),
            Some(egui::KeyboardShortcut::new(
                egui::Modifiers::NONE,
                egui::Key::Escape
            ))
        );
    }

    #[test]
    fn shortcut_can_be_changed_or_disabled() {
        let changed = Config::parse("[terminal]\ntoggle_key = 'Ctrl+F2'").unwrap();
        assert_eq!(
            changed.terminal.shortcut().unwrap(),
            Some(egui::KeyboardShortcut::new(
                egui::Modifiers::CTRL,
                egui::Key::F2
            ))
        );
        let disabled = Config::parse("[terminal]\ntoggle_key = ''").unwrap();
        assert_eq!(disabled.terminal.shortcut().unwrap(), None);
    }

    #[test]
    fn invalid_terminal_settings_are_reported() {
        assert!(Config::parse("[terminal]\nwidth_fraction = 1.0").is_err());
        assert!(Config::parse("[terminal]\ntoggle_key = 'Hyper+X'").is_err());
    }
}
