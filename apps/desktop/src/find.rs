//! Cmd+F: search the focused window, a chat or a file, for a word. The bar counts the matches
//! and Enter (Shift+Enter backwards) steps through them; the window scrolls to the current one
//! and marks them all.

use std::sync::Arc;

use egui::epaint::text::cursor::CCursor;
use egui::{Align, Align2, Key, RichText};
use xade_core::{Message, Role};

use crate::app::App;
use crate::theme;
use crate::workspace::WindowKind;

/// What the matches were computed for: the query, the window and a revision of its content.
type MatchKey = (String, Option<WindowKind>, u64);

pub struct FindState {
    pub query: String,
    /// Index of the current match.
    current: usize,
    matches: Arc<Vec<usize>>,
    key: Option<MatchKey>,
    /// Bumped whenever the view should scroll to the current match.
    seq: u64,
    focus: bool,
}

/// What a window needs to draw its matches; published each frame for the window searched.
#[derive(Clone)]
pub struct FindMark {
    pub target: WindowKind,
    /// A file: the char index of each match. A chat: the message index of each match.
    pub matches: Arc<Vec<usize>>,
    pub query: String,
    /// Chars in the query.
    pub len: usize,
    pub current: usize,
    pub seq: u64,
}

fn mark_id() -> egui::Id {
    egui::Id::unique("find_mark")
}

/// The matches to draw in `window`, if it is the one being searched.
pub fn mark(ctx: &egui::Context, window: WindowKind) -> Option<FindMark> {
    ctx.data(|d| d.get_temp::<FindMark>(mark_id()))
        .filter(|m| m.target == window)
}

/// Whether the view should scroll to the current match now, and remembers that it did.
pub fn take_jump(ui: &egui::Ui, mark: &FindMark) -> bool {
    let id = egui::Id::unique(("find_seen", mark.target));
    let jump = ui.data(|d| d.get_temp::<u64>(id)) != Some(mark.seq);
    if jump {
        ui.data_mut(|d| d.insert_temp(id, mark.seq));
    }
    jump
}

impl FindState {
    pub fn new() -> Self {
        FindState {
            query: String::new(),
            current: 0,
            matches: Arc::new(Vec::new()),
            key: None,
            seq: 0,
            focus: true,
        }
    }

    /// Cmd+F while the bar is open: back to the text field.
    pub fn refocus(&mut self) {
        self.focus = true;
    }

    fn step(&mut self, forward: bool) {
        let n = self.matches.len();
        if n == 0 {
            return;
        }
        self.current = if forward {
            (self.current + 1) % n
        } else {
            (self.current + n - 1) % n
        };
        self.seq += 1;
    }
}

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Char indexes where `query` starts in `text`, ignoring case, without overlapping.
pub fn find_chars(text: &str, query: &str) -> Vec<usize> {
    let needle: Vec<char> = query.chars().map(lower).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let hay: Vec<char> = text.chars().map(lower).collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()] == needle[..] {
            found.push(i);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    found
}

/// The message index of every match in the chat's own text (yours and the assistant's), one
/// entry per match.
pub fn find_messages(messages: &[Message], query: &str) -> Vec<usize> {
    let mut found = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if matches!(m.role, Role::User | Role::Assistant) {
            let n = find_chars(&m.content_text, query).len();
            found.extend(std::iter::repeat_n(i, n));
        }
    }
    found
}

/// Follows the focused window, recomputes the matches when the query, the window or its content
/// changed, and publishes them for the window to draw. Runs before the windows draw.
pub fn update(app: &mut App, ctx: &egui::Context) {
    let Some(f) = app.find.as_mut() else {
        ctx.data_mut(|d| d.remove::<FindMark>(mark_id()));
        return;
    };
    let target = app.focused;
    let revision = match target {
        Some(WindowKind::Editor(id)) => app.editors.map.get(&id).map_or(0, |vm| vm.revision),
        Some(WindowKind::Session(id)) => app
            .sessions
            .get(&id)
            .map_or(0, |vm| vm.messages.len() as u64),
        None => 0,
    };
    let key: MatchKey = (f.query.clone(), target, revision);
    if f.key.as_ref() != Some(&key) {
        let moved = f.key.as_ref().is_none_or(|k| k.0 != key.0 || k.1 != key.1);
        let found = match target {
            Some(WindowKind::Editor(id)) => app
                .editors
                .map
                .get(&id)
                .map(|vm| find_chars(&vm.text, &f.query)),
            Some(WindowKind::Session(id)) => app
                .sessions
                .get(&id)
                .map(|vm| find_messages(&vm.messages, &f.query)),
            None => None,
        };
        f.matches = Arc::new(found.unwrap_or_default());
        if moved {
            f.current = 0;
            f.seq += 1;
        } else {
            f.current = f.current.min(f.matches.len().saturating_sub(1));
        }
        f.key = Some(key);
    }
    match target {
        Some(target) if !f.query.is_empty() => {
            let mark = FindMark {
                target,
                matches: Arc::clone(&f.matches),
                query: f.query.clone(),
                len: f.query.chars().count(),
                current: f.current,
                seq: f.seq,
            };
            ctx.data_mut(|d| d.insert_temp(mark_id(), mark));
        }
        _ => ctx.data_mut(|d| d.remove::<FindMark>(mark_id())),
    }
}

