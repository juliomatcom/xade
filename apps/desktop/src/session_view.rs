//! One session window's content: header, conversation, permission banner and composer.

use egui::RichText;
use xade_core::SessionStatus;
use xade_runtime::ManagerCmd;

use crate::app::SessionVm;
use crate::diff::is_code_change;
use crate::workspace::Env;
use crate::{chrome, composer, conversation, home, icons, running, send_anim, theme};

pub const HEADER_HEIGHT: f32 = 34.0;

#[derive(Default)]
pub struct ViewOut {
    pub close: bool,
    pub collapse: bool,
    /// Toggles maximize for this window.
    pub maximize: bool,
    /// Close the session and reopen it with what other processes wrote.
    pub reload: bool,
    /// A file path clicked in the transcript, to open in Xade's editor.
    pub open_file: Option<std::path::PathBuf>,
    /// The attach button was clicked: pick files to mention.
    pub attach: bool,
    /// The `@` popup needs the folder's file list built.
    pub index_files: bool,
    /// A clipboard image to save and mention.
    pub pasted_image: Option<std::sync::Arc<egui::ColorImage>>,
}

pub fn show(ui: &mut egui::Ui, vm: &mut SessionVm, env: &Env) -> ViewOut {
    let mut out = ViewOut::default();
    header(ui, vm, env, &mut out);
    if vm.collapsed {
        return out;
    }
    out.reload = fork_strips(ui, vm);
    egui::Panel::bottom("composer")
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(theme::BG_WINDOW)
                .stroke(egui::Stroke::new(1.0, theme::BORDER))
                .inner_margin(egui::Margin::same(8)),
        )
        .show(ui, |ui| {
            banner(ui, vm, env);
            let c = composer::show(ui, vm, env);
            out.attach = c.attach;
            out.index_files = c.index;
            out.pasted_image = c.pasted_image;
        });
    let chat = egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show(ui, |ui| out.open_file = conversation::show(ui, vm));
    send_anim::show(ui, vm.session.id, chat.response.rect);
    subagent_window(ui.ctx(), vm);
    out
}

fn header(ui: &mut egui::Ui, vm: &SessionVm, env: &Env, out: &mut ViewOut) {
    let found = env
        .providers
        .iter()
        .find(|p| p.id() == vm.session.provider_id);
    let provider = found.map_or("agent", |p| p.display_name());
    let model = found
        .and_then(|p| p.models().into_iter().find(|m| m.id == vm.session.model_id))
        .map_or_else(|| vm.session.model_id.clone(), |m| m.label);
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
    let maximized = env.maximized == Some(vm.session.id);
    let badge = |ui: &mut egui::Ui| home::origin_badge(ui, vm.session.origin);
    chrome::header(
        ui,
        maximized,
        vm.collapsed,
        "Close session",
        out,
        Some(&badge),
        |ui| {
            status_dot(ui, &vm.status);
            if !cwd.is_empty() {
                ui.label(RichText::new(cwd).small().color(theme::TEXT_MUTED))
                    .on_hover_text(vm.cwd.display().to_string());
            }
            if let Some(tex) = conversation::claude_symbol(ui.ctx()) {
                ui.add(egui::Image::new(&tex).fit_to_exact_size(egui::vec2(14.0, 14.0)));
            }
            ui.add(
                egui::Label::new(RichText::new(model).strong())
                    .truncate()
                    .sense(egui::Sense::hover()),
            )
            .on_hover_text(provider);
            ui.add(egui::Label::new(RichText::new(format!("• {title}")).strong()).truncate());
        },
    );
}

/// The fork warnings under the header: the session is open in another process too, or that
/// process appended to the transcript. Returns whether Reload was clicked.
fn fork_strips(ui: &mut egui::Ui, vm: &SessionVm) -> bool {
    if let Some(location) = &vm.sync.elsewhere {
        egui::Frame::new()
            .fill(theme::WARN)
            .inner_margin(egui::Margin::symmetric(10, 4))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(
                    egui::Label::new(
                        RichText::new(format!(
                            "{} Also open in {location} \u{2014} replies here and there will fork. \
                             Close one of them.",
                            icons::WARNING
                        ))
                        .color(theme::BG_APP),
                    )
                    .truncate(),
                );
            });
    }
    let t = &vm.sync.transcript;
    let idle = vm.status == SessionStatus::Idle;
    let mut reload = false;
    if running::updated_elsewhere(t.mtime, t.own_write, idle) {
        egui::Frame::new()
            .fill(theme::BG_RAISED)
            .stroke(egui::Stroke::new(1.0, theme::WARN))
            .inner_margin(egui::Margin::symmetric(10, 2))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Updated elsewhere \u{2014}")
                            .small()
                            .color(theme::WARN),
                    );
                    reload = ui.small_button("Reload").clicked();
                });
            });
    }
    reload
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
                    review = ui
                        .button(icons::with(icons::REVIEW, "Review changes"))
                        .clicked();
                } else {
                    if ui.button(icons::with(icons::APPROVE, "Approve")).clicked() {
                        respond = Some((true, false));
                    }
                    if ui
                        .button(icons::with(icons::APPROVE, "Approve & Enable Auto"))
                        .clicked()
                    {
                        respond = Some((true, true));
                    }
                    if ui.button(icons::with(icons::DENY, "Deny")).clicked() {
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

/// The subagent chat in its own floating window: read-only, so no composer, with a note saying
/// whose chat it is. Shown while the composer's subagent badge is on.
fn subagent_window(ctx: &egui::Context, vm: &mut SessionVm) {
    if !vm.view_subagents {
        return;
    }
    let mut open = true;
    egui::Window::new("Subagent")
        .id(egui::Id::unique(("subagent_window", vm.session.id)))
        .open(&mut open)
        .default_size([480.0, 520.0])
        .resizable(true)
        .show(ctx, |ui| {
            ui.label(
                RichText::new("This is a subagent working for the main chat. Read-only.")
                    .small()
                    .color(theme::TEXT_MUTED),
            );
            ui.separator();
            std::mem::swap(&mut vm.messages, &mut vm.subagent_msgs);
            conversation::show(ui, vm);
            std::mem::swap(&mut vm.messages, &mut vm.subagent_msgs);
        });
    vm.view_subagents = open && vm.view_subagents;
}
