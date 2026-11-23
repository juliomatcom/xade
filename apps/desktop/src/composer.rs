//! Prompt input, send/stop, model and effort pickers, Auto toggle and the context and usage meters.

use egui::{Key, Modifiers, RichText};
use xade_core::{Effort, SessionStatus, UsageTotals};
use xade_runtime::ManagerCmd;

use crate::app::{push_user_message, SessionVm};
use crate::theme;
use crate::workspace::Env;

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
                let w = ui.available_width() - 52.0;
                ui.add_sized(
                    [w.max(40.0), 44.0],
                    egui::TextEdit::multiline(&mut vm.input)
                        .id(input_id)
                        .frame(egui::Frame::NONE)
                        .hint_text("Message (Cmd+Enter to send)"),
                );
                let can_send = idle && !vm.input.trim().is_empty();
                if streaming {
                    let stop = egui::Button::new(RichText::new("Stop").color(theme::TEXT))
                        .fill(theme::DANGER);
                    if ui.add(stop).clicked() {
                        let _ = env.cmd_tx.send(ManagerCmd::Interrupt { session_id: id });
                    }
                } else {
                    let btn = egui::Button::new(RichText::new("↑").strong().color(theme::TEXT))
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
    let efforts = provider.map(|p| p.effort_levels()).unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Model").small().color(theme::TEXT_MUTED));
        let current = models
            .iter()
            .find(|m| m.id == vm.session.model_id)
            .map_or(vm.session.model_id.clone(), |m| m.label.clone());
        egui::ComboBox::from_id_salt(("model", id))
            .selected_text(current)
            .width(80.0)
            .show_ui(ui, |ui| {
                for m in &models {
                    let picked = ui
                        .selectable_label(vm.session.model_id == m.id, &m.label)
                        .clicked();
                    if picked && vm.session.model_id != m.id {
                        vm.session.model_id = m.id.clone();
                        vm.settings_pending = true;
                        let _ = env.cmd_tx.send(ManagerCmd::SetModel {
                            session_id: id,
                            model_id: m.id.clone(),
                        });
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
        let (text, fill) = if on {
            ("Auto: ON", theme::OK)
        } else {
            ("Auto: OFF", theme::BG_HOVER)
        };
        let pill = egui::Button::new(RichText::new(text).small().color(theme::TEXT))
            .fill(fill)
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
        return "ctx —".into();
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
mod tests {
    use super::*;

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
    }
}
