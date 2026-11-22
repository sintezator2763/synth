use std::collections::HashMap;

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Layout, RichText, ScrollArea, TextEdit, Ui, UiBuilder,
    text::{CCursor, CCursorRange, LayoutJob, TextFormat},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{HighlightState, StyleModifier, Theme as SyntaxTheme, ThemeItem},
    parsing::{ParseState, SyntaxSet},
    util::LinesWithEndings,
};

use crate::{
    config::{Config, Palette},
    cursor_trail::{Cursor, Target, Trail},
    document::{Document, Documents, LineEnding},
};

struct EditBuffer<'a> {
    document: &'a mut Document,
    config: &'a crate::config::Editor,
}

impl EditBuffer<'_> {
    fn insertion_cursor(&self, cursor: usize) -> usize {
        let cursor = self.document.clamp_cursor(cursor);
        let byte = self.document.byte_at_char(cursor);
        if self.document.text[..byte].ends_with('\r')
            && self.document.text[byte..].starts_with('\n')
        {
            cursor - 1
        } else {
            cursor
        }
    }

    fn deletion_range(&self, mut range: std::ops::Range<usize>) -> std::ops::Range<usize> {
        if range.is_empty() {
            return range;
        }
        let text = &self.document.text;
        let start = self.document.byte_at_char(range.start);
        let end = self.document.byte_at_char(range.end);
        if text[..start].ends_with('\r') && text[start..].starts_with('\n') {
            range.start -= 1;
        }
        if text[..end].ends_with('\r') && text[end..].starts_with('\n') {
            range.end += 1;
        }
        range
    }
}

impl egui::TextBuffer for EditBuffer<'_> {
    fn is_mutable(&self) -> bool {
        true
    }
    fn as_str(&self) -> &str {
        &self.document.text
    }

    fn insert_text(&mut self, text: &str, cursor: usize) -> usize {
        let cursor = self.insertion_cursor(cursor);
        let mut insertion = std::borrow::Cow::Borrowed(text);
        if text == "\t" && self.config.insert_spaces {
            let column = self.document.column_at(cursor);
            insertion = " "
                .repeat(self.config.tab_size - (column - 1) % self.config.tab_size)
                .into();
        } else if text == "\n" && self.config.auto_indent {
            insertion = format!("\n{}", self.document.leading_spaces_at(cursor)).into();
        }
        if self.document.ending == LineEnding::Crlf && insertion.contains('\n') {
            insertion = insertion.replace("\r\n", "\n").replace('\n', "\r\n").into();
        } else if self.document.ending == LineEnding::Lf && insertion.contains("\r\n") {
            insertion = insertion.replace("\r\n", "\n").into();
        }
        self.document.replace_chars(cursor..cursor, &insertion)
    }

    fn insert_text_at(&mut self, cursor: &mut CCursor, text: &str, char_limit: usize) {
        cursor.index = self.insertion_cursor(cursor.index);
        let text = if char_limit == usize::MAX {
            text
        } else {
            let remaining = char_limit.saturating_sub(self.as_str().chars().count());
            text.char_indices()
                .nth(remaining)
                .map_or(text, |(byte, _)| &text[..byte])
        };
        cursor.index += self.insert_text(text, cursor.index);
    }

    fn decrease_indentation(&mut self, cursor: &mut CCursor) {
        let doc = &mut self.document;
        let column = doc.column_at(cursor.index);
        let row = doc.clamp_cursor(cursor.index) + 1 - column;
        let suffix = &doc.text[doc.byte_at_char(row)..];
        let remove = if suffix.starts_with('\t') {
            1
        } else {
            suffix
                .bytes()
                .take(self.config.tab_size)
                .take_while(|&byte| byte == b' ')
                .count()
        };
        doc.replace_chars(row..row + remove, "");
        cursor.index = cursor.index.saturating_sub(remove).max(row);
    }

    fn delete_char_range(&mut self, range: std::ops::Range<usize>) {
        let range = self.deletion_range(range);
        self.document.replace_chars(range, "");
    }

    fn delete_selected_ccursor_range(&mut self, [min, max]: [CCursor; 2]) -> CCursor {
        let range = self.deletion_range(min.index..max.index);
        let cursor = self.insertion_cursor(range.start);
        self.document.replace_chars(range, "");
        CCursor::new(cursor)
    }

    fn byte_index_from_char_index(&self, index: usize) -> usize {
        self.document.byte_at_char(index)
    }

    fn replace_with(&mut self, text: &str) {
        if text.contains('\n') {
            self.document.ending = LineEnding::detect(text);
        }
        self.document.text.clear();
        self.document.text.push_str(text);
        self.document.text_changed();
    }
}

