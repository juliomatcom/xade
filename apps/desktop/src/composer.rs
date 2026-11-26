//! Prompt input, send/stop, model and effort pickers, Auto toggle and the context and usage meters.

mod controls;
mod mention_popup;
mod meters;
mod notes;
mod paste;
#[cfg(test)]
pub(super) mod test_support;

pub(crate) use controls::effort_choices;
pub use mention_popup::{append_mentions, insert_mentions};
pub use notes::set_attach_note;

use egui::RichText;
use xade_core::{Delivery, SessionStatus};
use xade_runtime::ManagerCmd;

use crate::app::{push_user_message, SessionVm};
use crate::config::SendMode;
use crate::keymap::{self, Action};
use crate::workspace::Env;
use crate::{icons, send_anim, theme};
use controls::controls;
use mention_popup::{mention_popup_show, mention_popup_start};
use notes::{
    note_frame, running_subagents, send_note, settings_note, subagent_badge, ATTACH_NOTE_TTL,
};
use paste::{clipboard_files, take_pasted_files, take_pasted_image};

/// The input grows with its text between these row counts, then scrolls.
const INPUT_MIN_ROWS: usize = 2;
const INPUT_MAX_ROWS: usize = 10;
/// Side of the (square) send button, and height of Stop.
const BUTTON_SIZE: f32 = 48.0;

/// What the composer asks of the app after drawing.
#[derive(Default)]
pub struct ComposerOut {
    /// The attach button was clicked.
    pub attach: bool,
    /// The `@` popup is open and needs the folder's file list.
    pub index: bool,
    /// An image pasted from the clipboard, still to be encoded and saved.
    pub pasted_image: Option<std::sync::Arc<egui::ColorImage>>,
}

pub fn show(ui: &mut egui::Ui, vm: &mut SessionVm, env: &Env) -> ComposerOut {
    let mut out = ComposerOut::default();
    let id = vm.session.id;
    let idle = vm.status == SessionStatus::Idle;
    let streaming = vm.status == SessionStatus::Streaming;
    let running = streaming || vm.status == SessionStatus::NeedsApproval;

    // `Some(other)`: send requested, `other` = the opposite of the setting while running.
    let mut send: Option<bool> = None;
    let input_id = ui.scope_id().with("composer_input");
    let focused = ui.memory(|m| m.has_focus(input_id));
    let pressed = if focused {
        ui.input_mut(keymap::take_send)
    } else {
        None
    };
    if focused {
        crate::widgets::selection_menu::mark_text_box(ui.ctx());
    }
    let hotkey_other = pressed == Some(Action::SendOther);
    let hotkey = pressed == Some(Action::Send);
    // Pasting copied files or an image attaches them, like the attach button.
    if focused {
        if let Some(files) = ui.input_mut(|i| take_pasted_files(&mut i.events, clipboard_files)) {
            insert_mentions(vm, &files);
        }
        out.pasted_image = ui.input_mut(|i| take_pasted_image(&mut i.events));
    }
    let popup = mention_popup_start(ui, vm, input_id, focused, &mut out);
    if let Some((text, at)) = &vm.attach_note {
        if at.elapsed() >= ATTACH_NOTE_TTL {
            vm.attach_note = None;
        } else {
            ui.ctx()
                .request_repaint_after(ATTACH_NOTE_TTL.saturating_sub(at.elapsed()));
            note_frame(ui, text);
        }
    }
    if let Some(text) = settings_note(vm, env) {
        note_frame(ui, &text);
    }
    let agents = running_subagents(vm);
    if agents == 0 {
        vm.view_subagents = false;
    } else if subagent_badge(ui, agents, vm.view_subagents) {
        vm.view_subagents = !vm.view_subagents;
    }
    if idle {
        let last = vm
            .sync
            .transcript
            .mtime
            .map_or(vm.session.updated_at, time::OffsetDateTime::from);
        if let Some(text) = send_note(
            vm.session.usage.context_tokens_used,
            last,
            time::OffsetDateTime::now_utc(),
        ) {
            note_frame(ui, &text);
        }
    }
    if std::mem::take(&mut vm.focus_input) {
        let ctx = ui.ctx();
        let mut state = egui::TextEdit::load_state(ctx, input_id).unwrap_or_default();
        let end = egui::text::CCursor::new(vm.input.chars().count());
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(end)));
        state.store(ctx, input_id);
        ctx.memory_mut(|m| m.request_focus(input_id));
    }
    let break_all = env.break_all;
    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
        let mut job = egui::text::LayoutJob::simple(
            buf.as_str().to_owned(),
            egui::TextStyle::Body.resolve(ui.style()),
            ui.visuals().text_color(),
            wrap,
        );
        job.wrap.break_anywhere = break_all;
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let input_box = egui::Frame::new()
        .fill(theme::bg_hover())
        .stroke(egui::Stroke::new(1.5, theme::border_strong()))
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
                                        .layouter(&mut layouter)
                                        .hint_text(format!(
                                            "Message ({} to {})",
                                            keymap::shortcut_text(Action::Send),
                                            if running { "queue or steer" } else { "send" }
                                        )),
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
                            RichText::new(icons::SEND).size(26.0).color(theme::text()),
                        )
                        .fill(theme::accent())
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
                                RichText::new(icons::with(icons::STOP, "Stop"))
                                    .color(theme::text()),
                            )
                            .fill(theme::danger())
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
    mention_popup_show(ui, vm, input_id, input_box.response.rect, popup);
    if let Some(other) = send {
        send_anim::trigger(ui.ctx(), id);
        vm.scroll_to_end = true;
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
            let delivery = running.then_some(Delivery::Steered);
            send_now(vm, env, text, delivery);
        }
    }
    queued_rows(ui, vm);

    // The row under the box ends where the box does, not at the panel edge.
    let box_width = input_box.response.rect.width();
    out.attach = ui
        .scope(|ui| {
            ui.set_max_width(box_width);
            controls(ui, vm, env)
        })
        .inner;
    out
}

/// Shows the user's message and hands it to the session.
pub fn send_now(vm: &mut SessionVm, env_cmd: &Env, text: String, delivery: Option<Delivery>) {
    push_user_message(vm, text.clone(), delivery);
    vm.error = None;
    vm.attach_note = None;
    let _ = env_cmd.cmd_tx.send(ManagerCmd::Send {
        session_id: vm.session.id,
        text,
        delivery,
        context: env_cmd.ide_context.map(str::to_owned),
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
                        .color(theme::text_muted()),
                )
                .truncate(),
            );
        });
    }
    if let Some(i) = cancel {
        vm.queued.remove(i);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::composer::test_support::{render, vm};

    #[test]
    fn sending_clears_the_note() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let env = Env {
            cmd_tx: &tx,
            providers: &[],
            fork_targets: &[],
            vscode: None,
            maximized: None,
            send_mode: crate::config::SendMode::Queue,
            break_all: true,
            ide_context: None,
            focused: false,
        };
        let mut v = vm("hi".into());
        set_attach_note(&mut v, "x".into());
        send_now(&mut v, &env, "hi".into(), None);
        assert!(v.attach_note.is_none());
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
}
