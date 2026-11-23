//! One session window's content: header, conversation, permission banner and composer.

use egui::{Align, Layout, RichText};
use xade_core::SessionStatus;
use xade_runtime::ManagerCmd;

use crate::app::SessionVm;
use crate::diff::is_code_change;
use crate::workspace::Env;
use crate::{composer, conversation, theme};

pub const HEADER_HEIGHT: f32 = 34.0;

#[derive(Default)]
pub struct ViewOut {
    pub close: bool,
    pub collapse: bool,
}

pub fn show(ui: &mut egui::Ui, vm: &mut SessionVm, env: &Env) -> ViewOut {
    let mut out = ViewOut::default();
    header(ui, vm, env, &mut out);
    if vm.collapsed {
        return out;
    }
    egui::Panel::bottom("composer")
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(theme::BG_PANEL)
                .stroke(egui::Stroke::new(1.0, theme::BORDER))
                .inner_margin(egui::Margin::same(8)),
        )
        .show(ui, |ui| {
            banner(ui, vm, env);
            composer::show(ui, vm, env);
        });
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show(ui, |ui| conversation::show(ui, vm));
    out
}

fn header(ui: &mut egui::Ui, vm: &SessionVm, env: &Env, out: &mut ViewOut) {
    let provider = env
        .providers
        .iter()
        .find(|p| p.id() == vm.session.provider_id)
        .map_or("agent", |p| p.display_name());
    let title = if vm.session.title.is_empty() {
        "New session"
    } else {
        vm.session.title.as_str()
    };
    let cwd = vm
        .cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_height(HEADER_HEIGHT - 12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .small_button("×")
                    .on_hover_text("Close session")
                    .clicked()
                {
                    out.close = true;
                }
                let (glyph, tip) = if vm.collapsed {
                    ("+", "Expand")
                } else {
                    ("–", "Collapse")
                };
                if ui.small_button(glyph).on_hover_text(tip).clicked() {
                    out.collapse = true;
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    status_dot(ui, &vm.status);
                    ui.label(RichText::new(format!("{title} • {provider}")).strong());
                    if !cwd.is_empty() {
                        ui.label(RichText::new(cwd).small().color(theme::TEXT_MUTED))
                            .on_hover_text(vm.cwd.display().to_string());
                    }
                });
            });
        });
}

fn status_dot(ui: &mut egui::Ui, status: &SessionStatus) {
    let (color, tip) = match status {
        SessionStatus::Idle => (theme::TEXT_MUTED, "Idle".to_string()),
        SessionStatus::Streaming => (theme::ACCENT, "Streaming".to_string()),
        SessionStatus::NeedsApproval => (theme::WARN, "Needs approval".to_string()),
        SessionStatus::Error(e) => (theme::DANGER, format!("Error: {e}")),
        SessionStatus::Starting => (theme::TEXT_MUTED, "Starting".to_string()),
        SessionStatus::Closed => (theme::TEXT_MUTED, "Closed".to_string()),
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    let mut radius = 4.0;
    if *status == SessionStatus::Streaming {
        let t = ui.input(|i| i.time) as f32;
        radius = 3.5 + 1.0 * (t * 4.0).sin().abs();
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(33));
    }
    ui.painter().circle_filled(rect.center(), radius, color);
    resp.on_hover_text(tip);
}

/// Approve / Deny for tools that are not reviewed in the diff modal.
fn banner(ui: &mut egui::Ui, vm: &mut SessionVm, env: &Env) {
    let Some(p) = vm.pending_permission.as_ref() else {
        return;
    };
    let id = vm.session.id;
    let (request_id, tool_use_id) = (p.request_id.clone(), p.tool_use_id.clone());
    let mut respond = None;
    let mut review = false;
    egui::Frame::new()
        .fill(theme::BG_RAISED)
        .stroke(egui::Stroke::new(1.0, theme::WARN))
        .corner_radius(6)
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let summary = crate::widgets::tool_card::summary(&p.tool_name, &p.input_json);
            ui.label(RichText::new(format!("Allow {}?", p.tool_name)).color(theme::WARN));
            ui.add(egui::Label::new(RichText::new(summary).color(theme::TEXT_MUTED)).truncate());
            ui.horizontal_wrapped(|ui| {
                if p.reviewable && is_code_change(&p.tool_name) {
                    review = ui.button("Review changes").clicked();
                } else {
                    if ui.button("Approve").clicked() {
                        respond = Some((true, false));
                    }
                    if ui.button("Approve & Enable Auto").clicked() {
                        respond = Some((true, true));
                    }
                    if ui.button("Deny").clicked() {
                        respond = Some((false, false));
                    }
                }
            });
        });
    if review {
        vm.open_diff(&tool_use_id);
    }
    if let Some((allow, enable_auto)) = respond {
        let _ = env.cmd_tx.send(ManagerCmd::RespondPermission {
            session_id: id,
            request_id,
            allow,
            enable_auto,
        });
        if enable_auto {
            vm.session.auto_mode = true;
        }
        vm.pending_permission = None;
    }
}
