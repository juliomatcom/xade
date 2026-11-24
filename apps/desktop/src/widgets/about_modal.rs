//! About dialog: version, the commit this build came from, and the repository.

use egui::RichText;

use crate::app::App;
use crate::theme;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT: &str = env!("XADE_COMMIT");
const REPO: &str = env!("CARGO_PKG_REPOSITORY");

/// Draws the dialog while `app.about_open`; Close, Esc or a click outside closes it.
pub fn show(app: &mut App, ctx: &egui::Context) {
    if !app.about_open {
        return;
    }
    let mut close = false;
    let resp = crate::widgets::modal(ctx, egui::Id::unique("about")).show(ctx, |ui| {
        ui.set_min_width(360.0);
        ui.label(RichText::new("Xade").strong().size(20.0));
        ui.add_space(8.0);
        egui::Grid::new("about_grid")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.label(RichText::new("Version").color(theme::text_muted()));
                ui.label(VERSION);
                ui.end_row();
                ui.label(RichText::new("Commit").color(theme::text_muted()));
                if COMMIT == "unknown" {
                    ui.label(COMMIT);
                } else {
                    ui.hyperlink_to(COMMIT, format!("{REPO}/commit/{COMMIT}"));
                }
                ui.end_row();
                ui.label(RichText::new("GitHub").color(theme::text_muted()));
                ui.hyperlink(REPO);
                ui.end_row();
            });
        ui.add_space(12.0);
        close = ui.button("Close").clicked();
    });
    if close || resp.should_close() {
        app.about_open = false;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn shows_the_build_and_closes_on_escape() {
        assert!(!COMMIT.is_empty());
        assert!(REPO.starts_with("https://github.com/"));
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-about-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        app.about_open = true;
        let frame = |app: &mut App, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                events,
                ..Default::default()
            };
            ctx.run_pass(input, |ctx| show(app, ctx))
                .drop_without_applying_deltas();
        };
        frame(&mut app, vec![]);
        frame(&mut app, vec![]);
        assert!(app.about_open);
        frame(
            &mut app,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(!app.about_open);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
