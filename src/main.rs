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

enum PaneLayout {
    Leaf(u64),
    Split {
        down: bool,
        first: Box<Self>,
        second: Box<Self>,
    },
}

impl PaneLayout {
    fn split(&mut self, active: u64, new: u64, down: bool) -> bool {
        match self {
            Self::Leaf(id) if *id == active => {
                *self = Self::Split {
                    down,
                    first: Box::new(Self::Leaf(active)),
                    second: Box::new(Self::Leaf(new)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split(active, new, down) || second.split(active, new, down)
            }
        }
    }

    fn close(&mut self, active: u64) -> bool {
        let Self::Split { first, second, .. } = self else {
            return false;
        };
        if matches!(**first, Self::Leaf(id) if id == active) {
            *self = std::mem::replace(second.as_mut(), Self::Leaf(active));
            true
        } else if matches!(**second, Self::Leaf(id) if id == active) {
            *self = std::mem::replace(first.as_mut(), Self::Leaf(active));
            true
        } else {
            first.close(active) || second.close(active)
        }
    }

    fn leaves(&self, rect: egui::Rect, out: &mut Vec<(u64, egui::Rect)>) {
        match self {
            Self::Leaf(id) => out.push((*id, rect)),
            Self::Split {
                down,
                first,
                second,
            } => {
                let mut a = rect;
                let mut b = rect;
                if *down {
                    a.max.y = rect.center().y - 3.0;
                    b.min.y = rect.center().y + 3.0;
                } else {
                    a.max.x = rect.center().x - 3.0;
                    b.min.x = rect.center().x + 3.0;
                }
                first.leaves(a, out);
                second.leaves(b, out);
            }
        }
    }
}

#[derive(Default)]
struct PaneState {
    document: u64,
    cursors: HashMap<u64, (usize, Option<std::ops::Range<usize>>)>,
}

type FormatResult = (u64, u64, Result<String, String>);

struct Synth {
    config: Config,
    config_path: PathBuf,
    bindings: Bindings,
    watcher: Option<FileWatch>,
    watched_document: Option<WatchedDocument>,
    commands: mpsc::Receiver<Request>,
    shell_errors: (mpsc::Sender<String>, mpsc::Receiver<String>),
    documents: Documents,
    layout: PaneLayout,
    panes: HashMap<u64, PaneState>,
    active_pane: u64,
    next_pane: u64,
    formatted: (mpsc::Sender<FormatResult>, mpsc::Receiver<FormatResult>),
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
            layout: PaneLayout::Leaf(0),
            panes: HashMap::from([(0, PaneState::default())]),
            active_pane: 0,
            next_pane: 1,
            formatted: mpsc::channel(),
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

    fn remember_pane(&mut self) {
        let doc = self.documents.current();
        let pane = self.panes.get_mut(&self.active_pane).unwrap();
        pane.document = doc.id;
        pane.cursors.insert(
            doc.id,
            (
                doc.cursor,
                doc.selection.clone().or_else(|| doc.selected_range.clone()),
            ),
        );
    }

    fn focus_pane(&mut self, id: u64) {
        if self.active_pane != id {
            self.remember_pane();
            self.active_pane = id;
            let pane = &self.panes[&id];
            if let Some(index) = self
                .documents
                .items
                .iter()
                .position(|doc| doc.id == pane.document)
            {
                self.documents.active = index;
            }
            let doc = self.documents.current_mut();
            let (cursor, selection) = pane.cursors.get(&doc.id).cloned().unwrap_or_default();
            doc.cursor = doc.clamp_cursor(cursor);
            doc.selection = Some(selection.map_or(doc.cursor..doc.cursor, |range| {
                doc.clamp_cursor(range.start)..doc.clamp_cursor(range.end)
            }));
            doc.selected_range = doc.selection.clone();
        }
        self.pending_focus = Some(Focus::Editor);
    }

    fn restore_tab_cursor(&mut self) {
        let doc = self.documents.current_mut();
        let pane = &self.panes[&self.active_pane];
        if let Some((cursor, selection)) = pane.cursors.get(&doc.id) {
            doc.cursor = doc.clamp_cursor(*cursor);
            doc.selection = Some(selection.clone().map_or(doc.cursor..doc.cursor, |range| {
                doc.clamp_cursor(range.start)..doc.clamp_cursor(range.end)
            }));
            doc.selected_range = doc.selection.clone();
        }
    }

    fn pane_ids(&self) -> Vec<u64> {
        let mut leaves = Vec::new();
        self.layout.leaves(
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::splat(256.0)),
            &mut leaves,
        );
        leaves.into_iter().map(|(id, _)| id).collect()
    }

    fn show_panes(&mut self, ui: &mut egui::Ui) {
        let mut leaves = Vec::new();
        let area = ui.available_rect_before_wrap();
        self.layout.leaves(area, &mut leaves);
        let active = self.active_pane;
        let fallback = self.documents.active;
        let mut focus = None;
        for (id, rect) in leaves {
            if !rect.is_positive() {
                continue;
            }
            let pane = self.panes.get_mut(&id).unwrap();
            let index = if id == active {
                fallback
            } else {
                self.documents
                    .items
                    .iter()
                    .position(|doc| doc.id == pane.document)
                    .unwrap_or(fallback)
            };
            let doc = &mut self.documents.items[index];
            pane.document = doc.id;
            let mut child = ui.new_child(egui::UiBuilder::new().id_salt(id).max_rect(rect));
            child.set_clip_rect(rect.intersect(ui.clip_rect()));
            if self.panes.len() > 1 {
                child.painter().rect_stroke(
                    rect,
                    0.0,
                    egui::Stroke::new(
                        1.0_f32,
                        if id == active {
                            self.config.palette().primary
                        } else {
                            ui.visuals().widgets.noninteractive.bg_stroke.color
                        },
                    ),
                    egui::StrokeKind::Inside,
                );
            }
            let pane = self.panes.get_mut(&id).unwrap();
            let inactive = id != active;
            let (saved_cursor, saved_selection, saved_range) = if inactive {
                let (cursor, _) = pane.cursors.get(&doc.id).cloned().unwrap_or_default();
                (
                    std::mem::replace(&mut doc.cursor, cursor),
                    doc.selection.take(),
                    doc.selected_range.take(),
                )
            } else {
                (0, None, None)
            };
            let highlight = (!inactive).then(|| self.search_bar.highlight()).flatten();
            let focused = self
                .editor
                .show(&mut child, doc, &self.config, id, highlight);
            if inactive {
                pane.cursors
                    .insert(doc.id, (doc.cursor, doc.selected_range.clone()));
                doc.cursor = saved_cursor;
                doc.selection = saved_selection;
                doc.selected_range = saved_range;
                if focused {
                    focus = Some(id);
                }
            }
        }
        if let Some(id) = focus {
            self.focus_pane(id);
        }
        ui.advance_cursor_after_rect(area);
    }

    fn format_document(&self, ctx: &egui::Context) -> Reply {
        let script = self
            .config
            .editor
            .formatter
            .clone()
            .ok_or("Set editor.formatter in config.toml first")?;
        let doc = self.documents.current();
        let (id, revision, text) = (doc.id, doc.revision, doc.text.clone());
        let directory = doc
            .path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_owned);
        let sender = self.formatted.0.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("synth-format".into())
            .spawn(move || {
                let output = format_text(&script, directory.as_deref(), text);
                let _ = sender.send((id, revision, output));
                ctx.request_repaint();
            })
            .map_err(|error| io_message(&error))?;
        Ok("Formatting document".into())
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
        self.remember_pane();
        let previous_pane = self.active_pane;
        let previous = self.documents.current().id;
        let focus = self.focus(ctx);
        if matches!(
            command,
            Command::DuplicateLine
                | Command::MoveLine(_)
                | Command::Indent(_)
                | Command::LineEnding(_)
        ) {
            self.editor
                .add_undo(ctx, self.documents.current(), self.active_pane);
            self.pending_focus = Some(Focus::Editor);
        }
        let reply = match command {
            Command::Open(path) => self.documents.open(Path::new(&path)),
            Command::OpenEncoded(encoding, path) => self
                .documents
                .open_encoded(Path::new(&path), Some(encoding)),
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
            Command::SaveAll => self.documents.save_all(),
            Command::SelectTab(number) => self.documents.select(number),
            Command::Reopen => self.documents.reopen(),
            Command::Encoding(encoding) => self.documents.current_mut().set_encoding(encoding),
            Command::LineEnding(ending) => self.documents.current_mut().set_ending(ending),
            Command::DuplicateLine => self.documents.current_mut().duplicate_line(),
            Command::MoveLine(down) => self.documents.current_mut().move_line(down),
            Command::Indent(increase) => self.documents.current_mut().indent(
                increase,
                self.config.editor.insert_spaces,
                self.config.editor.tab_size,
            ),
            Command::ConfigEdit => self.documents.open(&self.config_path),
            Command::RestartTerminal => {
                self.terminal = None;
                self.terminal_error = None;
                self.terminal_open = true;
                self.pending_focus = Some(Focus::Terminal);
                Ok("Restarting terminal".into())
            }
            Command::Format => self.format_document(ctx),
            Command::Split(down) => {
                if self.panes.len() >= 16 {
                    return Err("At most 16 editor panes are supported".into());
                }
                let id = self.next_pane;
                self.next_pane += 1;
                self.layout.split(self.active_pane, id, down);
                self.panes.insert(
                    id,
                    PaneState {
                        document: previous,
                        cursors: self.panes[&self.active_pane].cursors.clone(),
                    },
                );
                self.focus_pane(id);
                Ok("Split editor".into())
            }
            Command::SplitClose => {
                let active = self.active_pane;
                let ids = self.pane_ids();
                if !self.layout.close(active) {
                    return Err("The editor has only one pane".into());
                }
                let index = ids.iter().position(|&id| id == active).unwrap();
                self.focus_pane(ids[(index + 1) % ids.len()]);
                self.panes.remove(&active);
                self.editor.forget_pane(ctx, active);
                Ok("Closed editor pane".into())
            }
            Command::SplitNext => {
                let ids = self.pane_ids();
                let index = ids.iter().position(|&id| id == self.active_pane).unwrap();
                self.focus_pane(ids[(index + 1) % ids.len()]);
                Ok("Focused next editor pane".into())
            }
        };
        if self.documents.current().id != previous && previous_pane == self.active_pane {
            self.restore_tab_cursor();
            if focus == Focus::Editor {
                self.pending_focus = Some(Focus::Editor);
            }
        }
        self.remember_pane();
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
        let reply = self.reload_in_panes(id)?;
        self.pending_focus = Some(Focus::Editor);
        Ok(reply)
    }