fn resize_tabs(job: &mut LayoutJob, width: usize, ui: &Ui) {
    let mut sections = Vec::with_capacity(job.sections.len());
    for section in std::mem::take(&mut job.sections) {
        let mut start = section.byte_range.start;
        let mut leading = section.leading_space;
        for (offset, _) in job.text[section.byte_range.clone()].match_indices('\t') {
            let tab = section.byte_range.start + offset;
            if start < tab {
                sections.push(egui::text::LayoutSection {
                    byte_range: start..tab,
                    leading_space: std::mem::take(&mut leading),
                    format: section.format.clone(),
                });
            }
            let mut format = section.format.clone();
            format.line_height = Some(ui.fonts(|fonts| fonts.row_height(&format.font_id)));
            format.font_id.size *= width as f32 / 4.0;
            sections.push(egui::text::LayoutSection {
                byte_range: tab..tab + 1,
                leading_space: std::mem::take(&mut leading),
                format,
            });
            start = tab + 1;
        }
        if start < section.byte_range.end {
            sections.push(egui::text::LayoutSection {
                byte_range: start..section.byte_range.end,
                leading_space: leading,
                format: section.format,
            });
        }
    }
    job.sections = sections;
}

#[derive(Default)]
pub struct Editor {
    syntax: Option<SyntaxLayouts>,
    trail: Trail,
    shared_state: HashMap<u64, egui::text_edit::TextEditState>,
    prepared_views: std::collections::HashSet<(u64, u64)>,
}

impl Editor {
    pub fn reset_highlighting(&mut self) {
        self.syntax = None;
    }
    pub fn id(document: &Document, pane: u64) -> egui::Id {
        egui::Id::new(("document", document.id, pane))
    }

    pub fn prepare_state(&mut self, ctx: &egui::Context, document: &Document, pane: u64) {
        if !self.prepared_views.insert((document.id, pane)) {
            return;
        }
        let id = Self::id(document, pane);
        let local = TextEdit::load_state(ctx, id).unwrap_or_default();
        let mut state = self.shared_state.entry(document.id).or_default().clone();
        state.cursor = local.cursor;
        state.store(ctx, id);
    }

    pub fn add_undo(&mut self, ctx: &egui::Context, document: &Document, pane: u64) {
        self.prepare_state(ctx, document, pane);
        let id = Self::id(document, pane);
        let mut state = TextEdit::load_state(ctx, id).unwrap_or_default();
        let mut undoer = state.undoer();
        undoer.add_undo(&(
            CCursorRange::one(CCursor::new(document.cursor)),
            document.text.clone(),
        ));
        state.set_undoer(undoer);
        state.store(ctx, id);
    }

    pub fn forget_pane(&mut self, ctx: &egui::Context, pane: u64) {
        self.prepared_views.retain(|&(id, view)| {
            if pane == view {
                ctx.data_mut(|data| {
                    data.remove::<egui::text_edit::TextEditState>(egui::Id::new((
                        "document", id, view,
                    )))
                });
            }
            pane != view
        });
    }

    pub fn retain_documents(&mut self, ctx: &egui::Context, documents: &Documents) {
        self.prepared_views.retain(|&(id, pane)| {
            let keep = documents.items.iter().any(|doc| doc.id == id);
            if !keep {
                ctx.data_mut(|data| {
                    data.remove::<egui::text_edit::TextEditState>(egui::Id::new((
                        "document", id, pane,
                    )))
                });
            }
            keep
        });
        self.shared_state
            .retain(|id, _| documents.items.iter().any(|document| document.id == *id));
        if let Some(syntax) = &mut self.syntax {
            syntax
                .cached
                .retain(|id, _| documents.items.iter().any(|document| document.id == *id));
        }
    }

