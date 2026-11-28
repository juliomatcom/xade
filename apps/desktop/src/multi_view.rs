//! Composite Multi-Session window: shared Team feed + per-member tabs + unified `#handle:` composer.

use std::collections::HashMap;
use std::path::PathBuf;

use egui::{Key, Modifiers, RichText};
use egui_commonmark::CommonMarkCache;
use xade_core::{
    parse_user_target, split_broadcast_segments, Effort, MultiSession, ProviderId, Role, SessionId,
    SessionStatus, TeamMessage, TeamMessageKind, TextSegment,
};
use xade_runtime::ManagerCmd;

use crate::app::SessionVm;
use crate::config::SendMode;
use crate::keymap::{self, Action};
use crate::session_view::{self, ViewOut};
use crate::widgets::markdown::markdown;
use crate::widgets::path_links::PathLinks;
use crate::widgets::scroll_nav;
use crate::widgets::working;
use crate::workspace::Env;
use crate::{chrome, composer, conversation, icons, theme};

/// Matches standalone `composer.rs` input sizing and button geometry.
const INPUT_MIN_ROWS: usize = 2;
const INPUT_MAX_ROWS: usize = 10;
const BUTTON_SIZE: f32 = 48.0;

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
    header(ui, vm, members, env, &agg_status, maximized, &mut out);
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
                    composer::cost_notes(ui, member, env);
                }
            }
            unified_composer(ui, vm, &handles, members, any_busy, env);
        });

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show(ui, |ui| match vm.active_tab {
            MultiTab::Team => {
                approval_jump_strip(ui, vm, members);
                out.open_file = team_feed(ui, vm, members, &handles);
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

fn header_cli_providers(
    member_ids: &[SessionId],
    members: &HashMap<SessionId, SessionVm>,
) -> Vec<ProviderId> {
    let mut out = Vec::new();
    for sid in member_ids {
        if let Some(m) = members.get(sid) {
            if !out.contains(&m.session.provider_id) {
                out.push(m.session.provider_id);
            }
        }
    }
    out
}

fn header(
    ui: &mut egui::Ui,
    vm: &mut MultiSessionVm,
    members: &HashMap<SessionId, SessionVm>,
    env: &Env,
    agg_status: &SessionStatus,
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
    let cli_providers = header_cli_providers(&vm.member_ids, members);
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
            for pid in &cli_providers {
                if let Some(tex) = conversation::cli_symbol(ui.ctx(), *pid) {
                    ui.add(
                        egui::Image::new(&tex)
                            .tint(conversation::symbol_tint(*pid))
                            .fit_to_exact_size(egui::vec2(14.0, 14.0)),
                    );
                }
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

/// Draws one clickable tab container whose selected/hover highlight wraps its entire contents
/// (`status dot + provider icon + handle`, or `Team`).
fn tab_item(
    ui: &mut egui::Ui,
    id_salt: impl egui::AsIdSalt,
    selected: bool,
    body: impl FnOnce(&mut egui::Ui),
) -> bool {
    let mut prepared = egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(8, 4))
        .begin(ui);
    prepared
        .content_ui
        .style_mut()
        .interaction
        .selectable_labels = false;
    prepared.content_ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        body(ui);
    });
    let rect = prepared.allocate_space(ui).rect;
    let resp = ui
        .interact(rect, ui.make_persistent_id(id_salt), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if selected {
        prepared.frame.fill = theme::bg_header_focus();
        prepared.frame.stroke = egui::Stroke::new(1.0, theme::accent());
    } else if resp.hovered() {
        prepared.frame.fill = theme::bg_hover();
        prepared.frame.stroke = egui::Stroke::new(1.0, theme::border_strong());
    }
    prepared.paint(ui);
    resp.clicked()
}

fn tab_bar(ui: &mut egui::Ui, vm: &mut MultiSessionVm, members: &HashMap<SessionId, SessionVm>) {
    egui::Frame::new()
        .fill(theme::bg_raised())
        .stroke(egui::Stroke::new(1.0, theme::border()))
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                if tab_item(
                    ui,
                    ("multi_tab_team", vm.multi.id),
                    vm.active_tab == MultiTab::Team,
                    |ui| {
                        ui.label(RichText::new("Team").strong());
                    },
                ) {
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
                    if tab_item(ui, ("multi_tab_member", vm.multi.id, sid), selected, |ui| {
                        session_view::status_dot(ui, &m.status);
                        if let Some(tex) = conversation::cli_symbol(ui.ctx(), m.session.provider_id)
                        {
                            ui.add(
                                egui::Image::new(&tex)
                                    .tint(conversation::symbol_tint(m.session.provider_id))
                                    .fit_to_exact_size(egui::vec2(12.0, 12.0)),
                            );
                        }
                        ui.label(label);
                    }) {
                        vm.active_tab = MultiTab::Member(sid);
                    }
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
                let on = member.session.auto_mode;
                let (auto_text, fill, fg, stroke) = if on {
                    (
                        icons::with(icons::AUTO, "Auto: ON"),
                        theme::ok_bg(),
                        theme::ok(),
                        theme::ok(),
                    )
                } else {
                    (
                        icons::with(icons::AUTO, "Auto: OFF"),
                        theme::bg_hover(),
                        theme::text(),
                        theme::border(),
                    )
                };
                let pill = egui::Button::new(RichText::new(auto_text).small().color(fg))
                    .fill(fill)
                    .stroke(egui::Stroke::new(1.0, stroke));
                if ui.add(pill).clicked() {
                    member.session.auto_mode = !on;
                    let _ = env.cmd_tx.send(ManagerCmd::SetAutoMode {
                        session_id: member.session.id,
                        on: member.session.auto_mode,
                    });
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

enum CombinedKind {
    User {
        target: Option<String>,
    },
    MemberReply {
        handle: String,
        provider: ProviderId,
    },
    Broadcast {
        sender_handle: String,
        sender_provider: ProviderId,
    },
}

struct CombinedEntry {
    id: uuid::Uuid,
    created_at: time::OffsetDateTime,
    kind_rank: u8,
    order_key: (usize, i64, usize),
    kind: CombinedKind,
    text: String,
}

fn collect_combined_feed(
    vm: &MultiSessionVm,
    members: &HashMap<SessionId, SessionVm>,
) -> Vec<CombinedEntry> {
    let mut entries = Vec::new();
    for tm in &vm.team_messages {
        let (kind_rank, kind) = match &tm.kind {
            TeamMessageKind::User { target } => (
                0,
                CombinedKind::User {
                    target: target.clone(),
                },
            ),
            TeamMessageKind::Broadcast {
                sender_handle,
                sender_provider,
            } => (
                2,
                CombinedKind::Broadcast {
                    sender_handle: sender_handle.clone(),
                    sender_provider: *sender_provider,
                },
            ),
        };
        entries.push(CombinedEntry {
            id: tm.id,
            created_at: tm.created_at,
            kind_rank,
            order_key: (0, tm.seq, 0),
            kind,
            text: tm.content_text.clone(),
        });
    }
    for (member_idx, sid) in vm.member_ids.iter().enumerate() {
        let Some(member) = members.get(sid) else {
            continue;
        };
        let handle = member
            .session
            .agent_handle
            .as_deref()
            .unwrap_or("agent")
            .to_string();
        let default_provider = member.session.provider_id;
        for msg in &member.messages {
            if msg.role != Role::Assistant {
                continue;
            }
            let provider = msg.provider_id.unwrap_or(default_provider);
            let segs = split_broadcast_segments(&msg.content_text);
            for (seg_idx, seg) in segs.into_iter().enumerate() {
                if let TextSegment::Plain(body) = seg {
                    if body.trim().is_empty() {
                        continue;
                    }
                    let id = uuid::Uuid::from_u128(msg.id.as_u128().wrapping_add(seg_idx as u128));
                    entries.push(CombinedEntry {
                        id,
                        created_at: msg.created_at,
                        kind_rank: 1,
                        order_key: (member_idx, msg.seq, seg_idx),
                        kind: CombinedKind::MemberReply {
                            handle: handle.clone(),
                            provider,
                        },
                        text: body,
                    });
                }
            }
        }
    }
    entries.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.kind_rank.cmp(&b.kind_rank))
            .then_with(|| a.order_key.cmp(&b.order_key))
    });
    entries
}

fn team_feed(
    ui: &mut egui::Ui,
    vm: &mut MultiSessionVm,
    members: &HashMap<SessionId, SessionVm>,
    handles: &[String],
) -> Option<PathBuf> {
    let entries = collect_combined_feed(vm, members);
    let any_live = vm.member_ids.iter().any(|sid| {
        members
            .get(sid)
            .is_some_and(|m| !m.live_text.is_empty() || m.thinking_tokens.is_some())
    });
    let working_clis = working::clis(
        vm.member_ids
            .iter()
            .filter_map(|id| members.get(id))
            .map(|m| (m.session.provider_id, &m.status)),
    );
    let mut open_file = None;
    let feed_id = ("team_feed", vm.multi.id);
    let nav_id = ui.make_persistent_id(feed_id);
    let nav = scroll_nav::take(ui.ctx(), nav_id);
    let out = nav.area(feed_id).show(ui, |ui| {
            let content_top = ui.cursor().min;
            egui::Frame::new()
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if entries.is_empty() && !any_live {
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
                    for entry in &entries {
                        let clicked = combined_entry_row(
                            ui,
                            &mut vm.md_cache,
                            &mut vm.path_links,
                            &vm.cwd,
                            entry,
                        );
                        if open_file.is_none() {
                            open_file = clicked;
                        }
                        ui.add_space(8.0);
                    }
                    // Show live streaming output or thinking state for busy members at the bottom.
                    for sid in &vm.member_ids {
                        let Some(member) = members.get(sid) else {
                            continue;
                        };
                        if member.live_text.is_empty() && member.thinking_tokens.is_none() {
                            continue;
                        }
                        let handle = member.session.agent_handle.as_deref().unwrap_or("agent");
                        let provider = member.session.provider_id;
                        ui.horizontal_top(|ui| {
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(20.0, 20.0),
                                egui::Sense::hover(),
                            );
                            if let Some(tex) = conversation::cli_symbol(ui.ctx(), provider) {
                                egui::Image::new(&tex)
                                    .tint(conversation::symbol_tint(provider))
                                    .paint_at(ui, rect);
                            }
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(format!("#{handle}"))
                                            .small()
                                            .strong()
                                            .color(theme::text()),
                                    );
                                    ui.label(
                                        RichText::new("responding…")
                                            .small()
                                            .color(theme::text_muted()),
                                    );
                                });
                                if !member.live_text.is_empty() {
                                    markdown(
                                        ui,
                                        &mut vm.md_cache,
                                        ("team_live", *sid),
                                        &member.live_text,
                                    );
                                } else if let Some(n) = member.thinking_tokens {
                                    ui.horizontal(|ui| {
                                        ui.add(egui::Spinner::new().size(12.0));
                                        ui.label(
                                            RichText::new(format!("Thinking… ~{n} tokens"))
                                                .color(theme::text_muted()),
                                        );
                                    });
                                }
                            });
                        });
                        ui.add_space(8.0);
                    }
                    working::show(ui, &working_clis);
                });
            nav.jump_top(ui, content_top);
            if std::mem::take(&mut vm.scroll_to_end) || nav.bottom {
                // Unanimated: output arriving mid-animation would end `stick_to_bottom`.
                ui.scroll_to_cursor_animation(
                    Some(egui::Align::BOTTOM),
                    egui::style::ScrollAnimation::none(),
                );
            }
        });
    scroll_nav::show(ui, &out, nav_id);
    open_file
}

