use eframe::egui::{
    self, Align, Align2, Color32, FontId, Layout, RichText, ScrollArea, TextEdit, Ui, UiBuilder,
    text::{CCursor, CCursorRange, LayoutJob, TextFormat},
};
use syntect::{
    easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet, util::LinesWithEndings,
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
        let mut layout = |ui: &Ui, text: &str, wrap_width: f32| {
            let mut job = if config.interface.enable_syntax_highlighting {
                self.syntax
                    .get_or_insert_with(SyntaxLayouts::new)
                    .highlight(
                        document.id,
                        document.path.as_deref(),
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
    let height = 36.0
        * config.animations.tabs.progress(
            ctx,
            "tabs animation",
            config.interface.enable_tabs,
            config.animations.enabled,
        );
    if height <= 0.0 {
        return None;
    }
    egui::TopBottomPanel::top("tabs")
        .frame(egui::Frame::NONE.fill(config.editor.theme.terminal_background()))
        .exact_height(height)
        .show(ctx, |ui| {
            let mut area = ui.max_rect().shrink2(egui::vec2(10.0, 0.0));
            area.max.y = area.min.y + 36.0;
            let mut bar = ui.new_child(
                UiBuilder::new()
                    .max_rect(area)
                    .layout(Layout::left_to_right(Align::Center)),
            );
            let mut selected = None;
            ScrollArea::horizontal()
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(&mut bar, |ui| {
                    for (index, document) in documents.items.iter().enumerate() {
                        let name = document.name();
                        let label = if document.dirty() {
                            format!("● {name}")
                        } else {
                            name
                        };
                        if ui
                            .selectable_label(
                                index == documents.active,
                                RichText::new(label).monospace(),
                            )
                            .clicked()
                        {
                            selected = Some(index);
                        }
                    }
                });
            selected
        })
        .inner
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
        .frame(egui::Frame::NONE.fill(config.editor.theme.terminal_background()))
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
                format!("● {name}")
            } else {
                name
            };
            let (line, column) = document.position();
            bar.label(RichText::new(label).monospace());
            bar.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format!("UTF-8   {line}:{column}")).monospace());
            });
        });
}

struct SyntaxLayouts {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
    cached: Option<(SyntaxStyle, LayoutJob)>,
}

#[derive(PartialEq)]
struct SyntaxStyle {
    id: u64,
    extension: Option<String>,
    theme: Theme,
    font: FontId,
}

impl SyntaxLayouts {
    fn new() -> Self {
        Self {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            themes: ThemeSet::load_defaults(),
            cached: None,
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
        let style = SyntaxStyle {
            id,
            extension,
            theme,
            font: font.clone(),
        };
        if let Some((previous, job)) = &self.cached
            && previous == &style
            && job.text == text
        {
            return job.clone();
        }
        let syntax = style
            .extension
            .as_deref()
            .and_then(|extension| self.syntaxes.find_syntax_by_extension(extension))
            .or_else(|| {
                text.lines()
                    .next()
                    .and_then(|first| self.syntaxes.find_syntax_by_first_line(first))
            });
        let Some(syntax) = syntax else {
            return LayoutJob::simple(text.to_owned(), font.clone(), foreground, f32::INFINITY);
        };
        let theme = match theme {
            Theme::Dark => "base16-ocean.dark",
            Theme::Light => "InspiredGitHub",
        };
        let mut highlighter = HighlightLines::new(syntax, &self.themes.themes[theme]);
        let mut job = LayoutJob::default();
        for line in LinesWithEndings::from(text) {
            let Ok(segments) = highlighter.highlight_line(line, &self.syntaxes) else {
                return LayoutJob::simple(text.to_owned(), font.clone(), foreground, f32::INFINITY);
            };
            for (style, segment) in segments {
                let color = style.foreground;
                job.append(
                    segment,
                    0.0,
                    TextFormat::simple(font.clone(), Color32::from_rgb(color.r, color.g, color.b)),
                );
            }
        }
        self.cached = Some((style, job.clone()));
        job
    }
}
