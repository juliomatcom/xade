//! Multi-session end-to-end flow: create a team via the modal, address one member with
//! `#handle:`, verify two-channel separation on the Team feed and broadcast delivery to the
//! other teammate, and confirm the Multi-Session card persists on Home.

use egui::{Key, Modifiers};
use egui_kittest::kittest::Queryable;

use crate::support::Xade;

#[test]
fn create_multi_session_send_targeted_broadcast_and_persist_card() {
    let mut x = Xade::start(&[("README.md", "hi")], [1280.0, 800.0]);

    // Open the "New Multi-Session" modal from the top bar and launch the default 2-model team.
    x.ui.get_by_label_contains("Multi").click();
    x.settle();
    x.wait_for("the New Multi-Session modal", |ui| {
        ui.query_by_label("New Multi-Session").is_some()
    });
    x.ui.get_by_label("Start Multi-Session").click();
    x.settle();

    x.wait_for("the composite Multi-Session window", |ui| {
        ui.query_by_label("Team").is_some() && ui.get_all_by_label("#fake-model-1").next().is_some()
    });

    // Send a targeted prompt to #fake-model-1 that produces both private text and a <broadcast>.
    x.write_where("Team", "#fake-model-1: TRIGGER_BROADCAST");
    x.press(Modifiers::COMMAND, Key::Enter);

    // Combined Team view shows all responses together (#fake-model-1's reply, its distinct
    // model-to-model broadcast container, and #fake-model-2's reply to that broadcast).
    x.wait_for("combined responses and broadcast in the Team view", |ui| {
        ui.query_by_label_contains("Private note to user.")
            .is_some()
            && ui
                .query_by_label_contains("Plan ready from teammate")
                .is_some()
            && ui.query_by_label("Model-to-model").is_some()
            && ui
                .query_by_label_contains("Received team broadcast!")
                .is_some()
    });

    // Usage stats belong to one session: the Team tab adds none to the one that the startup
    // session window already shows.
    let stats = "in 0 \u{b7} out 0 \u{b7} cache 0";
    assert_eq!(x.ui.query_all_by_label(stats).count(), 1);

    // Click the #fake-model-1 tab (on the same row as the "Team" tab) and check that its private
    // note is shown in its own transcript.
    let tab_y = x.ui.get_by_label("Team").rect().center().y;
    let tab1 =
        x.ui.get_all_by_label("#fake-model-1")
            .find(|n| n.rect().y_range().contains(tab_y))
            .expect("#fake-model-1 tab");
    tab1.click();
    x.settle();
    x.wait_for("#fake-model-1 private reply", |ui| {
        ui.query_by_label_contains("Private note to user.")
            .is_some()
            && ui.query_all_by_label(stats).count() == 2
    });

    // Switch to #fake-model-2 and confirm the broadcast was routed to it and produced a turn.
    let tab2 =
        x.ui.get_all_by_label("#fake-model-2")
            .find(|n| n.rect().y_range().contains(tab_y))
            .expect("#fake-model-2 tab");
    tab2.click();
    x.settle();
    x.wait_for("#fake-model-2 reply to the broadcast", |ui| {
        ui.query_by_label_contains("Received team broadcast!")
            .is_some()
    });

    // Close the Multi-Session window, switch to Home, verify the Multi-Session appears as a single
    // card (latest team message preview + member handles) without leaking member sessions, then
    // reopen it from Home and verify SQLite round-trip of the team feed and member transcripts.
    x.press(Modifiers::COMMAND, Key::W);
    x.wait_for("Multi-Session window closed", |ui| {
        ui.query_by_label("Team").is_none()
    });

    x.ui.get_by_label("🏠 Home").click();
    x.settle();
    x.wait_for("Multi-Session card on Home", |ui| {
        ui.query_by_label_contains("Plan ready from teammate")
            .is_some()
            && ui
                .query_by_label_contains("#fake-model-1 \u{00B7} #fake-model-2")
                .is_some()
    });
    assert!(
        x.ui.query_by_label_contains("Private note to user.")
            .is_none(),
        "member sessions must not appear as standalone cards on Home"
    );

    x.ui.get_by_label_contains("Plan ready from teammate")
        .click();
    x.settle();
    x.wait_for("reopened Multi-Session Team feed", |ui| {
        ui.query_by_label("Team").is_some()
            && ui
                .query_by_label_contains("Plan ready from teammate")
                .is_some()
    });
    let reopened_tab_y = x.ui.get_by_label("Team").rect().center().y;
    x.ui.get_all_by_label("#fake-model-2")
        .find(|n| n.rect().y_range().contains(reopened_tab_y))
        .expect("reopened #fake-model-2 tab")
        .click();
    x.settle();
    x.wait_for("reopened #fake-model-2 transcript", |ui| {
        ui.query_by_label_contains("Received team broadcast!")
            .is_some()
    });
}
