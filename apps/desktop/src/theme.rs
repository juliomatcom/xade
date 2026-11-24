//! Dark theme tokens; the only place that names colors. Each one can be overridden in the `theme`
//! block of settings.json.

use std::sync::{LazyLock, RwLock};

use egui::{Color32, CornerRadius, FontId, Stroke, TextStyle, Theme, Visuals};
use serde_json::{Map, Value};

/// Hex of `c` as written in settings.json: `#rrggbb`, or `#rrggbbaa` when not opaque.
fn to_hex(c: Color32) -> String {
    // `Color32` stores premultiplied alpha; settings.json holds the straight color.
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

/// `#rrggbb` or `#rrggbbaa` (the `#` is optional, any case).
fn parse_hex(text: &str) -> Option<Color32> {
    let digits = text.trim().trim_start_matches('#');
    if !matches!(digits.len(), 6 | 8) || !digits.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
    let (r, g, b) = (byte(0)?, byte(2)?, byte(4)?);
    Some(match digits.len() {
        6 => Color32::from_rgb(r, g, b),
        _ => Color32::from_rgba_unmultiplied(r, g, b, byte(6)?),
    })
}

/// The palette in use; replaced when settings.json is (re)loaded.
static PALETTE: LazyLock<RwLock<Palette>> = LazyLock::new(|| RwLock::new(Palette::default()));

pub fn set_palette(palette: Palette) {
    *PALETTE.write().unwrap_or_else(|e| e.into_inner()) = palette;
}

/// Declares every color once: the key it has in `settings.json`'s `theme` block (also the name of
/// the function that reads it) and its default.
macro_rules! palette {
    ($($(#[$doc:meta])* $name:ident = $hex:literal;)*) => {
        #[derive(Clone, Debug, PartialEq)]
        pub struct Palette {
            $(pub $name: Color32,)*
        }

        impl Default for Palette {
            fn default() -> Self {
                Palette {
                    $($name: parse_hex($hex).unwrap_or(Color32::MAGENTA),)*
                }
            }
        }

        impl Palette {
            /// Every color as hex, keyed as in settings.json.
            pub fn defaults_json() -> Value {
                let defaults = Palette::default();
                let mut map = Map::new();
                $(map.insert(stringify!($name).to_string(), Value::String(to_hex(defaults.$name)));)*
                Value::Object(map)
            }

            /// Colors missing from `map` keep their defaults; unknown keys are ignored.
            pub fn parse(map: &Map<String, Value>) -> Result<Palette, String> {
                let mut palette = Palette::default();
                $(
                    if let Some(value) = map.get(stringify!($name)) {
                        let text = value.as_str().ok_or_else(|| {
                            format!("theme.{} must be a hex color like \"#1a1919\"", stringify!($name))
                        })?;
                        palette.$name = parse_hex(text).ok_or_else(|| {
                            format!("theme.{}: \"{}\" is not a hex color (#rrggbb)", stringify!($name), text)
                        })?;
                    }
                )*
                Ok(palette)
            }
        }

        $(
            $(#[$doc])*
            pub fn $name() -> Color32 {
                PALETTE.read().unwrap_or_else(|e| e.into_inner()).$name
            }
        )*
    };
}

palette! {
    bg_app = "#0f1216";
    bg_panel = "#161a20";
    /// Session window frame and body.
    bg_window = "#141313";
    /// File (editor) window frame and body: a bit darker than `bg_window`.
    bg_file = "#0f0e0e";
    /// Find (Cmd+F) in files: every match (faint white-mint), then the current word (jade, black text).
    find_match = "#cdf3e035";
    find_current = "#34d399";
    /// Find (Cmd+F) in chats: the tint behind the current message only.
    find_block = "#cdf3e02a";
    /// Markers for spaces and tabs in file windows (when `show_whitespace` is on): barely there.
    whitespace = "#3a3838";
    bg_raised = "#1f1e1e";
    /// Tool and command cards and code blocks in the chat: darker than the window around them.
    bg_terminal = "#0d0c0c";
    /// Hover fill, one step above `bg_raised`.
    bg_hover = "#2a2828";
    border = "#2e2c2c";
    /// Border of the message input: clearly visible against `bg_hover`.
    border_strong = "#4a4848";
    /// Whitish outline of the user's message bubbles.
    border_light = "#bfb9b9";
    text = "#d6dae1";
    text_muted = "#8a93a1";
    /// Brand jade; primary buttons, selection, links.
    accent = "#059669";
    /// Hover and active fills of accent widgets.
    accent_hover = "#10b981";
    /// Header strip of the focused window: a darker take on `accent`.
    bg_header_focus = "#0b4a39";
    /// Left sidebar fill.
    bg_sidebar = "#161a20";
    /// Bottom status bar fill.
    bg_status = "#0c1714";
    ok = "#34d399";
    /// Muted fill behind OK-colored text (e.g. Auto: ON), avoiding a glowing solid mint.
    ok_bg = "#123b2f";
    /// Prompt-cache "N min left" text: a muted jade readable on card fills.
    cache_fresh = "#4fae8e";
    warn = "#d29922";
    danger = "#f85149";
    diff_del_bg = "#4b1d22";
    diff_add_bg = "#1a3d2a";
    claude = "#d97757";
}

const RADIUS: u8 = 6;

/// Applied once at startup; dark only.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(Theme::Dark);
    ctx.set_visuals_of(Theme::Dark, visuals());
    ctx.style_mut_of(Theme::Dark, |s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
    });
    set_font_size(ctx, crate::config::DEFAULT_FONT_SIZE);
}

/// Text styles from the body size: small is 2 pt below it, code 1 pt below.
pub fn set_font_size(ctx: &egui::Context, body: f32) {
    ctx.style_mut_of(Theme::Dark, |s| {
        s.text_styles
            .insert(TextStyle::Body, FontId::proportional(body));
        s.text_styles
            .insert(TextStyle::Button, FontId::proportional(body));
        s.text_styles
            .insert(TextStyle::Small, FontId::proportional(body - 2.0));
        s.text_styles
            .insert(TextStyle::Monospace, FontId::monospace(body - 1.0));
    });
}

/// Re-applies the palette to egui's own widgets; call after `set_palette`.
pub fn refresh(ctx: &egui::Context) {
    ctx.set_visuals_of(Theme::Dark, visuals());
}

fn visuals() -> Visuals {
    let mut v = Visuals::dark();
    let r = CornerRadius::same(RADIUS);
    v.panel_fill = bg_panel();
    v.window_fill = bg_raised();
    v.extreme_bg_color = bg_raised();
    v.faint_bg_color = bg_panel();
    v.override_text_color = Some(text());
    v.weak_text_color = Some(text_muted());
    v.hyperlink_color = accent();
    v.window_stroke = Stroke::new(1.0, border());
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = r;
    v.selection.bg_fill = bg_header_focus();
    v.selection.stroke = Stroke::new(1.0, text());
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = bg_panel();
    w.noninteractive.bg_stroke = Stroke::new(1.0, border());
    w.noninteractive.fg_stroke = Stroke::new(1.0, text());
    for s in [&mut w.inactive, &mut w.open] {
        s.bg_fill = bg_raised();
        s.weak_bg_fill = bg_raised();
        s.bg_stroke = Stroke::new(1.0, border());
        s.fg_stroke = Stroke::new(1.0, text());
    }
    w.hovered.bg_fill = bg_hover();
    w.hovered.weak_bg_fill = bg_hover();
    w.hovered.bg_stroke = Stroke::new(1.0, text_muted());
    w.hovered.fg_stroke = Stroke::new(1.0, text());
    w.active.bg_fill = accent_hover();
    w.active.weak_bg_fill = accent_hover();
    w.active.bg_stroke = Stroke::new(1.0, accent_hover());
    w.active.fg_stroke = Stroke::new(1.0, text());
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

/// Usage bar color: green (`ok`) at 0%, amber (`warn`) at 50%, red (`danger`) at 100%.
pub fn usage_color(fraction: f32) -> Color32 {
    fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
        let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
        Color32::from_rgb(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()))
    }
    let f = fraction.clamp(0.0, 1.0);
    if f < 0.5 {
        lerp(ok(), warn(), f * 2.0)
    } else {
        lerp(warn(), danger(), (f - 0.5) * 2.0)
    }
}

#[cfg(test)]
mod usage_color_tests {
    use super::*;

    #[test]
    fn endpoints_and_midpoint() {
        assert_eq!(usage_color(0.0), ok());
        assert_eq!(usage_color(0.5), warn());
        assert_eq!(usage_color(1.0), danger());
        assert_eq!(usage_color(2.0), danger());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod palette_tests {
    use super::*;

    #[test]
    fn hex_parses_with_or_without_hash_in_any_case_and_with_alpha() {
        assert_eq!(
            parse_hex("#1a1919"),
            Some(Color32::from_rgb(0x1a, 0x19, 0x19))
        );
        assert_eq!(
            parse_hex("1A1919"),
            Some(Color32::from_rgb(0x1a, 0x19, 0x19))
        );
        assert_eq!(
            parse_hex("#ff000080"),
            Some(Color32::from_rgba_unmultiplied(255, 0, 0, 0x80))
        );
        for bad in ["", "#fff", "#12345", "#gggggg", "red", "#1234567", "#é2345"] {
            assert_eq!(parse_hex(bad), None, "{bad}");
        }
        assert_eq!(to_hex(Color32::from_rgb(0x1a, 0x19, 0x19)), "#1a1919");
    }

    #[test]
    fn the_default_palette_round_trips_through_its_settings_block() {
        let json = Palette::defaults_json();
        let map = json.as_object().unwrap();
        assert!(map
            .values()
            .all(|v| v.as_str().is_some_and(|s| s.starts_with('#'))));
        assert_eq!(Palette::parse(map).unwrap(), Palette::default());
        assert_eq!(map["bg_window"], "#141313");
        // File windows sit a step darker than session windows.
        let p = Palette::default();
        let lum = |c: Color32| u32::from(c.r()) + u32::from(c.g()) + u32::from(c.b());
        assert!(lum(p.bg_file) < lum(p.bg_window));
    }

    #[test]
    fn a_partial_block_overrides_only_what_it_names() {
        let map = serde_json::json!({"bg_window": "#342c2c", "unknown": 1});
        let p = Palette::parse(map.as_object().unwrap()).unwrap();
        assert_eq!(p.bg_window, Color32::from_rgb(0x34, 0x2c, 0x2c));
        assert_eq!(p.bg_raised, Palette::default().bg_raised);
    }

    #[test]
    fn a_bad_color_names_its_key() {
        let map = serde_json::json!({"accent": "green"});
        let e = Palette::parse(map.as_object().unwrap()).unwrap_err();
        assert!(e.contains("theme.accent") && e.contains("green"), "{e}");
        let map = serde_json::json!({"text": 5});
        assert!(Palette::parse(map.as_object().unwrap())
            .unwrap_err()
            .contains("theme.text"));
    }
}