    pub fn show(
        &mut self,
        ui: &mut Ui,
        document: &mut Document,
        config: &Config,
        pane: u64,
        search_match: Option<&std::ops::Range<usize>>,
    ) -> bool {
        self.prepare_state(ui.ctx(), document, pane);
        let font = FontId::monospace(config.editor.font_size);
        let id = Self::id(document, pane);
        let selected = document.selection.take();
        if let Some(range) = &selected {
            let mut state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
            state.cursor.set_char_range(Some(CCursorRange::two(
                CCursor::new(range.start),
                CCursor::new(range.end),
            )));
            state.store(ui.ctx(), id);
        }
        let show_numbers = config.animations.line_numbers.progress(
            ui.ctx(),
            "line numbers animation",
            config.interface.show_line_numbers,
            config.animations.enabled,
        );
        let gutter = if show_numbers > 0.0 {
            let digits = document.line_count().ilog10() + 1;
            let digit_width = ui.fonts(|fonts| fonts.glyph_width(&font, '0'));
            show_numbers * (digits as f32 * digit_width + 16.0)
        } else {
            0.0
        };
        let number_color = ui
            .visuals()
            .weak_text_color()
            .gamma_multiply(show_numbers.min(1.0));
        let foreground = ui.visuals().text_color();
        let wrap_lines = document.wrap_lines.unwrap_or(config.editor.wrap_lines);
        let document_id = document.id;
        let path = document.path.clone();
        let mut layout = |ui: &Ui, text: &str, wrap_width: f32| {
            let wrap_width = if wrap_lines {
                wrap_width
            } else {
                f32::INFINITY
            };
            let mut job = if config.interface.syntax_highlighting {
                self.syntax
                    .get_or_insert_with(|| SyntaxLayouts::new(config.palette()))
                    .highlight(document_id, path.as_deref(), text, &font, foreground)
            } else {
                LayoutJob::simple(text.to_owned(), font.clone(), foreground, wrap_width)
            };
            if config.editor.tab_size != 4 && job.text.contains('\t') {
                resize_tabs(&mut job, config.editor.tab_size, ui);
            }
            job.wrap.max_width = wrap_width;
            ui.fonts(|fonts| fonts.layout_job(job))
        };
        let scroll = ScrollArea::new([!wrap_lines, true]).id_salt(id);
        scroll
            .show(ui, |ui| {
                let mut area = ui.available_rect_before_wrap();
                let gutter_right = area.left() + gutter;
                area.min.x = gutter_right;
                let mut editor = ui.new_child(
                    UiBuilder::new()
                        .max_rect(area)
                        .layout(Layout::top_down(Align::Min)),
                );
                let line_shape = ui.painter().add(egui::Shape::Noop);
                let mut buffer = EditBuffer {
                    document,
                    config: &config.editor,
                };
                let output = TextEdit::multiline(&mut buffer)
                    .id(id)
                    .font(font.clone())
                    .lock_focus(true)
                    .frame(false)
                    .margin(egui::Margin::ZERO)
                    .min_size(area.size())
                    .desired_width(f32::INFINITY)
                    .desired_rows(1)
                    .layouter(&mut layout)
                    .show(&mut editor);
                ui.advance_cursor_after_rect(editor.min_rect().union(area));
                if let Some(range) = output.cursor_range {
                    document.cursor = range.primary.ccursor.index;
                    let other = range.secondary.ccursor.index;
                    document.selected_range = (document.cursor != other)
                        .then_some(document.cursor.min(other)..document.cursor.max(other));
                    if output.response.has_focus() {
                        let caret = output
                            .galley
                            .pos_from_cursor(&range.primary)
                            .translate(output.galley_pos.to_vec2());
                        let line =
                            egui::Rect::from_x_y_ranges(ui.clip_rect().x_range(), caret.y_range())
                                .intersect(ui.clip_rect());
                        if line.is_positive() {
                            ui.painter().set(
                                line_shape,
                                egui::Shape::rect_filled(line, 0.0, config.palette().current_line),
                            );
                        }
                    }
                }
                if let Some(found) = search_match {
                    let galley = &output.galley;
                    let start = galley.from_ccursor(CCursor::new(found.start)).rcursor;
                    let end = galley.from_ccursor(CCursor::new(found.end)).rcursor;
                    let painter = ui.painter().with_clip_rect(output.text_clip_rect);
                    let color = ui.visuals().selection.stroke.color.gamma_multiply(0.75);
                    for index in start.row..=end.row {
                        let Some(row) = galley.rows.get(index) else {
                            break;
                        };
                        let left = if index == start.row {
                            row.x_offset(start.column)
                        } else {
                            row.rect.left()
                        };
                        let right = if index == end.row {
                            row.x_offset(end.column)
                        } else {
                            row.rect.right()
                        };
                        if right > left {
                            let rect = egui::Rect::from_min_max(
                                egui::pos2(left, row.min_y()),
                                egui::pos2(right, row.max_y()),
                            )
                            .translate(output.galley_pos.to_vec2());
                            painter.rect_stroke(
                                rect,
                                2.0,
                                egui::Stroke::new(1.0_f32, color),
                                egui::StrokeKind::Inside,
                            );
                        }
                    }
                }
                let cursor_style = ui.visuals().text_cursor.stroke;
                let cursor = if config.cursor_trail.enabled_for(Target::Editor)
                    && output.response.has_focus()
                    && let Some(range) = output.cursor_range
                {
                    let caret = output.galley.pos_from_cursor(&range.primary);
                    let rect = egui::Rect::from_center_size(
                        caret.center(),
                        egui::vec2(cursor_style.width.max(1.0), caret.height()),
                    );
                    let cell_width = ui.fonts(|fonts| fonts.glyph_width(&font, 'M'));
                    Some(Cursor {
                        surface: id,
                        rect,
                        origin: output.galley_pos.to_vec2(),
                        cell: egui::vec2(cell_width, caret.height()),
                        color: cursor_style.color,
                    })
                } else {
                    None
                };
                self.trail.draw(
                    &ui.painter().with_clip_rect(output.text_clip_rect),
                    &config.cursor_trail,
                    cursor,
                );
                if let Some(range) = selected {
                    let cursor = output.galley.from_ccursor(CCursor::new(range.end));
                    ui.scroll_to_rect(
                        output
                            .galley
                            .pos_from_cursor(&cursor)
                            .translate(output.galley_pos.to_vec2()),
                        Some(Align::Center),
                    );
                }
                if gutter > 0.0 {
                    let mut line = 1;
                    let mut start = true;
                    let painter = ui.painter().with_clip_rect(ui.clip_rect().intersect(
                        egui::Rect::from_min_max(
                            egui::pos2(gutter_right - gutter, ui.clip_rect().top()),
                            egui::pos2(gutter_right, ui.clip_rect().bottom()),
                        ),
                    ));
                    let top = ui.clip_rect().top() - output.galley_pos.y;
                    let bottom = ui.clip_rect().bottom() - output.galley_pos.y;
                    let rows = &output.galley.rows;
                    let first = rows.partition_point(|row| row.rect.bottom() < top);
                    if !wrap_lines {
                        line = first + 1;
                    } else {
                        line += rows[..first]
                            .iter()
                            .filter(|row| row.ends_with_newline)
                            .count();
                        start = first == 0 || rows[first - 1].ends_with_newline;
                    }
                    for row in rows[first..]
                        .iter()
                        .take_while(|row| row.rect.top() <= bottom)
                    {
                        let y = output.galley_pos.y + row.rect.top();
                        if start
                            && y + row.rect.height() >= ui.clip_rect().top()
                            && y <= ui.clip_rect().bottom()
                        {
                            painter.text(
                                egui::pos2(gutter_right - 10.0, y),
                                Align2::RIGHT_TOP,
                                line,
                                font.clone(),
                                number_color,
                            );
                        }
                        start = row.ends_with_newline;
                        line += usize::from(start);
                    }
                }
                output.response.has_focus()
            })
            .inner
    }
}