/// The find bar, top right of the focused window under its header (top right of the screen when
/// no window is focused); Esc closes it.
pub fn show(app: &mut App, ctx: &egui::Context) {
    if app.find.is_none() {
        return;
    }
    let anchor = app
        .focused
        .and_then(|k| crate::workspace::geom_of(app, k))
        .and_then(|g| *g.1)
        .map(|r| r.right_top() + egui::vec2(-12.0, 42.0));
    let Some(f) = app.find.as_mut() else { return };
    let mut close = false;
    let area = egui::Area::new(egui::Id::unique("find_bar")).order(egui::Order::Foreground);
    match anchor {
        Some(p) => area.pivot(Align2::RIGHT_TOP).fixed_pos(p),
        None => area.anchor(Align2::RIGHT_TOP, egui::vec2(-16.0, 56.0)),
    }
    .show(ctx, |ui| {
        egui::Frame::popup(ui.style())
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut f.query)
                            .hint_text("Find in window")
                            .desired_width(220.0),
                    );
                    if f.focus {
                        edit.request_focus();
                        f.focus = false;
                    }
                    let (enter, esc, shift) = ui.input(|i| {
                        (
                            i.key_pressed(Key::Enter),
                            i.key_pressed(Key::Escape),
                            i.modifiers.shift,
                        )
                    });
                    if edit.lost_focus() && enter {
                        f.step(!shift);
                        edit.request_focus();
                    } else if edit.lost_focus() && esc {
                        close = true;
                    }
                    let has_window = f.key.as_ref().is_some_and(|k| k.1.is_some());
                    let text = if !has_window {
                        "No window".to_string()
                    } else if f.query.is_empty() {
                        String::new()
                    } else if f.matches.is_empty() {
                        "No results".to_string()
                    } else {
                        format!("{} / {}", f.current + 1, f.matches.len())
                    };
                    ui.label(RichText::new(text).color(theme::text_muted()));
                });
            });
    });
    if close {
        app.find = None;
    }
}

/// Fills one match in `galley` (drawn at `pos`); in a file the current one is jade with its glyphs
/// redrawn in black, in a chat the matched text alone is relaid out in black. Matches wrapped over two rows are
/// skipped.
pub fn paint_word(
    painter: &egui::Painter,
    galley: &std::sync::Arc<egui::Galley>,
    pos: egui::Pos2,
    start: usize,
    len: usize,
    current: bool,
    in_chat: bool,
) {
    let a = galley.pos_from_cursor(CCursor::new(start));
    let b = galley.pos_from_cursor(CCursor::new(start + len));
    if (b.min.y - a.min.y).abs() > 1.0 {
        return;
    }
    let rect = egui::Rect::from_min_max(
        egui::pos2(pos.x + a.min.x, pos.y + a.min.y),
        egui::pos2(pos.x + b.min.x, pos.y + a.max.y),
    );
    let color = if current {
        theme::find_current()
    } else {
        theme::find_match()
    };
    painter.rect_filled(rect, 2.0, color);
    if current && in_chat {
        // Redrawing the whole galley would paint inline code's own background back over the
        // fill, so lay out just the matched text in the font of its first glyph.
        let height = galley
            .rows
            .iter()
            .find(|r| r.rect().min.y >= a.min.y - 0.5)
            .and_then(|r| r.glyphs.first())
            .map_or(0.0, |g| g.font_height);
        let word: String = galley.text().chars().skip(start).take(len).collect();
        let font = [
            egui::TextStyle::Body,
            egui::TextStyle::Monospace,
            egui::TextStyle::Small,
            egui::TextStyle::Button,
            egui::TextStyle::Heading,
        ]
        .into_iter()
        .filter_map(|s| painter.ctx().global_style().text_styles.get(&s).cloned())
        .find(|f| (painter.ctx().fonts_mut(|fs| fs.row_height(f)) - height).abs() < 0.5)
        .unwrap_or_else(|| egui::FontId::monospace(height.max(8.0)));
        let text = painter.layout_no_wrap(word, font, egui::Color32::BLACK);
        painter.galley(rect.min, text, egui::Color32::BLACK);
    } else if current {
        painter
            .with_clip_rect(rect.intersect(painter.clip_rect()))
            .galley_with_override_text_color(pos, galley.clone(), egui::Color32::BLACK);
    }
}

