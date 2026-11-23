//! One-shot flourish on send: a hammer swings down and shatters a jade crystal above the send
//! button. Drawn with the painter from the egui clock; no assets, no state beyond a start time.

use egui::{epaint::PathStroke, Color32, Pos2, Shape, Stroke, Vec2};

use xade_core::SessionId;

use crate::theme;

/// Seconds from send to the last fading shard.
pub const DURATION: f64 = 0.9;
/// Second the hammer lands and the crystal breaks.
const IMPACT: f64 = 0.22;
const SHARDS: usize = 9;
/// Size of the effect relative to its design units (it plays in the middle of the chat).
const SCALE: f32 = 2.5;
const HANDLE: f32 = 30.0;
/// Hammer angle (screen radians, 0 = pointing left) when cocked and when it lands.
const COCKED: f32 = 3.96;
const LANDED: f32 = 2.65;
const GRAVITY: f32 = 260.0;

/// What to draw at time `t` seconds after the send.
#[derive(Debug, PartialEq)]
pub struct Frame {
    /// Hammer head direction from the pivot, `None` once it has faded.
    pub hammer: Option<f32>,
    pub hammer_alpha: f32,
    /// Opacity of the whole crystal; 0 after the impact.
    pub crystal: f32,
    /// Offset from the crystal center, rotation and opacity of each shard.
    pub shards: Vec<(Vec2, f32, f32)>,
    /// Expanding ring at the impact: radius and opacity.
    pub flash: Option<(f32, f32)>,
}

pub fn frame(t: f64) -> Frame {
    let empty = Frame {
        hammer: None,
        hammer_alpha: 0.0,
        crystal: 0.0,
        shards: Vec::new(),
        flash: None,
    };
    if !(0.0..DURATION).contains(&t) {
        return empty;
    }
    let hammer_alpha = (1.0 - ((t - 0.5) / 0.3).clamp(0.0, 1.0)) as f32;
    let hammer = if t < IMPACT {
        // Ease in: the swing accelerates into the crystal.
        let p = (t / IMPACT) as f32;
        COCKED + (LANDED - COCKED) * p * p
    } else {
        // A short rebound after the hit.
        let p = ((t - IMPACT) / 0.12).clamp(0.0, 1.0) as f32;
        LANDED + 0.22 * (p * std::f32::consts::PI).sin()
    };
    if t < IMPACT {
        return Frame {
            hammer: Some(hammer),
            hammer_alpha: 1.0,
            crystal: (t / 0.08).min(1.0) as f32,
            ..empty
        };
    }
    let age = (t - IMPACT) as f32;
    let life = (DURATION - IMPACT) as f32;
    let fade = (1.0 - age / life).clamp(0.0, 1.0);
    let shards = (0..SHARDS)
        .map(|k| {
            let k = k as f32;
            // Evenly spread directions, nudged so the burst is not a perfect star.
            let dir = k / SHARDS as f32 * std::f32::consts::TAU + (k * 2.3).sin() * 0.25;
            let speed = 70.0 + 35.0 * (k * 1.7).cos().abs();
            let pos = Vec2::new(
                dir.cos() * speed * age,
                dir.sin() * speed * age + GRAVITY * age * age * 0.5,
            );
            (pos, k + age * (6.0 + k), fade)
        })
        .collect();
    let flash_age = age / 0.14;
    Frame {
        hammer: Some(hammer),
        hammer_alpha,
        crystal: 0.0,
        shards,
        flash: (flash_age < 1.0).then_some((6.0 + 16.0 * flash_age, 1.0 - flash_age)),
    }
}

fn fade(c: Color32, alpha: f32) -> Color32 {
    c.gamma_multiply(alpha.clamp(0.0, 1.0))
}

fn rotate(v: Vec2, a: f32) -> Vec2 {
    let (s, c) = a.sin_cos();
    Vec2::new(v.x * c - v.y * s, v.x * s + v.y * c)
}

fn start_id(id: SessionId) -> egui::Id {
    egui::Id::unique(("send_anim", id))
}

/// Starts the flourish for a session, now.
pub fn trigger(ctx: &egui::Context, id: SessionId) {
    let now = ctx.input(|i| i.time);
    ctx.data_mut(|d| d.insert_temp(start_id(id), now));
    ctx.request_repaint();
}