pub fn tabs(ctx: &egui::Context, documents: &Documents, config: &Config) -> Option<usize> {
    let height = config.interface.tab_height
        * config.animations.tabs.progress(
            ctx,
            "tabs animation",
            config.interface.show_tabs,
            config.animations.enabled,
        );
    if height <= 0.0 {
        return None;
    }
    let palette = config.palette();
    egui::TopBottomPanel::top("tabs")
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::NONE.fill(palette.tabs_background))
        .exact_height(height)
        .show(ctx, |ui| {
            let mut area = ui.max_rect().shrink2(egui::vec2(10.0, 0.0));
            area.max.y = area.min.y + config.interface.tab_height;
            let mut bar = ui.new_child(
                UiBuilder::new()
                    .max_rect(area)
                    .layout(Layout::left_to_right(Align::Center)),
            );
            bar.spacing_mut().item_spacing.x = 0.0;
            bar.add_space(8.0);
            let mut action = None;
            ScrollArea::horizontal()
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(&mut bar, |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for (index, document) in documents.items.iter().enumerate() {
                        let active = index == documents.active;
                        let foreground = palette.tab_text(active);
                        let fill = if active {
                            palette.active_tab_background
                        } else {
                            Color32::TRANSPARENT
                        };
                        let tab = egui::Frame::NONE
                            .fill(fill)
                            .inner_margin(egui::Margin::symmetric(12, 6))
                            .show(ui, |ui| {
                                ui.set_min_width(config.interface.tab_min_width);
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 8.0;
                                    if document.dirty() {
                                        let (dot, _) = ui.allocate_exact_size(
                                            egui::vec2(6.0, config.interface.tab_font_size),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().circle_filled(
                                            dot.center(),
                                            2.5,
                                            palette.accent,
                                        );
                                    }
                                    if config.interface.show_file_icons {
                                        let ext = document
                                            .path
                                            .as_deref()
                                            .and_then(std::path::Path::extension)
                                            .and_then(std::ffi::OsStr::to_str);
                                        let icon = match ext {
                                            Some("rs" | "js" | "ts" | "c" | "cpp" | "py") => "<>",
                                            Some("toml" | "json" | "yaml" | "yml") => "{}",
                                            _ => "≡",
                                        };
                                        ui.label(
                                            RichText::new(icon).color(palette.tab_text_inactive),
                                        );
                                    }
                                    ui.label(
                                        RichText::new(truncate_tab_name(
                                            &document.name(),
                                            config.interface.tab_max_chars,
                                        ))
                                        .size(config.interface.tab_font_size)
                                        .color(foreground),
                                    );
                                });
                            });
                        if active {
                            ui.painter().hline(
                                tab.response.rect.x_range(),
                                tab.response.rect.bottom(),
                                egui::Stroke::new(2.0_f32, palette.accent),
                            );
                            let key = ui.id().with("visible active tab");
                            let previous = ui.data_mut(|data| data.get_temp::<u64>(key));
                            if previous != Some(document.id) {
                                tab.response.scroll_to_me(None);
                                ui.data_mut(|data| data.insert_temp(key, document.id));
                            }
                        }
                        if ui
                            .interact(
                                tab.response.rect,
                                ui.make_persistent_id(("synth_tab", document.id)),
                                egui::Sense::click(),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text(document.display_path())
                            .clicked()
                        {
                            action = Some(index);
                        }
                    }
                });
            action
        })
        .inner
}

