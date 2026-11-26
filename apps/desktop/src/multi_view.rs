//! Composite Multi-Session window: shared Team feed + per-member tabs + unified `#handle:` composer.

use std::collections::HashMap;
use std::path::PathBuf;

use egui::{Key, Modifiers, RichText};
use egui_commonmark::CommonMarkCache;
use xade_core::{
    parse_user_target, Effort, MultiSession, SessionId, SessionStatus, TeamMessage, TeamMessageKind,
};
use xade_runtime::ManagerCmd;

use crate::app::SessionVm;
use crate::config::SendMode;
use crate::keymap::{self, Action};
use crate::session_view::{self, ViewOut};
use crate::widgets::path_links::PathLinks;
use crate::workspace::Env;
use crate::{chrome, conversation, icons, theme};

const INPUT_MAX_HEIGHT: f32 = 140.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MultiTab {
    #[default]
    Team,
    Member(SessionId),
}

pub struct MultiSessionVm {
    pub multi: MultiSession,
    pub cwd: PathBuf,
    /// Ordered team member session ids (each member's `SessionVm` lives in `App.sessions`).
    pub member_ids: Vec<SessionId>,
    pub team_messages: Vec<TeamMessage>,
    pub active_tab: MultiTab,
    pub input: String,
    pub handle_popup_sel: usize,
    pub handle_popup_dismissed: bool,
    pub handle_popup_rect: Option<egui::Rect>,
    pub broadcasts_paused: bool,
    pub paused_hops: u32,
    pub renaming: Option<String>,
    pub md_cache: CommonMarkCache,
    pub path_links: PathLinks,
    pub scroll_to_end: bool,
    pub focus_input: bool,
    pub collapsed: bool,
    pub window_rect: Option<egui::Rect>,
    pub apply_rect: Option<egui::Rect>,
}

impl MultiSessionVm {
    pub fn new(
        multi: MultiSession,
        cwd: PathBuf,
        member_ids: Vec<SessionId>,
        team_messages: Vec<TeamMessage>,
    ) -> Self {
        Self {
            multi,
            cwd,
            member_ids,
            team_messages,
            active_tab: MultiTab::Team,
            input: String::new(),
            handle_popup_sel: 0,
            handle_popup_dismissed: false,
            handle_popup_rect: None,
            broadcasts_paused: false,
            paused_hops: 0,
            renaming: None,
            md_cache: crate::widgets::markdown::new_cache(),
            path_links: PathLinks::default(),
            scroll_to_end: false,
            focus_input: false,
            collapsed: false,
            window_rect: None,
            apply_rect: None,
        }
    }

    /// Aggregate status across members: NeedsApproval > Streaming > Error > Idle.
    pub fn aggregate_status(&self, sessions: &HashMap<SessionId, SessionVm>) -> SessionStatus {
        let mut any_streaming = false;
        let mut first_error = None;
        for sid in &self.member_ids {
            if let Some(vm) = sessions.get(sid) {
                match &vm.status {
                    SessionStatus::NeedsApproval => return SessionStatus::NeedsApproval,
                    SessionStatus::Streaming | SessionStatus::Starting => any_streaming = true,
                    SessionStatus::Error(e) if first_error.is_none() => {
                        first_error = Some(e.clone())
                    }
                    _ => {}
                }
            }
        }
        if any_streaming {
            SessionStatus::Streaming
        } else if let Some(e) = first_error {
            SessionStatus::Error(e)
        } else {
            SessionStatus::Idle
        }
    }
}

