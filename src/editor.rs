use std::collections::HashMap;

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Layout, RichText, ScrollArea, TextEdit, Ui, UiBuilder,
    text::{CCursor, CCursorRange, LayoutJob, TextFormat},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{HighlightState, ThemeSet},
    parsing::{ParseState, SyntaxSet},
    util::LinesWithEndings,
};

use crate::{
    config::{Config, Theme},
    cursor_trail::{Cursor, Target, Trail},
    document::{Document, Documents},
};

#[derive(Default)]
pub struct Editor {
    syntax: Option<SyntaxLayouts>,
    trail: Trail,
}

impl Editor {
    pub fn id(document: &Document) -> egui::Id {
        egui::Id::new(("document", document.id))
    }

    pub fn retain_documents(&mut self, documents: &Documents) {
        if let Some(syntax) = &mut self.syntax {
            syntax
                .cached
                .retain(|id, _| documents.items.iter().any(|document| document.id == *id));
        }
    }

    pub fn show(&mut self, ui: &mut Ui, document: &mut Document, config: &Config) {
        let font = FontId::monospace(config.editor.font_size);
        let id = Self::id(document);
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
            config.interface.enable_line_numbers,
            config.animations.enabled,
        );
        let gutter = show_numbers * (font.size * 3.4);
        let number_color = ui
            .visuals()
            .weak_text_color()
            .gamma_multiply(show_numbers.min(1.0));
        let foreground = ui.visuals().text_color();
        let document_id = document.id;
        let path = document.path.clone();
        let mut layout = |ui: &Ui, text: &str, wrap_width: f32| {
            let mut job = if config.interface.enable_syntax_highlighting {
                self.syntax
                    .get_or_insert_with(SyntaxLayouts::new)
                    .highlight(
                        document_id,
                        path.as_deref(),
                        text,
                        &font,
                        config.editor.theme,
                        foreground,
                    )
            } else {
                LayoutJob::simple(text.to_owned(), font.clone(), foreground, wrap_width)
            };
            job.wrap.max_width = wrap_width;
            ui.fonts(|fonts| fonts.layout_job(job))
        };
        ScrollArea::vertical().id_salt(id).show(ui, |ui| {
            let mut area = ui.available_rect_before_wrap();
            let gutter_right = area.left() + gutter;
            area.min.x = gutter_right;
            let mut editor = ui.new_child(
                UiBuilder::new()
                    .max_rect(area)
                    .layout(Layout::top_down(Align::Min)),
            );
            let output = TextEdit::multiline(&mut document.text)
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
            if output.response.changed() {
                document.text_changed();
            }
            if let Some(range) = output.cursor_range {
                document.cursor = range.primary.ccursor.index;
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
                for row in &output.galley.rows {
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
        });
    }
}

pub fn tabs(ctx: &egui::Context, documents: &Documents, config: &Config) -> Option<usize> {
    let height = config.interface.tab_height
        * config.animations.tabs.progress(
            ctx,
            "tabs animation",
            config.interface.enable_tabs,
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
                            .corner_radius(egui::CornerRadius::same(6))
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
                                        ui.painter().circle_filled(dot.center(), 2.5, foreground);
                                    }
                                    ui.label(
                                        RichText::new(truncate_tab_name(
                                            &document.name(),
                                            config.interface.tab_max_chars,
                                        ))
                                        .monospace()
                                        .size(config.interface.tab_font_size)
                                        .color(foreground),
                                    );
                                });
                            });
                        let tooltip = document.path.as_ref().map_or_else(
                            || document.name(),
                            |path| path.display().to_string(),
                        );
                        if ui
                            .interact(
                                tab.response.rect,
                                ui.make_persistent_id(("synth_tab", document.id)),
                                egui::Sense::click(),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text(tooltip)
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
    let height = 26.0
        * config.animations.status_bar.progress(
            ctx,
            "status animation",
            config.interface.enable_status_bar,
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
            area.max.y = area.min.y + 26.0;
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
            bar.label(RichText::new(label).monospace());
            bar.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format!("UTF-8  |  {lines} lines  |  {line}:{column}")).monospace());
            });
        });
}

struct SyntaxLayouts {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
    cached: HashMap<u64, HighlightedDocument>,
}

#[derive(Clone, PartialEq)]
struct SyntaxStyle {
    syntax: usize,
    theme: Theme,
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
    fn new() -> Self {
        Self {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            themes: ThemeSet::load_defaults(),
            cached: HashMap::new(),
        }
    }

    fn highlight(
        &mut self,
        id: u64,
        path: Option<&std::path::Path>,
        text: &str,
        font: &FontId,
        theme: Theme,
        foreground: Color32,
    ) -> LayoutJob {
        let extension = path
            .and_then(std::path::Path::extension)
            .and_then(std::ffi::OsStr::to_str)
            .map(str::to_owned);
        let syntax = extension
            .as_deref()
            .and_then(|extension| self.syntaxes.find_syntax_by_extension(extension))
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
            theme,
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
        let theme = match theme {
            Theme::Dark => "base16-ocean.dark",
            Theme::Light => "InspiredGitHub",
        };
        let theme = &self.themes.themes[theme];
        let prefix = cached
            .job
            .text
            .bytes()
            .zip(text.bytes())
            .take_while(|(previous, current)| previous == current)
            .count();
        cached.checkpoints.truncate(
            cached
                .checkpoints
                .partition_point(|checkpoint| checkpoint.byte <= prefix),
        );
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
        for (index, line) in LinesWithEndings::from(&text[offset..]).enumerate() {
            if index > 0 && index % HIGHLIGHT_CHECKPOINT_LINES == 0 {
                let (highlight, parse) = highlighter.state();
                highlighter = HighlightLines::from_state(theme, highlight.clone(), parse.clone());
                cached.checkpoints.push(HighlightCheckpoint {
                    byte: cached.job.text.len(),
                    sections: cached.job.sections.len(),
                    highlight,
                    parse,
                });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_highlighting_matches_a_full_pass_after_multiline_edits() {
        let mut layouts = SyntaxLayouts::new();
        let path = Some(std::path::Path::new("file.rs"));
        let font = FontId::monospace(16.0);
        let mut text = "let value = 1;\n".repeat(90);
        text.push_str("/* begin\n");
        text.push_str(&"inside\n".repeat(80));
        text.push_str("*/\nlet tail = 2;\n");
        layouts.highlight(1, path, &text, &font, Theme::Dark, Color32::WHITE);
        layouts.highlight(2, path, "fn main() {}", &font, Theme::Dark, Color32::WHITE);
        assert!(layouts.cached.contains_key(&1));
        for edited in [
            text.replace("inside\n*/", "changed\n*/"),
            text.replace("/* begin", "// begin"),
            text[..text.find("/* begin").unwrap()].to_owned(),
        ] {
            let incremental =
                layouts.highlight(1, path, &edited, &font, Theme::Dark, Color32::WHITE);
            let full = SyntaxLayouts::new().highlight(
                1,
                path,
                &edited,
                &font,
                Theme::Dark,
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
