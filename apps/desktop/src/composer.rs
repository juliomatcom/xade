//! Prompt input, send/stop, model and effort pickers, Auto toggle and the context and usage meters.

use std::path::{Path, PathBuf};

use egui::{Key, Modifiers, RichText};
use xade_core::{Delivery, Effort, ModelOption, Role, SessionStatus, UsageTotals};
use xade_runtime::ManagerCmd;

use crate::app::{push_user_message, SessionVm};
use crate::config::SendMode;
use crate::keymap::{self, Action};
use crate::running::{self, CacheKind};
use crate::workspace::Env;
use crate::{attach, icons, mention, send_anim, theme};

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
    let input_box = egui::Frame::new()
        .fill(theme::BG_HOVER)
        .stroke(egui::Stroke::new(1.5, theme::BORDER_STRONG))
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
    mention_popup_show(ui, vm, input_id, input_box.response.rect, popup);
    if let Some(other) = send {
        send_anim::trigger(ui.ctx(), id);
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

    out.attach = controls(ui, vm, env);
    out
}

/// A mention being typed at the cursor, while the popup is showing.
struct Typing {
    start: usize,
    cursor: usize,
}

/// Decides whether the `@` popup is open this frame and handles its keys (Up/Down, Enter or
/// Tab to pick, Esc to dismiss) before the text box sees them.
fn mention_popup_start(
    ui: &egui::Ui,
    vm: &mut SessionVm,
    input_id: egui::Id,
    focused: bool,
    out: &mut ComposerOut,
) -> Option<Typing> {
    let ctx = ui.ctx();
    let cursor = egui::TextEdit::load_state(ctx, input_id)
        .and_then(|s| s.cursor.char_range())
        .map(|r| r.primary.index.0);
    let pointer = ui.input(|i| i.pointer.latest_pos());
    let on_popup = vm
        .mention
        .popup_rect
        .zip(pointer)
        .is_some_and(|(r, p)| r.contains(p));
    let found = cursor
        .filter(|_| focused || on_popup)
        .and_then(|c| mention::active(&vm.input, c).map(|a| (a, c)));
    let Some((active, cursor)) = found else {
        vm.mention.dismissed = None;
        vm.mention.popup_rect = None;
        return None;
    };
    if vm.mention.dismissed == Some(active.start) {
        vm.mention.popup_rect = None;
        return None;
    }
    if vm.mention.needs_index() {
        out.index = true;
    }
    vm.mention.update(&active.query);
    let (esc, up, down, accept) = ui.input_mut(|i| {
        let none = Modifiers::NONE;
        let esc = i.consume_key(none, Key::Escape);
        let has = !vm.mention.matches.is_empty();
        (
            esc,
            has && i.consume_key(none, Key::ArrowUp),
            has && i.consume_key(none, Key::ArrowDown),
            has && (i.consume_key(none, Key::Enter) || i.consume_key(none, Key::Tab)),
        )
    });
    if esc {
        vm.mention.dismissed = Some(active.start);
        vm.mention.popup_rect = None;
        return None;
    }
    if up {
        vm.mention.step(false);
    }
    if down {
        vm.mention.step(true);
    }
    let typing = Typing {
        start: active.start,
        cursor,
    };
    if accept {
        let picked = vm.mention.selected;
        accept_mention(ctx, vm, input_id, &typing, picked);
        vm.mention.popup_rect = None;
        return None;
    }
    Some(typing)
}

/// Replaces the typed `@query` with the `idx`-th match and puts the cursor after it.
fn accept_mention(
    ctx: &egui::Context,
    vm: &mut SessionVm,
    input_id: egui::Id,
    typing: &Typing,
    idx: usize,
) {
    let Some(path) = vm.mention.matches.get(idx).cloned() else {
        return;
    };
    let (text, cursor) = mention::apply(&vm.input, typing.start, typing.cursor, &path);
    vm.input = text;
    let mut state = egui::TextEdit::load_state(ctx, input_id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(
            egui::text::CCursor::new(cursor),
        )));
    state.store(ctx, input_id);
    ctx.memory_mut(|m| m.request_focus(input_id));
}

