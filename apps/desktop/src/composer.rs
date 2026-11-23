//! Prompt input, send/stop, model and effort pickers, Auto toggle and the context and usage meters.

use egui::{Key, Modifiers, RichText};
use xade_core::{Effort, ModelOption, SessionStatus, UsageTotals};
use xade_runtime::ManagerCmd;

use crate::app::{push_user_message, SessionVm};
use crate::config::SendMode;
use crate::running::{self, CacheKind};
use crate::workspace::Env;
use crate::{icons, theme};

/// The input grows with its text between these row counts, then scrolls.
const INPUT_MIN_ROWS: usize = 2;
const INPUT_MAX_ROWS: usize = 10;
/// Side of the (square) send button, and height of Stop.
const BUTTON_SIZE: f32 = 48.0;

pub fn show(ui: &mut egui::Ui, vm: &mut SessionVm, env: &Env) {
    let id = vm.session.id;
    let idle = vm.status == SessionStatus::Idle;
    let streaming = vm.status == SessionStatus::Streaming;
    let running = streaming || vm.status == SessionStatus::NeedsApproval;

    // `Some(other)`: send requested, `other` = the opposite of the setting while running.
    let mut send: Option<bool> = None;
    let input_id = ui.scope_id().with("composer_input");
    let focused = ui.memory(|m| m.has_focus(input_id));
    // The Shift variant first: Cmd+Enter would also match it.
    let hotkey_other = focused
        && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Enter));
    let hotkey = focused && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter));
    egui::Frame::new()
        .fill(theme::BG_RAISED)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(8)
        .inner_margin(egui::Margin::same(6))
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let w = (ui.available_width() - if streaming { 130.0 } else { 64.0 }).max(40.0);
                // A pasted wall of text scrolls inside the box instead of growing the panel.
                let max_h = INPUT_MAX_ROWS as f32 * ui.text_style_height(&egui::TextStyle::Body);
                let input_h = ui
                    .allocate_ui(egui::vec2(w, max_h), |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("composer_scroll")
                            .max_height(max_h)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut vm.input)
                                        .id(input_id)
                                        .frame(egui::Frame::NONE)
                                        .desired_width(f32::INFINITY)
                                        .desired_rows(INPUT_MIN_ROWS)
                                        .hint_text(if running {
                                            "Message (Cmd+Enter to queue or steer)"
                                        } else {
                                            "Message (Cmd+Enter to send)"
                                        }),
                                );
                            });
                    })
                    .response
                    .rect
                    .height();
                // The buttons sit in the vertical middle of the input.
                let pad = ((input_h - BUTTON_SIZE) / 2.0).max(0.0);
                ui.vertical(|ui| {
                    ui.add_space(pad);
                    ui.horizontal(|ui| {
                        let can_send = (idle || running) && !vm.input.trim().is_empty();
                        let btn = egui::Button::new(
                            RichText::new(icons::SEND).size(26.0).color(theme::TEXT),
                        )
                        .fill(theme::ACCENT)
                        .min_size(egui::vec2(BUTTON_SIZE, BUTTON_SIZE));
                        let mut resp = ui.add_enabled(can_send, btn);
                        if running {
                            resp = resp.on_hover_text(match env.send_mode {
                                SendMode::Queue => "Queue: sent when the turn ends",
                                SendMode::Steer => "Steer: sent into the running turn",
                            });
                        }
                        if resp.clicked() {
                            send = Some(false);
                        }
                        if streaming {
                            let stop = egui::Button::new(
                                RichText::new(icons::with(icons::STOP, "Stop")).color(theme::TEXT),
                            )
                            .fill(theme::DANGER)
                            .min_size(egui::vec2(0.0, BUTTON_SIZE));
                            if ui.add(stop).clicked() {
                                vm.queued.clear();
                                let _ = env.cmd_tx.send(ManagerCmd::Interrupt { session_id: id });
                            }
                        }
                        if can_send {
                            if hotkey {
                                send = Some(false);
                            } else if hotkey_other {
                                send = Some(true);
                            }
                        }
                    });
                });
            });
        });
    if let Some(other) = send {
        let text = std::mem::take(&mut vm.input);
        let mode = if other {
            env.send_mode.other()
        } else {
            env.send_mode
        };
        // Steering needs a turn that is reading; while waiting for approval, queue.
        if running && !(mode == SendMode::Steer && streaming) {
            vm.queued.push_back(text);
        } else {
            send_now(vm, env, text);
        }
    }
    queued_rows(ui, vm);

    controls(ui, vm, env);
    if idle {
        let last = vm
            .sync
            .transcript
            .mtime
            .map_or(vm.session.updated_at, time::OffsetDateTime::from);
        if let Some((text, kind)) = send_note(
            vm.session.usage.context_tokens_used,
            last,
            time::OffsetDateTime::now_utc(),
        ) {
            let color = match kind {
                CacheKind::Fresh => theme::TEXT_MUTED,
                CacheKind::Expired => theme::WARN,
            };
            ui.label(RichText::new(text).small().color(color));
        }
    }
    ui.horizontal_wrapped(|ui| {
        let u = &vm.session.usage;
        ui.label(
            RichText::new(context_text(u))
                .small()
                .color(theme::TEXT_MUTED),
        );
        ui.label(
            RichText::new(usage_text(u))
                .small()
                .color(theme::TEXT_MUTED),
        );
    });
}

