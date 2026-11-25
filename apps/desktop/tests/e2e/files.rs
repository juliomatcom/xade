//! Opening a file and working with it.

use egui::{Key, Modifiers};
use egui_kittest::kittest::Queryable;

use crate::support::{wide_table, Xade};

/// Open a markdown file from the keyboard, preview it in a narrow window, then Cmd+F for a word
/// that is only inside the table: the table wraps inside the window, and the bar finds it.
#[test]
fn a_markdown_file_opens_previews_wrapped_and_is_searchable() {
    let mut x = Xade::start(&[("notes.md", &wide_table())], [1280.0, 800.0]);

    x.press(Modifiers::COMMAND, Key::P);
    x.type_text("notes");
    x.wait_for("the picker to list notes.md", |ui| {
        ui.query_by_label_contains("notes.md").is_some()
    });
    x.press(Modifiers::NONE, Key::Enter);
    x.wait_for("the file window", |ui| {
        ui.query_by_label("Preview").is_some()
    });

    x.ui.get_by_label("Preview").click();
    x.settle();
    let cells: Vec<_> =
        x.ui.get_all_by_label_contains("a sentence that is long")
            .map(|n| n.rect())
            .collect();
    assert!(!cells.is_empty(), "the table is not on screen");
    for r in &cells {
        assert!(r.right() <= 1280.0, "a cell runs off the window: {r:?}");
        assert!(r.height() > 60.0, "a cell did not wrap: {r:?}");
    }

    x.press(Modifiers::COMMAND, Key::F);
    x.type_text("completed");
    assert!(
        x.ui.query_by_label("1 / 1").is_some(),
        "the bar did not find the match in the table"
    );
    x.press(Modifiers::NONE, Key::Escape);
    assert!(x.ui.query_by_label("1 / 1").is_none(), "Esc closes the bar");
}