pub fn show(
    ui: &mut egui::Ui,
    vm: &mut MultiSessionVm,
    members: &mut HashMap<SessionId, SessionVm>,
    env: &Env,
    maximized: bool,
) -> ViewOut {
    let mut out = ViewOut::default();
    let any_busy = vm.member_ids.iter().any(|id| {
        members.get(id).is_some_and(|m| {
            matches!(
                m.status,
                SessionStatus::Streaming | SessionStatus::NeedsApproval
            )
        })
    });
    let agg_status = vm.aggregate_status(members);
    header(ui, vm, env, &agg_status, any_busy, maximized, &mut out);
    if vm.collapsed {
        return out;
    }

    tab_bar(ui, vm, members);
    if vm.broadcasts_paused {
        paused_banner(ui, vm, env);
    }

    // Unified bottom composer stays visible on both Team and Member tabs.
    let handles: Vec<String> = vm
        .member_ids
        .iter()
        .filter_map(|id| members.get(id)?.session.agent_handle.clone())
        .collect();
    egui::Panel::bottom(("multi_composer", vm.multi.id))
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(theme::bg_window())
                .stroke(egui::Stroke::new(1.0, theme::border()))
                .inner_margin(egui::Margin::same(8)),
        )
        .show(ui, |ui| {
            if let MultiTab::Member(sid) = vm.active_tab {
                if let Some(member) = members.get_mut(&sid) {
                    session_view::banner(ui, member, env);
                }
            }
            unified_composer(ui, vm, &handles, members, env);
        });

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show(ui, |ui| match vm.active_tab {
            MultiTab::Team => {
                approval_jump_strip(ui, vm, members);
                out.open_file = team_feed(ui, vm, &handles);
            }
            MultiTab::Member(sid) => {
                if let Some(member) = members.get_mut(&sid) {
                    member_subbar(ui, member, env);
                    out.open_file = conversation::show(ui, member);
                }
            }
        });
    out
}

fn paused_banner(ui: &mut egui::Ui, vm: &MultiSessionVm, env: &Env) {
    egui::Frame::new()
        .fill(theme::bg_raised())
        .stroke(egui::Stroke::new(1.0, theme::warn()))
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!(
                        "{} Broadcast auto-delivery paused after {} consecutive hops.",
                        icons::WARNING,
                        vm.paused_hops
                    ))
                    .color(theme::warn()),
                );
                if ui.button("Resume broadcasts").clicked() {
                    let _ = env.cmd_tx.send(ManagerCmd::ResumeMultiBroadcasts {
                        multi_id: vm.multi.id,
                    });
                }
            });
        });
}

fn approval_jump_strip(
    ui: &mut egui::Ui,
    vm: &mut MultiSessionVm,
    members: &HashMap<SessionId, SessionVm>,
) {
    let awaiting: Vec<(SessionId, String)> = vm
        .member_ids
        .iter()
        .filter_map(|&sid| {
            let m = members.get(&sid)?;
            (m.status == SessionStatus::NeedsApproval)
                .then(|| (sid, m.session.agent_handle.clone().unwrap_or_default()))
        })
        .collect();
    if awaiting.is_empty() {
        return;
    }
    egui::Frame::new()
        .fill(theme::bg_raised())
        .stroke(egui::Stroke::new(1.0, theme::warn()))
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("{} Awaiting approval:", icons::WARNING))
                        .small()
                        .color(theme::warn()),
                );
                for (sid, handle) in awaiting {
                    if ui.small_button(format!("Review #{handle}")).clicked() {
                        vm.active_tab = MultiTab::Member(sid);
                    }
                }
            });
        });
}

