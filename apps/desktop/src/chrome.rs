//! Window chrome shared by every window kind: the header strip and its badge.

use egui::{Align, Layout, RichText};

use crate::icons;
use crate::session_view::{ViewOut, HEADER_HEIGHT};
use crate::theme;

/// The header: maximize / collapse / close on the right, `left` fills the rest.
pub fn header(
    ui: &mut egui::Ui,
    maximized: bool,
    collapsed: bool,
    close_tip: &str,
    out: &mut ViewOut,
    left: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_height(HEADER_HEIGHT - 12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .small_button(icons::CLOSE)
                    .on_hover_text(close_tip)
                    .clicked()
                {
                    out.close = true;
                }
                let (glyph, tip) = if collapsed {
                    (icons::RESTORE, "Expand")
                } else {
                    (icons::COLLAPSE, "Collapse")
                };
                if ui.small_button(glyph).on_hover_text(tip).clicked() {
                    out.collapse = true;
                }
                let (glyph, tip) = if maximized {
                    (icons::RESTORE, "Restore")
                } else {
                    (icons::MAXIMIZE, "Maximize")
                };
                if ui.small_button(glyph).on_hover_text(tip).clicked() {
                    out.maximize = true;
                }
                ui.with_layout(Layout::left_to_right(Align::Center), left);
            });
        });
}

/// A small outlined pill, like the session origin badge.
pub fn badge(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(6, 0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().color(color));
        });
}

/// The neutral badge editor windows carry where sessions show their origin.
pub fn file_badge(ui: &mut egui::Ui) {
    badge(ui, "File", theme::TEXT_MUTED);
}