fn combined_entry_row(
    ui: &mut egui::Ui,
    cache: &mut CommonMarkCache,
    links: &mut PathLinks,
    cwd: &std::path::Path,
    entry: &CombinedEntry,
) -> Option<PathBuf> {
    match &entry.kind {
        CombinedKind::User { target } => conversation::bubble(
            ui,
            conversation::Tail::Right,
            theme::bg_raised(),
            theme::border_light(),
            |ui| {
                let to = target
                    .as_deref()
                    .map_or_else(|| "To: All".to_string(), |h| format!("To: #{h}"));
                ui.label(RichText::new(to).small().strong().color(theme::accent()));
                links.markdown(ui, cache, cwd, entry.id, &entry.text)
            },
        ),
        CombinedKind::MemberReply { handle, provider } => {
            ui.horizontal_top(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
                if let Some(tex) = conversation::cli_symbol(ui.ctx(), *provider) {
                    egui::Image::new(&tex)
                        .tint(conversation::symbol_tint(*provider))
                        .paint_at(ui, rect);
                }
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(format!("#{handle}"))
                            .small()
                            .strong()
                            .color(theme::text()),
                    );
                    links.markdown(ui, cache, cwd, entry.id, &entry.text)
                })
                .inner
            })
            .inner
        }
        CombinedKind::Broadcast {
            sender_handle,
            sender_provider,
        } => {
            // Subtle bluish bubble on the left, so model-to-model messages stand out from the
            // user's (right) while blending with the window background.
            conversation::bubble(
                ui,
                conversation::Tail::Left,
                theme::broadcast_bg(),
                theme::broadcast_border(),
                |ui| {
                    ui.horizontal(|ui| {
                        if let Some(tex) = conversation::cli_symbol(ui.ctx(), *sender_provider) {
                            ui.add(
                                egui::Image::new(&tex)
                                    .tint(conversation::symbol_tint(*sender_provider))
                                    .fit_to_exact_size(egui::vec2(14.0, 14.0)),
                            );
                        }
                        ui.label(
                            RichText::new(format!("#{sender_handle} \u{2192} Team"))
                                .small()
                                .strong()
                                .color(theme::text()),
                        );
                        egui::Frame::new()
                            .fill(theme::bg_app())
                            .stroke(egui::Stroke::new(1.0, theme::broadcast_border()))
                            .inner_margin(egui::Margin::symmetric(5, 1))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new("Model-to-model")
                                        .small()
                                        .color(theme::text_muted()),
                                );
                            });
                    });
                    ui.add_space(4.0);
                    links.markdown(ui, cache, cwd, entry.id, &entry.text)
                },
            )
        }
    }
}