fn header(
    ui: &mut egui::Ui,
    vm: &mut MultiSessionVm,
    env: &Env,
    agg_status: &SessionStatus,
    any_busy: bool,
    maximized: bool,
    out: &mut ViewOut,
) {
    let title = if vm.multi.title.is_empty() {
        "New Multi-Session".to_string()
    } else {
        vm.multi.title.clone()
    };
    let cwd = vm
        .cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let badge = |ui: &mut egui::Ui| chrome::header_badge(ui, "Multi");
    let mut renamed = None;
    chrome::header(
        ui,
        chrome::HeaderState {
            maximized,
            collapsed: vm.collapsed,
            focused: env.focused,
        },
        "Close multi-session",
        out,
        &badge,
        |ui| {
            session_view::status_dot(ui, agg_status);
            if !cwd.is_empty() {
                ui.label(RichText::new(cwd).small().color(theme::text_muted()))
                    .on_hover_text(vm.cwd.display().to_string());
            }
            if any_busy
                && ui
                    .small_button(
                        RichText::new(icons::with(icons::STOP, "Stop All")).color(theme::danger()),
                    )
                    .on_hover_text("Stop all running models and clear queued broadcasts")
                    .clicked()
            {
                let _ = env.cmd_tx.send(ManagerCmd::InterruptMultiAll {
                    multi_id: vm.multi.id,
                });
            }
            renamed = multi_title_label(ui, vm, &title);
        },
    );
    out.rename = renamed;
}

fn multi_title_label(ui: &mut egui::Ui, vm: &mut MultiSessionVm, title: &str) -> Option<String> {
    let id = egui::Id::unique(("rename_multi_title", vm.multi.id));
    if let Some(buf) = vm.renaming.as_mut() {
        crate::widgets::selection_menu::mark_text_box(ui.ctx());
        ui.label(RichText::new("•").strong());
        let edit = ui.add(
            egui::TextEdit::singleline(buf)
                .id(id)
                .desired_width(f32::INFINITY),
        );
        if edit.lost_focus() {
            let cancelled = ui.input(|i| i.key_pressed(Key::Escape));
            let text = vm.renaming.take().unwrap_or_default();
            return (!cancelled).then_some(text);
        }
        return None;
    }
    let resp = ui
        .add(
            egui::Label::new(RichText::new(format!("• {title}")).strong())
                .truncate()
                .sense(egui::Sense::click()),
        )
        .on_hover_cursor(egui::CursorIcon::Text)
        .on_hover_text("Click to rename");
    if resp.clicked() {
        let len = vm.multi.title.chars().count();
        vm.renaming = Some(vm.multi.title.clone());
        let mut state = egui::text_edit::TextEditState::default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(len),
            )));
        state.store(ui.ctx(), id);
        ui.memory_mut(|m| m.request_focus(id));
    }
    None
}

fn tab_bar(ui: &mut egui::Ui, vm: &mut MultiSessionVm, members: &HashMap<SessionId, SessionVm>) {
    egui::Frame::new()
        .fill(theme::bg_raised())
        .stroke(egui::Stroke::new(1.0, theme::border()))
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                if ui
                    .selectable_label(
                        vm.active_tab == MultiTab::Team,
                        RichText::new("Team").strong(),
                    )
                    .clicked()
                {
                    vm.active_tab = MultiTab::Team;
                }
                ui.separator();
                for &sid in &vm.member_ids {
                    let Some(m) = members.get(&sid) else {
                        continue;
                    };
                    let handle = m.session.agent_handle.as_deref().unwrap_or("agent");
                    let selected = vm.active_tab == MultiTab::Member(sid);
                    let needs_approval = m.status == SessionStatus::NeedsApproval;
                    let label = if needs_approval {
                        RichText::new(format!("#{handle} !"))
                            .color(theme::warn())
                            .strong()
                    } else if matches!(m.status, SessionStatus::Error(_)) {
                        RichText::new(format!("#{handle}")).color(theme::danger())
                    } else {
                        RichText::new(format!("#{handle}"))
                    };
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        session_view::status_dot(ui, &m.status);
                        if let Some(tex) =
                            conversation::company_symbol(ui.ctx(), m.session.provider_id)
                        {
                            ui.add(
                                egui::Image::new(&tex).fit_to_exact_size(egui::vec2(12.0, 12.0)),
                            );
                        }
                        if ui.selectable_label(selected, label).clicked() {
                            vm.active_tab = MultiTab::Member(sid);
                        }
                    });
                }
            });
        });
}

