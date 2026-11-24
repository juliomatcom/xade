//! The per-turn "files changed" card and the pure code behind it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use egui::{Align, Layout, RichText, Stroke};
use uuid::Uuid;
use xade_core::{Message, Role};

use crate::diff::{is_code_change, DiffData};
use crate::theme;

/// One file edited during a turn, summed over its successful calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: PathBuf,
    pub added: usize,
    pub removed: usize,
    /// `tool_use_id`s of the edits, in order.
    pub calls: Vec<String>,
}

impl From<&crate::turn_files::TurnFile> for FileChange {
    /// No calls: the diff comes from the file's two versions, not from edit tool calls.
    fn from(f: &crate::turn_files::TurnFile) -> Self {
        FileChange {
            path: f.path.clone(),
            added: f.added,
            removed: f.removed,
            calls: Vec::new(),
        }
    }
}

/// Files changed by the successful code-change calls of `turn`.
pub fn turn_changes(turn: &[Message], cwd: &Path) -> Vec<FileChange> {
    let results: HashMap<&str, &Message> = turn
        .iter()
        .filter(|m| m.role == Role::ToolResult)
        .filter_map(|m| Some((m.tool_use_id.as_deref()?, m)))
        .collect();
    let mut files: Vec<FileChange> = Vec::new();
    for call in turn.iter().filter(|m| m.role == Role::ToolCall) {
        let (Some(tool), Some(id)) = (call.tool_name.as_deref(), call.tool_use_id.as_deref())
        else {
            continue;
        };
        let Some(result) = results.get(id).filter(|r| !r.is_error) else {
            continue;
        };
        if !is_code_change(tool) {
            continue;
        }
        let Some(d) = DiffData::build_offline(
            tool,
            call.tool_payload_json.as_deref().unwrap_or_default(),
            result.tool_payload_json.as_deref(),
            cwd,
        ) else {
            continue;
        };
        match files.iter_mut().find(|f| f.path == d.path) {
            Some(f) => {
                f.added += d.added;
                f.removed += d.removed;
                f.calls.push(id.to_string());
            }
            None => files.push(FileChange {
                path: d.path,
                added: d.added,
                removed: d.removed,
                calls: vec![id.to_string()],
            }),
        }
    }
    files
}

/// Per-turn results, keyed by the turn's last message id and checked against its length.
#[derive(Default)]
pub struct ChangesCache(HashMap<Uuid, (usize, Vec<FileChange>)>);

impl ChangesCache {
    /// The changes of a finished `turn`, computed once.
    pub fn get(&mut self, turn: &[Message], cwd: &Path) -> &[FileChange] {
        let Some(last) = turn.last() else { return &[] };
        let entry = self.0.entry(last.id).or_default();
        if entry.0 != turn.len() {
            *entry = (turn.len(), turn_changes(turn, cwd));
        }
        &entry.1
    }
}

fn counts(ui: &mut egui::Ui, added: usize, removed: usize) {
    // Right-to-left, so the removed count goes first.
    ui.label(RichText::new(format!("\u{2212}{removed}")).color(theme::DANGER));
    ui.label(RichText::new(format!("+{added}")).color(theme::OK));
}

/// Where a card's list came from, which sets how it words itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// The turn's edit tool calls.
    Edits,
    /// The working tree before and after the turn.
    Tree,
}