/// What the next message will resend: the whole context, cheap while the prompt cache is warm.
fn send_note(
    context_tokens: u64,
    last_activity: time::OffsetDateTime,
    now: time::OffsetDateTime,
) -> Option<(String, CacheKind)> {
    if context_tokens == 0 {
        return None;
    }
    let tokens = fmt_tokens(context_tokens);
    let (cache, kind) = running::cache_label(last_activity, now);
    let text = match kind {
        CacheKind::Fresh => format!("Sending resends ~{tokens} tokens of context ({cache})"),
        CacheKind::Expired => format!(
            "{} Cache expired: sending resends ~{tokens} tokens of context at full price",
            icons::WARNING
        ),
    };
    Some((text, kind))
}

/// Shows the user's message and hands it to the session.
pub fn send_now(vm: &mut SessionVm, env_cmd: &Env, text: String) {
    push_user_message(vm, text.clone());
    vm.error = None;
    let _ = env_cmd.cmd_tx.send(ManagerCmd::Send {
        session_id: vm.session.id,
        text,
    });
}

/// One row per queued message, each cancellable.
fn queued_rows(ui: &mut egui::Ui, vm: &mut SessionVm) {
    let mut cancel = None;
    for (i, text) in vm.queued.iter().enumerate() {
        ui.horizontal(|ui| {
            if ui
                .small_button(icons::CLOSE)
                .on_hover_text("Cancel this queued message")
                .clicked()
            {
                cancel = Some(i);
            }
            let line = text.lines().next().unwrap_or_default();
            ui.add(
                egui::Label::new(
                    RichText::new(format!("Queued: {line}"))
                        .small()
                        .color(theme::TEXT_MUTED),
                )
                .truncate(),
            );
        });
    }
    if let Some(i) = cancel {
        vm.queued.remove(i);
    }
}