fn member_subbar(ui: &mut egui::Ui, member: &mut SessionVm, env: &Env) {
    let found = env
        .providers
        .iter()
        .find(|p| p.id() == member.session.provider_id);
    let model_label = found
        .and_then(|p| {
            p.models()
                .into_iter()
                .find(|m| m.id == member.session.model_id)
        })
        .map_or_else(|| member.session.model_id.clone(), |m| m.label);
    let mut efforts = vec![Effort::Default];
    if let Some(p) = found {
        let extra = p
            .models()
            .into_iter()
            .find(|m| m.id == member.session.model_id)
            .map(|m| m.effort_levels)
            .unwrap_or_else(|| p.effort_levels());
        for e in extra {
            if !efforts.contains(&e) {
                efforts.push(e);
            }
        }
    }
    egui::Frame::new()
        .fill(theme::bg_app())
        .stroke(egui::Stroke::new(1.0, theme::border()))
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(model_label).small().strong());
                ui.label(RichText::new("(locked)").small().color(theme::text_muted()));
                let mut effort = member.session.effort;
                egui::ComboBox::from_id_salt(("member_effort", member.session.id))
                    .selected_text(effort_name(effort))
                    .width(72.0)
                    .show_ui(ui, |ui| {
                        for e in efforts {
                            ui.selectable_value(&mut effort, e, effort_name(e));
                        }
                    });
                if effort != member.session.effort {
                    member.session.effort = effort;
                    let _ = env.cmd_tx.send(ManagerCmd::SetEffort {
                        session_id: member.session.id,
                        effort,
                    });
                }
                let mut auto = member.session.auto_mode;
                if ui.checkbox(&mut auto, "Auto").changed() {
                    member.session.auto_mode = auto;
                    let _ = env.cmd_tx.send(ManagerCmd::SetAutoMode {
                        session_id: member.session.id,
                        on: auto,
                    });
                }
                let u = &member.session.usage;
                if u.context_window_max > 0 {
                    let pct = (u.context_tokens_used as f64 / u.context_window_max as f64 * 100.0)
                        .round() as u64;
                    ui.label(
                        RichText::new(format!("ctx {pct}%"))
                            .small()
                            .color(theme::text_muted()),
                    );
                }
                if matches!(
                    member.status,
                    SessionStatus::Streaming | SessionStatus::NeedsApproval
                ) && ui
                    .small_button(
                        RichText::new(icons::with(icons::STOP, "Stop")).color(theme::danger()),
                    )
                    .clicked()
                {
                    let _ = env.cmd_tx.send(ManagerCmd::Interrupt {
                        session_id: member.session.id,
                    });
                }
            });
        });
}

fn effort_name(e: Effort) -> &'static str {
    match e {
        Effort::Default => "Default",
        Effort::Low => "Low",
        Effort::Medium => "Medium",
        Effort::High => "High",
        Effort::XHigh => "X-High",
        Effort::Max => "Max",
    }
}

fn team_feed(ui: &mut egui::Ui, vm: &mut MultiSessionVm, handles: &[String]) -> Option<PathBuf> {
    let mut open_file = None;
    egui::ScrollArea::vertical()
        .id_salt(("team_feed", vm.multi.id))
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Frame::new()
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if vm.team_messages.is_empty() {
                        let roster = handles
                            .iter()
                            .map(|h| format!("#{h}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        ui.label(
                            RichText::new(format!(
                                "Team ready ({roster}). Send a prompt to everyone, or address one model with #handle: message."
                            ))
                            .color(theme::text_muted()),
                        );
                    }
                    for tm in &vm.team_messages {
                        let clicked = team_message_row(
                            ui,
                            &mut vm.md_cache,
                            &mut vm.path_links,
                            &vm.cwd,
                            tm,
                        );
                        if open_file.is_none() {
                            open_file = clicked;
                        }
                        ui.add_space(8.0);
                    }
                });
            if std::mem::take(&mut vm.scroll_to_end) {
                ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
            }
        });
    open_file
}

