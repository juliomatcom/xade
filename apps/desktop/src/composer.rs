//! Prompt input, send/stop, model and effort pickers, Auto toggle and the context and usage meters.

use egui::{Key, Modifiers, RichText};
use xade_core::{Effort, ModelOption, SessionStatus, UsageTotals};
use xade_runtime::ManagerCmd;

use crate::app::{push_user_message, SessionVm};
use crate::workspace::Env;
use crate::{icons, theme};

/// The input grows with its text between these row counts, then scrolls.
const INPUT_MIN_ROWS: usize = 2;
const INPUT_MAX_ROWS: usize = 10;

pub fn show(ui: &mut egui::Ui, vm: &mut SessionVm, env: &Env) {
    let id = vm.session.id;
    let idle = vm.status == SessionStatus::Idle;
    let streaming = vm.status == SessionStatus::Streaming;

    let mut send = false;
    let input_id = ui.scope_id().with("composer_input");
    let focused = ui.memory(|m| m.has_focus(input_id));
    let hotkey = focused && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter));
    egui::Frame::new()
        .fill(theme::BG_RAISED)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(8)
        .inner_margin(egui::Margin::same(6))
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let w = (ui.available_width() - 52.0).max(40.0);
                // A pasted wall of text scrolls inside the box instead of growing the panel.
                let max_h = INPUT_MAX_ROWS as f32 * ui.text_style_height(&egui::TextStyle::Body);
                ui.allocate_ui(egui::vec2(w, max_h), |ui| {
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
                                    .hint_text("Message (Cmd+Enter to send)"),
                            );
                        });
                });
                let can_send = idle && !vm.input.trim().is_empty();
                if streaming {
                    let stop = egui::Button::new(
                        RichText::new(icons::with(icons::STOP, "Stop")).color(theme::TEXT),
                    )
                    .fill(theme::DANGER);
                    if ui.add(stop).clicked() {
                        let _ = env.cmd_tx.send(ManagerCmd::Interrupt { session_id: id });
                    }
                } else {
                    let btn =
                        egui::Button::new(RichText::new(icons::SEND).strong().color(theme::TEXT))
                            .fill(theme::ACCENT);
                    send = ui.add_enabled(can_send, btn).clicked();
                }
                send |= hotkey && can_send;
            });
        });
    if send {
        let text = std::mem::take(&mut vm.input);
        push_user_message(vm, text.clone());
        vm.error = None;
        let _ = env.cmd_tx.send(ManagerCmd::Send {
            session_id: id,
            text,
        });
    }

    controls(ui, vm, env);
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
            messages: Vec::new(),
            live_text: String::new(),
            thinking_tokens: None,
            pending_permission: None,
            input,
            error: None,
            md_cache: egui_commonmark::CommonMarkCache::default(),
            diff: None,
            settings_pending: false,
            collapsed: false,
            window_rect: None,
            apply_rect: None,
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
