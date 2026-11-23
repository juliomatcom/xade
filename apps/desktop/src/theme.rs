//! Dark theme tokens; the only place that names colors.

use egui::{Color32, CornerRadius, FontId, Stroke, TextStyle, Theme, Visuals};

pub const BG_APP: Color32 = Color32::from_rgb(0x0F, 0x12, 0x16);
pub const BG_PANEL: Color32 = Color32::from_rgb(0x16, 0x1A, 0x20);
pub const BG_RAISED: Color32 = Color32::from_rgb(0x1E, 0x23, 0x2B);
/// Hover fill, one step above `BG_RAISED`.
pub const BG_HOVER: Color32 = Color32::from_rgb(0x27, 0x2D, 0x37);
pub const BORDER: Color32 = Color32::from_rgb(0x2A, 0x30, 0x3A);
pub const TEXT: Color32 = Color32::from_rgb(0xD6, 0xDA, 0xE1);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x8A, 0x93, 0xA1);
pub const ACCENT: Color32 = Color32::from_rgb(0x2F, 0x81, 0xF7);
pub const OK: Color32 = Color32::from_rgb(0x2E, 0xA0, 0x43);
pub const WARN: Color32 = Color32::from_rgb(0xD2, 0x99, 0x22);
pub const DANGER: Color32 = Color32::from_rgb(0xF8, 0x51, 0x49);
pub const CLAUDE: Color32 = Color32::from_rgb(0xD9, 0x77, 0x57);

const RADIUS: u8 = 6;

/// Applied once at startup; dark only.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(Theme::Dark);
    ctx.set_visuals_of(Theme::Dark, visuals());
    ctx.style_mut_of(Theme::Dark, |s| {
        s.text_styles
            .insert(TextStyle::Body, FontId::proportional(13.5));
        s.text_styles
            .insert(TextStyle::Button, FontId::proportional(13.5));
        s.text_styles
            .insert(TextStyle::Small, FontId::proportional(11.5));
        s.text_styles
            .insert(TextStyle::Monospace, FontId::monospace(12.5));
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
    });
}

fn visuals() -> Visuals {
    let mut v = Visuals::dark();
    let r = CornerRadius::same(RADIUS);
    v.panel_fill = BG_PANEL;
    v.window_fill = BG_RAISED;
    v.extreme_bg_color = BG_RAISED;
    v.faint_bg_color = BG_PANEL;
    v.override_text_color = Some(TEXT);
    v.weak_text_color = Some(TEXT_MUTED);
    v.hyperlink_color = ACCENT;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = r;
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0, TEXT);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = BG_PANEL;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    for s in [&mut w.inactive, &mut w.open] {
        s.bg_fill = BG_RAISED;
        s.weak_bg_fill = BG_RAISED;
        s.bg_stroke = Stroke::new(1.0, BORDER);
        s.fg_stroke = Stroke::new(1.0, TEXT);
    }
    w.hovered.bg_fill = BG_HOVER;
    w.hovered.weak_bg_fill = BG_HOVER;
    w.hovered.bg_stroke = Stroke::new(1.0, TEXT_MUTED);
    w.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    w.active.bg_fill = ACCENT;
    w.active.weak_bg_fill = ACCENT;
    w.active.bg_stroke = Stroke::new(1.0, ACCENT);
    w.active.fg_stroke = Stroke::new(1.0, TEXT);
    for s in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        s.corner_radius = r;
    }
    v
}