fn unified_composer(
    ui: &mut egui::Ui,
    vm: &mut MultiSessionVm,
    handles: &[String],
    members: &HashMap<SessionId, SessionVm>,
    any_busy: bool,
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
        ui.input_mut(|i| keymap::take_send(i, env.send_on_enter))
    } else {
        None
    };
    let hotkey_other = pressed == Some(Action::SendOther);
    let mut do_send = pressed.is_some();
    let mut steer = match env.send_mode {
        SendMode::Steer => !hotkey_other,
        SendMode::Queue => hotkey_other,
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
        "Message all models or #handle: ({} to {})",
        keymap::send_hint_key(env.send_on_enter),
        if target_busy {
            "queue or steer"
        } else {
            "send"
        }
    );
    let has_body = !parse_user_target(&vm.input, handles).1.is_empty();
    let can_send = has_body && target_alive;

    // Input box matching standalone `composer.rs`: same min/max rows, frame, and right-aligned
    // 48x48 Send button + Stop All button.
    let input_box = egui::Frame::new()
        .fill(theme::bg_hover())
        .stroke(egui::Stroke::new(1.5, theme::border_strong()))
        .inner_margin(egui::Margin::same(6))
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let w = (ui.available_width() - if any_busy { 176.0 } else { 64.0 }).max(40.0);
                let max_h = INPUT_MAX_ROWS as f32 * ui.text_style_height(&egui::TextStyle::Body);
                let input_h = ui
                    .allocate_ui(egui::vec2(w, max_h), |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt(("multi_input_scroll", vm.multi.id))
                            .max_height(max_h)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                if ui
                                    .add(
                                        egui::TextEdit::multiline(&mut vm.input)
                                            .id(edit_id)
                                            .frame(egui::Frame::NONE)
                                            .desired_width(f32::INFINITY)
                                            .desired_rows(INPUT_MIN_ROWS)
                                            .layouter(&mut layouter)
                                            .hint_text(hint),
                                    )
                                    .changed()
                                {
                                    vm.handle_popup_dismissed = false;
                                }
                            });
                    })
                    .response
                    .rect
                    .height();
                let pad = ((input_h - BUTTON_SIZE) / 2.0).max(0.0);
                ui.vertical(|ui| {
                    ui.add_space(pad);
                    ui.horizontal(|ui| {
                        let btn = egui::Button::new(
                            RichText::new(icons::SEND).size(26.0).color(theme::text()),
                        )
                        .fill(theme::accent())
                        .min_size(egui::vec2(BUTTON_SIZE, BUTTON_SIZE));
                        let mut resp = ui.add_enabled(can_send, btn).on_disabled_hover_text(
                            if !target_alive {
                                "Target model is not running"
                            } else {
                                "Type a message to send"
                            },
                        );
                        if target_busy {
                            resp = resp.on_hover_text(match env.send_mode {
                                SendMode::Queue => format!(
                                    "Queue: sent when the turn ends ({} to steer)",
                                    keymap::shortcut_text(Action::SendOther)
                                ),
                                SendMode::Steer => format!(
                                    "Steer: sent into the running turn ({} to queue)",
                                    keymap::shortcut_text(Action::SendOther)
                                ),
                            });
                        }
                        if resp.clicked() {
                            do_send = true;
                            steer = env.send_mode == SendMode::Steer;
                        }
                        if any_busy {
                            let stop = egui::Button::new(
                                RichText::new(icons::with(icons::STOP, "Stop All"))
                                    .color(theme::text()),
                            )
                            .fill(theme::danger())
                            .min_size(egui::vec2(0.0, BUTTON_SIZE));
                            if ui
                                .add(stop)
                                .on_hover_text(
                                    "Stop all running models and clear queued broadcasts",
                                )
                                .clicked()
                            {
                                let _ = env.cmd_tx.send(ManagerCmd::InterruptMultiAll {
                                    multi_id: vm.multi.id,
                                });
                            }
                        }
                    });
                });
            });
        });

    // Quick target pills sit directly under the input box (same row position as standalone
    // composer controls). A member tab adds that session's stats; the Team tab has none.
    let tab_usage = match vm.active_tab {
        MultiTab::Team => None,
        MultiTab::Member(sid) => members.get(&sid).map(|m| &m.session.usage),
    };
    let box_width = input_box.response.rect.width();
    ui.add_space(4.0);
    composer::controls_row(ui, box_width, |ui| {
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
                    .add_enabled(
                        can_send,
                        egui::Button::new(RichText::new(alt_label).small()),
                    )
                    .on_hover_text(alt_tip)
                    .clicked()
                {
                    do_send = true;
                    steer = alt_steer;
                }
            }
            if let Some(usage) = tab_usage {
                composer::session_stats(ui, usage);
            }
        });
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
#[allow(clippy::unwrap_used)]
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

    /// Every text painted by the window on `tab`, with where it landed.
    fn painted(tab: MultiTab, member: SessionVm) -> Vec<(String, egui::Rect)> {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let env = Env {
            cmd_tx: &tx,
            providers: &[],
            fork_targets: &[],
            vscode: None,
            maximized: None,
            send_mode: SendMode::Queue,
            send_on_enter: false,
            break_all: true,
            ide_context: None,
            focused: false,
        };
        let sid = member.session.id;
        let now = member.session.created_at;
        let multi = MultiSession {
            id: xade_core::MultiSessionId::new(),
            workspace_id: member.session.workspace_id,
            title: "t".into(),
            preview_snippet: String::new(),
            created_at: now,
            updated_at: now,
        };
        let mut vm = MultiSessionVm::new(multi, "/tmp".into(), vec![sid], Vec::new());
        if tab != MultiTab::Team {
            vm.active_tab = MultiTab::Member(sid);
        }
        let mut members = HashMap::from([(sid, member)]);
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(700.0, 600.0),
            )),
            ..Default::default()
        };
        for _ in 0..2 {
            ctx.run_ui(input(), |ui| {
                show(ui, &mut vm, &mut members, &env, false);
            })
            .drop_without_applying_deltas();
        }
        let mut out = ctx.run_ui(input(), |ui| {
            show(ui, &mut vm, &mut members, &env, false);
        });
        out.textures_delta.clear();
        fn walk(s: &egui::Shape, t: &mut Vec<(String, egui::Rect)>) {
            match s {
                egui::Shape::Text(x) => {
                    t.push((x.galley.text().to_string(), s.visual_bounding_rect()))
                }
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, t)),
                _ => {}
            }
        }
        let mut texts = Vec::new();
        out.shapes.iter().for_each(|c| walk(&c.shape, &mut texts));
        texts
    }

    #[test]
    fn only_a_member_tab_shows_usage_and_the_expired_cache_note() {
        let member = || {
            let mut m = crate::composer::test_support::vm(String::new());
            m.session.agent_handle = Some("opus-1".into());
            m.session.updated_at -= time::Duration::hours(2);
            m.session.usage.context_tokens_used = 42_000;
            m.session.usage.input_tokens = 1_200;
            m
        };
        let find = |texts: &[(String, egui::Rect)], s: &str| {
            texts.iter().find(|(t, _)| t.contains(s)).map(|(_, r)| *r)
        };

        let texts = painted(MultiTab::Member(SessionId::new()), member());
        assert!(find(&texts, "Cache expired").is_some(), "{texts:?}");
        let stats = find(&texts, "in 1.2k").unwrap();
        let to = find(&texts, "To:").unwrap();
        assert!(
            stats.y_range().intersects(to.y_range()) && stats.left() > to.right(),
            "stats sit right of the target pills: {stats:?} {to:?}"
        );

        let texts = painted(MultiTab::Team, member());
        assert!(find(&texts, "Cache expired").is_none(), "{texts:?}");
        assert!(find(&texts, "in 1.2k").is_none(), "{texts:?}");
    }
}
