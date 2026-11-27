//! Notes above the input and the running-subagents badge.

use egui::RichText;
use xade_core::{Role, SessionStatus};

use super::controls::effort_label;
use super::meters::fmt_tokens;
use crate::app::SessionVm;
use crate::running::{self, CacheKind};
use crate::workspace::Env;
use crate::{icons, theme};

/// Shown above the input when the prompt cache has expired: the next message resends the whole
/// context at full price. While the cache is warm there is nothing worth saying.
pub(super) fn send_note(
    context_tokens: u64,
    last_activity: time::OffsetDateTime,
    now: time::OffsetDateTime,
) -> Option<String> {
    if context_tokens == 0 {
        return None;
    }
    let (_, kind) = running::cache_label(last_activity, now);
    (kind == CacheKind::Expired).then(|| {
        format!(
            "Cache expired: sending will resend ~{} tokens of context at full price",
            fmt_tokens(context_tokens)
        )
    })
}

/// A warning line with the warning icon, above the input.
/// Says what changed since the last message and what the next one costs for it: a different
/// model or thinking effort resends the whole context at full price.
pub(super) fn settings_note(vm: &SessionVm, env: &Env) -> Option<String> {
    let context = vm.session.usage.context_tokens_used;
    if context == 0 {
        return None;
    }
    let models = env
        .providers
        .iter()
        .find(|p| p.id() == vm.session.provider_id)
        .map(|p| p.models())
        .unwrap_or_default();
    let label = |id: &str| {
        models
            .iter()
            .find(|m| m.id == id)
            .map_or_else(|| id.to_string(), |m| m.label.clone())
    };
    let mut changes = Vec::new();
    if vm.session.effort != vm.sent_effort {
        changes.push(format!(
            "Thinking effort changed from {} to {}",
            effort_label(vm.sent_effort),
            effort_label(vm.session.effort)
        ));
    }
    if vm.session.model_id != vm.sent_model {
        changes.push(format!(
            "Model changed from {} to {}",
            label(&vm.sent_model),
            label(&vm.session.model_id)
        ));
    }
    if changes.is_empty() {
        return None;
    }
    Some(format!(
        "{}. The next message resends ~{} tokens of context at full price.",
        changes.join("; "),
        fmt_tokens(context)
    ))
}

/// Subagents (the `Task` / `Agent` tool) whose call has no result yet; none once the turn ended.
pub(super) fn running_subagents(vm: &SessionVm) -> usize {
    if !matches!(
        vm.status,
        SessionStatus::Streaming | SessionStatus::NeedsApproval
    ) {
        return 0;
    }
    let done: std::collections::HashSet<&str> = vm
        .messages
        .iter()
        .filter(|m| m.role == Role::ToolResult)
        .filter_map(|m| m.tool_use_id.as_deref())
        .collect();
    vm.messages
        .iter()
        .filter(|m| m.role == Role::ToolCall)
        .filter(|m| matches!(m.tool_name.as_deref(), Some("Task" | "Agent")))
        .filter(|m| m.tool_use_id.as_deref().is_some_and(|t| !done.contains(t)))
        .count()
}

/// A pill toggling the subagent chat; returns whether it was clicked.
pub(super) fn subagent_badge(ui: &mut egui::Ui, count: usize, active: bool) -> bool {
    let frame = egui::Frame::new()
        .fill(if active {
            theme::claude().gamma_multiply(0.25)
        } else {
            egui::Color32::TRANSPARENT
        })
        .stroke(egui::Stroke::new(1.0, theme::claude()))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(6, 0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(10.0).color(theme::claude()));
                let text = if count == 1 {
                    "Subagent running".to_string()
                } else {
                    format!("{count} subagents running")
                };
                ui.label(RichText::new(text).small().color(theme::claude()));
            });
        });
    let resp = ui
        .interact(
            frame.response.rect,
            ui.scope_id().with("subagent_badge"),
            egui::Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(if active {
            "Close the subagent window"
        } else {
            "Open the subagent chat in its own window"
        });
    ui.add_space(4.0);
    resp.clicked()
}

