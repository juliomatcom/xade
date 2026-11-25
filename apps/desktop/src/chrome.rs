//! Window chrome shared by every window kind: the header strip and its badge.

use egui::{Align, Layout, RichText};

use crate::icons;
use crate::session_view::{ViewOut, HEADER_HEIGHT};
use crate::theme;

/// What the header shows of its window.
pub struct HeaderState {
    pub maximized: bool,
    pub collapsed: bool,
    pub focused: bool,
}

/// The header: maximize / collapse / close on the right, then `badge` (the window's kind) right
/// beside them, and `left` fills the rest.
pub fn header(
    ui: &mut egui::Ui,
    state: HeaderState,
    close_tip: &str,
    out: &mut ViewOut,
    badge: &dyn Fn(&mut egui::Ui),
    left: impl FnOnce(&mut egui::Ui),
) {
    let HeaderState {
        maximized,
        collapsed,
        focused,
    } = state;
    let (fill, radius) = match (focused, collapsed) {
        (false, _) => (egui::Color32::TRANSPARENT, egui::CornerRadius::ZERO),
        (true, true) => (theme::bg_header_focus(), egui::CornerRadius::same(7)),
        (true, false) => (
            theme::bg_header_focus(),
            egui::CornerRadius {
                nw: 7,
                ne: 7,
                sw: 0,
                se: 0,
            },
        ),
    };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(radius)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            // The bar's text is chrome, not content: selecting it must not offer Copy / Quote.
            ui.style_mut().interaction.selectable_labels = false;
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
                badge(ui);
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
    badge(ui, "File", theme::text_muted());
}

/// A window header's badge: every kind shares one muted color, focused or not.
pub fn header_badge(ui: &mut egui::Ui, text: &str) {
    badge(ui, text, theme::text_muted());
}
