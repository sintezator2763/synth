use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use eframe::egui::{self, Color32};
use serde::Deserialize;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub editor: Editor,
    pub terminal: Terminal,
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

#[derive(Clone, Copy, Deserialize)]
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
    pub animation_ms: u16,
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
            animation_ms: 180,
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
        let mut parts = self.toggle_key.split('+').map(str::trim);
        let key = parts
            .next_back()
            .and_then(egui::Key::from_name)
            .ok_or_else(|| format!("Invalid terminal.toggle_key: {}", self.toggle_key))?;
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
        Ok(Some(egui::KeyboardShortcut::new(modifiers, key)))
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
    pub fn load(installation: &Path) -> Result<Self, String> {
        let path = installation.with_file_name("config.toml");
        match fs::read_to_string(&path) {
            Ok(source) => {
                Self::parse(&source).map_err(|error| format!("{}: {error}", path.display()))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
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
