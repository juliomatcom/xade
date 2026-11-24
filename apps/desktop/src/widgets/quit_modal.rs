//! Confirm modal before quitting while sessions are mid-turn.

use egui::RichText;

use crate::app::App;
use crate::theme;

/// Minimum width of the modal's content.
const MIN_WIDTH: f32 = 480.0;

/// Draws the modal when a quit is pending; Quit closes the window, Cancel or Esc drops it.
pub fn show(app: &mut App, ctx: &egui::Context) {
    if !app.pending_quit {
        return;
    }
    let n = app.busy_sessions();
    let body = match n {
        0 => "No session is running any more.".to_string(),
        1 => "1 session is still running. Quitting stops it and cuts its current turn short."
            .to_string(),
        n => format!(
            "{n} sessions are still running. Quitting stops them and cuts their current turns \
             short."
        ),
    };
    let (mut quit, mut cancel) = (false, false);
    let resp = egui::Modal::new(egui::Id::unique("confirm_quit")).show(ctx, |ui| {
        ui.set_min_width(MIN_WIDTH);
        ui.label(RichText::new("Quit Xade?").strong().size(18.0));
        ui.add_space(6.0);
        ui.label(body);
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            quit = ui
                .add(
                    egui::Button::new(RichText::new("Quit").color(theme::bg_app()))
                        .fill(theme::danger()),
                )
                .clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    if quit {
        app.confirm_quit(ctx);
    } else if cancel || resp.should_close() {
        app.pending_quit = false;
    }
}
