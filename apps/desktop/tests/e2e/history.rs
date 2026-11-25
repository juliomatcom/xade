//! Sessions the provider already had: they load from history and can be continued.

use egui_kittest::kittest::Queryable;
use xade_core::Role;

use crate::support::Xade;

/// A past conversation shows on Home; clicking it opens the session with its messages, and a new
/// prompt continues it.
#[test]
fn a_session_from_history_loads_and_can_be_continued() {
    let history: &[(&str, &[(Role, &str)])] = &[(
        "Fix the login bug",
        &[
            (Role::User, "why is login failing"),
            (Role::Assistant, "The token expires too early."),
        ],
    )];
    let mut x = Xade::start_with(&[("README.md", "hi")], [1280.0, 800.0], history);

    x.ui.get_by_label("🏠 Home").click();
    x.wait_for("the imported session on Home", |ui| {
        ui.query_by_label_contains("The token expires too early.")
            .is_some()
    });

    x.ui.get_by_label_contains("The token expires too early.")
        .click();
    x.wait_for("the session's messages", |ui| {
        ui.query_by_label("why is login failing").is_some()
    });
    assert!(x
        .ui
        .query_by_label("The token expires too early.")
        .is_some());

    x.write_where("why is login failing", "and the refresh token?");
    x.press(egui::Modifiers::COMMAND, egui::Key::Enter);
    x.wait_for("the reply to the follow-up", |ui| {
        ui.query_by_label("Echo: and the refresh token?").is_some()
    });
    assert!(
        x.ui.query_by_label("why is login failing").is_some(),
        "the history stays above the new messages"
    );
}