    fn reload_in_panes(&mut self, id: u64) -> Reply {
        let reply = self.documents.reload(id)?;
        let new = self.documents.current().id;
        for pane in self.panes.values_mut() {
            if pane.document == id {
                pane.document = new;
            }
            if let Some(cursor) = pane.cursors.remove(&id) {
                pane.cursors.insert(new, cursor);
            }
        }
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
                Confirmation::Reload(id) => self.reload_in_panes(*id).map(|_| ()),
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
                if terminal.process_events(
                    ctx,
                    palette.on_surface,
                    palette.surface_container_lowest,
                ) {
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
        let background = self.config.palette().surface_container_lowest;
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
        while let Ok((id, revision, result)) = self.formatted.1.try_recv() {
            let Some(doc) = self
                .documents
                .items
                .iter_mut()
                .find(|doc| doc.id == id && doc.revision == revision)
            else {
                self.notice =
                    Some("Formatter result discarded because the document changed".into());
                continue;
            };
            match result {
                Ok(text) => {
                    if text != doc.text {
                        self.editor.add_undo(ctx, doc, self.active_pane);
                        let cursor = doc.position();
                        let _ = doc.replace_formatted(text, cursor);
                    }
                    self.notice = None;
                }
                Err(error) => self.notice = Some(error),
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
            self.remember_pane();
            self.documents.active = index;
            self.restore_tab_cursor();
            self.pending_focus = Some(Focus::Editor);
        }
        self.check_document_on_disk(ctx);
        editor::status(ctx, self.documents.current(), &self.config);
        if self.pending_focus == Some(Focus::Editor) {
            let id = Editor::id(self.documents.current(), self.active_pane);
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
                    self.editor.prepare_state(ctx, document, self.active_pane);
                    self.search_bar.show(ui, document, self.active_pane);
                    self.show_panes(ui);
                    self.remember_pane();
                });
                if reload && let Err(error) = self.execute(ctx, Command::Reload) {
                    self.notice = Some(error);
                }
            });
        self.editor.retain_documents(ctx, &self.documents);
        for pane in self.panes.values_mut() {
            pane.cursors
                .retain(|id, _| self.documents.items.iter().any(|doc| doc.id == *id));
        }
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

fn shell_command(script: &str) -> std::process::Command {
    #[cfg(unix)]
    let shell = {
        let mut shell = std::process::Command::new("/bin/sh");
        shell.args(["-c", script]);
        shell
    };
    #[cfg(windows)]
    let shell = {
        use std::os::windows::process::CommandExt;

        let executable = env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join("System32").join("cmd.exe"))
            .unwrap_or_else(|| PathBuf::from("cmd.exe"));
        let mut shell = std::process::Command::new(executable);
        shell
            .args(["/D", "/S", "/C"])
            .raw_arg(format!("\"{script}\""))
            .creation_flags(0x08000000);
        shell
    };
    shell
}

