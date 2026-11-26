//! The `@` file-mention popup and inserting `@path` mentions into the input.

use std::path::{Path, PathBuf};

use super::notes::set_attach_note;
use super::ComposerOut;
use crate::app::SessionVm;
use crate::{attach, mention, theme};
use egui::{Key, Modifiers, RichText};

/// A mention being typed at the cursor, while the popup is showing.
pub(super) struct Typing {
    start: usize,
    cursor: usize,
}

/// Decides whether the `@` popup is open this frame and handles its keys (Up/Down, Enter or
/// Tab to pick, Esc to dismiss) before the text box sees them.
pub(super) fn mention_popup_start(
    ui: &egui::Ui,
    vm: &mut SessionVm,
    input_id: egui::Id,
    focused: bool,
    out: &mut ComposerOut,
) -> Option<Typing> {
    let ctx = ui.ctx();
    let cursor = egui::TextEdit::load_state(ctx, input_id)
        .and_then(|s| s.cursor.char_range())
        .map(|r| r.primary.index.0);
    let pointer = ui.input(|i| i.pointer.latest_pos());
    let on_popup = vm
        .mention
        .popup_rect
        .zip(pointer)
        .is_some_and(|(r, p)| r.contains(p));
    let found = cursor
        .filter(|_| focused || on_popup)
        .and_then(|c| mention::active(&vm.input, c).map(|a| (a, c)));
    let Some((active, cursor)) = found else {
        vm.mention.dismissed = None;
        vm.mention.popup_rect = None;
        return None;
    };
    if vm.mention.dismissed == Some(active.start) {
        vm.mention.popup_rect = None;
        return None;
    }
    if vm.mention.needs_index() {
        out.index = true;
    }
    vm.mention.update(&active.query);
    let (esc, up, down, accept) = ui.input_mut(|i| {
        let none = Modifiers::NONE;
        let esc = i.consume_key(none, Key::Escape);
        let has = !vm.mention.matches.is_empty();
        (
            esc,
            has && i.consume_key(none, Key::ArrowUp),
            has && i.consume_key(none, Key::ArrowDown),
            has && (i.consume_key(none, Key::Enter) || i.consume_key(none, Key::Tab)),
        )
    });
    if esc {
        vm.mention.dismissed = Some(active.start);
        vm.mention.popup_rect = None;
        return None;
    }
    if up {
        vm.mention.step(false);
    }
    if down {
        vm.mention.step(true);
    }
    let typing = Typing {
        start: active.start,
        cursor,
    };
    if accept {
        let picked = vm.mention.selected;
        accept_mention(ctx, vm, input_id, &typing, picked);
        vm.mention.popup_rect = None;
        return None;
    }
    Some(typing)
}

/// Replaces the typed `@query` with the `idx`-th match and puts the cursor after it.
fn accept_mention(
    ctx: &egui::Context,
    vm: &mut SessionVm,
    input_id: egui::Id,
    typing: &Typing,
    idx: usize,
) {
    let Some(path) = vm.mention.matches.get(idx).cloned() else {
        return;
    };
    let (text, cursor) = mention::apply(&vm.input, typing.start, typing.cursor, &path);
    vm.input = text;
    let mut state = egui::TextEdit::load_state(ctx, input_id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(
            egui::text::CCursor::new(cursor),
        )));
    state.store(ctx, input_id);
    ctx.memory_mut(|m| m.request_focus(input_id));
}

/// Draws the popup above the input box; a click picks a row.
pub(super) fn mention_popup_show(
    ui: &egui::Ui,
    vm: &mut SessionVm,
    input_id: egui::Id,
    input_box: egui::Rect,
    typing: Option<Typing>,
) {
    let Some(typing) = typing else {
        return;
    };
    let ctx = ui.ctx().clone();
    let width = input_box.width().min(560.0);
    let mut clicked = None;
    let area = egui::Area::new(egui::Id::unique(("mention_popup", vm.session.id)))
        .order(egui::Order::Foreground)
        .pivot(egui::Align2::LEFT_BOTTOM)
        .fixed_pos(input_box.left_top() - egui::vec2(0.0, 4.0))
        .show(&ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(width - 16.0);
                if vm.mention.matches.is_empty() {
                    let msg = if matches!(vm.mention.files, mention::Files::Ready { .. }) {
                        "No matching files"
                    } else {
                        "Listing files\u{2026}"
                    };
                    ui.label(RichText::new(msg).small().color(theme::text_muted()));
                }
                for (i, path) in vm.mention.matches.iter().enumerate() {
                    let row = egui::Button::selectable(i == vm.mention.selected, path)
                        .truncate()
                        .min_size(egui::vec2(ui.available_width(), 0.0));
                    if ui.add(row).clicked() {
                        clicked = Some(i);
                    }
                }
            });
        });
    vm.mention.popup_rect = Some(area.response.rect);
    if let Some(i) = clicked {
        accept_mention(&ctx, vm, input_id, &typing, i);
        vm.mention.popup_rect = None;
    }
}

/// `@path` for the CLI to expand: relative to the session folder when inside it, quoted when
/// the path has whitespace.
fn mention(cwd: &Path, path: &Path) -> String {
    mention::token(&path.strip_prefix(cwd).unwrap_or(path).to_string_lossy())
}