/// Draws the card, collapsed to its summary line until clicked (`id` keeps that per turn);
/// returns the index of the file row clicked.
pub fn show(
    ui: &mut egui::Ui,
    id: Uuid,
    cwd: &Path,
    files: &[FileChange],
    source: Source,
) -> Option<usize> {
    let mut clicked = None;
    egui::Frame::new()
        .fill(theme::BG_RAISED)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(6)
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
                ui.ctx(),
                egui::Id::unique(("changes_card", id)),
                false,
            );
            ui.horizontal(|ui| {
                state.show_toggle_button(ui, egui::collapsing_header::paint_default_icon);
                let noun = if files.len() == 1 { "file" } else { "files" };
                let what = match source {
                    Source::Edits => "changed",
                    Source::Tree => "changed in the working tree",
                };
                let title = egui::Label::new(
                    RichText::new(format!("{} {noun} {what}", files.len())).strong(),
                )
                .sense(egui::Sense::click());
                if ui
                    .add(title)
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    state.toggle(ui);
                }
                let (added, removed) = files
                    .iter()
                    .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
                ui.label(RichText::new(format!("+{added}")).color(theme::OK));
                ui.label(RichText::new(format!("\u{2212}{removed}")).color(theme::DANGER));
            });
            state.show_body_unindented(ui, |ui| {
                for (i, f) in files.iter().enumerate() {
                    let rel = f.path.strip_prefix(cwd).unwrap_or(&f.path);
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            counts(ui, f.added, f.removed);
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                let name =
                                    RichText::new(rel.display().to_string()).color(theme::ACCENT);
                                let row = ui.add(egui::Button::new(name).frame(false).truncate());
                                if row.clicked() {
                                    clicked = Some(i);
                                }
                            });
                        });
                    });
                }
            });
            state.store(ui.ctx());
        });
    clicked
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use time::OffsetDateTime;
    use xade_core::SessionId;

    fn msg(role: Role, tool: Option<(&str, &str)>, payload: Option<&str>, err: bool) -> Message {
        Message {
            id: Uuid::new_v4(),
            session_id: SessionId::new(),
            seq: 0,
            role,
            content_text: String::new(),
            thinking_text: None,
            tool_name: tool.map(|t| t.0.to_string()),
            tool_use_id: tool.map(|t| t.1.to_string()),
            tool_payload_json: payload.map(str::to_string),
            is_error: err,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    /// An Edit call (a -> b, c) with a matching result, failed or not.
    fn edit(id: &str, file: &str, err: bool) -> [Message; 2] {
        let input = format!(r#"{{"file_path":"{file}","old_string":"a","new_string":"b\nc"}}"#);
        [
            msg(Role::ToolCall, Some(("Edit", id)), Some(&input), false),
            msg(Role::ToolResult, Some(("Edit", id)), Some("{}"), err),
        ]
    }

    fn turn(edits: &[[Message; 2]]) -> Vec<Message> {
        let mut t = vec![msg(Role::User, None, None, false)];
        t.extend(edits.iter().flatten().cloned());
        t.push(msg(Role::Assistant, None, None, false));
        t
    }

    #[test]
    fn one_file_is_listed() {
        let t = turn(&[edit("1", "/w/a.rs", false)]);
        assert_eq!(turn_changes(&t, Path::new("/w")).len(), 1);
    }

    #[test]
    fn two_files_are_listed_with_counts() {
        let t = turn(&[edit("1", "/w/a.rs", false), edit("2", "src/b.rs", false)]);
        let files = turn_changes(&t, Path::new("/w"));
        let got: Vec<_> = files
            .iter()
            .map(|f| (f.path.clone(), f.added, f.removed))
            .collect();
        assert_eq!(
            got,
            [
                (PathBuf::from("/w/a.rs"), 2, 1),
                (PathBuf::from("/w/src/b.rs"), 2, 1)
            ]
        );
    }

    #[test]
    fn the_same_file_twice_is_merged() {
        let t = turn(&[
            edit("1", "/w/a.rs", false),
            edit("2", "/w/b.rs", false),
            edit("3", "/w/a.rs", false),
        ]);
        let files = turn_changes(&t, Path::new("/w"));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].calls, ["1", "3"]);
        assert_eq!((files[0].added, files[0].removed), (4, 2));
    }

    #[test]
    fn failed_and_unfinished_calls_are_excluded() {
        let mut t = turn(&[edit("1", "/w/a.rs", false), edit("2", "/w/b.rs", true)]);
        assert_eq!(turn_changes(&t, Path::new("/w")).len(), 1);
        let [call, _] = edit("3", "/w/c.rs", false);
        t.insert(1, call);
        assert_eq!(turn_changes(&t, Path::new("/w")).len(), 1);
    }

    #[test]
    fn the_cache_recomputes_only_when_the_turn_changes() {
        let mut t = turn(&[edit("1", "/w/a.rs", false)]);
        let mut cache = ChangesCache::default();
        assert_eq!(cache.get(&t, Path::new("/w")).len(), 1);
        // Same last message id but a longer turn: recomputed.
        let last = t.pop().unwrap();
        t.extend(edit("2", "/w/b.rs", false));
        t.push(last);
        assert_eq!(cache.get(&t, Path::new("/w")).len(), 2);
    }

    /// Every text the card draws for `source`.
    fn card_texts(source: Source) -> String {
        let files = ["a.rs", "b.rs"].map(|n| FileChange {
            path: PathBuf::from(format!("/w/{n}")),
            added: 1,
            removed: 0,
            calls: Vec::new(),
        });
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            show(ui, Uuid::nil(), Path::new("/w"), &files, source);
        });
        out.textures_delta.clear();
        fn walk(s: &egui::Shape, t: &mut Vec<String>) {
            match s {
                egui::Shape::Text(x) => t.push(x.galley.text().to_string()),
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, t)),
                _ => {}
            }
        }
        let mut texts = Vec::new();
        out.shapes.iter().for_each(|c| walk(&c.shape, &mut texts));
        texts.join("|")
    }

    #[test]
    fn the_card_says_where_its_list_came_from() {
        let edits = card_texts(Source::Edits);
        assert!(
            edits.contains("2 files changed") && !edits.contains("working tree"),
            "{edits}"
        );
        let tree = card_texts(Source::Tree);
        assert!(
            tree.contains("2 files changed in the working tree"),
            "{tree}"
        );
        assert!(!tree.contains("Another session"), "{tree}");
    }
}