fn truncate_tab_name(name: &str, max_chars: usize) -> String {
    let mut chars = name.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

pub fn status(ctx: &egui::Context, document: &Document, config: &Config) {
    let height = config.interface.status_bar_height
        * config.animations.status_bar.progress(
            ctx,
            "status animation",
            config.interface.show_status_bar,
            config.animations.enabled,
        );
    if height <= 0.0 {
        return;
    }
    egui::TopBottomPanel::bottom("status")
        .frame(egui::Frame::NONE.fill(config.palette().status_background))
        .exact_height(height)
        .show(ctx, |ui| {
            let mut area = ui.max_rect().shrink2(egui::vec2(12.0, 0.0));
            area.max.y = area.min.y + config.interface.status_bar_height;
            let mut bar = ui.new_child(
                UiBuilder::new()
                    .max_rect(area)
                    .layout(Layout::left_to_right(Align::Center)),
            );
            let name = document.name();
            let label = if document.dirty() {
                format!("{name} *")
            } else {
                name
            };
            let (line, column) = document.position();
            let lines = document.line_count();
            let encoding = document.encoding();
            let ending = document.ending.label();
            let unit = if lines == 1 { "line" } else { "lines" };
            bar.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!(
                        "{encoding}  |  {ending}  |  {lines} {unit}  |  {line}:{column}"
                    ))
                    .size(config.interface.status_bar_font_size),
                );
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(label).size(config.interface.status_bar_font_size),
                        )
                        .truncate(),
                    )
                    .on_hover_text(document.display_path());
                });
            });
        });
}

struct SyntaxLayouts {
    syntaxes: SyntaxSet,
    theme: SyntaxTheme,
    cached: HashMap<u64, HighlightedDocument>,
}

#[derive(Clone, PartialEq)]
struct SyntaxStyle {
    syntax: usize,
    font: FontId,
}

struct HighlightedDocument {
    style: SyntaxStyle,
    job: LayoutJob,
    checkpoints: Vec<HighlightCheckpoint>,
}

struct HighlightCheckpoint {
    byte: usize,
    sections: usize,
    highlight: HighlightState,
    parse: ParseState,
}

const HIGHLIGHT_CHECKPOINT_LINES: usize = 64;

impl SyntaxLayouts {
    fn new(palette: Palette) -> Self {
        let color = |value: Color32| syntect::highlighting::Color {
            r: value.r(),
            g: value.g(),
            b: value.b(),
            a: 255,
        };
        let mut theme = SyntaxTheme::default();
        theme.settings.foreground = Some(color(palette.foreground));
        for (scope, ink) in [
            ("keyword, storage, entity.name.tag", palette.accent),
            ("string", palette.syntax_string),
            ("constant, support.type", palette.syntax_literal),
            ("comment", palette.syntax_comment),
        ] {
            theme.scopes.push(ThemeItem {
                scope: scope.parse().expect("built-in syntax scope"),
                style: StyleModifier {
                    foreground: Some(color(ink)),
                    ..Default::default()
                },
            });
        }
        Self {
            syntaxes: two_face::syntax::extra_newlines(),
            theme,
            cached: HashMap::new(),
        }
    }