/// Draws the popup above the input box; a click picks a row.
fn mention_popup_show(
    ui: &egui::Ui,
    vm: &mut SessionVm,
    input_id: egui::Id,
    input_box: egui::Rect,
    typing: Option<Typing>,
) {
    let Some(typing) = typing else {
        return;
    };
    let ctx = ui.ctx().clone();
    let width = input_box.width().min(560.0);
    let mut clicked = None;
    let area = egui::Area::new(egui::Id::unique(("mention_popup", vm.session.id)))
        .order(egui::Order::Foreground)
        .pivot(egui::Align2::LEFT_BOTTOM)
        .fixed_pos(input_box.left_top() - egui::vec2(0.0, 4.0))
        .show(&ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(width - 16.0);
                if vm.mention.matches.is_empty() {
                    let msg = if matches!(vm.mention.files, mention::Files::Ready { .. }) {
                        "No matching files"
                    } else {
                        "Listing files\u{2026}"
                    };
                    ui.label(RichText::new(msg).small().color(theme::TEXT_MUTED));
                }
                for (i, path) in vm.mention.matches.iter().enumerate() {
                    let row = egui::Button::selectable(i == vm.mention.selected, path)
                        .truncate()
                        .min_size(egui::vec2(ui.available_width(), 0.0));
                    if ui.add(row).clicked() {
                        clicked = Some(i);
                    }
                }
            });
        });
    vm.mention.popup_rect = Some(area.response.rect);
    if let Some(i) = clicked {
        accept_mention(&ctx, vm, input_id, &typing, i);
        vm.mention.popup_rect = None;
    }
}

