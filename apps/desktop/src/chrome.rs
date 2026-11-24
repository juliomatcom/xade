//! Window chrome shared by every window kind: the header strip and its badge.

use egui::{Align, Layout, RichText};

use crate::icons;
use crate::session_view::{ViewOut, HEADER_HEIGHT};
use crate::theme;

/// Space kept free around a centered badge: half its widest form plus a gap.
const CENTER_CLEAR: f32 = 50.0;

/// The header: maximize / collapse / close on the right, `left` fills the rest; `center`, when
/// given, is drawn in the middle of the strip and `left` stops short of it.
pub fn header(
    ui: &mut egui::Ui,
    maximized: bool,
    collapsed: bool,
    close_tip: &str,
    out: &mut ViewOut,
    center: Option<&dyn Fn(&mut egui::Ui)>,
    left: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_height(HEADER_HEIGHT - 12.0);
            let strip = ui.max_rect();
            if let Some(center) = center {
                // Child uis: they draw without taking room from the buttons or `left`. A first,
                // invisible pass measures the piece, since a frame cannot be centered blind.
                let top_left = |w: f32| {
                    let x = strip.center().x - w / 2.0;
                    egui::Rect::from_min_size(
                        egui::pos2(x, strip.top()),
                        egui::vec2(w.max(1.0), strip.height()),
                    )
                };
                let mut probe = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(strip)
                        .layout(Layout::left_to_right(Align::Center))
                        .invisible(),
                );
                center(&mut probe);
                let width = probe.min_rect().width();
                let mut mid = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(top_left(width))
                        .layout(Layout::left_to_right(Align::Center)),
                );
                center(&mut mid);
            }
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
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    if center.is_some() {
                        ui.set_max_width((strip.width() / 2.0 - CENTER_CLEAR).max(0.0));
                    }
                    left(ui);
                });
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
