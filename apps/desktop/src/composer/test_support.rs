//! Shared helpers for the composer tests.

use super::{show, ComposerOut};
use crate::app::SessionVm;
use crate::workspace::Env;
use egui::Modifiers;
use time::OffsetDateTime;
use xade_core::{Effort, ProviderId, Session, SessionId, SessionStatus, UsageTotals, WorkspaceId};

pub(super) fn vm(input: String) -> SessionVm {
    let now = OffsetDateTime::now_utc();
    SessionVm {
        session: Session {
            origin: xade_core::SessionOrigin::default(),
            id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            provider_id: ProviderId::Claude,
            provider_session_id: None,
            title: "t".into(),
            preview_snippet: String::new(),
            model_id: "opus".into(),
            effort: Effort::Default,
            sent_model_id: String::new(),
            sent_effort: Effort::Default,
            auto_mode: false,
            usage: UsageTotals::default(),
            created_at: now,
            updated_at: now,
        },
        cwd: "/tmp".into(),
        status: SessionStatus::Idle,
        queued: Default::default(),
        messages: Vec::new(),
        live_text: String::new(),
        thinking_tokens: None,
        pending_permission: None,
        input,
        error: None,
        md_cache: egui_commonmark::CommonMarkCache::default(),
        path_links: Default::default(),
        diff: None,
        changes: Default::default(),
        settings_pending: false,
        turn_base: None,
        turn_snapshotting: false,
        turn_files: Default::default(),
        sent_model: String::new(),
        sent_effort: xade_core::Effort::Default,
        collapsed: false,
        window_rect: None,
        apply_rect: None,
        sync: Default::default(),
        mention: Default::default(),
        attach_note: None,
        subagent_msgs: Vec::new(),
        view_subagents: false,
    }
}

/// Height of the composer after a frame, plus where the note's text sits relative to the
/// input box.
pub(super) fn height_with_note(note: Option<(String, std::time::Instant)>) -> f32 {
    let ctx = egui::Context::default();
    crate::theme::apply(&ctx);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let env = Env {
        cmd_tx: &tx,
        providers: &[],
        fork_targets: &[],
        vscode: None,
        maximized: None,
        send_mode: crate::config::SendMode::Queue,
        ide_context: None,
        focused: false,
    };
    let mut v = vm(String::new());
    v.attach_note = note;
    let mut height = 0.0;
    for _ in 0..3 {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(700.0, 900.0),
            )),
            ..Default::default()
        };
        ctx.run_ui(raw, |ui| {
            height = ui.scope(|ui| show(ui, &mut v, &env)).response.rect.height();
        })
        .drop_without_applying_deltas();
    }
    assert_eq!(
        v.attach_note.is_some(),
        height > 0.0 && v.attach_note.is_some()
    );
    height
}

/// The composer's rect after a few frames at this input.
pub(super) fn render(input: String) -> egui::Rect {
    let ctx = egui::Context::default();
    crate::theme::apply(&ctx);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let env = Env {
        cmd_tx: &tx,
        providers: &[],
        fork_targets: &[],
        vscode: None,
        maximized: None,
        send_mode: crate::config::SendMode::Queue,
        ide_context: None,
        focused: false,
    };
    let mut v = vm(input);
    let mut out = egui::Rect::NOTHING;
    for _ in 0..4 {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(700.0, 900.0),
            )),
            ..Default::default()
        };
        ctx.run_ui(raw, |ui| {
            out = ui.scope(|ui| show(ui, &mut v, &env)).response.rect;
        })
        .drop_without_applying_deltas();
    }
    out
}

/// One frame of the composer with `events` fed in; returns what it asked of the app.
pub(super) fn frame(
    ctx: &egui::Context,
    v: &mut SessionVm,
    events: Vec<egui::Event>,
    focus: bool,
) -> ComposerOut {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let env = Env {
        cmd_tx: &tx,
        providers: &[],
        fork_targets: &[],
        vscode: None,
        maximized: None,
        send_mode: crate::config::SendMode::Queue,
        ide_context: None,
        focused: false,
    };
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(700.0, 900.0),
        )),
        events,
        ..Default::default()
    };
    let mut out = ComposerOut::default();
    ctx.run_ui(raw, |ui| {
        if focus {
            let id = ui.scope_id().with("composer_input");
            ui.memory_mut(|m| m.request_focus(id));
        }
        out = show(ui, v, &env);
    })
    .drop_without_applying_deltas();
    out
}

pub(super) fn press(key: egui::Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    }
}
