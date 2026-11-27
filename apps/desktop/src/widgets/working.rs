//! The last line of a chat while agents work: each working CLI's icon hopping in place.

use std::time::Duration;

use egui::{pos2, vec2, Rect, Sense};
use xade_core::{ProviderId, SessionStatus};

use crate::conversation::{self, AVATAR as ICON};

const GAP: f32 = 8.0;
const HOP: f32 = 7.0;
/// Seconds per hop.
const PERIOD: f64 = 0.6;
/// How far, as a share of a hop, each icon trails the one before it.
const TRAIL: f64 = 0.25;
/// Redraw interval while the row is on screen.
const FRAME: Duration = Duration::from_millis(50);

/// The CLIs with a turn running, once each, in session order. A turn waiting for approval is
/// waiting for the user, so it does not count.
pub fn clis<'a>(
    sessions: impl IntoIterator<Item = (ProviderId, &'a SessionStatus)>,
) -> Vec<ProviderId> {
    let mut out = Vec::new();
    for (provider, status) in sessions {
        if *status == SessionStatus::Streaming && !out.contains(&provider) {
            out.push(provider);
        }
    }
    out
}

/// How high icon `i` is at time `t`: one parabola per hop.
fn lift(t: f64, i: usize) -> f32 {
    let p = (t / PERIOD - i as f64 * TRAIL).rem_euclid(1.0) as f32;
    HOP * 4.0 * p * (1.0 - p)
}

/// Draws the icons centered across the chat, nothing when `clis` is empty. The row's height never
/// changes, so the chat does not shift, and it asks for frames only while it is on screen.
pub fn show(ui: &mut egui::Ui, clis: &[ProviderId]) {
    if clis.is_empty() {
        return;
    }
    let width = clis.len() as f32 * (ICON + GAP) - GAP;
    let size = vec2(ui.available_width().max(width), ICON + HOP);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let left = rect.center().x - width / 2.0;
    let t = ui.input(|i| i.time);
    for (i, &provider) in clis.iter().enumerate() {
        let Some(tex) = conversation::cli_symbol(ui.ctx(), provider) else {
            continue;
        };
        let min = pos2(
            left + i as f32 * (ICON + GAP),
            rect.bottom() - ICON - lift(t, i),
        );
        egui::Image::new(&tex)
            .tint(conversation::symbol_tint(provider))
            .paint_at(ui, Rect::from_min_size(min, vec2(ICON, ICON)));
    }
    ui.ctx().request_repaint_after(FRAME);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_running_turns_count_once_per_cli() {
        use ProviderId::{Antigravity, Claude, Codex};
        let (run, idle) = (SessionStatus::Streaming, SessionStatus::Idle);
        let approval = SessionStatus::NeedsApproval;
        let sessions = [
            (Claude, &run),
            (Codex, &idle),
            (Claude, &run),
            (Antigravity, &approval),
        ];
        assert_eq!(clis(sessions), vec![Claude]);
        assert_eq!(clis([(Codex, &idle)]), Vec::new());
    }

    #[test]
    fn a_hop_stays_between_the_floor_and_its_height_and_neighbours_trail() {
        for step in 0..100 {
            let h = lift(f64::from(step) * 0.037, 1);
            assert!((0.0..=HOP).contains(&h), "{h}");
        }
        assert!((lift(PERIOD / 2.0, 0) - HOP).abs() < 1e-3);
        assert!(lift(PERIOD / 2.0, 1) < HOP);
    }

    #[test]
    fn frames_are_asked_for_only_while_an_agent_works() {
        let delay = |clis: &[ProviderId]| {
            let ctx = egui::Context::default();
            let mut out = None;
            // The first passes may repaint for their own setup; the last one settles.
            for _ in 0..3 {
                let full = ctx.run_ui(egui::RawInput::default(), |ui| show(ui, clis));
                out = full
                    .viewport_output
                    .get(&egui::ViewportId::ROOT)
                    .map(|v| v.repaint_delay);
                full.drop_without_applying_deltas();
            }
            out
        };
        assert!(delay(&[ProviderId::Claude]).is_some_and(|d| d <= FRAME));
        assert!(delay(&[]).is_some_and(|d| d > FRAME));
    }
}