fn team_message_row(
    ui: &mut egui::Ui,
    cache: &mut CommonMarkCache,
    links: &mut PathLinks,
    cwd: &std::path::Path,
    tm: &TeamMessage,
) -> Option<PathBuf> {
    match &tm.kind {
        TeamMessageKind::User { target } => {
            let indent = ui.available_width() * 0.15;
            ui.horizontal_top(|ui| {
                ui.add_space(indent);
                egui::Frame::new()
                    .fill(theme::bg_raised())
                    .stroke(egui::Stroke::new(1.0, theme::border_light()))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.vertical(|ui| {
                            let to = target
                                .as_deref()
                                .map_or_else(|| "To: All".to_string(), |h| format!("To: #{h}"));
                            ui.label(RichText::new(to).small().strong().color(theme::accent()));
                            links.markdown(ui, cache, cwd, tm.id, &tm.content_text)
                        })
                        .inner
                    })
                    .inner
            })
            .inner
        }
        TeamMessageKind::Broadcast {
            sender_handle,
            sender_provider,
        } => {
            egui::Frame::new()
                .fill(theme::bg_raised())
                .stroke(egui::Stroke::new(1.0, theme::accent()))
                .corner_radius(8)
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            if let Some(tex) = conversation::cli_symbol(ui.ctx(), *sender_provider)
                            {
                                ui.add(
                                    egui::Image::new(&tex)
                                        .fit_to_exact_size(egui::vec2(14.0, 14.0)),
                                );
                            }
                            ui.label(
                                RichText::new(format!("#{sender_handle}"))
                                    .small()
                                    .strong()
                                    .color(theme::accent()),
                            );
                            ui.label(
                                RichText::new("broadcast")
                                    .small()
                                    .color(theme::text_muted()),
                            );
                        });
                        links.markdown(ui, cache, cwd, tm.id, &tm.content_text)
                    })
                    .inner
                })
                .inner
        }
    }
}

