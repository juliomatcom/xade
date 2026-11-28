//! The one markdown helper every message text goes through.

use std::fmt::Debug;
use std::hash::Hash;

use egui::text::LayoutJob;
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use crate::find::FindMark;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

const CODE_BLOCK_OVERSHOOT: f32 = 16.0;
/// Narrowest a table column gets before its text wraps word by word.
const TABLE_MIN_COL: f32 = 80.0;
/// Absolute floor when even the minimum widths do not fit the window.
const TABLE_FLOOR_COL: f32 = 24.0;
/// Space between table columns.
const TABLE_GAP_X: f32 = 16.0;
/// Padding inside a table row, around its cells.
const TABLE_PAD_X: f32 = 6.0;
const TABLE_PAD_Y: f32 = 5.0;
/// Code block theme: the default one paints blocks a bluish gray, lighter than the window.
const CODE_THEME: &str = "xade-code";
const CODE_THEME_FILE: &[u8] = include_bytes!("../../../../assets/xade-code.tmTheme");

/// A markdown cache that knows the code block theme; use it wherever a message is rendered.
pub fn new_cache() -> CommonMarkCache {
    let mut cache = CommonMarkCache::default();
    if let Err(e) = cache.add_syntax_theme_from_bytes(CODE_THEME, CODE_THEME_FILE) {
        tracing::warn!("code theme not loaded: {e}");
    }
    cache
}

/// Renders `text` as markdown. An unclosed fence renders as code to the end of the text.
/// Top-level tables are laid out here, wrapped to the window; the rest goes to the viewer.
pub fn markdown(ui: &mut egui::Ui, cache: &mut CommonMarkCache, id: impl Hash + Debug, text: &str) {
    ui.push_id(id, |ui| {
        prepare(ui);
        for (i, segment) in segments(text).iter().enumerate() {
            match segment {
                Segment::Text(t) => viewer(ui, cache, i, t),
                Segment::Table(t) => {
                    table_view(ui, t, None);
                }
            }
        }
    });
}

/// Code blocks are text edits that overshoot the width they are given by their frame, and the
/// code theme sets no background, so blocks take egui's `extreme_bg_color`.
fn prepare(ui: &mut egui::Ui) {
    ui.set_max_width((ui.available_width() - CODE_BLOCK_OVERSHOOT).max(40.0));
    ui.visuals_mut().extreme_bg_color = crate::theme::bg_terminal();
}

/// A markdown file's preview, searchable. Text is drawn by the viewer, which keeps its matches
/// and the current one under `id.with(segment)` in `cache` (the bar's count comes from
/// `find::preview_matches`); tables are drawn here and mark their own cells. `mark` is the
/// search to show, if this window is being searched, and `jump` scrolls to its current match.
pub fn preview(
    ui: &mut egui::Ui,
    cache: &mut CommonMarkCache,
    id: egui::Id,
    text: &str,
    mark: Option<&FindMark>,
    jump: bool,
) {
    ui.push_id(id, |ui| {
        prepare(ui);
        // Matches before the segment being drawn: a match's place in the whole document.
        let mut before = 0;
        for (k, segment) in segments(text).iter().enumerate() {
            let sid = id.with(k);
            before += match segment {
                Segment::Text(t) => {
                    let n = crate::find::sync_text(cache, sid, mark, before, jump);
                    search_viewer(ui, cache, sid, t);
                    n
                }
                Segment::Table(t) => {
                    table_view(ui, t, mark.map(|mark| TableFind { mark, before, jump }))
                }
            };
        }
    });
}

fn search_viewer(ui: &mut egui::Ui, cache: &mut CommonMarkCache, id: egui::Id, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    CommonMarkViewer::new()
        .syntax_theme_dark(CODE_THEME)
        .search_match_color(crate::theme::find_match())
        .search_active_match_color(crate::theme::find_current())
        .show_with_id(id, ui, cache, text);
}

fn viewer(ui: &mut egui::Ui, cache: &mut CommonMarkCache, id: impl Hash + Debug, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    ui.push_id(id, |ui| {
        CommonMarkViewer::new()
            .syntax_theme_dark(CODE_THEME)
            .show(ui, cache, text);
    });
}

/// A table's cells as plain text: header row first.
type Table = Vec<Vec<String>>;

/// The markdown split around its top-level tables, in document order.
pub enum Segment<'a> {
    Text(&'a str),
    Table(Table),
}