/// Marks the words of a chat message just drawn: every text shape added to the ui's layer since
/// `from` (a `next_idx` taken before drawing). `nth` is which occurrence in the message is the
/// current match, if any.
pub fn paint_message_words(ui: &egui::Ui, from: usize, query: &str, nth: usize) {
    let len = query.chars().count();
    let shapes: Vec<_> = ui.ctx().graphics(|g| {
        g.get(ui.layer_id()).map_or_else(Vec::new, |list| {
            list.all_entries()
                .skip(from)
                .filter_map(|c| match &c.shape {
                    egui::Shape::Text(t) => Some((t.galley.clone(), t.pos, c.clip_rect)),
                    _ => None,
                })
                .collect()
        })
    });
    let mut seen = 0;
    for (galley, pos, clip) in shapes {
        let painter = ui.painter().with_clip_rect(clip.intersect(ui.clip_rect()));
        for start in find_chars(galley.text(), query) {
            paint_word(&painter, &galley, pos, start, len, seen == nth, true);
            seen += 1;
        }
    }
}

/// Marks every match in a file's text edit and scrolls to the current one when asked. Only the
/// matches on screen are drawn.
pub fn paint_text_matches(
    ui: &egui::Ui,
    galley: &std::sync::Arc<egui::Galley>,
    pos: egui::Pos2,
    mark: &FindMark,
) {
    let clip = ui.clip_rect();
    let len = mark.len;
    let rect_of = |start: usize| {
        let a = galley.pos_from_cursor(CCursor::new(start));
        let b = galley.pos_from_cursor(CCursor::new(start + len));
        egui::Rect::from_min_max(
            egui::pos2(pos.x + a.min.x, pos.y + a.min.y),
            egui::pos2(pos.x + b.min.x, pos.y + a.max.y),
        )
    };
    let top: usize = galley
        .cursor_from_pos(egui::vec2(0.0, clip.top() - pos.y))
        .index
        .into();
    let bottom: usize = galley
        .cursor_from_pos(egui::vec2(1.0e6, clip.bottom() - pos.y))
        .index
        .into();
    let m = &mark.matches;
    let (lo, hi) = (
        m.partition_point(|&s| s + len < top),
        m.partition_point(|&s| s <= bottom),
    );
    for (i, &start) in m.iter().enumerate().take(hi).skip(lo) {
        paint_word(
            ui.painter(),
            galley,
            pos,
            start,
            len,
            i == mark.current,
            false,
        );
    }
    if take_jump(ui, mark) {
        if let Some(&start) = m.get(mark.current) {
            ui.scroll_to_rect(rect_of(start), Some(Align::Center));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn finds_every_occurrence_ignoring_case_without_overlap() {
        assert_eq!(find_chars("Foo bar foo FOO", "foo"), [0, 8, 12]);
        assert_eq!(find_chars("aaaa", "aa"), [0, 2]);
        assert_eq!(find_chars("a\u{e9}b", "\u{c9}"), [1]);
        assert!(find_chars("abc", "").is_empty());
        assert!(find_chars("abc", "abcd").is_empty());
    }

    #[test]
    fn the_bar_follows_the_focused_file_and_publishes_its_matches() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-find-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        // Nothing focused: the action only warns.
        app.run_action(crate::keymap::Action::Find);
        assert!(app.find.is_none());
        let id = app.editors.create(None, "Foo bar foo\nfoo".into());
        let other = app.editors.create(None, "nothing here".into());
        app.focus(WindowKind::Editor(id));
        app.run_action(crate::keymap::Action::Find);
        app.find.as_mut().unwrap().query = "foo".into();
        let pass = |app: &mut App| {
            ctx.run_pass(egui::RawInput::default(), |ctx| update(app, ctx))
                .drop_without_applying_deltas();
        };
        pass(&mut app);
        let mark = mark(&ctx, WindowKind::Editor(id)).expect("published");
        assert_eq!((mark.matches.as_slice(), mark.len), (&[0, 8, 12][..], 3));
        assert!(super::mark(&ctx, WindowKind::Editor(other)).is_none());
        // Enter steps to the next match and asks the view to scroll again.
        let seq = mark.seq;
        app.find.as_mut().unwrap().step(true);
        pass(&mut app);
        let next = super::mark(&ctx, WindowKind::Editor(id)).unwrap();
        assert_eq!(next.current, 1);
        assert!(next.seq > seq);
        // Focusing another window retargets the search.
        app.focus(WindowKind::Editor(other));
        pass(&mut app);
        assert!(super::mark(&ctx, WindowKind::Editor(id)).is_none());
        assert_eq!(app.find.as_ref().unwrap().matches.len(), 0);
        // Closing the bar clears the marks.
        app.find = None;
        pass(&mut app);
        assert!(super::mark(&ctx, WindowKind::Editor(other)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stepping_wraps_both_ways_and_asks_the_view_to_scroll() {
        let mut f = FindState::new();
        f.matches = Arc::new(vec![3, 9, 20]);
        let seq = f.seq;
        f.step(true);
        f.step(true);
        f.step(true);
        assert_eq!(f.current, 0);
        f.step(false);
        assert_eq!(f.current, 2);
        assert!(f.seq > seq);
        f.matches = Arc::new(Vec::new());
        let seq = f.seq;
        f.step(true);
        assert_eq!(f.seq, seq, "nothing to step to");
    }
}
