//! The keymaps modal: every shortcut of `keymap::BINDINGS`, grouped.

use egui::RichText;

use crate::keymap::{self, Show, BINDINGS, GROUPS};
use crate::theme;

/// Draws the modal; returns true when it should close.
pub fn show(ctx: &egui::Context) -> bool {
    let mac = cfg!(target_os = "macos");
    let resp = egui::Modal::new(egui::Id::unique("keymaps")).show(ctx, |ui| {
        ui.set_width(460.0);
        ui.label(RichText::new("Keyboard shortcuts").strong().size(16.0));
        ui.add_space(6.0);
        for (group, name) in GROUPS {
            ui.label(RichText::new(name).small().color(theme::TEXT_MUTED));
            egui::Grid::new(("keymaps_group", name))
                .num_columns(2)
                .spacing([16.0, 4.0])
                .show(ui, |ui| {
                    for b in BINDINGS
                        .iter()
                        .filter(|b| b.group == group && b.show != Show::Hidden)
                    {
                        ui.label(b.title);
                        egui::Frame::new()
                            .fill(theme::BG_HOVER)
                            .corner_radius(4)
                            .inner_margin(egui::Margin::symmetric(6, 1))
                            .show(ui, |ui| {
                                ui.label(RichText::new(keymap::binding_label(b, mac)).monospace());
                            });
                        ui.end_row();
                    }
                });
            ui.add_space(6.0);
        }
        ui.label(
            RichText::new("Press ? outside a text field to open this list.")
                .small()
                .color(theme::TEXT_MUTED),
        );
    });
    resp.should_close()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_headless_and_closes_on_escape() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0));
        let run = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            };
            let mut closed = false;
            ctx.run_pass(input, |ctx| closed = show(ctx))
                .drop_without_applying_deltas();
            closed
        };
        assert!(!run(vec![]));
        assert!(!run(vec![]));
        let esc = egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        assert!(run(vec![esc]));
    }
}
