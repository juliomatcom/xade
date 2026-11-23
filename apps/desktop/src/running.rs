//! Sessions running outside Xade and prompt-cache freshness: the dot, the cache label, and
//! the confirm modal shown before opening a session that is running elsewhere.

use std::time::Duration;

use egui::RichText;
use time::OffsetDateTime;
use xade_core::{RunState, RunningSession, SessionId};

use crate::app::App;
use crate::theme;

/// How often `AgentProvider::running_sessions` is polled.
pub const POLL: Duration = Duration::from_secs(3);
/// Claude Code writes 1 h prompt-cache entries.
const CACHE_TTL_SECS: i64 = 3600;
const PULSE_FRAME: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    Fresh,
    Expired,
}

/// Prompt-cache freshness of a session last active at `updated_at`; a future timestamp counts as now.
pub fn cache_label(updated_at: OffsetDateTime, now: OffsetDateTime) -> (String, CacheKind) {
    let elapsed = (now - updated_at).whole_seconds().max(0);
    if elapsed >= CACHE_TTL_SECS {
        return ("Expired".into(), CacheKind::Expired);
    }
    let left = (CACHE_TTL_SECS - elapsed + 59) / 60;
    (format!("Cache \u{b7} {left}m left"), CacheKind::Fresh)
}

pub fn cache_color(kind: CacheKind) -> egui::Color32 {
    match kind {
        CacheKind::Fresh => theme::CACHE_FRESH,
        CacheKind::Expired => theme::TEXT_MUTED,
    }
}

/// Small colored cache label.
pub fn cache_text(updated_at: OffsetDateTime) -> RichText {
    let (text, kind) = cache_label(updated_at, OffsetDateTime::now_utc());
    RichText::new(text).small().color(cache_color(kind))
}

pub fn tooltip(r: &RunningSession) -> String {
    let state = match r.state {
        RunState::Busy => "busy",
        RunState::Idle => "idle",
    };
    format!("Running in {} \u{b7} {state}", r.location)
}

/// A filled dot: steady while idle, pulsing while busy.
pub fn paint_dot(ui: &egui::Ui, center: egui::Pos2, radius: f32, state: RunState) {
    let mut color = theme::OK;
    if state == RunState::Busy {
        let t = ui.input(|i| i.time) as f32;
        color = color.gamma_multiply(0.55 + 0.45 * (t * 4.0).sin());
        ui.ctx().request_repaint_after(PULSE_FRAME);
    }
    ui.painter().circle_filled(center, radius, color);
}

/// A session waiting for the user to confirm opening it although it runs elsewhere.
pub struct PendingOpen {
    pub id: SessionId,
    pub location: String,
    pub background: bool,
}

/// Draws the confirm modal when one is pending and applies the decision.
pub fn confirm_modal(app: &mut App, ctx: &egui::Context) {
    let Some(p) = app.pending_open.as_ref() else {
        return;
    };
    let location = p.location.clone();
    let (mut accept, mut cancel) = (false, false);
    let resp = egui::Modal::new(egui::Id::unique("confirm_open_running")).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.label(RichText::new("Session is running elsewhere").strong().size(16.0));
        ui.add_space(4.0);
        ui.label(format!(
            "This session is running in {location}. Opening it here too can make both copies diverge."
        ));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            accept = ui
                .add(egui::Button::new("Open anyway").fill(theme::ACCENT))
                .clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    if accept {
        if let Some(p) = app.pending_open.take() {
            app.open_confirmed(p.id, p.background);
        }
    } else if cancel || resp.should_close() {
        app.pending_open = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(elapsed_secs: i64) -> (String, CacheKind) {
        let now = OffsetDateTime::now_utc();
        cache_label(now - time::Duration::seconds(elapsed_secs), now)
    }

    #[test]
    fn cache_minutes_left_and_expiry() {
        assert_eq!(
            label(60),
            ("Cache \u{b7} 59m left".into(), CacheKind::Fresh)
        );
        assert_eq!(
            label(59 * 60),
            ("Cache \u{b7} 1m left".into(), CacheKind::Fresh)
        );
        assert_eq!(label(3600), ("Expired".into(), CacheKind::Expired));
        assert_eq!(label(86_400).1, CacheKind::Expired);
    }

    #[test]
    fn future_timestamps_clamp_to_a_full_cache() {
        assert_eq!(
            label(-600),
            ("Cache \u{b7} 60m left".into(), CacheKind::Fresh)
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn modal_renders_and_escape_cancels() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-confirm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        app.pending_open = Some(PendingOpen {
            id: SessionId::new(),
            location: "terminal".into(),
            background: false,
        });
        let frame = |app: &mut App, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            ctx.run_pass(input, |ctx| confirm_modal(app, ctx))
                .drop_without_applying_deltas();
        };
        frame(&mut app, vec![]);
        frame(&mut app, vec![]);
        assert!(app.pending_open.is_some());
        let esc = egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        frame(&mut app, vec![esc]);
        assert!(app.pending_open.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