/// Shown above the input when the prompt cache has expired: the next message resends the whole
/// context at full price. While the cache is warm there is nothing worth saying.
fn send_note(
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

/// Shows the user's message and hands it to the session.
pub fn send_now(vm: &mut SessionVm, env_cmd: &Env, text: String, delivery: Option<Delivery>) {
    push_user_message(vm, text.clone(), delivery);
    vm.error = None;
    vm.attach_note = None;
    let _ = env_cmd.cmd_tx.send(ManagerCmd::Send {
        session_id: vm.session.id,
        text,
        delivery,
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

/// Returns whether the attach button was clicked.
fn controls(ui: &mut egui::Ui, vm: &mut SessionVm, env: &Env) -> bool {
    let id = vm.session.id;
    let provider = env
        .providers
        .iter()
        .find(|p| p.id() == vm.session.provider_id);
    let models = provider.map(|p| p.models()).unwrap_or_default();
    let fallback = provider.map(|p| p.effort_levels()).unwrap_or_default();
    let efforts = effort_choices(&models, &vm.session.model_id, &fallback);
    ui.horizontal(|ui| {
        let attach = ui
            .add(
                egui::Button::new(RichText::new("+").size(20.0).strong())
                    .min_size(egui::vec2(30.0, 28.0)),
            )
            .on_hover_text("Attach files (or type @ or drop files here): adds @path mentions")
            .clicked();
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
            .corner_radius(3);
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
        // Session stats take the rest of the row, right-aligned; they are cut with "…" when
        // narrow (never a second row) and show in full on hover.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let u = &vm.session.usage;
            let stats = format!("{} · {}", context_text(u), usage_text(u));
            ui.add(
                egui::Label::new(RichText::new(&stats).small().color(theme::TEXT_MUTED)).truncate(),
            )
            .on_hover_text(stats);
        });
        attach
    })
    .inner
}

/// `@path` for the CLI to expand: relative to the session folder when inside it, quoted when
/// the path has whitespace.
pub fn mention(cwd: &Path, path: &Path) -> String {
    mention::token(&path.strip_prefix(cwd).unwrap_or(path).to_string_lossy())
}

/// A warning line with the warning icon, above the input.
/// Says what changed since the last message and what the next one costs for it: a different
/// model or thinking effort resends the whole context at full price.
fn settings_note(vm: &SessionVm, env: &Env) -> Option<String> {
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
fn running_subagents(vm: &SessionVm) -> usize {
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
fn subagent_badge(ui: &mut egui::Ui, count: usize, active: bool) -> bool {
    let frame = egui::Frame::new()
        .fill(if active {
            theme::CLAUDE.gamma_multiply(0.25)
        } else {
            egui::Color32::TRANSPARENT
        })
        .stroke(egui::Stroke::new(1.0, theme::CLAUDE))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(6, 0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(10.0).color(theme::CLAUDE));
                let text = if count == 1 {
                    "Subagent running".to_string()
                } else {
                    format!("{count} subagents running")
                };
                ui.label(RichText::new(text).small().color(theme::CLAUDE));
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

fn note_frame(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(theme::BG_RAISED)
        .stroke(egui::Stroke::new(1.0, theme::WARN))
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(icons::with(icons::WARNING, text))
                        .small()
                        .color(theme::WARN),
                )
                .wrap(),
            );
        });
    ui.add_space(4.0);
}

/// Files on the system clipboard (e.g. copied in Finder), if any.
fn clipboard_files() -> Option<Vec<PathBuf>> {
    let files = arboard::Clipboard::new().ok()?.get().file_list().ok()?;
    (!files.is_empty()).then_some(files)
}

/// When the events hold a paste and `files` finds copied files, drops the paste (it would
/// only insert the file names as text) and returns them.
fn take_pasted_files(
    events: &mut Vec<egui::Event>,
    files: impl FnOnce() -> Option<Vec<PathBuf>>,
) -> Option<Vec<PathBuf>> {
    let is_paste =
        |e: &egui::Event| matches!(e, egui::Event::Paste(_) | egui::Event::PasteImage(_));
    if !events.iter().any(is_paste) {
        return None;
    }
    let files = files()?;
    events.retain(|e| !is_paste(e));
    Some(files)
}

/// When the events hold a pasted image, drops them and returns the image.
fn take_pasted_image(events: &mut Vec<egui::Event>) -> Option<std::sync::Arc<egui::ColorImage>> {
    let mut image = None;
    events.retain(|e| match e {
        egui::Event::PasteImage(img) => {
            image = Some(img.clone());
            false
        }
        _ => true,
    });
    image
}

/// How long a refused-attachment note stays above the input.
const ATTACH_NOTE_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// Shows `message` above the input for a while.
pub fn set_attach_note(vm: &mut SessionVm, message: String) {
    vm.attach_note = Some((message, std::time::Instant::now()));
}

/// Appends a mention per attachable path to the input, separated by spaces. Files refused
/// (binary, over the size limit, unreadable) are explained in a note above the input.
pub fn insert_mentions(vm: &mut SessionVm, paths: &[PathBuf]) {
    let mut rejected = Vec::new();
    for path in paths {
        if let Err(why) = attach::check(path) {
            rejected.push((path.clone(), why));
            continue;
        }
        if !vm.input.is_empty() && !vm.input.ends_with(char::is_whitespace) {
            vm.input.push(' ');
        }
        vm.input.push_str(&mention(&vm.cwd, path));
        vm.input.push(' ');
    }
    if !rejected.is_empty() {
        set_attach_note(vm, attach::rejected_message(&rejected));
    }
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
            changes: Default::default(),
            settings_pending: false,
            turn_base: None,
            turn_snapshotting: false,
            turn_files: Default::default(),
            turn_shared: false,
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

    #[test]
    fn mentions_are_relative_inside_the_folder_and_quoted_with_spaces() {
        let cwd = Path::new("/p");
        assert_eq!(mention(cwd, Path::new("/p/src/a.rs")), "@src/a.rs");
        assert_eq!(
            mention(cwd, Path::new("/elsewhere/b.txt")),
            "@/elsewhere/b.txt"
        );
        assert_eq!(
            mention(cwd, Path::new("/p/my notes.txt")),
            "@\"my notes.txt\""
        );
    }

    #[test]
    fn pasting_copied_files_replaces_the_text_paste() {
        let files = || Some(vec![PathBuf::from("/p/a.rs")]);
        let typed = egui::Event::Text("x".into());
        let paste = egui::Event::Paste("a.rs".into());

        let mut events = vec![typed.clone(), paste.clone()];
        assert_eq!(
            take_pasted_files(&mut events, files),
            Some(vec![PathBuf::from("/p/a.rs")])
        );
        assert_eq!(events.len(), 1, "the text paste is dropped");
        assert!(matches!(events[0], egui::Event::Text(_)));

        // Plain text on the clipboard keeps its normal paste.
        let mut events = vec![paste.clone()];
        assert_eq!(take_pasted_files(&mut events, || None), None);
        assert_eq!(events, [paste]);

        // Without a paste the clipboard is not even read.
        let mut events = vec![typed];
        assert_eq!(take_pasted_files(&mut events, || panic!("read")), None);
    }

    #[test]
    fn inserted_mentions_are_space_separated_and_keep_the_typed_text() {
        let dir = std::env::temp_dir().join(format!("xade-ins-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.rs", "b c.rs", "d.rs"] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let mut v = vm("look at".into());
        v.cwd = dir.clone();
        insert_mentions(&mut v, &[dir.join("a.rs"), dir.join("b c.rs")]);
        assert!(v.attach_note.is_none());
        assert_eq!(v.input, "look at @a.rs @\"b c.rs\" ");
        insert_mentions(&mut v, &[dir.join("d.rs")]);
        assert_eq!(v.input, "look at @a.rs @\"b c.rs\" @d.rs ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn binary_and_missing_files_are_refused_with_a_message_and_the_rest_attached() {
        let dir = std::env::temp_dir().join(format!("xade-ins-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ok.rs"), "x").unwrap();
        std::fs::write(dir.join("blob.bin"), [0u8, 1, 2]).unwrap();
        let mut v = vm(String::new());
        v.cwd = dir.clone();
        insert_mentions(
            &mut v,
            &[
                dir.join("blob.bin"),
                dir.join("ok.rs"),
                dir.join("gone.txt"),
            ],
        );
        assert_eq!(v.input, "@ok.rs ");
        let (note, _) = v.attach_note.clone().unwrap();
        assert!(note.contains("blob.bin (binary)"), "{note}");
        assert!(note.contains("gone.txt (unreadable)"), "{note}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Height of the composer after a frame, plus where the note's text sits relative to the
    /// input box.
    fn height_with_note(note: Option<(String, std::time::Instant)>) -> f32 {
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
            vscode: None,
            maximized: None,
            send_mode: crate::config::SendMode::Queue,
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
    fn sending_clears_the_note() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let env = Env {
            cmd_tx: &tx,
            providers: &[],
            vscode: None,
            maximized: None,
            send_mode: crate::config::SendMode::Queue,
        };
        let mut v = vm("hi".into());
        set_attach_note(&mut v, "x".into());
        send_now(&mut v, &env, "hi".into(), None);
        assert!(v.attach_note.is_none());
    }

    #[test]
    fn a_pasted_image_event_is_taken_and_other_events_stay() {
        let img = std::sync::Arc::new(egui::ColorImage::example());
        let typed = egui::Event::Text("x".into());
        let mut events = vec![typed, egui::Event::PasteImage(img.clone())];
        let got = take_pasted_image(&mut events).unwrap();
        assert!(std::sync::Arc::ptr_eq(&got, &img));
        assert_eq!(events.len(), 1);
        assert_eq!(take_pasted_image(&mut events), None);
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

    /// One frame of the composer with `events` fed in; returns what it asked of the app.
    fn frame(
        ctx: &egui::Context,
        v: &mut SessionVm,
        events: Vec<egui::Event>,
        focus: bool,
    ) -> ComposerOut {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let env = Env {
            cmd_tx: &tx,
            providers: &[],
            vscode: None,
            maximized: None,
            send_mode: crate::config::SendMode::Queue,
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

    fn press(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn typing_at_opens_the_popup_and_enter_inserts_the_file() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut v = vm(String::new());
        v.mention
            .set_files(vec!["README.md".into(), "src/main.rs".into()]);
        frame(&ctx, &mut v, vec![], true);
        frame(&ctx, &mut v, vec![egui::Event::Text("@ma".into())], false);
        let out = frame(&ctx, &mut v, vec![], false);
        assert!(!out.index, "the list is already loaded");
        assert_eq!(v.input, "@ma");
        assert_eq!(v.mention.matches, ["src/main.rs"]);
        assert!(v.mention.popup_rect.is_some(), "popup drawn");

        // Enter picks the row and does not add a newline.
        frame(&ctx, &mut v, vec![press(egui::Key::Enter)], false);
        assert_eq!(v.input, "@src/main.rs ");
        assert!(v.mention.popup_rect.is_none());

        // Typing on continues after the inserted mention.
        frame(&ctx, &mut v, vec![egui::Event::Text("ok".into())], false);
        assert_eq!(v.input, "@src/main.rs ok");
    }

    #[test]
    fn escape_dismisses_the_popup_until_a_new_mention_starts() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut v = vm(String::new());
        v.mention.set_files(vec!["src/main.rs".into()]);
        frame(&ctx, &mut v, vec![], true);
        frame(&ctx, &mut v, vec![egui::Event::Text("@m".into())], false);
        frame(&ctx, &mut v, vec![], false);
        assert!(v.mention.popup_rect.is_some());
        frame(&ctx, &mut v, vec![press(egui::Key::Escape)], false);
        frame(&ctx, &mut v, vec![], false);
        assert!(v.mention.popup_rect.is_none());
        assert_eq!(v.input, "@m");
    }

    #[test]
    fn an_unloaded_list_is_requested_when_the_popup_opens() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut v = vm(String::new());
        frame(&ctx, &mut v, vec![], true);
        frame(&ctx, &mut v, vec![egui::Event::Text("@".into())], false);
        let out = frame(&ctx, &mut v, vec![], false);
        assert!(out.index);
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