fn controls(ui: &mut egui::Ui, vm: &mut SessionVm, env: &Env) {
    let id = vm.session.id;
    let provider = env
        .providers
        .iter()
        .find(|p| p.id() == vm.session.provider_id);
    let models = provider.map(|p| p.models()).unwrap_or_default();
    let fallback = provider.map(|p| p.effort_levels()).unwrap_or_default();
    let efforts = effort_choices(&models, &vm.session.model_id, &fallback);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Model").small().color(theme::TEXT_MUTED));
        let current = models
            .iter()
            .find(|m| m.id == vm.session.model_id)
            .map_or(vm.session.model_id.clone(), |m| m.label.clone());
        egui::ComboBox::from_id_salt(("model", id))
            .selected_text(current)
            .width(110.0)
            .show_ui(ui, |ui| {
                for m in &models {
                    let picked = ui
                        .selectable_label(vm.session.model_id == m.id, &m.label)
                        .on_hover_text(&m.description)
                        .clicked();
                    if picked && vm.session.model_id != m.id {
                        vm.session.model_id = m.id.clone();
                        vm.settings_pending = true;
                        let _ = env.cmd_tx.send(ManagerCmd::SetModel {
                            session_id: id,
                            model_id: m.id.clone(),
                        });
                        let supported = effort_choices(&models, &m.id, &fallback);
                        let effort = reconcile_effort(vm.session.effort, &supported);
                        if effort != vm.session.effort {
                            vm.session.effort = effort;
                            let _ = env.cmd_tx.send(ManagerCmd::SetEffort {
                                session_id: id,
                                effort,
                            });
                        }
                    }
                }
            });
        ui.label(RichText::new("Thinking").small().color(theme::TEXT_MUTED));
        egui::ComboBox::from_id_salt(("effort", id))
            .selected_text(effort_label(vm.session.effort))
            .width(80.0)
            .show_ui(ui, |ui| {
                for e in &efforts {
                    let picked = ui
                        .selectable_label(vm.session.effort == *e, effort_label(*e))
                        .clicked();
                    if picked && vm.session.effort != *e {
                        vm.session.effort = *e;
                        vm.settings_pending = true;
                        let _ = env.cmd_tx.send(ManagerCmd::SetEffort {
                            session_id: id,
                            effort: *e,
                        });
                    }
                }
            });
        let on = vm.session.auto_mode;
        let (text, fill, fg, stroke) = if on {
            (
                icons::with(icons::AUTO, "Auto: ON"),
                theme::OK_BG,
                theme::OK,
                theme::OK,
            )
        } else {
            (
                icons::with(icons::AUTO, "Auto: OFF"),
                theme::BG_HOVER,
                theme::TEXT,
                theme::BORDER,
            )
        };
        let pill = egui::Button::new(RichText::new(text).small().color(fg))
            .fill(fill)
            .stroke(egui::Stroke::new(1.0, stroke))
            .corner_radius(10);
        if ui.add(pill).clicked() {
            vm.session.auto_mode = !on;
            let _ = env.cmd_tx.send(ManagerCmd::SetAutoMode {
                session_id: id,
                on: !on,
            });
        }
        if vm.settings_pending {
            ui.label(
                RichText::new("applies to next turn")
                    .small()
                    .color(theme::WARN),
            );
        }
    });
}

/// `Default` plus the levels of the model with this exact id; `fallback` when the id is unknown.
fn effort_choices(models: &[ModelOption], model_id: &str, fallback: &[Effort]) -> Vec<Effort> {
    match models.iter().find(|m| m.id == model_id) {
        Some(m) => std::iter::once(Effort::Default)
            .chain(
                m.effort_levels
                    .iter()
                    .copied()
                    .filter(|e| *e != Effort::Default),
            )
            .collect(),
        None => fallback.to_vec(),
    }
}

/// Keeps `current` when supported, else falls back to `Default`.
fn reconcile_effort(current: Effort, supported: &[Effort]) -> Effort {
    if supported.contains(&current) {
        current
    } else {
        Effort::Default
    }
}

fn effort_label(e: Effort) -> &'static str {
    match e {
        Effort::Default => "Default",
        Effort::Low => "Low",
        Effort::Medium => "Medium",
        Effort::High => "High",
        Effort::XHigh => "X-High",
        Effort::Max => "Max",
    }
}

pub fn fmt_tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

pub fn context_text(u: &UsageTotals) -> String {
    if u.context_window_max == 0 {
        if u.context_tokens_used == 0 {
            return "ctx —".into();
        }
        return format!("ctx {} / ?", fmt_tokens(u.context_tokens_used));
    }
    let pct = u.context_tokens_used as f64 / u.context_window_max as f64 * 100.0;
    format!(
        "ctx {} / {} ({pct:.0}%)",
        fmt_tokens(u.context_tokens_used),
        fmt_tokens(u.context_window_max)
    )
}