/// Appends a mention per attachable path to `input`, separated by spaces. Returns a rejection
/// notice when any path is refused (binary, over the size limit, unreadable).
pub fn append_mentions(input: &mut String, cwd: &Path, paths: &[PathBuf]) -> Option<String> {
    let mut rejected = Vec::new();
    for path in paths {
        if let Err(why) = attach::check(path) {
            rejected.push((path.clone(), why));
            continue;
        }
        if !input.is_empty() && !input.ends_with(char::is_whitespace) {
            input.push(' ');
        }
        input.push_str(&mention(cwd, path));
        input.push(' ');
    }
    (!rejected.is_empty()).then(|| attach::rejected_message(&rejected))
}

/// Appends a mention per attachable path to the input, separated by spaces. Files refused
/// (binary, over the size limit, unreadable) are explained in a note above the input.
pub fn insert_mentions(vm: &mut SessionVm, paths: &[PathBuf]) {
    if let Some(msg) = append_mentions(&mut vm.input, &vm.cwd, paths) {
        set_attach_note(vm, msg);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::composer::test_support::{frame, press, vm};

    #[test]
    fn mentions_are_relative_inside_the_folder_and_quoted_with_spaces() {
        let cwd = Path::new("/p");
        assert_eq!(mention(cwd, Path::new("/p/src/a.rs")), "@src/a.rs");
        assert_eq!(
            mention(cwd, Path::new("/elsewhere/b.txt")),
            "@/elsewhere/b.txt"
        );
        assert_eq!(
            mention(cwd, Path::new("/p/my notes.txt")),
            "@\"my notes.txt\""
        );
    }

    #[test]
    fn inserted_mentions_are_space_separated_and_keep_the_typed_text() {
        let dir = std::env::temp_dir().join(format!("xade-ins-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.rs", "b c.rs", "d.rs"] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let mut v = vm("look at".into());
        v.cwd = dir.clone();
        insert_mentions(&mut v, &[dir.join("a.rs"), dir.join("b c.rs")]);
        assert!(v.attach_note.is_none());
        assert_eq!(v.input, "look at @a.rs @\"b c.rs\" ");
        insert_mentions(&mut v, &[dir.join("d.rs")]);
        assert_eq!(v.input, "look at @a.rs @\"b c.rs\" @d.rs ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn binary_and_missing_files_are_refused_with_a_message_and_the_rest_attached() {
        let dir = std::env::temp_dir().join(format!("xade-ins-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ok.rs"), "x").unwrap();
        std::fs::write(dir.join("blob.bin"), [0u8, 1, 2]).unwrap();
        let mut v = vm(String::new());
        v.cwd = dir.clone();
        insert_mentions(
            &mut v,
            &[
                dir.join("blob.bin"),
                dir.join("ok.rs"),
                dir.join("gone.txt"),
            ],
        );
        assert_eq!(v.input, "@ok.rs ");
        let (note, _) = v.attach_note.clone().unwrap();
        assert!(note.contains("blob.bin (binary)"), "{note}");
        assert!(note.contains("gone.txt (unreadable)"), "{note}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn typing_at_opens_the_popup_and_enter_inserts_the_file() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut v = vm(String::new());
        v.mention
            .set_files(vec!["README.md".into(), "src/main.rs".into()]);
        frame(&ctx, &mut v, vec![], true);
        frame(&ctx, &mut v, vec![egui::Event::Text("@ma".into())], false);
        let out = frame(&ctx, &mut v, vec![], false);
        assert!(!out.index, "the list is already loaded");
        assert_eq!(v.input, "@ma");
        assert_eq!(v.mention.matches, ["src/main.rs"]);
        assert!(v.mention.popup_rect.is_some(), "popup drawn");

        // Enter picks the row and does not add a newline.
        frame(&ctx, &mut v, vec![press(egui::Key::Enter)], false);
        assert_eq!(v.input, "@src/main.rs ");
        assert!(v.mention.popup_rect.is_none());

        // Typing on continues after the inserted mention.
        frame(&ctx, &mut v, vec![egui::Event::Text("ok".into())], false);
        assert_eq!(v.input, "@src/main.rs ok");
    }

    #[test]
    fn escape_dismisses_the_popup_until_a_new_mention_starts() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut v = vm(String::new());
        v.mention.set_files(vec!["src/main.rs".into()]);
        frame(&ctx, &mut v, vec![], true);
        frame(&ctx, &mut v, vec![egui::Event::Text("@m".into())], false);
        frame(&ctx, &mut v, vec![], false);
        assert!(v.mention.popup_rect.is_some());
        frame(&ctx, &mut v, vec![press(egui::Key::Escape)], false);
        frame(&ctx, &mut v, vec![], false);
        assert!(v.mention.popup_rect.is_none());
        assert_eq!(v.input, "@m");
    }

    #[test]
    fn an_unloaded_list_is_requested_when_the_popup_opens() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut v = vm(String::new());
        frame(&ctx, &mut v, vec![], true);
        frame(&ctx, &mut v, vec![egui::Event::Text("@".into())], false);
        let out = frame(&ctx, &mut v, vec![], false);
        assert!(out.index);
    }
}