fn unified_composer(
    ui: &mut egui::Ui,
    vm: &mut MultiSessionVm,
    handles: &[String],
    members: &HashMap<SessionId, SessionVm>,
    env: &Env,
) {
    let (parsed_target, _) = parse_user_target(&vm.input, handles);
    let edit_id = egui::Id::unique(("multi_input", vm.multi.id));
    let input_focused = ui.memory(|m| m.has_focus(edit_id));

    // Only suppress the Copy/Quote popup when this text box actually has focus.
    if input_focused {
        crate::widgets::selection_menu::mark_text_box(ui.ctx());
    }

    // Check status of the *targeted* member(s) specifically.
    let mut target_busy = false;
    let mut target_alive = false;
    for &sid in &vm.member_ids {
        let Some(m) = members.get(&sid) else {
            continue;
        };
        let matches = parsed_target
            .as_deref()
            .is_none_or(|t| m.session.agent_handle.as_deref() == Some(t));
        if !matches {
            continue;
        }
        match &m.status {
            SessionStatus::Idle => target_alive = true,
            SessionStatus::Starting | SessionStatus::Streaming | SessionStatus::NeedsApproval => {
                target_alive = true;
                target_busy = true;
            }
            SessionStatus::Error(_) | SessionStatus::Closed => {}
        }
    }

    // Quick target pills: clicking sets or clears the leading `#handle: ` prefix.
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("To:").small().color(theme::text_muted()));
        let is_all = parsed_target.is_none();
        if ui
            .selectable_label(is_all, RichText::new("All").small())
            .clicked()
        {
            strip_target_prefix(&mut vm.input, handles);
            vm.handle_popup_dismissed = false;
            vm.focus_input = true;
        }
        for h in handles {
            let sel = parsed_target.as_deref() == Some(h.as_str());
            if ui
                .selectable_label(sel, RichText::new(format!("#{h}")).small())
                .clicked()
            {
                set_target_prefix(&mut vm.input, handles, h);
                vm.handle_popup_dismissed = false;
                vm.focus_input = true;
            }
        }
    });
    ui.add_space(4.0);

    // `#` handle autocomplete popup: keep visible while pointer is over the popup so clicks land
    // before focus leaves `edit_id`; keyboard navigation stays strictly gated on `input_focused`.
    let completion_matches = if vm.handle_popup_dismissed {
        Vec::new()
    } else {
        handle_completions(&vm.input, handles)
    };
    let on_popup = vm
        .handle_popup_rect
        .is_some_and(|r| ui.input(|i| i.pointer.latest_pos().is_some_and(|p| r.contains(p))));
    if !completion_matches.is_empty() && (input_focused || on_popup) {
        vm.handle_popup_sel = vm
            .handle_popup_sel
            .min(completion_matches.len().saturating_sub(1));
        let (up, down, pick, esc) = if input_focused {
            ui.input_mut(|i| {
                (
                    i.consume_key(Modifiers::NONE, Key::ArrowUp),
                    i.consume_key(Modifiers::NONE, Key::ArrowDown),
                    i.consume_key(Modifiers::NONE, Key::Tab)
                        || i.consume_key(Modifiers::NONE, Key::Enter),
                    i.consume_key(Modifiers::NONE, Key::Escape),
                )
            })
        } else {
            (false, false, false, false)
        };
        if esc {
            vm.handle_popup_dismissed = true;
            vm.handle_popup_rect = None;
        } else if pick {
            let chosen = completion_matches[vm.handle_popup_sel].clone();
            vm.input = format!("#{chosen}: ");
            vm.focus_input = true;
            vm.handle_popup_rect = None;
        } else {
            if up {
                vm.handle_popup_sel = vm.handle_popup_sel.saturating_sub(1);
            }
            if down {
                vm.handle_popup_sel = (vm.handle_popup_sel + 1).min(completion_matches.len() - 1);
            }
            let mut picked_click = false;
            let frame_resp = egui::Frame::new()
                .fill(theme::bg_raised())
                .stroke(egui::Stroke::new(1.0, theme::border_strong()))
                .corner_radius(6)
                .inner_margin(egui::Margin::symmetric(6, 4))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for (idx, h) in completion_matches.iter().enumerate() {
                            if ui
                                .selectable_label(idx == vm.handle_popup_sel, format!("#{h}"))
                                .clicked()
                            {
                                vm.input = format!("#{h}: ");
                                vm.focus_input = true;
                                picked_click = true;
                            }
                        }
                    });
                });
            vm.handle_popup_rect = (!picked_click).then_some(frame_resp.response.rect);
            ui.add_space(4.0);
        }
    } else {
        vm.handle_popup_rect = None;
    }

    let pressed = if input_focused {
        ui.input_mut(keymap::take_send)
    } else {
        None
    };

    if std::mem::take(&mut vm.focus_input) {
        let ctx = ui.ctx();
        let mut state = egui::TextEdit::load_state(ctx, edit_id).unwrap_or_default();
        let end = egui::text::CCursor::new(vm.input.chars().count());
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(end)));
        state.store(ctx, edit_id);
        ctx.memory_mut(|m| m.request_focus(edit_id));
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

    let hint = format!(
        "Message all models, or #handle: message ({} to send)",
        keymap::shortcut_text(Action::Send)
    );
    let edit_changed = egui::ScrollArea::vertical()
        .id_salt(("multi_input_scroll", vm.multi.id))
        .max_height(INPUT_MAX_HEIGHT)
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut vm.input)
                    .id(edit_id)
                    .desired_rows(2)
                    .desired_width(f32::INFINITY)
                    .hint_text(hint)
                    .layouter(&mut layouter),
            )
            .changed()
        })
        .inner;
    if edit_changed {
        vm.handle_popup_dismissed = false;
    }

    let hotkey_other = pressed == Some(Action::SendOther);
    let mut do_send = pressed.is_some();
    let mut steer = match env.send_mode {
        SendMode::Steer => !hotkey_other,
        SendMode::Queue => hotkey_other,
    };

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let has_body = !parse_user_target(&vm.input, handles).1.is_empty();
        let can_send = has_body && target_alive;
        let primary_label = if target_busy && env.send_mode == SendMode::Steer {
            icons::with(icons::SEND, "Steer")
        } else if target_busy {
            icons::with(icons::SEND, "Queue")
        } else {
            icons::with(icons::SEND, "Send")
        };
        if ui
            .add_enabled(
                can_send,
                egui::Button::new(RichText::new(primary_label).color(theme::text()))
                    .fill(theme::accent()),
            )
            .on_disabled_hover_text(if !target_alive {
                "Target model is not running"
            } else {
                "Type a message to send"
            })
            .clicked()
        {
            do_send = true;
            steer = env.send_mode == SendMode::Steer;
        }
        if target_busy {
            let (alt_label, alt_steer, alt_tip) = match env.send_mode {
                SendMode::Queue => (
                    "Steer",
                    true,
                    format!(
                        "Send immediately into running turn ({})",
                        keymap::shortcut_text(Action::SendOther)
                    ),
                ),
                SendMode::Steer => (
                    "Queue",
                    false,
                    format!(
                        "Queue until running turn finishes ({})",
                        keymap::shortcut_text(Action::SendOther)
                    ),
                ),
            };
            if ui
                .add_enabled(can_send, egui::Button::new(alt_label))
                .on_hover_text(alt_tip)
                .clicked()
            {
                do_send = true;
                steer = alt_steer;
            }
        }
    });

    if do_send && target_alive {
        let (_, body) = parse_user_target(&vm.input, handles);
        if !body.is_empty() {
            let text = std::mem::take(&mut vm.input);
            vm.handle_popup_dismissed = false;
            vm.scroll_to_end = true;
            let _ = env.cmd_tx.send(ManagerCmd::SendMulti {
                multi_id: vm.multi.id,
                text,
                steer,
                context: env.ide_context.map(str::to_string),
            });
        }
    }
}

