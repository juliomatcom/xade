//! Opening a workspace and the files inside it.

use egui_kittest::kittest::Queryable;

use crate::support::Xade;

/// `xade <folder>` opens the folder as a workspace with a session; its files are in the
/// sidebar, and a click on one opens it in an editor window with its text.
#[test]
fn a_workspace_opens_and_a_file_inside_it_opens_in_an_editor() {
    let mut x = Xade::start(
        &[
            ("README.md", "# Project notes\n\nFirst line."),
            ("todo.txt", "ship it"),
        ],
        [1280.0, 800.0],
    );
    // Expanding the workspace lists its files.
    x.ui.get_by_label_contains("workspace  (1)").click();
    x.wait_for("the files of the workspace", |ui| {
        ui.query_by_label("README.md").is_some() && ui.query_by_label("todo.txt").is_some()
    });

    x.ui.get_by_label("README.md").click();
    // The editor's text box holds exactly the file.
    x.wait_for("the editor window with the file's text", |ui| {
        ui.query_by_value("# Project notes\n\nFirst line.")
            .is_some()
    });
}