    fn highlight(
        &mut self,
        id: u64,
        path: Option<&std::path::Path>,
        text: &str,
        font: &FontId,
        foreground: Color32,
    ) -> LayoutJob {
        let syntax = path
            .and_then(|path| {
                let name = path.file_name()?.to_str()?;
                self.syntaxes
                    .find_syntax_by_extension(name)
                    .or_else(|| self.syntaxes.find_syntax_by_name(name))
                    .or_else(|| {
                        self.syntaxes
                            .find_syntax_by_extension(path.extension()?.to_str()?)
                    })
            })
            .or_else(|| {
                text.lines()
                    .next()
                    .and_then(|first| self.syntaxes.find_syntax_by_first_line(first))
            });
        let Some(syntax) = syntax else {
            self.cached.remove(&id);
            return LayoutJob::simple(text.to_owned(), font.clone(), foreground, f32::INFINITY);
        };
        let style = SyntaxStyle {
            syntax: self
                .syntaxes
                .syntaxes()
                .iter()
                .position(|candidate| std::ptr::eq(candidate, syntax))
                .expect("syntax in this set"),
            font: font.clone(),
        };
        let cached = self
            .cached
            .entry(id)
            .or_insert_with(|| HighlightedDocument {
                style: style.clone(),
                job: LayoutJob::default(),
                checkpoints: Vec::new(),
            });
        if cached.style == style && cached.job.text == text {
            return cached.job.clone();
        }
        if cached.style != style {
            cached.style = style;
            cached.job = LayoutJob::default();
            cached.checkpoints.clear();
        }
        let theme = &self.theme;
        let prefix = cached
            .job
            .text
            .bytes()
            .zip(text.bytes())
            .take_while(|(previous, current)| previous == current)
            .count();
        let old_len = cached.job.text.len();
        let suffix = cached.job.text.as_bytes()[prefix..]
            .iter()
            .rev()
            .zip(text.as_bytes()[prefix..].iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        let prefix_points = cached
            .checkpoints
            .partition_point(|point| point.byte <= prefix);
        let old_points = cached.checkpoints.split_off(prefix_points);
        let old_section_start = cached.checkpoints.last().map_or(0, |point| point.sections);
        let old_sections = cached.job.sections.split_off(old_section_start);
        let shift = text.len() as isize - old_len as isize;
        let mut suffix_points = old_points
            .into_iter()
            .filter(|point| point.byte >= old_len - suffix)
            .peekable();
        let (offset, mut highlighter) = if let Some(checkpoint) = cached.checkpoints.last() {
            cached.job.text.truncate(checkpoint.byte);
            cached.job.sections.truncate(checkpoint.sections);
            (
                checkpoint.byte,
                HighlightLines::from_state(
                    theme,
                    checkpoint.highlight.clone(),
                    checkpoint.parse.clone(),
                ),
            )
        } else {
            cached.job = LayoutJob::default();
            (0, HighlightLines::new(syntax, theme))
        };
        let mut old_sections = old_sections.into_iter();
        for (index, line) in LinesWithEndings::from(&text[offset..]).enumerate() {
            let byte = cached.job.text.len();
            while suffix_points
                .peek()
                .is_some_and(|point| point.byte.saturating_add_signed(shift) < byte)
            {
                suffix_points.next();
            }
            let candidate = suffix_points
                .peek()
                .filter(|point| point.byte.saturating_add_signed(shift) == byte);
            if (index > 0 && index % HIGHLIGHT_CHECKPOINT_LINES == 0) || candidate.is_some() {
                let (highlight, parse) = highlighter.state();
                if let Some(old) = candidate
                    && old.highlight == highlight
                    && old.parse == parse
                {
                    let section_shift = cached.job.sections.len() as isize - old.sections as isize;
                    let skip = old.sections - old_section_start;
                    cached.job.text.push_str(&text[byte..]);
                    cached
                        .job
                        .sections
                        .extend(old_sections.by_ref().skip(skip).map(|mut section| {
                            section.byte_range =
                                section.byte_range.start.saturating_add_signed(shift)
                                    ..section.byte_range.end.saturating_add_signed(shift);
                            section
                        }));
                    cached.checkpoints.extend(suffix_points.map(|mut point| {
                        point.byte = point.byte.saturating_add_signed(shift);
                        point.sections = point.sections.saturating_add_signed(section_shift);
                        point
                    }));
                    break;
                }
                highlighter = HighlightLines::from_state(theme, highlight.clone(), parse.clone());
                if cached
                    .checkpoints
                    .last()
                    .is_none_or(|point| point.byte < byte)
                {
                    cached.checkpoints.push(HighlightCheckpoint {
                        byte,
                        sections: cached.job.sections.len(),
                        highlight,
                        parse,
                    });
                }
            }
            let Ok(segments) = highlighter.highlight_line(line, &self.syntaxes) else {
                self.cached.remove(&id);
                return LayoutJob::simple(text.to_owned(), font.clone(), foreground, f32::INFINITY);
            };
            for (style, segment) in segments {
                let color = style.foreground;
                cached.job.append(
                    segment,
                    0.0,
                    TextFormat::simple(font.clone(), Color32::from_rgb(color.r, color.g, color.b)),
                );
            }
        }
        cached.job.clone()
    }
}

#[derive(Default)]
pub struct SearchBar {
    open: bool,
    focus_query: bool,
    next_requested: bool,
    show_replacement: bool,
    query: String,
    replacement: String,
    document: Option<(u64, u64)>,
    matches: Vec<std::ops::Range<usize>>,
    current: usize,
    highlight: Option<std::ops::Range<usize>>,
}

impl SearchBar {
    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open = true;
            self.focus_query = true;
        }
    }

    pub fn close(&mut self) {
        self.open = false;
        self.next_requested = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn highlight(&self) -> Option<&std::ops::Range<usize>> {
        self.highlight.as_ref().filter(|_| self.open)
    }

    fn update_matches(&mut self, document: &Document) {
        self.document = Some((document.id, document.revision));
        self.matches.clear();
        if !self.query.is_empty() {
            self.matches.extend(
                document
                    .text
                    .match_indices(&self.query)
                    .map(|(start, text)| start..start + text.len()),
            );
        }
    }

    fn refresh_after_edit(&mut self, document: &Document) {
        self.update_matches(document);
        let byte = document.byte_at_char(document.cursor);
        self.current = self.matches.partition_point(|found| found.end < byte);
        if self.current == self.matches.len() {
            self.current = 0;
        }
        self.highlight = self.matches.get(self.current).map(|found| {
            let start = document.text[..found.start].chars().count();
            start..start + self.query.chars().count()
        });
    }

    fn select(&mut self, document: &mut Document) {
        self.highlight = None;
        if let Some(found) = self.matches.get(self.current) {
            let start = document.text[..found.start].chars().count();
            let end = start + self.query.chars().count();
            document.cursor = end;
            document.selection = Some(start..end);
            self.highlight = Some(start..end);
        }
    }

    pub fn query_id() -> egui::Id {
        egui::Id::new("search query")
    }

    pub fn request_next(&mut self) {
        self.next_requested = true;
    }

    pub fn show(&mut self, ui: &mut Ui, document: &mut Document, pane: u64) {
        if !self.open {
            return;
        }
        let mut previous = false;
        let mut next = std::mem::take(&mut self.next_requested);
        let mut replace = false;
        let mut replace_all = false;
        ui.horizontal_wrapped(|ui| {
            let query_width = (ui.available_width() - 170.0).clamp(65.0, 180.0);
            let query = ui.add(
                TextEdit::singleline(&mut self.query)
                    .id(Self::query_id())
                    .desired_width(query_width)
                    .hint_text("Find"),
            );
            if std::mem::take(&mut self.focus_query) {
                query.request_focus();
            }
            if query.changed() {
                self.update_matches(document);
                self.current = 0;
                self.select(document);
            } else if self.document != Some((document.id, document.revision)) {
                self.refresh_after_edit(document);
            }
            let count = self.matches.len();
            ui.label(if count == 0 {
                "0 matches".to_owned()
            } else {
                format!("{} of {count}", self.current + 1)
            });
            previous = ui
                .add_enabled(count > 0, egui::Button::new("<"))
                .on_hover_text("Previous match")
                .clicked();
            next |= ui
                .add_enabled(count > 0, egui::Button::new(">"))
                .on_hover_text("Next match")
                .clicked();
            ui.toggle_value(&mut self.show_replacement, "Replace…");
            if ui.button("×").on_hover_text("Close search").clicked() {
                self.close();
                ui.memory_mut(|memory| memory.request_focus(Editor::id(document, pane)));
            }
        });
        if self.show_replacement {
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    TextEdit::singleline(&mut self.replacement)
                        .desired_width(155.0)
                        .hint_text("Replace with"),
                );
                replace = ui
                    .add_enabled(!self.matches.is_empty(), egui::Button::new("Replace"))
                    .clicked();
                replace_all = ui
                    .add_enabled(!self.matches.is_empty(), egui::Button::new("Replace all"))
                    .clicked();
            });
        }
        ui.separator();

        if (replace || replace_all) && self.replacement != self.query {
            let id = Editor::id(document, pane);
            let mut state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
            let before = CCursorRange::one(CCursor::new(document.cursor));
            let mut undoer = state.undoer();
            undoer.add_undo(&(before, document.text.clone()));
            let byte = if replace_all {
                document.text = document.text.replace(&self.query, &self.replacement);
                0
            } else {
                let range = self.matches[self.current].clone();
                let end = range.start + self.replacement.len();
                document.text.replace_range(range, &self.replacement);
                end
            };
            document.text_changed();
            self.update_matches(document);
            self.current = self
                .matches
                .iter()
                .position(|found| found.start >= byte)
                .unwrap_or(0);
            self.select(document);
            if self.matches.is_empty() {
                let cursor = document.text[..byte].chars().count();
                document.cursor = cursor;
                document.selection = Some(cursor..cursor);
            }
            state.set_undoer(undoer);
            state.store(ui.ctx(), id);
            ui.memory_mut(|memory| memory.request_focus(id));
        } else if !self.matches.is_empty() && (next || previous || replace) {
            let step = if previous { self.matches.len() - 1 } else { 1 };
            self.current = (self.current + step) % self.matches.len();
            self.select(document);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_insertions_and_deletions_preserve_indentation_and_crlf() {
        use egui::TextBuffer;
        let mut docs = Documents::default();
        let doc = docs.current_mut();
        doc.text = "  猫".into();
        doc.set_ending(LineEnding::Crlf).unwrap();
        let config = crate::config::Editor {
            tab_size: 3,
            insert_spaces: true,
            auto_indent: true,
            ..Default::default()
        };
        let mut buffer = EditBuffer {
            document: doc,
            config: &config,
        };
        assert_eq!(buffer.insert_text("\n", 3), 4);
        assert_eq!(buffer.as_str(), "  猫\r\n  ");
        assert_eq!(buffer.insert_text("\t", 7), 1);
        assert_eq!(buffer.as_str(), "  猫\r\n   ");
        let cursor = buffer.delete_previous_char(CCursor::new(5));
        assert_eq!(cursor.index, 3);
        assert_eq!(buffer.as_str(), "  猫   ");
    }

    #[test]
    #[ignore = "manual large-document timing"]
    fn large_document_edit_latency() {
        use std::{hint::black_box, time::Instant};

        let source = "let token = \"猫\";\n".repeat(100_000);
        for at_end in [false, true] {
            for search_open in [false, true] {
                let mut docs = Documents::default();
                let doc = docs.current_mut();
                doc.text.clone_from(&source);
                doc.text_changed();
                let mut bar = SearchBar {
                    open: search_open,
                    query: "token".into(),
                    ..Default::default()
                };
                if search_open {
                    bar.refresh_after_edit(doc);
                }
                let cursor = if at_end { source.chars().count() } else { 0 };
                let mut times = Vec::new();
                for sample in 0..35 {
                    let start = Instant::now();
                    doc.replace_chars(cursor..cursor, "x");
                    doc.cursor = cursor + 1;
                    let indexed = start.elapsed();
                    if search_open {
                        bar.refresh_after_edit(doc);
                        assert_eq!(bar.matches.len(), 100_000);
                    }
                    black_box((&*doc, &bar));
                    let elapsed = start.elapsed();
                    if sample >= 4 {
                        times.push((elapsed, indexed));
                    }
                    doc.replace_chars(cursor..cursor + 1, "");
                    doc.cursor = cursor;
                    if search_open {
                        bar.refresh_after_edit(doc);
                    }
                }
                times.sort_unstable();
                let (total, index) = times[times.len() / 2];
                println!(
                    "end={at_end} search={search_open} median={total:?} edit_and_index={index:?}"
                );
            }
        }
    }

    #[test]
    fn refreshing_search_preserves_the_editing_cursor() {
        let mut docs = Documents::default();
        let doc = docs.current_mut();
        doc.text = "cat cat\nxsecond".into();
        doc.text_changed();
        doc.cursor = 9;
        let mut bar = SearchBar {
            query: "cat".into(),
            ..Default::default()
        };
        bar.refresh_after_edit(doc);
        assert_eq!(bar.matches.len(), 2);
        assert_eq!(doc.cursor, 9);
        assert_eq!(doc.selection, None);
    }

    #[test]
    fn extra_languages_produce_styled_tokens() {
        let mut layouts = SyntaxLayouts::new(Config::default().palette());
        let font = FontId::monospace(16.0);
        for (path, text) in [
            ("app.ts", "const greeting: string = 'hello';"),
            ("main.tsx", "export const App = () => <div>Hello</div>;"),
            ("build.zig", "const name = \"synth\";"),
            ("app.kt", "val name = \"synth\""),
            ("theme.toml", "mode = 'dark'"),
            ("Dockerfile", "FROM alpine:latest"),
        ] {
            let job = layouts.highlight(
                0,
                Some(std::path::Path::new(path)),
                text,
                &font,
                Color32::WHITE,
            );
            assert_eq!(job.text, text);
            assert!(
                job.sections
                    .iter()
                    .any(|s| s.format.color != job.sections[0].format.color),
                "{path}"
            );
        }
    }

    #[test]
    fn incremental_highlighting_matches_a_full_pass_after_multiline_edits() {
        let mut layouts = SyntaxLayouts::new(Config::default().palette());
        let path = Some(std::path::Path::new("file.rs"));
        let font = FontId::monospace(16.0);
        let mut text = "let value = 1;\n".repeat(90);
        text.push_str("/* begin\n");
        text.push_str(&"inside\n".repeat(80));
        text.push_str("*/\nlet tail = 2;\n");
        layouts.highlight(1, path, &text, &font, Color32::WHITE);
        layouts.highlight(2, path, "fn main() {}", &font, Color32::WHITE);
        assert!(layouts.cached.contains_key(&1));
        for edited in [
            text.replace("inside\n*/", "changed\n*/"),
            text.replace("/* begin", "// begin"),
            text[..text.find("/* begin").unwrap()].to_owned(),
        ] {
            let incremental = layouts.highlight(1, path, &edited, &font, Color32::WHITE);
            let full = SyntaxLayouts::new(Config::default().palette()).highlight(
                1,
                path,
                &edited,
                &font,
                Color32::WHITE,
            );
            assert_eq!(incremental, full);
        }
    }

    #[test]
    fn truncation_respects_unicode_characters() {
        assert_eq!(truncate_tab_name("document.txt", 8), "document...");
        assert_eq!(truncate_tab_name("猫猫猫", 2), "猫猫...");
        assert_eq!(truncate_tab_name("short", 8), "short");
    }
}