fn handle_completions(input: &str, handles: &[String]) -> Vec<String> {
    let trimmed = input.trim_start();
    let Some(rest) = trimmed.strip_prefix('#') else {
        return Vec::new();
    };
    if rest.contains(':') || rest.contains(char::is_whitespace) {
        return Vec::new();
    }
    let q = rest.to_ascii_lowercase();
    handles
        .iter()
        .filter(|h| h.to_ascii_lowercase().starts_with(&q))
        .cloned()
        .collect()
}

fn strip_target_prefix(input: &mut String, handles: &[String]) {
    let (target, body) = parse_user_target(input, handles);
    if target.is_some() {
        *input = body.to_string();
    }
}

fn set_target_prefix(input: &mut String, handles: &[String], handle: &str) {
    let (_, body) = parse_user_target(input, handles);
    let body = body.to_string();
    if body.is_empty() {
        *input = format!("#{handle}: ");
    } else {
        *input = format!("#{handle}: {body}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_completion_and_target_prefix_helpers() {
        let handles = vec!["opus-1".into(), "sonnet-1".into(), "neon-1".into()];
        assert_eq!(handle_completions("#", &handles), handles);
        assert_eq!(handle_completions("#so", &handles), ["sonnet-1"]);
        assert!(handle_completions("#opus-1: hi", &handles).is_empty());
        assert!(handle_completions("hello", &handles).is_empty());

        let mut input = "plan feature x".to_string();
        set_target_prefix(&mut input, &handles, "opus-1");
        assert_eq!(input, "#opus-1: plan feature x");
        set_target_prefix(&mut input, &handles, "neon-1");
        assert_eq!(input, "#neon-1: plan feature x");
        strip_target_prefix(&mut input, &handles);
        assert_eq!(input, "plan feature x");
    }
}
