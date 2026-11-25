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

pub const FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 8.0..=48.0;

pub type Bindings = Vec<(egui::KeyboardShortcut, ShortcutAction)>;

#[derive(Clone)]
pub enum ShortcutAction {
    Synth(Command),
    Shell(String),
}

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
    pub tab_size: usize,
    pub insert_spaces: bool,
    pub auto_indent: bool,
    pub formatter: Option<String>,
    pub wrap_lines: bool,
    pub recover_unsaved_documents: bool,
    pub font_size: f32,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            tab_size: 4,
            insert_spaces: false,
            auto_indent: false,
            formatter: None,
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
            .filter(|hex| hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    mode: Theme,
    #[serde(default)]
    palette: ColorOverrides,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ColorOverrides {
    #[serde(alias = "accent")]
    primary: Option<HexColor>,
    #[serde(alias = "syntax_string")]
    secondary: Option<HexColor>,
    #[serde(alias = "syntax_literal")]
    tertiary: Option<HexColor>,
    #[serde(alias = "syntax_comment")]
    outline: Option<HexColor>,
    #[serde(alias = "selection_bg", alias = "selection")]
    primary_container: Option<HexColor>,
    #[serde(alias = "tab_active_bg", alias = "active_tab_background")]
    secondary_container: Option<HexColor>,
    #[serde(alias = "tab_active_fg", alias = "tab_text_active")]
    on_secondary_container: Option<HexColor>,
    #[serde(alias = "editor_bg", alias = "editor_background")]
    surface: Option<HexColor>,
    #[serde(alias = "text_fg", alias = "foreground")]
    on_surface: Option<HexColor>,
    #[serde(alias = "tab_inactive_fg", alias = "tab_text_inactive")]
    on_surface_variant: Option<HexColor>,
    #[serde(alias = "terminal_bg", alias = "terminal_background")]
    surface_container_lowest: Option<HexColor>,
    #[serde(alias = "current_line")]
    surface_container_low: Option<HexColor>,
    #[serde(alias = "tab_bar_bg", alias = "tabs_background")]
    surface_container: Option<HexColor>,
    #[serde(alias = "status_bar_bg", alias = "status_background")]
    surface_container_high: Option<HexColor>,
    #[serde(alias = "surface_bg", alias = "muted_surface")]
    surface_container_highest: Option<HexColor>,
    error: Option<HexColor>,
    #[serde(rename = "decoration_accent_1", alias = "window_dot_red")]
    _legacy_decoration_accent_1: Option<HexColor>,
    #[serde(rename = "decoration_accent_2", alias = "window_dot_yellow")]
    _legacy_decoration_accent_2: Option<HexColor>,
    #[serde(rename = "decoration_accent_3", alias = "window_dot_green")]
    _legacy_decoration_accent_3: Option<HexColor>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub primary: Color32,
    pub secondary: Color32,
    pub tertiary: Color32,
    pub outline: Color32,
    pub primary_container: Color32,
    pub secondary_container: Color32,
    pub on_secondary_container: Color32,
    pub surface: Color32,
    pub on_surface: Color32,
    pub on_surface_variant: Color32,
    pub surface_container_lowest: Color32,
    pub surface_container_low: Color32,
    pub surface_container: Color32,
    pub surface_container_high: Color32,
    pub surface_container_highest: Color32,
    error: Option<Color32>,
}

impl Palette {
    fn defaults(theme: Theme) -> Self {
        match theme {
            Theme::Dark => Self {
                primary: Color32::from_rgb(222, 145, 109),
                secondary: Color32::from_rgb(173, 190, 145),
                tertiary: Color32::from_rgb(222, 184, 131),
                outline: Color32::from_rgb(156, 151, 140),
                primary_container: Color32::from_rgb(85, 69, 59),
                secondary_container: Color32::from_rgb(52, 51, 47),
                on_secondary_container: Color32::from_rgb(243, 233, 215),
                surface: Color32::from_rgb(34, 35, 34),
                on_surface: Color32::from_rgb(232, 223, 206),
                on_surface_variant: Color32::from_rgb(183, 174, 161),
                surface_container_lowest: Color32::from_rgb(28, 29, 28),
                surface_container_low: Color32::from_rgb(42, 43, 41),
                surface_container: Color32::from_rgb(41, 41, 39),
                surface_container_high: Color32::from_rgb(41, 41, 39),
                surface_container_highest: Color32::from_rgb(46, 45, 43),
                error: None,
            },
            Theme::Light => Self {
                primary: Color32::from_rgb(155, 77, 40),
                secondary: Color32::from_rgb(75, 112, 46),
                tertiary: Color32::from_rgb(133, 90, 40),
                outline: Color32::from_rgb(89, 102, 116),
                primary_container: Color32::from_rgb(179, 214, 247),
                secondary_container: Color32::from_rgb(255, 255, 255),
                on_secondary_container: Color32::from_rgb(28, 43, 61),
                surface: Color32::from_rgb(246, 249, 253),
                on_surface: Color32::from_rgb(28, 43, 61),
                on_surface_variant: Color32::from_rgb(92, 103, 118),
                surface_container_lowest: Color32::from_rgb(234, 241, 249),
                surface_container_low: Color32::from_rgb(232, 237, 243),
                surface_container: Color32::from_rgb(230, 235, 243),
                surface_container_high: Color32::from_rgb(234, 239, 246),
                surface_container_highest: Color32::from_rgb(234, 239, 246),
                error: None,
            },
        }
    }

    pub fn visuals(self, theme: Theme) -> egui::Visuals {
        let mut visuals = match theme {
            Theme::Dark => egui::Visuals::dark(),
            Theme::Light => egui::Visuals::light(),
        };
        visuals.text_cursor.stroke.color = self.primary;
        visuals.selection.stroke.color = self.primary;
        visuals.window_fill = self.surface_container_highest;
        visuals.panel_fill = self.surface;
        visuals.override_text_color = Some(self.on_surface);
        visuals.selection.bg_fill = self.primary_container;
        visuals.extreme_bg_color = self.surface;
        visuals.faint_bg_color = self.surface_container_highest;
        if let Some(error) = self.error {
            visuals.error_fg_color = error;
        }
        visuals
    }

    pub fn tab_text(self, active: bool) -> Color32 {
        if active {
            self.on_secondary_container
        } else {
            self.on_surface_variant
        }
    }
}

impl Colors {
    fn palette(&self, theme: Theme) -> Palette {
        let overrides = match theme {
            Theme::Dark => &self.dark,
            Theme::Light => &self.light,
        };
        let defaults = Palette::defaults(theme);
        let color = |value: Option<HexColor>, base| value.map_or(base, |value| value.0);
        Palette {
            primary: color(overrides.primary, defaults.primary),
            secondary: color(overrides.secondary, defaults.secondary),
            tertiary: color(overrides.tertiary, defaults.tertiary),
            outline: color(overrides.outline, defaults.outline),
            primary_container: color(overrides.primary_container, defaults.primary_container),
            secondary_container: color(overrides.secondary_container, defaults.secondary_container),
            on_secondary_container: color(
                overrides.on_secondary_container,
                defaults.on_secondary_container,
            ),
            surface: color(overrides.surface, defaults.surface),
            on_surface: color(overrides.on_surface, defaults.on_surface),
            on_surface_variant: color(overrides.on_surface_variant, defaults.on_surface_variant),
            surface_container_lowest: color(
                overrides.surface_container_lowest,
                defaults.surface_container_lowest,
            ),
            surface_container_low: color(
                overrides.surface_container_low,
                defaults.surface_container_low,
            ),
            surface_container: color(overrides.surface_container, defaults.surface_container),
            surface_container_high: color(
                overrides.surface_container_high,
                defaults.surface_container_high,
            ),
            surface_container_highest: color(
                overrides.surface_container_highest,
                defaults.surface_container_highest,
            ),
            error: overrides.error.map(|value| value.0),
        }
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
    [
        Command::CancelDialog,
        Command::CloseSearch,
        Command::ToggleEditorTerminal,
    ]
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
    pub status_bar_height: f32,
    pub status_bar_font_size: f32,
    pub show_file_icons: bool,
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
            status_bar_height: 26.0,
            status_bar_font_size: 12.0,
            show_file_icons: false,
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
        Self::watch(path, ctx, false)
    }

    pub fn watch_settings(path: &Path, ctx: egui::Context) -> notify::Result<Self> {
        Self::watch(path, ctx, true)
    }

    fn watch(path: &Path, ctx: egui::Context, include_theme: bool) -> notify::Result<Self> {
        let directory = path
            .parent()
            .ok_or_else(|| notify::Error::generic("File has no parent directory"))?;
        let filename = path.file_name().map(ToOwned::to_owned);
        let (sender, changes) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            let changed = result.map(|event| {
                event.need_rescan()
                    || (!matches!(event.kind, EventKind::Access(_))
                        && event.paths.iter().any(|path| {
                            path.file_name() == filename.as_deref()
                                || (include_theme
                                    && path.file_name() == Some(std::ffi::OsStr::new("theme.toml")))
                        }))
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
        let directory = path
            .parent()
            .ok_or_else(|| io::Error::other("File has no parent directory"))?;
        fs::create_dir_all(directory)?;
        let legacy = installation.with_file_name("config.toml");
        let default = include_bytes!("../config.toml");
        let theme = path.with_file_name("theme.toml");
        if !path.try_exists()? {
            if legacy != path && legacy.try_exists()? {
                Self::write_new(path, &fs::read(&legacy)?)?;
                let adjacent_theme = legacy.with_file_name("theme.toml");
                if adjacent_theme.try_exists()? {
                    Self::write_new(&theme, &fs::read(adjacent_theme)?)?;
                }
            } else {
                Self::write_new(path, default)?;
            }
        }
        if theme.try_exists()? {
            return Ok(());
        }
        let settings: toml::Table =
            toml::from_str(&fs::read_to_string(path)?).map_err(io::Error::other)?;
        let mode = settings
            .get("editor")
            .and_then(|editor| editor.get("theme"))
            .and_then(toml::Value::as_str);
        if mode.is_some() || settings.contains_key("colors") {
            let mode = mode.unwrap_or("dark");
            let mut migrated = toml::Table::new();
            migrated.insert("mode".into(), mode.into());
            if let Some(colors) = settings.get("colors").and_then(|colors| colors.get(mode)) {
                migrated.insert("palette".into(), colors.clone());
            }
            let source = toml::to_string_pretty(&migrated).map_err(io::Error::other)?;
            toml::from_str::<ThemeFile>(&source).map_err(io::Error::other)?;
            return Self::write_new(&theme, source.as_bytes());
        }
        Self::write_new(&theme, include_bytes!("../theme.toml"))
    }

    fn write_new(path: &Path, contents: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let directory = path
            .parent()
            .ok_or_else(|| io::Error::other("Missing parent directory"))?;
        let mut output = tempfile::NamedTempFile::new_in(directory)?;
        output.write_all(contents)?;
        match output.persist_noclobber(path) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error.error),
        }
    }

    pub fn load(path: &Path) -> Result<(Self, Bindings), String> {
        let mut config = match fs::read_to_string(path) {
            Ok(source) => {
                Self::parse(&source).map_err(|error| format!("{}: {error}", path.display()))?
            }
            Err(error) => return Err(format!("{}: {}", path.display(), crate::io_message(&error))),
        };
        let theme_path = path.with_file_name("theme.toml");
        match fs::read_to_string(&theme_path) {
            Ok(source) => {
                let theme: ThemeFile = toml::from_str(&source)
                    .map_err(|error| format!("{}: {error}", theme_path.display()))?;
                config.editor.theme = theme.mode;
                match theme.mode {
                    Theme::Dark => config.colors.dark = theme.palette,
                    Theme::Light => config.colors.light = theme.palette,
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", theme_path.display())),
        }
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
                    .map(|command| (shortcut, ShortcutAction::Synth(command)))
                    .collect()
            } else {
                vec![(shortcut, ShortcutAction::Synth(Command::ToggleTerminal))]
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
                return Err(format!("{key}: Expected at least one command"));
            }
            for text in binding.commands() {
                let text = text.trim();
                let words = shell_words::split(text);
                let internal = match &words {
                    Ok(words) => words.first().is_some_and(|program| program == "synth"),
                    Err(_) => text.split_whitespace().next() == Some("synth"),
                };
                if !internal {
                    let script = text.strip_prefix("shell:").unwrap_or(text).trim();
                    if script.is_empty() || script.contains('\0') {
                        return Err(format!("{key}: Expected a nonempty shell command"));
                    }
                    bindings.push((shortcut, ShortcutAction::Shell(script.to_owned())));
                    continue;
                }
                let words = words.map_err(|error| format!("{key}: {error}"))?;
                let args = &words[1..];
                let args = args.iter().map(Into::into).collect::<Vec<_>>();
                let command =
                    Command::parse(&args, directory).map_err(|error| format!("{key}: {error}"))?;
                if self.keybindings.is_some()
                    && bare_escape(shortcut)
                    && matches!(command, Command::ToggleTerminal)
                {
                    bindings.extend(
                        legacy_escape_commands()
                            .into_iter()
                            .map(|command| (shortcut, ShortcutAction::Synth(command))),
                    );
                } else {
                    bindings.push((shortcut, ShortcutAction::Synth(command)));
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
        if !FONT_SIZE_RANGE.contains(&config.editor.font_size)
            || !FONT_SIZE_RANGE.contains(&config.terminal.font_size)
        {
            return Err("Font sizes must be between 8 and 48".into());
        }
        if !(0.15..=0.85).contains(&config.terminal.width_fraction) {
            return Err("terminal.width_fraction must be between 0.15 and 0.85".into());
        }
        if !(1..=16).contains(&config.editor.tab_size) {
            return Err("editor.tab_size must be between 1 and 16".into());
        }
        if !(16.0..=64.0).contains(&config.interface.status_bar_height)
            || !(8.0..=24.0).contains(&config.interface.status_bar_font_size)
        {
            return Err("Status bar height must be 16–64 and font size 8–24".into());
        }
        if config
            .editor
            .formatter
            .as_ref()
            .is_some_and(|command| command.trim().is_empty())
        {
            return Err("editor.formatter must be a nonempty command".into());
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
        if config.keybinds.is_none() && config.keybindings.is_none() {
            config.terminal.shortcut()?;
        }
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
    fn imports_theme_and_preserves_legacy_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let installation = dir.path().join("synth");
        let theme = path.with_file_name("theme.toml");
        let original = "[editor]\ntheme='light'\n[colors.light]\ntext_fg='#123456'";
        fs::write(&path, original).unwrap();
        Config::ensure_file(&path, &installation).unwrap();
        assert!(theme.is_file());
        assert_eq!(
            Config::load(&path).unwrap().0.palette().on_surface,
            Color32::from_rgb(18, 52, 86)
        );
        fs::write(&theme, "mode='dark'\n[palette]\naccent='#123456'").unwrap();
        Config::ensure_file(&path, &installation).unwrap();
        let palette = Config::load(&path).unwrap().0.palette();
        assert_eq!(palette.primary, Color32::from_rgb(18, 52, 86));
        assert_eq!(
            palette.on_surface,
            Palette::defaults(Theme::Dark).on_surface
        );
        fs::write(&theme, "accent='wrong'").unwrap();
        assert!(Config::load(&path).err().unwrap().contains("theme.toml"));
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn example_uses_default_shortcut_and_panel() {
        let config = Config::parse(include_str!("../config.toml")).unwrap();
        assert!(matches!(config.terminal.side, Side::Right));
        assert!(matches!(
            config.terminal.command_target,
            CommandTarget::Window
        ));
        assert!(!config.animations.enabled);
        assert_eq!(config.cursor_trail.target, cursor_trail::Target::None);
        assert!(config.editor.wrap_lines);
        assert!(config.editor.recover_unsaved_documents);
        assert!(!config.interface.enable_search_and_replace);
        assert!(config.interface.show_tabs);
        assert!(config.interface.show_line_numbers);
        assert!(config.interface.syntax_highlighting);
        assert!(config.interface.show_status_bar);
        let bindings = config.bindings(Path::new("/tmp/config.toml")).unwrap();
        assert!(bindings.iter().any(|(shortcut, command)| *shortcut
            == egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::Escape)
            && matches!(
                command,
                ShortcutAction::Synth(Command::ToggleEditorTerminal)
            )));
        assert!(bindings.iter().any(|(shortcut, command)| *shortcut
            == egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Escape)
            && matches!(command, ShortcutAction::Synth(Command::HideTerminal))));
    }

    #[test]
    fn first_launch_creates_config_and_theme_and_preserves_user_changes() {
        let workspace = tempfile::tempdir().unwrap();
        let installation = workspace.path().join("synth");
        let path = workspace.path().join("settings/config.toml");
        let theme = path.with_file_name("theme.toml");
        assert!(Config::load(&path).is_err());

        Config::ensure_file(&path, &installation).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            include_str!("../config.toml")
        );
        assert_eq!(
            fs::read_to_string(&theme).unwrap(),
            include_str!("../theme.toml")
        );
        assert_eq!(Config::load(&path).unwrap().1.len(), 17);

        let custom_theme = "mode = 'light'";
        fs::write(&theme, custom_theme).unwrap();
        fs::write(&path, "[keybindings]").unwrap();
        Config::ensure_file(&path, &installation).unwrap();
        assert_eq!(fs::read_to_string(theme).unwrap(), custom_theme);
        assert!(Config::load(&path).unwrap().1.is_empty());
    }

    #[test]
    fn search_shortcut_uses_the_configured_table() {
        let path = Path::new("config.toml");
        let empty =
            Config::parse("[interface]\nenable_search_and_replace=true\n[keybindings]").unwrap();
        assert!(empty.bindings(path).unwrap().is_empty());
        let custom = Config::parse("[interface]\nenable_search_and_replace=true\n[keybindings]\n'Alt+F'='synth search toggle'").unwrap();
        let bindings = custom.bindings(path).unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(
            bindings[0].0,
            egui::KeyboardShortcut::new(egui::Modifiers::ALT, egui::Key::F)
        );
        assert!(matches!(
            bindings[0].1,
            ShortcutAction::Synth(Command::ToggleSearch)
        ));
    }

    #[test]
    fn external_keybinds_preserve_shell_syntax() {
        let settings = Config::parse(
            r#"
[keybinds]
F7 = ['synth search next', "printf '%s' 'hello world' > out.txt"]
F8 = 'shell: synth properties | cat'
"#,
        )
        .unwrap();
        let bindings = settings.bindings(Path::new("config.toml")).unwrap();
        assert!(matches!(
            bindings[0].1,
            ShortcutAction::Synth(Command::SearchNext)
        ));
        assert!(
            matches!(&bindings[1].1, ShortcutAction::Shell(script) if script == "printf '%s' 'hello world' > out.txt")
        );
        assert!(
            matches!(&bindings[2].1, ShortcutAction::Shell(script) if script == "synth properties | cat")
        );
        assert!(
            Config::parse("[keybinds]\nF7='shell:'")
                .unwrap()
                .bindings(Path::new("config.toml"))
                .is_err()
        );
        assert!(
            Config::parse("[keybinds]\nF7='synth typo'")
                .unwrap()
                .bindings(Path::new("config.toml"))
                .is_err()
        );
    }

    #[test]
    fn keybinds_keep_contextual_fallbacks_in_order() {
        let path = Path::new("config.toml");
        let config = Config::parse(
            "[keybinds]\nF7 = ['synth dialog cancel', 'synth search close', 'synth terminal toggle-edit']",
        ).unwrap();
        let bindings = config.bindings(path).unwrap();
        let commands: Vec<_> = bindings.into_iter().map(|(_, cmd)| cmd).collect();
        assert!(matches!(
            &commands[..],
            [
                ShortcutAction::Synth(Command::CancelDialog),
                ShortcutAction::Synth(Command::CloseSearch),
                ShortcutAction::Synth(Command::ToggleEditorTerminal)
            ]
        ));
        assert!(
            Config::parse("[keybinds]\n[keybindings]")
                .unwrap()
                .bindings(path)
                .is_err()
        );
        assert!(
            Config::parse("[keybinds]\nF7 = []")
                .unwrap()
                .bindings(path)
                .is_err()
        );
    }

    #[test]
    fn inactive_legacy_shortcut_does_not_break_keybinds() {
        let source = "[terminal]\ntoggle_key='UnknownKey'\n[keybinds]\nF7='synth new'";
        let config = Config::parse(source).unwrap();
        assert_eq!(config.bindings(Path::new("config.toml")).unwrap().len(), 1);
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
        let config =
            Config::parse("[interface]\ntab_height = 42.0\ntab_max_chars = 32\neditor_padding = 8")
                .unwrap();
        assert_eq!(config.interface.tab_height, 42.0);
        assert_eq!(config.interface.tab_max_chars, 32);
        assert_eq!(config.interface.editor_padding, 8);
    }

    #[test]
    fn dark_palette_matches_tab_reference() {
        let dark = Palette::defaults(Theme::Dark);
        let light = Palette::defaults(Theme::Light);
        assert_eq!(dark.surface_container, Color32::from_rgb(41, 41, 39));
        assert_eq!(dark.secondary_container, Color32::from_rgb(52, 51, 47));
        assert_eq!(dark.surface_container_lowest, Color32::from_rgb(28, 29, 28));
        assert_eq!(dark.surface_container_high, Color32::from_rgb(41, 41, 39));
        assert_ne!(light.surface_container, dark.surface_container);
    }

    #[test]
    fn palette_overrides_are_partial_and_theme_specific() {
        let config = Config::parse(
            "[colors.dark]\neditor_bg = '#12A0ef'\ntab_inactive_fg = '#000000'\n[colors.light]\neditor_bg = '#445566'",
        )
        .unwrap();
        let dark = config.colors.palette(Theme::Dark);
        let light = config.colors.palette(Theme::Light);
        assert_eq!(dark.surface, Color32::from_rgb(18, 160, 239));
        assert_eq!(dark.on_surface_variant, Color32::BLACK);
        assert_eq!(
            dark.surface_container_lowest,
            Palette::defaults(Theme::Dark).surface_container_lowest
        );
        assert_eq!(light.surface, Color32::from_rgb(68, 85, 102));
        assert_eq!(
            light.on_surface_variant,
            Palette::defaults(Theme::Light).on_surface_variant
        );
    }

    #[test]
    fn color_roles_accept_legacy_aliases() {
        let aliases = [
            ("primary", &["accent"][..]),
            ("secondary", &["syntax_string"]),
            ("tertiary", &["syntax_literal"]),
            ("outline", &["syntax_comment"]),
            ("primary_container", &["selection_bg", "selection"]),
            (
                "secondary_container",
                &["tab_active_bg", "active_tab_background"],
            ),
            (
                "on_secondary_container",
                &["tab_active_fg", "tab_text_active"],
            ),
            ("surface", &["editor_bg", "editor_background"]),
            ("on_surface", &["text_fg", "foreground"]),
            (
                "on_surface_variant",
                &["tab_inactive_fg", "tab_text_inactive"],
            ),
            (
                "surface_container_lowest",
                &["terminal_bg", "terminal_background"],
            ),
            ("surface_container_low", &["current_line"]),
            ("surface_container", &["tab_bar_bg", "tabs_background"]),
            (
                "surface_container_high",
                &["status_bar_bg", "status_background"],
            ),
            (
                "surface_container_highest",
                &["surface_bg", "muted_surface"],
            ),
        ];
        for (role, legacy_names) in aliases {
            let current = format!("[colors.dark]\n{role} = '#123ABC'");
            let palette = Config::parse(&current).unwrap().palette();
            assert_ne!(palette, Palette::defaults(Theme::Dark), "{role}");
            for legacy in legacy_names {
                let old = Config::parse(&format!("[colors.dark]\n{legacy} = '#123ABC'"))
                    .unwrap()
                    .palette();
                assert_eq!(old, palette, "{legacy}");
                let duplicate = format!("{current}\n{legacy} = '#654321'");
                assert!(Config::parse(&duplicate).is_err(), "{legacy} and {role}");
            }
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
        assert_eq!(baseline.surface, legacy.surface);
        assert_eq!(baseline.surface, recent.surface);
        assert_eq!(baseline.surface_container, legacy.surface_container);
        assert_eq!(baseline.surface_container, recent.surface_container);
    }

    #[test]
    fn invalid_hex_colors_are_rejected() {
        assert!(Config::parse("[colors.dark]\neditor_bg = '#+abcde'").is_err());
        for color in ["'123456'", "'#12345'", "'#GG0000'", "'#12345678'", "42"] {
            let source = format!("[colors.dark]\neditor_bg = {color}");
            assert!(Config::parse(&source).is_err(), "{color}");
        }
        assert!(Config::parse("[colors.dark]\nunknown = '#010203'").is_err());
        assert!(Config::parse("[colors.unknown]").is_err());
    }

    #[test]
    fn bundled_theme_preserves_the_default_dark_palette() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::ensure_file(&path, &dir.path().join("synth")).unwrap();
        let (config, _) = Config::load(&path).unwrap();
        assert_eq!(config.palette(), Palette::defaults(Theme::Dark));
    }

    #[test]
    fn theme_file_applies_color_roles_over_mode_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[colors.light]\non_surface = '#010203'").unwrap();
        fs::write(
            path.with_file_name("theme.toml"),
            r##"mode = "light"

[palette]
primary = "#123456"
secondary = "#345678"
tertiary = "#56789A"
primary_container = "#ABCDEF"
error = "#9A5678"
"##,
        )
        .unwrap();

        let (config, _) = Config::load(&path).unwrap();
        let palette = config.palette();
        let defaults = Palette::defaults(Theme::Light);
        assert_eq!(palette.primary, Color32::from_rgb(18, 52, 86));
        assert_eq!(palette.secondary, Color32::from_rgb(52, 86, 120));
        assert_eq!(palette.tertiary, Color32::from_rgb(86, 120, 154));
        assert_eq!(palette.surface, defaults.surface);
        assert_eq!(palette.on_surface, defaults.on_surface);
        let visuals = palette.visuals(config.editor.theme);
        assert_eq!(visuals.text_cursor.stroke.color, palette.primary);
        assert_eq!(visuals.selection.bg_fill, Color32::from_rgb(171, 205, 239));
        assert_eq!(visuals.error_fg_color, Color32::from_rgb(154, 86, 120));
    }

    #[test]
    fn legacy_new_tab_button_setting_does_not_change_the_interface() {
        let old_config = Config::parse("[interface]\ntab_new_button = true").unwrap();
        let new_config = Config::parse("[interface]").unwrap();
        assert_eq!(
            old_config.interface.tab_height,
            new_config.interface.tab_height
        );
        assert_eq!(
            old_config.interface.tab_min_width,
            new_config.interface.tab_min_width
        );
        assert!(Config::parse("[interface]\ntab_new_button = false").is_ok());
    }

    #[test]
    fn configurable_tab_dimensions_validate() {
        assert!(Config::parse("[interface]\ntab_font_size = 8.0").is_err());
        assert!(Config::parse("[interface]\ntab_min_width = 40.0").is_err());
        let config =
            Config::parse("[interface]\ntab_font_size = 13.0\ntab_min_width = 120.0").unwrap();
        assert_eq!(config.interface.tab_font_size, 13.0);
        assert_eq!(config.interface.tab_min_width, 120.0);
    }
}
