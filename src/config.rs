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
    cursor_trail,
};

pub type Bindings = Vec<(egui::KeyboardShortcut, Command)>;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub editor: Editor,
    pub colors: Colors,
    pub terminal: Terminal,
    pub interface: Interface,
    pub animations: Animations,
    pub cursor_trail: cursor_trail::Settings,
    keybinds: Option<BTreeMap<String, Keybind>>,
    keybindings: Option<BTreeMap<String, Keybind>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Keybind {
    Command(String),
    Fallbacks(Vec<String>),
}

impl Keybind {
    fn commands(&self) -> &[String] {
        match self {
            Self::Command(command) => std::slice::from_ref(command),
            Self::Fallbacks(commands) => commands,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Editor {
    pub theme: Theme,
    pub wrap_lines: bool,
    pub recover_unsaved_documents: bool,
    pub font_size: f32,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            wrap_lines: true,
            recover_unsaved_documents: true,
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

#[derive(Clone, Copy)]
struct HexColor(Color32);

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let hex = value
            .strip_prefix('#')
            .filter(|hex| hex.len() == 6)
            .ok_or_else(|| serde::de::Error::custom("Expected a color in #RRGGBB format"))?;
        let rgb = u32::from_str_radix(hex, 16)
            .map_err(|_| serde::de::Error::custom("Expected a color in #RRGGBB format"))?;
        Ok(Self(Color32::from_rgb(
            (rgb >> 16) as u8,
            (rgb >> 8) as u8,
            rgb as u8,
        )))
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    dark: ColorOverrides,
    light: ColorOverrides,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ColorOverrides {
    #[serde(rename = "editor_bg", alias = "editor_background")]
    editor_background: Option<HexColor>,
    #[serde(rename = "text_fg", alias = "foreground")]
    foreground: Option<HexColor>,
    #[serde(rename = "selection_bg", alias = "selection")]
    selection: Option<HexColor>,
    #[serde(rename = "surface_bg", alias = "muted_surface")]
    muted_surface: Option<HexColor>,
    #[serde(rename = "terminal_bg", alias = "terminal_background")]
    terminal_background: Option<HexColor>,
    #[serde(rename = "tab_bar_bg", alias = "tabs_background")]
    tabs_background: Option<HexColor>,
    #[serde(rename = "tab_active_bg", alias = "active_tab_background")]
    active_tab_background: Option<HexColor>,
    #[serde(rename = "tab_active_fg", alias = "tab_text_active")]
    tab_text_active: Option<HexColor>,
    #[serde(rename = "tab_inactive_fg", alias = "tab_text_inactive")]
    tab_text_inactive: Option<HexColor>,
    #[serde(rename = "status_bar_bg", alias = "status_background")]
    status_background: Option<HexColor>,
    #[serde(rename = "decoration_accent_1", alias = "window_dot_red")]
    _legacy_decoration_accent_1: Option<HexColor>,
    #[serde(rename = "decoration_accent_2", alias = "window_dot_yellow")]
    _legacy_decoration_accent_2: Option<HexColor>,
    #[serde(rename = "decoration_accent_3", alias = "window_dot_green")]
    _legacy_decoration_accent_3: Option<HexColor>,
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub editor_background: Color32,
    pub foreground: Color32,
    pub selection: Color32,
    pub muted_surface: Color32,
    pub terminal_background: Color32,
    pub tabs_background: Color32,
    pub active_tab_background: Color32,
    pub tab_text_active: Color32,
    pub tab_text_inactive: Color32,
    pub status_background: Color32,
}

impl Palette {
    fn defaults(theme: Theme) -> Self {
        match theme {
            Theme::Dark => Self {
                editor_background: Color32::from_rgb(17, 19, 24),
                foreground: Color32::from_rgb(214, 220, 232),
                selection: Color32::from_rgb(55, 63, 80),
                muted_surface: Color32::from_rgb(27, 30, 37),
                terminal_background: Color32::from_rgb(23, 27, 34),
                tabs_background: Color32::from_rgb(27, 30, 37),
                active_tab_background: Color32::from_rgb(41, 45, 54),
                tab_text_active: Color32::from_rgb(214, 220, 232),
                tab_text_inactive: Color32::from_rgb(166, 175, 190),
                status_background: Color32::from_rgb(32, 37, 46),
            },
            Theme::Light => Self {
                editor_background: Color32::from_rgb(246, 249, 253),
                foreground: Color32::from_rgb(28, 43, 61),
                selection: Color32::from_rgb(179, 214, 247),
                muted_surface: Color32::from_rgb(234, 239, 246),
                terminal_background: Color32::from_rgb(234, 241, 249),
                tabs_background: Color32::from_rgb(230, 235, 243),
                active_tab_background: Color32::from_rgb(255, 255, 255),
                tab_text_active: Color32::from_rgb(28, 43, 61),
                tab_text_inactive: Color32::from_rgb(92, 103, 118),
                status_background: Color32::from_rgb(234, 239, 246),
            },
        }
    }

    pub fn visuals(self, theme: Theme) -> egui::Visuals {
        let mut visuals = match theme {
            Theme::Dark => egui::Visuals::dark(),
            Theme::Light => egui::Visuals::light(),
        };
        visuals.panel_fill = self.editor_background;
        visuals.override_text_color = Some(self.foreground);
        visuals.selection.bg_fill = self.selection;
        visuals.extreme_bg_color = self.editor_background;
        visuals.faint_bg_color = self.muted_surface;
        visuals
    }

    pub fn tab_text(self, active: bool) -> Color32 {
        if active {
            self.tab_text_active
        } else {
            self.tab_text_inactive
        }
    }
}

impl Colors {
    fn palette(&self, theme: Theme) -> Palette {
        let overrides = match theme {
            Theme::Dark => &self.dark,
            Theme::Light => &self.light,
        };
        let mut palette = Palette::defaults(theme);
        if let Some(value) = overrides.editor_background {
            palette.editor_background = value.0;
        }
        if let Some(value) = overrides.foreground {
            palette.foreground = value.0;
        }
        if let Some(value) = overrides.selection {
            palette.selection = value.0;
        }
        if let Some(value) = overrides.muted_surface {
            palette.muted_surface = value.0;
        }
        if let Some(value) = overrides.terminal_background {
            palette.terminal_background = value.0;
        }
        if let Some(value) = overrides.tabs_background {
            palette.tabs_background = value.0;
        }
        if let Some(value) = overrides.active_tab_background {
            palette.active_tab_background = value.0;
        }
        if let Some(value) = overrides.tab_text_active {
            palette.tab_text_active = value.0;
        }
        if let Some(value) = overrides.tab_text_inactive {
            palette.tab_text_inactive = value.0;
        }
        if let Some(value) = overrides.status_background {
            palette.status_background = value.0;
        }
        palette
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandTarget {
    #[default]
    Window,
    Shared,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Terminal {
    pub side: Side,
    pub width_fraction: f32,
    pub command_target: CommandTarget,
    animation_ms: Option<u16>,
    pub font_size: f32,
    pub toggle_key: Option<String>,
    pub shell: Option<String>,
    pub args: Vec<String>,
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            side: Side::Right,
            width_fraction: 0.44,
            command_target: CommandTarget::default(),
            animation_ms: None,
            font_size: 14.0,
            toggle_key: None,
            shell: None,
            args: Vec::new(),
        }
    }
}

impl Terminal {
    pub fn shortcut(&self) -> Result<Option<egui::KeyboardShortcut>, String> {
        self.toggle_key
            .as_deref()
            .filter(|key| !key.trim().is_empty())
            .map(parse_shortcut)
            .transpose()
    }
}

fn bare_escape(shortcut: egui::KeyboardShortcut) -> bool {
    shortcut.logical_key == egui::Key::Escape && shortcut.modifiers.is_none()
}

fn legacy_escape_commands() -> [Command; 3] {
    [Command::CancelDialog, Command::CloseSearch, Command::ToggleEditorTerminal]
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
            "Command" => modifiers.command = true,
            "Cmd" => modifiers.mac_cmd = true,
            _ => return Err(format!("Invalid keyboard modifier: {part}")),
        }
    }
    Ok(egui::KeyboardShortcut::new(modifiers, key))
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Interface {
    #[serde(alias = "enable_tabs")]
    pub show_tabs: bool,
    #[serde(alias = "enable_line_numbers")]
    pub show_line_numbers: bool,
    #[serde(alias = "enable_syntax_highlighting")]
    pub syntax_highlighting: bool,
    #[serde(alias = "enable_status_bar")]
    pub show_status_bar: bool,
    pub show_external_file_changes: bool,
    pub enable_search_and_replace: bool,
    pub tab_height: f32,
    pub tab_max_chars: usize,
    pub tab_font_size: f32,
    pub tab_min_width: f32,
    #[serde(rename = "tab_new_button")]
    _legacy_tab_new_button: Option<bool>,
    pub editor_padding: i8,
}

impl Default for Interface {
    fn default() -> Self {
        Self {
            show_tabs: false,
            show_line_numbers: false,
            syntax_highlighting: false,
            show_status_bar: false,
            show_external_file_changes: true,
            enable_search_and_replace: false,
            tab_height: 36.0,
            tab_max_chars: 28,
            tab_font_size: 12.0,
            tab_min_width: 96.0,
            _legacy_tab_new_button: None,
            editor_padding: 18,
        }
    }
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
            enabled: false,
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

pub struct FileWatch {
    _watcher: RecommendedWatcher,
    changes: mpsc::Receiver<notify::Result<()>>,
    next_reload: Option<Instant>,
}

impl FileWatch {
    pub fn new(path: &Path, ctx: egui::Context) -> notify::Result<Self> {
        let directory = path
            .parent()
            .ok_or_else(|| notify::Error::generic("File has no parent directory"))?;
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

    pub fn ready(&mut self, ctx: &egui::Context) -> notify::Result<bool> {
        let now = Instant::now();
        for change in self.changes.try_iter() {
            change?;
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn user_home() -> io::Result<PathBuf> {
    env::home_dir()
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::other("Home directory is unavailable"))
}

impl Config {
    pub fn palette(&self) -> Palette {
        self.colors.palette(self.editor.theme)
    }

    pub fn path(_installation: &Path) -> io::Result<PathBuf> {
        #[cfg(windows)]
        return Ok(_installation.with_file_name("config.toml"));

        #[cfg(target_os = "linux")]
        {
            if let Some(directory) = env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
            {
                return Ok(directory.join("synth/config.toml"));
            }
            Ok(user_home()?.join(".config/synth/config.toml"))
        }

        #[cfg(target_os = "macos")]
        Ok(user_home()?.join("Library/Application Support/Synth/config.toml"))
    }

    pub fn ensure_file(path: &Path, installation: &Path) -> io::Result<()> {
        use std::io::Write;

        if fs::symlink_metadata(path).is_ok() {
            return Ok(());
        }
        let directory = path
            .parent()
            .ok_or_else(|| io::Error::other("File has no parent directory"))?;
        fs::create_dir_all(directory)?;
        let legacy = installation.with_file_name("config.toml");
        let default = include_bytes!("../config.toml");
        let contents = if legacy != path {
            match fs::read(legacy) {
                Ok(contents) => contents,
                Err(error) if error.kind() == io::ErrorKind::NotFound => default.to_vec(),
                Err(error) => return Err(error),
            }
        } else {
            default.to_vec()
        };
        let mut output = tempfile::NamedTempFile::new_in(directory)?;
        output.write_all(&contents)?;
        match output.persist_noclobber(path) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error.error),
        }
    }

    pub fn load(path: &Path) -> Result<(Self, Bindings), String> {
        let config = match fs::read_to_string(path) {
            Ok(source) => {
                Self::parse(&source).map_err(|error| format!("{}: {error}", path.display()))?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(format!("{}: {}", path.display(), crate::io_message(&error))),
        };
        let bindings = config
            .bindings(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok((config, bindings))
    }

    pub fn bindings(&self, path: &Path) -> Result<Bindings, String> {
        if self.keybinds.is_some() && self.keybindings.is_some() {
            return Err("Use only one of [keybinds] or [keybindings]".into());
        }
        let Some(configured) = self.keybinds.as_ref().or(self.keybindings.as_ref()) else {
            let Some(shortcut) = self.terminal.shortcut()? else {
                return Ok(Vec::new());
            };
            return Ok(if bare_escape(shortcut) {
                legacy_escape_commands()
                    .into_iter()
                    .map(|command| (shortcut, command))
                    .collect()
            } else {
                vec![(shortcut, Command::ToggleTerminal)]
            });
        };
        let directory = path.parent().ok_or("File has no parent directory")?;
        let mut bindings = Vec::new();
        for (key, binding) in configured {
            let shortcut = parse_shortcut(key)?;
            if bindings.iter().any(|(existing, _)| *existing == shortcut) {
                return Err(format!("Duplicate shortcut: {key}"));
            }
            if binding.commands().is_empty() {
                return Err(format!("{key}: Expected at least one synth command"));
            }
            for text in binding.commands() {
                let words = shell_words::split(text).map_err(|error| format!("{key}: {error}"))?;
                let (program, args) = words.split_first().ok_or_else(|| format!("{key}: Expected synth command"))?;
                if program != "synth" {
                    return Err(format!("{key}: Expected synth command"));
                }
                let args = args.iter().map(Into::into).collect::<Vec<_>>();
                let command = Command::parse(&args, directory).map_err(|error| format!("{key}: {error}"))?;
                if self.keybindings.is_some() && bare_escape(shortcut) && matches!(command, Command::ToggleTerminal) {
                    bindings.extend(legacy_escape_commands().into_iter().map(|command| (shortcut, command)));
                } else {
                    bindings.push((shortcut, command));
                }
            }
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
        if !(28.0..=64.0).contains(&config.interface.tab_height) {
            return Err("interface.tab_height must be between 28 and 64".into());
        }
        if !(8..=128).contains(&config.interface.tab_max_chars) {
            return Err("interface.tab_max_chars must be between 8 and 128".into());
        }
        if !(9.0..=20.0).contains(&config.interface.tab_font_size) {
            return Err("interface.tab_font_size must be between 9 and 20".into());
        }
        if !(60.0..=260.0).contains(&config.interface.tab_min_width) {
            return Err("interface.tab_min_width must be between 60 and 260".into());
        }
        if !(0..=64).contains(&config.interface.editor_padding) {
            return Err("interface.editor_padding must be between 0 and 64".into());
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
        config.cursor_trail.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_interface_visibility_keys_are_still_accepted() {
        let config = Config::parse(
            "[interface]\nenable_tabs = true\nenable_line_numbers = true\nenable_syntax_highlighting = true\nenable_status_bar = true",
        ).unwrap();
        assert!(config.interface.show_tabs);
        assert!(config.interface.show_line_numbers);
        assert!(config.interface.syntax_highlighting);
        assert!(config.interface.show_status_bar);
    }

    #[test]
    fn example_uses_default_shortcut_and_panel() {
        let config = Config::parse(include_str!("../config.toml")).unwrap();
        assert!(matches!(config.terminal.side, Side::Right));
        assert!(matches!(config.terminal.command_target, CommandTarget::Window));
        assert!(!config.animations.enabled);
        assert_eq!(config.cursor_trail.target, cursor_trail::Target::None);
        assert!(config.editor.wrap_lines);
        assert!(config.editor.recover_unsaved_documents);
        assert!(!config.interface.enable_search_and_replace);
        assert!(!config.interface.show_tabs);
        assert!(!config.interface.show_line_numbers);
        assert!(!config.interface.syntax_highlighting);
        assert!(!config.interface.show_status_bar);
        let bindings = config.bindings(Path::new("/tmp/config.toml")).unwrap();
        assert!(bindings.iter().any(|(shortcut, command)|
            *shortcut == egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::Escape)
                && matches!(command, Command::ToggleEditorTerminal)
        ));
        assert!(bindings.iter().any(|(shortcut, command)|
            *shortcut == egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Escape)
                && matches!(command, Command::HideTerminal)
        ));
    }

    #[test]
    fn first_launch_materializes_bindings_and_leaves_user_changes_intact() {
        let workspace = tempfile::tempdir().unwrap();
        let installation = workspace.path().join("synth");
        let path = workspace.path().join("settings/config.toml");
        assert!(Config::load(&path).unwrap().1.is_empty());

        Config::ensure_file(&path, &installation).unwrap();
        assert_eq!(Config::load(&path).unwrap().1.len(), 7);
        fs::write(&path, "[keybindings]").unwrap();
        Config::ensure_file(&path, &installation).unwrap();
        assert!(Config::load(&path).unwrap().1.is_empty());
    }

    #[test]
    fn search_shortcut_uses_the_configured_table() {
        let path = Path::new("config.toml");
        let empty = Config::parse("[interface]\nenable_search_and_replace=true\n[keybindings]").unwrap();
        assert!(empty.bindings(path).unwrap().is_empty());
        let custom = Config::parse("[interface]\nenable_search_and_replace=true\n[keybindings]\n'Alt+F'='synth search toggle'").unwrap();
        let bindings = custom.bindings(path).unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].0, egui::KeyboardShortcut::new(egui::Modifiers::ALT, egui::Key::F));
        assert!(matches!(bindings[0].1, Command::ToggleSearch));
    }

    #[test]
    fn keybinds_keep_contextual_fallbacks_in_order() {
        let path = Path::new("config.toml");
        let config = Config::parse(
            "[keybinds]\nF7 = ['synth dialog cancel', 'synth search close', 'synth terminal toggle-edit']",
        ).unwrap();
        let bindings = config.bindings(path).unwrap();
        let commands: Vec<_> = bindings.into_iter().map(|(_, cmd)| cmd).collect();
        assert!(matches!(&commands[..], [Command::CancelDialog, Command::CloseSearch, Command::ToggleEditorTerminal]));
        assert!(Config::parse("[keybinds]\n[keybindings]").unwrap().bindings(path).is_err());
        assert!(Config::parse("[keybinds]\nF7 = []").unwrap().bindings(path).is_err());
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

    #[test]
    fn interface_layout_limits_are_validated() {
        assert!(Config::parse("[interface]\ntab_height = 24.0").is_err());
        assert!(Config::parse("[interface]\ntab_max_chars = 0").is_err());
        assert!(Config::parse("[interface]\neditor_padding = 65").is_err());
        let config = Config::parse(
            "[interface]\ntab_height = 42.0\ntab_max_chars = 32\neditor_padding = 8",
        )
        .unwrap();
        assert_eq!(config.interface.tab_height, 42.0);
        assert_eq!(config.interface.tab_max_chars, 32);
        assert_eq!(config.interface.editor_padding, 8);
    }

    #[test]
    fn dark_palette_matches_tab_reference() {
        let dark = Palette::defaults(Theme::Dark);
        let light = Palette::defaults(Theme::Light);
        assert_eq!(dark.tabs_background, Color32::from_rgb(27, 30, 37));
        assert_eq!(dark.active_tab_background, Color32::from_rgb(41, 45, 54));
        assert_eq!(dark.terminal_background, Color32::from_rgb(23, 27, 34));
        assert_eq!(dark.status_background, Color32::from_rgb(32, 37, 46));
        assert_ne!(light.tabs_background, dark.tabs_background);
    }

    #[test]
    fn palette_overrides_are_partial_and_theme_specific() {
        let config = Config::parse(
            "[colors.dark]\neditor_bg = '#12A0ef'\ntab_inactive_fg = '#000000'\n[colors.light]\neditor_bg = '#445566'",
        )
        .unwrap();
        let dark = config.colors.palette(Theme::Dark);
        let light = config.colors.palette(Theme::Light);
        assert_eq!(dark.editor_background, Color32::from_rgb(18, 160, 239));
        assert_eq!(dark.tab_text_inactive, Color32::BLACK);
        assert_eq!(dark.terminal_background, Palette::defaults(Theme::Dark).terminal_background);
        assert_eq!(light.editor_background, Color32::from_rgb(68, 85, 102));
        assert_eq!(light.tab_text_inactive, Palette::defaults(Theme::Light).tab_text_inactive);
    }

    #[test]
    fn renamed_color_options_accept_legacy_aliases() {
        let fields = [
            ("editor_background", "editor_bg"),
            ("foreground", "text_fg"),
            ("selection", "selection_bg"),
            ("muted_surface", "surface_bg"),
            ("terminal_background", "terminal_bg"),
            ("tabs_background", "tab_bar_bg"),
            ("active_tab_background", "tab_active_bg"),
            ("tab_text_active", "tab_active_fg"),
            ("tab_text_inactive", "tab_inactive_fg"),
            ("status_background", "status_bar_bg"),
        ];
        for (legacy, current) in fields {
            let old = Config::parse(&format!("[colors.dark]\n{legacy} = '#123ABC'"))
                .unwrap()
                .palette();
            let new = Config::parse(&format!("[colors.dark]\n{current} = '#123ABC'"))
                .unwrap()
                .palette();
            assert_eq!(old.editor_background, new.editor_background, "{current}");
            assert_eq!(old.foreground, new.foreground, "{current}");
            assert_eq!(old.selection, new.selection, "{current}");
            assert_eq!(old.muted_surface, new.muted_surface, "{current}");
            assert_eq!(old.terminal_background, new.terminal_background, "{current}");
            assert_eq!(old.tabs_background, new.tabs_background, "{current}");
            assert_eq!(old.active_tab_background, new.active_tab_background, "{current}");
            assert_eq!(old.tab_text_active, new.tab_text_active, "{current}");
            assert_eq!(old.tab_text_inactive, new.tab_text_inactive, "{current}");
            assert_eq!(old.status_background, new.status_background, "{current}");
            let both = format!(
                "[colors.dark]\n{legacy} = '#123ABC'\n{current} = '#654321'"
            );
            assert!(Config::parse(&both).is_err(), "{legacy} and {current}");
        }
    }

    #[test]
    fn legacy_decoration_colors_remain_accepted_and_unused() {
        let baseline = Config::parse("[colors.dark]").unwrap().palette();
        let legacy = Config::parse(
            "[colors.dark]\nwindow_dot_red = '#010203'\nwindow_dot_yellow = '#040506'\nwindow_dot_green = '#070809'",
        )
        .unwrap()
        .palette();
        let recent = Config::parse(
            "[colors.dark]\ndecoration_accent_1 = '#010203'\ndecoration_accent_2 = '#040506'\ndecoration_accent_3 = '#070809'",
        )
        .unwrap()
        .palette();
        assert_eq!(baseline.editor_background, legacy.editor_background);
        assert_eq!(baseline.editor_background, recent.editor_background);
        assert_eq!(baseline.tabs_background, legacy.tabs_background);
        assert_eq!(baseline.tabs_background, recent.tabs_background);
    }

    #[test]
    fn invalid_hex_colors_are_rejected() {
        for color in ["'123456'", "'#12345'", "'#GG0000'", "'#12345678'", "42"] {
            let source = format!("[colors.dark]\neditor_bg = {color}");
            assert!(Config::parse(&source).is_err(), "{color}");
        }
        assert!(Config::parse("[colors.dark]\nunknown = '#010203'").is_err());
        assert!(Config::parse("[colors.unknown]").is_err());
    }

    #[test]
    fn sample_preserves_original_palettes() {
        let config = Config::parse(include_str!("../config.toml")).unwrap();
        let dark = config.colors.palette(Theme::Dark);
        let light = config.colors.palette(Theme::Light);
        assert_eq!(dark.editor_background, Palette::defaults(Theme::Dark).editor_background);
        assert_eq!(dark.terminal_background, Palette::defaults(Theme::Dark).terminal_background);
        assert_eq!(light.editor_background, Palette::defaults(Theme::Light).editor_background);
        assert_eq!(light.active_tab_background, Palette::defaults(Theme::Light).active_tab_background);
    }

    #[test]
    fn legacy_new_tab_button_setting_does_not_change_the_interface() {
        let old_config = Config::parse("[interface]\ntab_new_button = true").unwrap();
        let new_config = Config::parse("[interface]").unwrap();
        assert_eq!(old_config.interface.tab_height, new_config.interface.tab_height);
        assert_eq!(old_config.interface.tab_min_width, new_config.interface.tab_min_width);
        assert!(Config::parse("[interface]\ntab_new_button = false").is_ok());
    }

    #[test]
    fn configurable_tab_dimensions_validate() {
        assert!(Config::parse("[interface]\ntab_font_size = 8.0").is_err());
        assert!(Config::parse("[interface]\ntab_min_width = 40.0").is_err());
        let config = Config::parse(
            "[interface]\ntab_font_size = 13.0\ntab_min_width = 120.0",
        )
        .unwrap();
        assert_eq!(config.interface.tab_font_size, 13.0);
        assert_eq!(config.interface.tab_min_width, 120.0);
    }


}