fn format_text(script: &str, directory: Option<&Path>, text: String) -> Result<String, String> {
    use std::io::Write as _;

    let mut command = shell_command(script);
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Formatter: {}", io_message(&error)))?;
    let mut input = child.stdin.take().ok_or("Could not open formatter stdin")?;
    let (output, written) = std::thread::scope(|scope| {
        let writer = scope.spawn(move || input.write_all(text.as_bytes()));
        (child.wait_with_output(), writer.join())
    });
    let output = output.map_err(|error| io_message(&error))?;
    if !output.status.success() {
        return Err(format!(
            "Formatter: {} {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    written
        .map_err(|_| "Formatter input worker stopped")?
        .map_err(|error| io_message(&error))?;
    String::from_utf8(output.stdout).map_err(|_| "Formatter output is not UTF-8".into())
}

fn run_shortcut_shell(script: &str, directory: Option<&Path>) -> Result<(), String> {
    let mut shell = shell_command(script);
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
        let mut command = Command::parse_cli(&args, &env::current_dir()?)?;
        if matches!(command, Command::ConfigEdit) {
            Config::ensure_file(&config_path, &installation)?;
            command = Command::Open(config_path.clone().into_os_string());
        }
        let reply = match &command {
            Command::ConfigPath => Ok(config_path.display().to_string()),
            Command::ConfigCheck => check_config(&config_path),
            Command::Open(path) | Command::OpenEncoded(_, path) => {
                match command::dispatch_if_running(&installation, &command)? {
                    Some(reply) => reply,
                    None => {
                        let encoding = match command {
                            Command::OpenEncoded(encoding, _) => Some(encoding),
                            _ => None,
                        };
                        initial_documents.open_encoded(Path::new(path), encoding)?;
                        Ok(String::new())
                    }
                }
            }
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
    fn nested_panes_close_without_losing_siblings() {
        let mut layout = PaneLayout::Leaf(0);
        assert!(layout.split(0, 1, false));
        assert!(layout.split(1, 2, true));
        let area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(240.0, 160.0));
        let mut panes = Vec::new();
        layout.leaves(area, &mut panes);
        assert_eq!(
            panes.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!(panes[0].1.right() < panes[1].1.left());
        assert!(panes[1].1.bottom() < panes[2].1.top());
        assert!(layout.close(1));
        assert!(layout.close(0));
        assert!(!layout.close(2));
        assert!(matches!(layout, PaneLayout::Leaf(2)));
    }

    #[cfg(unix)]
    #[test]
    fn formatter_handles_large_input_without_blocking_on_output() {
        let input = "cats and dogs\n".repeat(20_000);
        let output = format_text("tr a-z A-Z", None, input.clone()).unwrap();
        assert_eq!(output, input.to_uppercase());
        assert!(
            format_text("printf broken >&2; exit 2", None, "text".into())
                .unwrap_err()
                .contains("broken")
        );
    }

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
