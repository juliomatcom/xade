//! Throwaway single session view; P5 replaces it with the full session windows.

use aw_core::{Role, SessionStatus};
use aw_runtime::ManagerCmd;
use egui::{Key, Modifiers, RichText};

use crate::app::{push_user_message, App};
use crate::theme;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let Some(id) = app.focused else {
        ui.label(RichText::new("No open session").color(theme::TEXT_MUTED));
        return;
    };
    let mut close = None;
    let mut focus = None;
    ui.horizontal(|ui| {
        for sid in &app.open_windows {
            let title = app
                .sessions
                .get(sid)
                .map(|vm| vm.session.title.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "New session".into());
            if ui.selectable_label(*sid == id, title).clicked() {
                focus = Some(*sid);
            }
        }
    });
    if let Some(sid) = focus {
        app.focused = Some(sid);
    }
    let Some(vm) = app.sessions.get_mut(&id) else {
        ui.label(RichText::new("Opening session…").color(theme::TEXT_MUTED));
        return;
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(status_text(&vm.status)).color(status_color(&vm.status)));
        if ui.button("Close session").clicked() {
            close = Some(id);
        }
        if vm.status == SessionStatus::Streaming && ui.button("Stop").clicked() {
            let _ = app.cmd_tx.send(ManagerCmd::Interrupt { session_id: id });
        }
    });
    ui.separator();

    let mut send = None;
    let mut respond = None;
    egui::Panel::bottom("basic_input")
        .frame(egui::Frame::new())
        .show(ui, |ui| {
            if let Some(p) = &vm.pending_permission {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Allow {}?", p.tool_name)).color(theme::WARN));
                    if ui.button("Approve").clicked() {
                        respond = Some((p.request_id.clone(), true, false));
                    }
                    if ui.button("Approve & Auto").clicked() {
                        respond = Some((p.request_id.clone(), true, true));
                    }
                    if ui.button("Deny").clicked() {
                        respond = Some((p.request_id.clone(), false, false));
                    }
                });
            }
            let out = egui::TextEdit::multiline(&mut vm.input)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("Message (Cmd+Enter to send)")
                .show(ui);
            let hotkey = out.response.has_focus()
                && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter));
            let can_send = vm.status == SessionStatus::Idle && !vm.input.trim().is_empty();
            if (ui
                .add_enabled(can_send, egui::Button::new("Send"))
                .clicked()
                || hotkey)
                && can_send
            {
                send = Some(std::mem::take(&mut vm.input));
            }
        });

    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for m in &vm.messages {
                match m.role {
                    Role::User => {
                        ui.label(RichText::new("You").color(theme::ACCENT).strong());
                        ui.label(&m.content_text);
                    }
                    Role::Assistant => {
                        ui.label(RichText::new("Claude").color(theme::CLAUDE).strong());
                        ui.label(&m.content_text);
                    }
                    Role::ToolCall => {
                        let name = m.tool_name.as_deref().unwrap_or("tool");
                        ui.label(RichText::new(format!("tool: {name}")).color(theme::TEXT_MUTED));
                    }
                    Role::ToolResult => {
                        let color = if m.is_error {
                            theme::DANGER
                        } else {
                            theme::TEXT_MUTED
                        };
                        let first = m.content_text.lines().next().unwrap_or("");
                        ui.label(RichText::new(format!("  result: {first}")).color(color));
                    }
                    Role::System => {
                        ui.label(RichText::new(&m.content_text).color(theme::TEXT_MUTED));
                    }
                }
                ui.add_space(4.0);
            }
            if !vm.live_text.is_empty() {
                ui.label(RichText::new("Claude").color(theme::CLAUDE).strong());
                ui.label(&vm.live_text);
            }
            if let Some(n) = vm.thinking_tokens {
                ui.label(RichText::new(format!("Thinking… ~{n} tokens")).color(theme::TEXT_MUTED));
            }
            if let Some(e) = &vm.error {
                ui.label(RichText::new(e).color(theme::DANGER));
            }
        });

    if let Some(text) = send {
        push_user_message(vm, text.clone());
        vm.error = None;
        let _ = app.cmd_tx.send(ManagerCmd::Send {
            session_id: id,
            text,
        });
    }
    if let Some((request_id, allow, enable_auto)) = respond {
        let _ = app.cmd_tx.send(ManagerCmd::RespondPermission {
            session_id: id,
            request_id,
            allow,
            enable_auto,
        });
    }
    if let Some(id) = close {
        app.close_session(id);
    }
}

fn status_text(s: &SessionStatus) -> String {
    match s {
        SessionStatus::Error(e) => format!("Error: {e}"),
        other => format!("{other:?}"),
    }
}

fn status_color(s: &SessionStatus) -> egui::Color32 {
    match s {
        SessionStatus::Idle => theme::OK,
        SessionStatus::Streaming => theme::ACCENT,
        SessionStatus::NeedsApproval => theme::WARN,
        SessionStatus::Error(_) => theme::DANGER,
        SessionStatus::Closed | SessionStatus::Starting => theme::TEXT_MUTED,
    }
}