/// Plays the flourish centered in `area` while one is running for the session.
pub fn show(ui: &egui::Ui, id: SessionId, area: egui::Rect) {
    let Some(start) = ui.data(|d| d.get_temp::<f64>(start_id(id))) else {
        return;
    };
    let t = ui.input(|i| i.time) - start;
    if t >= DURATION {
        ui.data_mut(|d| d.remove::<f64>(start_id(id)));
        return;
    }
    ui.ctx().request_repaint();
    paint(&ui.painter_at(area), area.center(), t);
}

/// Draws `frame(t)` with the crystal centered on `center`.
fn paint(painter: &egui::Painter, center: Pos2, t: f64) {
    let f = frame(t);
    let k = SCALE;
    if f.crystal > 0.0 {
        let pts = [
            (0.0, -13.0),
            (9.0, -4.0),
            (7.0, 9.0),
            (0.0, 13.0),
            (-7.0, 9.0),
            (-9.0, -4.0),
        ];
        let poly: Vec<Pos2> = pts
            .iter()
            .map(|&(x, y)| center + Vec2::new(x, y) * k)
            .collect();
        painter.add(Shape::convex_polygon(
            poly,
            fade(theme::ACCENT, f.crystal),
            PathStroke::new(1.5 * k, fade(theme::ACCENT_HOVER, f.crystal)),
        ));
        painter.line_segment(
            [
                center + Vec2::new(-4.0, -3.0) * k,
                center + Vec2::new(0.0, -9.0) * k,
            ],
            Stroke::new(1.5 * k, fade(theme::OK, f.crystal)),
        );
    }
    if let Some((radius, alpha)) = f.flash {
        painter.circle_stroke(
            center,
            radius * k,
            Stroke::new(2.0 * k, fade(theme::OK, alpha)),
        );
    }
    for (offset, spin, alpha) in f.shards {
        let at = center + offset * k;
        let tri: Vec<Pos2> = [
            Vec2::new(0.0, -6.0),
            Vec2::new(4.5, 4.0),
            Vec2::new(-4.5, 4.0),
        ]
        .iter()
        .map(|&v| at + rotate(v, spin) * k)
        .collect();
        painter.add(Shape::convex_polygon(
            tri,
            fade(theme::ACCENT, alpha),
            PathStroke::new(k, fade(theme::ACCENT_HOVER, alpha)),
        ));
    }
    if let Some(a) = f.hammer {
        // The pivot sits up-left of the crystal; the head swings over the top onto it.
        let pivot = center + Vec2::new(-26.0, -26.0) * k;
        let along = Vec2::new(-a.cos(), a.sin());
        let head = pivot + along * HANDLE * k;
        let across = Vec2::new(-along.y, along.x);
        painter.line_segment(
            [pivot, head],
            Stroke::new(3.5 * k, fade(theme::TEXT_MUTED, f.hammer_alpha)),
        );
        let quad = vec![
            head + across * 9.0 * k - along * 5.0 * k,
            head + across * 9.0 * k + along * 5.0 * k,
            head - across * 9.0 * k + along * 5.0 * k,
            head - across * 9.0 * k - along * 5.0 * k,
        ];
        painter.add(Shape::convex_polygon(
            quad,
            fade(theme::TEXT, f.hammer_alpha),
            Stroke::NONE,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crystal_stands_until_the_hammer_lands_then_shatters_and_fades() {
        let before = frame(IMPACT - 0.01);
        assert!(before.crystal > 0.9 && before.shards.is_empty());
        assert!(before.hammer.is_some());

        let after = frame(IMPACT + 0.1);
        assert_eq!(after.crystal, 0.0);
        assert_eq!(after.shards.len(), SHARDS);
        assert!(after.flash.is_some());

        let late = frame(DURATION - 0.01);
        assert!(late.shards.iter().all(|s| s.2 < 0.05));
        assert_eq!(frame(DURATION), frame(-1.0));
        assert!(frame(DURATION).shards.is_empty() && frame(DURATION).hammer.is_none());
    }

    #[test]
    fn hammer_swings_from_cocked_to_landed() {
        assert!((frame(0.0).hammer.unwrap() - COCKED).abs() < 1e-4);
        assert!((frame(IMPACT).hammer.unwrap() - LANDED).abs() < 1e-3);
    }
}