pub fn segments(text: &str) -> Vec<Segment<'_>> {
    let tables = if text.contains('|') {
        top_level_tables(text)
    } else {
        Vec::new()
    };
    let mut out = Vec::new();
    let mut at = 0;
    for (range, table) in tables {
        out.push(Segment::Text(&text[at..range.start]));
        out.push(Segment::Table(table));
        at = range.end;
    }
    out.push(Segment::Text(&text[at..]));
    out
}

/// How many times `query` is in the table's cells.
pub fn table_matches(table: &Table, query: &str) -> usize {
    let cells = table.iter().flatten();
    cells.map(|c| crate::find::find_chars(c, query).len()).sum()
}

/// The search a table marks: the matches in the segments before it, and whether to scroll.
struct TableFind<'a> {
    mark: &'a FindMark,
    before: usize,
    jump: bool,
}

/// Tables that are not nested in a list or quote, with their source range.
fn top_level_tables(text: &str) -> Vec<(std::ops::Range<usize>, Table)> {
    let mut tables = Vec::new();
    let mut depth = 0;
    let mut cell = String::new();
    let mut current: Option<(usize, Table)> = None;
    for (event, range) in Parser::new_ext(text, Options::ENABLE_TABLES).into_offset_iter() {
        match event {
            Event::Start(Tag::Table(_)) if depth == 0 => current = Some((range.start, Vec::new())),
            Event::Start(Tag::List(_) | Tag::BlockQuote(_)) => depth += 1,
            Event::End(TagEnd::List(_) | TagEnd::BlockQuote(_)) => depth -= 1,
            Event::Start(Tag::TableHead | Tag::TableRow) => {
                if let Some((_, rows)) = &mut current {
                    rows.push(Vec::new());
                }
            }
            Event::Start(Tag::TableCell) => cell.clear(),
            Event::Text(t) | Event::Code(t) => cell.push_str(&t),
            Event::SoftBreak => cell.push(' '),
            Event::End(TagEnd::TableCell) => {
                if let Some(row) = current.as_mut().and_then(|(_, rows)| rows.last_mut()) {
                    row.push(std::mem::take(&mut cell));
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some((start, rows)) = current.take() {
                    tables.push((start..range.end, rows));
                }
            }
            _ => {}
        }
    }
    tables
}

/// Column widths: natural width when it all fits, else every column keeps at least
/// `TABLE_MIN_COL` and the rest of the window is shared by how much each column wants.
fn column_widths(natural: &[f32], avail: f32) -> Vec<f32> {
    if natural.iter().sum::<f32>() <= avail {
        return natural.to_vec();
    }
    let min: Vec<f32> = natural.iter().map(|n| n.min(TABLE_MIN_COL)).collect();
    let spare = avail - min.iter().sum::<f32>();
    if spare <= 0.0 {
        let each = (avail / natural.len() as f32).max(TABLE_FLOOR_COL);
        return min.iter().map(|m| m.min(each)).collect();
    }
    let want: f32 = natural.iter().zip(&min).map(|(n, m)| n - m).sum();
    natural
        .iter()
        .zip(&min)
        .map(|(n, m)| m + spare * (n - m) / want)
        .collect()
}

/// A table whose rows are laid out by hand: `egui::Grid` keeps last frame's column widths,
/// which stops a session window from getting narrower than its table.
fn table_view(ui: &mut egui::Ui, table: &Table, find: Option<TableFind>) -> usize {
    let cols = table.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return 0;
    }
    let mut seen = 0;
    let jobs: Vec<Vec<LayoutJob>> = table
        .iter()
        .enumerate()
        .map(|(r, row)| {
            let color = if r == 0 {
                ui.visuals().strong_text_color()
            } else {
                ui.visuals().text_color()
            };
            let font = egui::TextStyle::Body.resolve(ui.style());
            row.iter()
                .map(|c| LayoutJob::simple(c.clone(), font.clone(), color, f32::INFINITY))
                .collect()
        })
        .collect();
    let frame = egui::Frame::group(ui.style());
    let gaps = TABLE_GAP_X * (cols - 1) as f32 + 2.0 * TABLE_PAD_X;
    let avail = ui.available_width() - frame.total_margin().sum().x - gaps;
    let mut natural = vec![0.0_f32; cols];
    for row in &jobs {
        for (c, job) in row.iter().enumerate() {
            let mut line = job.clone();
            line.wrap.max_width = f32::INFINITY;
            let w = ui.fonts_mut(|f| f.layout_job(line)).size().x;
            natural[c] = natural[c].max(w.ceil() + 1.0);
        }
    }
    let widths = column_widths(&natural, avail.max(TABLE_FLOOR_COL * cols as f32));
    let stripe = ui.visuals().faint_bg_color;
    frame.show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for (r, row) in jobs.iter().enumerate() {
            let galleys: Vec<_> = widths
                .iter()
                .enumerate()
                .map(|(c, w)| {
                    let mut job = row.get(c).cloned().unwrap_or_default();
                    job.wrap.max_width = *w;
                    ui.fonts_mut(|f| f.layout_job(job))
                })
                .collect();
            let height = galleys.iter().map(|g| g.size().y).fold(0.0, f32::max);
            let size = egui::vec2(
                widths.iter().sum::<f32>() + gaps,
                height + 2.0 * TABLE_PAD_Y,
            );
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            if r % 2 == 1 {
                ui.painter().rect_filled(rect, 0.0, stripe);
            }
            let mut x = rect.min.x + TABLE_PAD_X;
            for (galley, w) in galleys.into_iter().zip(&widths) {
                // The slot is exactly the column wide, so a word wider than it can neither
                // stretch the table nor keep the window from shrinking.
                let slot = egui::Rect::from_min_size(
                    egui::pos2(x, rect.min.y + TABLE_PAD_Y),
                    egui::vec2(*w, height),
                );
                let mut cell = ui.new_child(egui::UiBuilder::new().max_rect(slot));
                cell.set_clip_rect(slot.intersect(ui.clip_rect()));
                cell.add(
                    egui::Label::new(egui::WidgetText::Galley(galley.clone())).selectable(true),
                );
                if let Some(f) = &find {
                    for start in crate::find::find_chars(galley.text(), &f.mark.query) {
                        let current = f.before + seen == f.mark.current;
                        seen += 1;
                        crate::find::paint_word(
                            &cell.painter().clone(),
                            &galley,
                            slot.min,
                            start,
                            f.mark.len,
                            current,
                            false,
                        );
                        if current && f.jump {
                            ui.scroll_to_rect(slot, Some(egui::Align::Center));
                        }
                    }
                }
                x += w + TABLE_GAP_X;
            }
        }
    });
    seen
}