pub fn usage_text(u: &UsageTotals) -> String {
    format!(
        "in {} · out {} · cache {} · ${:.2}",
        fmt_tokens(u.input_tokens),
        fmt_tokens(u.output_tokens),
        fmt_tokens(u.cache_read_tokens),
        u.cost_usd
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use time::OffsetDateTime;
    use xade_core::{ProviderId, Session, SessionId, SessionStatus, WorkspaceId};

    fn vm(input: String) -> SessionVm {
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
            settings_pending: false,
            collapsed: false,
            window_rect: None,
            apply_rect: None,
            sync: Default::default(),
        }
    }

    /// The composer's rect after a few frames at this input.
    fn render(input: String) -> egui::Rect {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let env = Env {
            cmd_tx: &tx,
            providers: &[],
            vscode: None,
            maximized: None,
            send_mode: crate::config::SendMode::Queue,
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

    #[test]
    fn a_pasted_wall_of_text_stays_inside_a_capped_input() {
        let wall = (0..500)
            .map(|i| format!("{i} line of a pasted log"))
            .collect::<Vec<_>>()
            .join("\n");
        let one = render("hi".into());
        let many = render(wall);
        // Two input rows, the 48px button row inside the frame, controls and meters.
        assert!(
            one.height() < 160.0,
            "composer is too tall at rest: {one:?}"
        );
        assert!(
            many.height() > one.height(),
            "input still grows: {one:?} {many:?}"
        );
        // Cap plus the controls row, the meters row and the frame margins.
        assert!(
            many.height() <= one.height() + 8.0 * 20.0,
            "uncapped: {one:?} {many:?}"
        );
        assert!(many.width() <= 700.0);
    }

    #[test]
    fn send_note_warns_when_the_cache_expired() {
        let now = OffsetDateTime::now_utc();
        assert!(send_note(0, now, now).is_none());
        let (text, kind) = send_note(42_000, now, now).unwrap();
        assert_eq!(kind, CacheKind::Fresh);
        assert!(text.contains("~42.0k tokens"), "{text}");
        let (text, kind) = send_note(42_000, now - time::Duration::hours(2), now).unwrap();
        assert_eq!(kind, CacheKind::Expired);
        assert!(text.contains("full price"), "{text}");
    }

    #[test]
    fn meters_format_compactly() {
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(12_345), "12.3k");
        assert_eq!(fmt_tokens(2_500_000), "2.5M");
        let u = UsageTotals {
            context_tokens_used: 50_000,
            context_window_max: 200_000,
            input_tokens: 1_200,
            output_tokens: 300,
            cache_read_tokens: 5_000,
            cost_usd: 0.1234,
        };
        assert_eq!(context_text(&u), "ctx 50.0k / 200.0k (25%)");
        assert_eq!(usage_text(&u), "in 1.2k · out 300 · cache 5.0k · $0.12");
        assert_eq!(context_text(&UsageTotals::default()), "ctx —");
        let imported = UsageTotals {
            context_tokens_used: 42_000,
            ..UsageTotals::default()
        };
        assert_eq!(context_text(&imported), "ctx 42.0k / ?");
    }

    #[test]
    fn effort_follows_the_model() {
        let models = [ModelOption {
            id: "m-1".into(),
            label: "M 1".into(),
            description: String::new(),
            effort_levels: vec![Effort::Low, Effort::High],
        }];
        let all = [Effort::Default, Effort::Low, Effort::Medium];
        assert_eq!(
            effort_choices(&models, "m-1", &all),
            [Effort::Default, Effort::Low, Effort::High]
        );
        assert_eq!(effort_choices(&models, "old-alias", &all), all);
        let supported = effort_choices(&models, "m-1", &all);
        assert_eq!(reconcile_effort(Effort::High, &supported), Effort::High);
        assert_eq!(reconcile_effort(Effort::Max, &supported), Effort::Default);
    }
}