pub(super) fn note_frame(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(theme::bg_raised())
        .stroke(egui::Stroke::new(1.0, theme::warn()))
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(icons::with(icons::WARNING, text))
                        .small()
                        .color(theme::warn()),
                )
                .wrap(),
            );
        });
    ui.add_space(4.0);
}

/// How long a refused-attachment note stays above the input.
pub(super) const ATTACH_NOTE_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// Shows `message` above the input for a while.
pub fn set_attach_note(vm: &mut SessionVm, message: String) {
    vm.attach_note = Some((message, std::time::Instant::now()));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::composer::send_now;
    use crate::composer::test_support::{height_with_note, vm};
    use time::OffsetDateTime;
    use xade_core::Effort;

    #[test]
    fn a_refused_attachment_note_adds_a_row_above_the_input_and_expires() {
        let plain = height_with_note(None);
        let now = std::time::Instant::now();
        let noted = height_with_note(Some(("Not attached: a.zip (binary).".into(), now)));
        assert!(noted > plain + 10.0, "note row: {plain} -> {noted}");
        let old = now - ATTACH_NOTE_TTL - std::time::Duration::from_secs(1);
        let expired = height_with_note(Some(("stale".into(), old)));
        assert!((expired - plain).abs() < 1.0, "expired note is gone");
    }

    #[test]
    fn a_changed_effort_is_named_with_the_previous_one_and_clears_on_send() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let env = Env {
            cmd_tx: &tx,
            providers: &[],
            fork_targets: &[],
            vscode: None,
            maximized: None,
            send_mode: crate::config::SendMode::Queue,
            send_on_enter: false,
            break_all: true,
            ide_context: None,
            focused: false,
        };
        let mut v = vm(String::new());
        v.sent_model.clone_from(&v.session.model_id);
        v.sent_effort = Effort::High;
        v.session.effort = Effort::Low;
        assert!(settings_note(&v, &env).is_none(), "nothing to resend yet");
        v.session.usage.context_tokens_used = 42_000;
        let note = settings_note(&v, &env).unwrap();
        assert!(note.contains("changed from High to Low"), "{note}");
        assert!(note.contains("42"), "{note}");
        v.session.effort = Effort::High;
        assert!(settings_note(&v, &env).is_none(), "changed back");
        v.session.effort = Effort::Low;
        send_now(&mut v, &env, "go".into(), None);
        assert!(
            settings_note(&v, &env).is_none(),
            "the message went out with Low"
        );
    }

    #[test]
    fn subagents_count_calls_without_results_while_busy() {
        let mut v = vm(String::new());
        let call = |v: &mut SessionVm, role, name: Option<&str>, id: &str| {
            let mut m = crate::app::new_message(v, role, String::new(), Some(id.into()));
            m.tool_name = name.map(Into::into);
            v.messages.push(m);
        };
        call(&mut v, Role::ToolCall, Some("Agent"), "a");
        call(&mut v, Role::ToolCall, Some("Agent"), "b");
        call(&mut v, Role::ToolCall, Some("Bash"), "c");
        call(&mut v, Role::ToolResult, None, "a");
        v.status = SessionStatus::Streaming;
        assert_eq!(running_subagents(&v), 1);
        v.status = SessionStatus::Idle;
        assert_eq!(running_subagents(&v), 0);
    }

    #[test]
    fn send_note_only_appears_once_the_cache_expired() {
        let now = OffsetDateTime::now_utc();
        let old = now - time::Duration::hours(2);
        assert!(send_note(0, old, now).is_none());
        assert!(send_note(42_000, now, now).is_none());
        let text = send_note(42_000, old, now).unwrap();
        assert!(
            text.contains("~42.0k tokens") && text.contains("full price"),
            "{text}"
        );
    }
}