/// A table whose cells are long sentences, as a model writes them (for the views' tests).
#[cfg(test)]
pub(crate) fn wide_table() -> String {
    let cell = "a sentence that is long enough to need several lines in a narrow window ";
    format!(
        "Results:\n\n| Use case | Status | Evidence |\n|---|---|---|\n| {} | PASS | {} |\n| short | NOT COMPLETED | {} |",
        cell.repeat(6),
        cell.repeat(6),
        cell.repeat(3),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(text: &str) {
        let ctx = egui::Context::default();
        let mut cache = CommonMarkCache::default();
        for _ in 0..2 {
            let out = ctx.run_ui(egui::RawInput::default(), |ui| {
                markdown(ui, &mut cache, 1, text);
            });
            out.drop_without_applying_deltas();
        }
    }

    #[test]
    fn the_code_theme_file_parses() {
        let mut cache = CommonMarkCache::default();
        cache
            .add_syntax_theme_from_bytes(CODE_THEME, CODE_THEME_FILE)
            .expect("assets/xade-code.tmTheme must be a valid tmTheme");
    }

    #[test]
    fn unclosed_fence_does_not_panic() {
        render("Here:\n\n```rust\nfn main() {\n    let x = 1;");
        render("```");
        render("");
    }

    #[test]
    fn table_list_and_inline_code_render() {
        render("# T\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n- one\n- `two`\n\n> quote\n\n[l](https://x.y)");
    }

    #[test]
    fn tables_are_found_at_top_level_only() {
        let text = "intro\n\n| a | `b` |\n|---|---|\n| long cell | 2 |\n\nafter\n\n- | x |\n  |---|\n  | y |";
        let tables = top_level_tables(text);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].1, vec![vec!["a", "b"], vec!["long cell", "2"]]);
        assert!(text[tables[0].0.clone()].starts_with("| a"));
    }

    #[test]
    fn columns_fit_the_window_and_keep_a_minimum() {
        assert_eq!(column_widths(&[50.0, 60.0], 400.0), vec![50.0, 60.0]);
        let w = column_widths(&[40.0, 900.0, 600.0], 500.0);
        assert!((w.iter().sum::<f32>() - 500.0).abs() < 0.01);
        assert_eq!(w[0], 40.0);
        assert!(w[1] > w[2] && w[2] > TABLE_MIN_COL);
        let tight = column_widths(&[300.0, 300.0, 300.0, 300.0], 200.0);
        assert!(tight.iter().all(|w| *w >= TABLE_FLOOR_COL));
    }

    #[test]
    fn a_wide_table_renders_within_the_window() {
        let ctx = egui::Context::default();
        let mut cache = CommonMarkCache::default();
        let text = format!("| a | b |\n|---|---|\n| {} | short |", "word ".repeat(200));
        let mut size = egui::Vec2::ZERO;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(500.0, 800.0),
            )),
            ..Default::default()
        };
        for _ in 0..2 {
            let out = ctx.run_ui(input.clone(), |ui| {
                let r = ui.scope(|ui| markdown(ui, &mut cache, 1, &text));
                size = r.response.rect.size();
            });
            out.drop_without_applying_deltas();
        }
        assert!(
            size.x <= ctx.content_rect().width() + 1.0,
            "table is {} wide",
            size.x
        );
        assert!(
            size.y > 100.0,
            "the long cell did not wrap: {} high",
            size.y
        );
    }

    /// One frame of a resizable window holding `text`; returns the window's rect.
    fn window_frame(
        ctx: &egui::Context,
        cache: &mut CommonMarkCache,
        text: &str,
        events: Vec<egui::Event>,
    ) -> egui::Rect {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            events,
            ..Default::default()
        };
        let mut rect = egui::Rect::NOTHING;
        let out = ctx.run_ui(input, |ui| {
            let shown = egui::Window::new("session")
                .id(egui::Id::unique("window"))
                .title_bar(false)
                .min_size([100.0, 80.0])
                .default_size([700.0, 400.0])
                .default_pos([10.0, 10.0])
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| markdown(ui, cache, 1, text));
                });
            rect = shown.map_or(rect, |s| s.response.rect);
        });
        out.drop_without_applying_deltas();
        rect
    }

    #[test]
    fn a_table_does_not_stop_the_window_from_shrinking() {
        let ctx = egui::Context::default();
        let mut cache = CommonMarkCache::default();
        let text = format!(
            "| Use case | Status | Evidence |\n|---|---|---|\n| {} | PASS | {} |",
            "word ".repeat(30),
            "other ".repeat(30)
        );
        let mut rect = egui::Rect::NOTHING;
        for _ in 0..4 {
            rect = window_frame(&ctx, &mut cache, &text, vec![]);
        }
        // Drag the bottom-right corner 400px to the left, 40px a frame.
        let corner = rect.right_bottom() - egui::vec2(2.0, 2.0);
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        window_frame(
            &ctx,
            &mut cache,
            &text,
            vec![egui::Event::PointerMoved(corner)],
        );
        window_frame(&ctx, &mut cache, &text, vec![button(corner, true)]);
        for step in 1..=10 {
            let to = corner - egui::vec2(40.0 * step as f32, 0.0);
            rect = window_frame(&ctx, &mut cache, &text, vec![egui::Event::PointerMoved(to)]);
        }
        assert!(
            rect.width() < 350.0,
            "the window stopped at {} wide",
            rect.width()
        );
    }

    #[test]
    fn a_preview_table_wraps_to_the_window_too() {
        let ctx = egui::Context::default();
        let mut cache = CommonMarkCache::default();
        let text = format!("| a | b |\n|---|---|\n| {} | short |", "word ".repeat(200));
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(500.0, 800.0),
            )),
            ..Default::default()
        };
        let mut size = egui::Vec2::ZERO;
        for _ in 0..2 {
            let out = ctx.run_ui(input.clone(), |ui| {
                let shown = ui
                    .scope(|ui| preview(ui, &mut cache, egui::Id::unique("p"), &text, None, false));
                size = shown.response.rect.size();
            });
            out.drop_without_applying_deltas();
        }
        assert!(size.x <= 501.0, "table is {} wide", size.x);
        assert!(
            size.y > 100.0,
            "the long cell did not wrap: {} high",
            size.y
        );
    }
}
