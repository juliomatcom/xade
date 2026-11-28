//! The scrolling transcript of one session.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use egui::{Color32, RichText};
use egui_commonmark::CommonMarkCache;
use xade_core::{Delivery, Message, ProviderId, Role, SessionId, SessionStatus};

use crate::app::SessionVm;
use crate::theme;
use crate::widgets::changes_summary;
use crate::widgets::markdown::markdown;
use crate::widgets::path_links::PathLinks;
use crate::widgets::scroll_nav;
use crate::widgets::tool_card::{self, CardStatus};
use crate::widgets::working;

/// Draws the transcript; returns a file path the user clicked.
pub fn show(ui: &mut egui::Ui, vm: &mut SessionVm) -> Option<PathBuf> {
    let mark_target = match vm.session.multi_session_id {
        Some(mid) => crate::workspace::WindowKind::MultiSession(mid),
        None => crate::workspace::WindowKind::Session(vm.session.id),
    };
    let find_mark = crate::find::mark(ui.ctx(), mark_target);
    let session_id = vm.session.id;
    let provider = vm.session.provider_id;
    let vm_multi = vm.session.multi_session_id.is_some();
    let mut review = None;
    let mut open = None;
    let mut changes_review = None;
    let mut turn_review: Option<(uuid::Uuid, usize)> = None;
    let SessionVm {
        cwd,
        messages,
        md_cache,
        path_links,
        changes,
        turn_files,
        live_text,
        thinking_tokens,
        status,
        pending_permission,
        error,
        scroll_to_end,
        ..
    } = &mut *vm;
    let results: HashMap<&str, &Message> = messages
        .iter()
        .filter(|m| m.role == Role::ToolResult)
        .filter_map(|m| Some((m.tool_use_id.as_deref()?, m)))
        .collect();
    let busy = matches!(
        status,
        SessionStatus::Streaming | SessionStatus::NeedsApproval
    );
    let runs = tool_runs(messages);
    // Scrolls to the current match once per step.
    let jump = find_mark
        .as_ref()
        .is_some_and(|m| crate::find::take_jump(ui, m));
    // A call's result and how its card reads.
    let outcome = |m: &Message| {
        let result = m
            .tool_use_id
            .as_deref()
            .and_then(|t| results.get(t))
            .copied();
        let status = match result {
            Some(r) if r.is_error => CardStatus::Error,
            Some(_) => CardStatus::Ok,
            None if busy => CardStatus::Running,
            None => CardStatus::Unknown,
        };
        (result, status)
    };
    let scroll_id = ("conversation", session_id);
    let nav_id = ui.make_persistent_id(scroll_id);
    let nav = scroll_nav::take(ui.ctx(), nav_id);
    let out = nav.area(scroll_id).show(ui, |ui| {
        let content_top = ui.cursor().min;
        egui::Frame::new()
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let mut turn_start = 0;
                for (i, m) in messages.iter().enumerate() {
                    if m.role == Role::User {
                        turn_start = i;
                    }
                    let run = runs.iter().find(|(a, b)| (*a..=*b).contains(&i));
                    // A group is drawn once, with its first call.
                    let draw = run.is_none_or(|(a, _)| *a == i);
                    if draw {
                        let shapes_before = ui
                            .ctx()
                            .graphics(|g| g.get(ui.layer_id()).map_or(0, |l| l.next_idx().0));
                        let rect = row(ui, |ui| match m.role {
                            _ if run.is_some() => {
                                let (a, b) = *run.unwrap_or(&(i, i));
                                let calls: Vec<tool_card::GroupCall> = messages[a..=b]
                                    .iter()
                                    .filter(|c| c.role == Role::ToolCall)
                                    .map(|call| {
                                        let (result, status) = outcome(call);
                                        tool_card::GroupCall {
                                            call,
                                            result,
                                            status,
                                        }
                                    })
                                    .collect();
                                tool_card::group(ui, md_cache, &calls);
                            }
                            Role::User => {
                                let clicked = with_time(ui, m, |ui| {
                                    user_bubble(ui, md_cache, path_links, cwd, m)
                                });
                                open = open.take().or(clicked);
                            }
                            Role::Assistant => {
                                let id = m.id;
                                let text = &m.content_text;
                                let is_multi = vm_multi;
                                let clicked = with_time(ui, m, |ui| {
                                    assistant(ui, avatar_provider(m, provider), |ui| {
                                        if is_multi {
                                            render_multi_assistant(
                                                ui, md_cache, path_links, cwd, id, text,
                                            )
                                        } else {
                                            path_links.markdown(ui, md_cache, cwd, id, text)
                                        }
                                    })
                                });
                                open = open.take().or(clicked);
                            }
                            Role::ToolCall => {
                                let (result, card_status) = outcome(m);
                                let awaiting = pending_permission.as_ref().is_some_and(|p| {
                                    m.tool_use_id.as_deref() == Some(&*p.tool_use_id)
                                });
                                if tool_card::show(ui, md_cache, m, result, card_status, awaiting) {
                                    review = m.tool_use_id.clone();
                                }
                            }
                            Role::System => {
                                ui.label(RichText::new(&m.content_text).color(theme::text_muted()));
                            }
                            Role::ToolResult => {}
                        });
                        if let Some(mark) = &find_mark {
                            // Only the current message is tinted, not every one that matches.
                            if mark.matches.get(mark.current) == Some(&i) {
                                ui.painter().rect_filled(
                                    rect.expand(3.0),
                                    0.0,
                                    theme::find_block(),
                                );
                                let nth = mark.current - mark.matches.partition_point(|&x| x < i);
                                crate::find::paint_message_words(
                                    ui,
                                    shapes_before,
                                    &mark.query,
                                    nth,
                                );
                                if jump {
                                    ui.scroll_to_rect(rect, Some(egui::Align::Center));
                                }
                            }
                        }
                    }
                    // Results draw nothing; cards of one run sit close together.
                    let next_call = messages[i + 1..]
                        .iter()
                        .find(|n| n.role != Role::ToolResult)
                        .is_some_and(|n| n.role == Role::ToolCall);
                    let end = run.map_or(i, |(_, b)| *b);
                    if i == end && (run.is_some() || m.role != Role::ToolResult) {
                        ui.add_space(if m.role == Role::ToolCall && next_call {
                            2.0
                        } else {
                            6.0
                        });
                    }
                    // A finished turn ends before the next user message, or at the last one once idle.
                    let ends = messages.get(i + 1).map_or(!busy, |n| n.role == Role::User);
                    if ends {
                        // What the working tree shows (Bash edits included) beats what the
                        // edit tools reported; the latter is all a reopened session has.
                        let tree = turn_files.get(&m.id).filter(|c| !c.files.is_empty());
                        if let Some(c) = tree {
                            let list: Vec<changes_summary::FileChange> = c
                                .files
                                .iter()
                                .map(changes_summary::FileChange::from)
                                .collect();
                            let source = changes_summary::Source::Tree;
                            row(ui, |ui| {
                                if let Some(n) = changes_summary::show(ui, m.id, cwd, &list, source)
                                {
                                    turn_review = Some((m.id, n));
                                }
                            });
                            ui.add_space(6.0);
                        } else {
                            let files = changes.get(&messages[turn_start..=i], cwd);
                            if !files.is_empty() {
                                row(ui, |ui| {
                                    if let Some(n) = changes_summary::show(
                                        ui,
                                        m.id,
                                        cwd,
                                        files,
                                        changes_summary::Source::Edits,
                                    ) {
                                        changes_review = Some(files[n].calls.clone());
                                    }
                                });
                                ui.add_space(6.0);
                            }
                        }
                    }
                }
                if !live_text.is_empty() {
                    assistant(ui, provider, |ui| {
                        markdown(ui, md_cache, "live", live_text);
                        None
                    });
                }
                if let (Some(n), true) = (*thinking_tokens, live_text.is_empty()) {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(12.0));
                        ui.label(
                            RichText::new(format!("Thinking… ~{n} tokens"))
                                .color(theme::text_muted()),
                        );
                    });
                }
                // When the model last answered; from the stored timestamp, so it survives a
                // restart. Hidden while a turn runs.
                let last_reply = messages.iter().rev().find(|m| m.role == Role::Assistant);
                if let (Some(m), false) = (last_reply, busy) {
                    ui.label(
                        RichText::new(format!(
                            "Last reply {}",
                            crate::home::relative_time(m.created_at)
                        ))
                        .small()
                        .color(theme::text_muted()),
                    )
                    .on_hover_text(m.created_at.to_string());
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_secs(30));
                }
                if let Some(e) = error {
                    error_block(ui, session_id, e);
                }
                working::show(ui, &working::clis([(provider, &*status)]));
            });
        // After a send: land on the end, which also re-arms `stick_to_bottom`.
        nav.jump_top(ui, content_top);
        if std::mem::take(scroll_to_end) || nav.bottom {
            ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
        }
    });
    scroll_nav::show(ui, &out, nav_id);
    if let Some(id) = review {
        vm.open_diff(&id);
    }
    if let Some((anchor, n)) = turn_review {
        vm.open_turn_file(anchor, n);
    }
    if let Some(calls) = changes_review {
        vm.open_changes(&calls);
    }
    open
}

const ERROR_SUMMARY_CHARS: usize = 160;
const ERROR_DETAILS_MAX_HEIGHT: f32 = 160.0;

/// Single-line summary of an error message, suitable for chat lines and hover tooltips.
pub(crate) fn error_summary(msg: &str) -> Cow<'_, str> {
    split_error(msg).0
}

/// Splits an error into its single-line summary and any remaining detail (from extra lines, or the
/// full message when a single line exceeds `ERROR_SUMMARY_CHARS`).
fn split_error(msg: &str) -> (Cow<'_, str>, Option<&str>) {
    let trimmed = msg.trim();
    if let Some((first, rest)) = trimmed.split_once('\n') {
        let first = first.trim_end();
        let rest = rest.trim();
        let details = (!rest.is_empty()).then_some(rest);
        if first.chars().count() > ERROR_SUMMARY_CHARS {
            let cut: String = first.chars().take(ERROR_SUMMARY_CHARS).collect();
            return (Cow::Owned(format!("{cut}…")), Some(trimmed));
        }
        return (Cow::Borrowed(first), details);
    }
    if trimmed.chars().count() > ERROR_SUMMARY_CHARS {
        let cut: String = trimmed.chars().take(ERROR_SUMMARY_CHARS).collect();
        return (Cow::Owned(format!("{cut}…")), Some(trimmed));
    }
    (Cow::Borrowed(trimmed), None)
}

/// Renders a session error at the bottom of the transcript: always one concise line, with any
/// multi-line or long detail collapsed under an inline `Details` toggle.
fn error_block(ui: &mut egui::Ui, session_id: SessionId, msg: &str) {
    let (summary, details) = split_error(msg);
    ui.label(RichText::new(summary.as_ref()).color(theme::danger()));
    let Some(details) = details else {
        return;
    };
    let id = egui::Id::unique(("session_error_details", session_id));
    let state =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
    state
        .show_header(ui, |ui| {
            ui.label(RichText::new("Details").small().color(theme::text_muted()));
        })
        .body(|ui| {
            egui::Frame::new()
                .fill(theme::bg_terminal())
                .stroke(egui::Stroke::new(1.0, theme::border()))
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical()
                        .id_salt((id, "scroll"))
                        .max_height(ERROR_DETAILS_MAX_HEIGHT)
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(details)
                                    .small()
                                    .monospace()
                                    .color(theme::text_muted()),
                            );
                        });
                });
        });
}

/// Runs of two or more consecutive tool calls other than code changes (their results
/// interleaved), as `(first, last)` message indexes. Anything else ends a run: code changes keep
/// their own review card.
fn tool_runs(messages: &[Message]) -> Vec<(usize, usize)> {
    let groupable = |m: &Message| {
        m.role == Role::ToolCall
            && !crate::diff::is_code_change(m.tool_name.as_deref().unwrap_or(""))
    };
    let mut runs = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        if !groupable(&messages[i]) {
            i += 1;
            continue;
        }
        let (start, mut end, mut calls) = (i, i, 0);
        while i < messages.len()
            && (groupable(&messages[i]) || messages[i].role == Role::ToolResult)
        {
            if groupable(&messages[i]) {
                calls += 1;
            }
            end = i;
            i += 1;
        }
        if calls >= 2 {
            runs.push((start, end));
        }
    }
    runs
}

/// One transcript row in its own scope, so its allocated rect is observable in tests.
fn row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) -> egui::Rect {
    #[cfg(test)]
    let painted = probe::start(ui);
    let rect = ui.scope(add).response.rect;
    #[cfg(test)]
    probe::finish(ui, painted, rect);
    rect
}

/// The clock time and the full date of a message, in the machine's timezone. `tz` is a
/// parameter so tests do not depend on the machine's.
fn time_labels<Tz: chrono::TimeZone>(at: time::OffsetDateTime, tz: &Tz) -> Option<(String, String)>
where
    Tz::Offset: std::fmt::Display,
{
    let local = chrono::DateTime::from_timestamp(at.unix_timestamp(), 0)?.with_timezone(tz);
    Some((
        local.format("%H:%M").to_string(),
        local.format("%a %-d %b %Y, %H:%M:%S").to_string(),
    ))
}

/// A message's time, small and right-aligned on its own line above the message; hovering it
/// shows the date as well. Then `body` draws the message in the full width.
fn with_time<R>(ui: &mut egui::Ui, m: &Message, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    if let Some((clock, full)) = time_labels(m.created_at, &chrono::Local) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            ui.add(egui::Label::new(
                RichText::new(clock).small().color(theme::text_muted()),
            ))
            .on_hover_text(full);
        });
    }
    body(ui)
}

/// User messages longer than this many lines show a head and a "Show all" toggle.
const USER_PREVIEW_LINES: usize = 2;

/// Corner radius of a chat bubble, the one rounded shape in the square UI.
const BUBBLE_RADIUS: u8 = 8;
/// Width and height of the bubble's tail.
const BUBBLE_TAIL: egui::Vec2 = egui::vec2(8.0, 10.0);
/// Share of the width left empty on the side opposite the tail.
const BUBBLE_INDENT: f32 = 0.15;

/// Which edge of a bubble carries its tail, and so which side of the chat it sits on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tail {
    Left,
    Right,
}

/// Draws the wedge that makes a bubble read as speech: from its sharp top corner out to the
/// side and back down the edge, filled like the bubble with its border on the outer sides.
fn paint_bubble_tail(
    ui: &egui::Ui,
    bubble: egui::Rect,
    tail: Tail,
    fill: Color32,
    border: Color32,
) {
    let (edge, out) = match tail {
        Tail::Right => (bubble.right(), 1.0),
        Tail::Left => (bubble.left(), -1.0),
    };
    let top = bubble.top();
    let inner = edge - out;
    let base = egui::pos2(edge, top + BUBBLE_TAIL.y);
    let tip = egui::pos2(edge + out * BUBBLE_TAIL.x, top + 0.5);
    let painter = ui.painter();
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(inner, top + 0.5),
            tip,
            base,
            egui::pos2(inner, base.y),
        ],
        fill,
        egui::Stroke::NONE,
    ));
    // Open the bubble's own border where the tail joins it, then stroke the tail's two sides.
    painter.rect_filled(
        egui::Rect::from_two_pos(
            egui::pos2(inner, top + 1.0),
            egui::pos2(edge + out * 0.5, base.y - 0.5),
        ),
        0.0,
        fill,
    );
    painter.add(egui::Shape::line(
        vec![egui::pos2(inner, top + 0.5), tip, base],
        egui::Stroke::new(1.0, border),
    ));
}

/// A chat bubble shared by single and multi-session chats: rounded, with a tail on one edge and
/// an empty margin on the other. `body` draws the content, stacked vertically.
pub(crate) fn bubble<R>(
    ui: &mut egui::Ui,
    tail: Tail,
    fill: Color32,
    border: Color32,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let indent = ui.available_width() * BUBBLE_INDENT;
    // The corner the tail continues from stays sharp.
    let (nw, ne) = match tail {
        Tail::Right => (BUBBLE_RADIUS, 0),
        Tail::Left => (0, BUBBLE_RADIUS),
    };
    ui.horizontal_top(|ui| {
        ui.add_space(match tail {
            Tail::Right => indent,
            Tail::Left => BUBBLE_TAIL.x,
        });
        let framed = egui::Frame::new()
            .fill(fill)
            .stroke(egui::Stroke::new(1.0, border))
            .corner_radius(egui::CornerRadius {
                nw,
                ne,
                sw: BUBBLE_RADIUS,
                se: BUBBLE_RADIUS,
            })
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                let room = match tail {
                    Tail::Right => BUBBLE_TAIL.x,
                    Tail::Left => indent,
                };
                ui.set_width((ui.available_width() - room).max(0.0));
                // The row is a horizontal layout; the bubble's own content stacks vertically.
                ui.vertical(body).inner
            });
        paint_bubble_tail(ui, framed.response.rect, tail, fill, border);
        framed.inner
    })
    .inner
}

fn user_bubble(
    ui: &mut egui::Ui,
    cache: &mut CommonMarkCache,
    links: &mut PathLinks,
    cwd: &Path,
    m: &Message,
) -> Option<PathBuf> {
    let total = m.content_text.lines().count();
    let long = total > USER_PREVIEW_LINES;
    let open_id = egui::Id::unique(("user_expanded", m.id));
    let open = long && ui.data(|d| d.get_temp::<bool>(open_id)).unwrap_or(false);
    bubble(
        ui,
        Tail::Right,
        theme::bg_raised(),
        theme::border_light(),
        |ui| {
            if let Some(sender) = &m.sender_handle {
                ui.label(
                    RichText::new(format!("Broadcast from #{sender}"))
                        .small()
                        .strong()
                        .color(theme::accent()),
                );
            }
            if let Some(d) = Delivery::of(m) {
                let (label, color) = match d {
                    Delivery::Queued => ("Queued", theme::text_muted()),
                    Delivery::Steered => ("Steering", theme::warn()),
                };
                ui.label(RichText::new(label).small().strong().color(color));
            }
            let shown = if long && !open {
                let cut = m
                    .content_text
                    .match_indices('\n')
                    .nth(USER_PREVIEW_LINES - 1)
                    .map_or(m.content_text.len(), |(i, _)| i);
                &m.content_text[..cut]
            } else {
                m.content_text.as_str()
            };
            let clicked = links.markdown(ui, cache, cwd, m.id, shown);
            if long {
                let label = if open {
                    "Show less".to_string()
                } else {
                    format!("Show all ({total} lines)")
                };
                if ui.small_button(label).clicked() {
                    ui.data_mut(|d| d.insert_temp(open_id, !open));
                }
            }
            clicked
        },
    )
}

/// Renders an assistant message in a multi-session member tab, framing `<broadcast>` segments
/// distinctly from user-only plain text.
fn render_multi_assistant(
    ui: &mut egui::Ui,
    cache: &mut CommonMarkCache,
    links: &mut PathLinks,
    cwd: &Path,
    id: uuid::Uuid,
    text: &str,
) -> Option<PathBuf> {
    let segs = xade_core::split_broadcast_segments(text);
    if segs.is_empty() {
        return links.markdown(ui, cache, cwd, id, text);
    }
    let mut clicked = None;
    for (idx, seg) in segs.iter().enumerate() {
        let seg_id = uuid::Uuid::from_u128(id.as_u128().wrapping_add(idx as u128));
        let seg_clicked = match seg {
            xade_core::TextSegment::Plain(body) => links.markdown(ui, cache, cwd, seg_id, body),
            xade_core::TextSegment::Broadcast(body) => {
                egui::Frame::new()
                    .fill(theme::broadcast_bg())
                    .stroke(egui::Stroke::new(1.0, theme::broadcast_border()))
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new("Broadcast to team")
                                .small()
                                .strong()
                                .color(theme::text_muted()),
                        );
                        links.markdown(ui, cache, cwd, seg_id, body)
                    })
                    .inner
            }
        };
        if clicked.is_none() {
            clicked = seg_clicked;
        }
        if idx + 1 < segs.len() {
            ui.add_space(4.0);
        }
    }
    clicked
}

/// Whose avatar an assistant message gets: its own provider (copied fork history), else the
/// session's.
fn avatar_provider(m: &Message, session: ProviderId) -> ProviderId {
    m.provider_id.unwrap_or(session)
}

/// Side of the CLI mark beside a message.
pub(crate) const AVATAR: f32 = 20.0;

/// The assistant avatar beside `body`, which draws the text.
fn assistant(
    ui: &mut egui::Ui,
    provider: ProviderId,
    body: impl FnOnce(&mut egui::Ui) -> Option<PathBuf>,
) -> Option<PathBuf> {
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(AVATAR), egui::Sense::hover());
        if let Some(tex) = cli_symbol(ui.ctx(), provider) {
            egui::Image::new(&tex)
                .tint(symbol_tint(provider))
                .paint_at(ui, rect);
        }
        ui.vertical(body).inner
    })
    .inner
}

// 80x80 PNGs (4x the avatar) pre-rendered from the unmodified SVG beside each in `assets/`; the
// marks are their owners' trademarks (sources: wiki-llm/ui.md).
const ANTHROPIC_SYMBOL: &[u8] = include_bytes!("../../../assets/anthropic-symbol.png");
const GOOGLE_SYMBOL: &[u8] = include_bytes!("../../../assets/google-symbol.png");
const CLAUDE_CODE_SYMBOL: &[u8] = include_bytes!("../../../assets/claude-code-symbol.png");
const ANTIGRAVITY_SYMBOL: &[u8] = include_bytes!("../../../assets/antigravity-symbol.png");
const OPENAI_SYMBOL: &[u8] = include_bytes!("../../../assets/openai-symbol.png");
const ANOMALY_SYMBOL: &[u8] = include_bytes!("../../../assets/anomaly-symbol.png");
const OPENCODE_SYMBOL: &[u8] = include_bytes!("../../../assets/opencode-symbol.png");

/// Tint applied when drawing a provider's symbol: the OpenAI PNG is white-on-transparent so
/// `theme::text()` keeps it legible on both dark and light themes.
pub(crate) fn symbol_tint(provider: ProviderId) -> egui::Color32 {
    match provider {
        ProviderId::Codex => theme::text(),
        ProviderId::Claude | ProviderId::Antigravity | ProviderId::OpenCode => egui::Color32::WHITE,
    }
}

/// The logo of the company behind a provider, for window headers and the top bar.
pub(crate) fn company_symbol(
    ctx: &egui::Context,
    provider: ProviderId,
) -> Option<egui::TextureHandle> {
    let (name, png) = match provider {
        ProviderId::Claude => ("anthropic_symbol", ANTHROPIC_SYMBOL),
        ProviderId::Antigravity => ("google_symbol", GOOGLE_SYMBOL),
        ProviderId::Codex => ("openai_symbol", OPENAI_SYMBOL),
        ProviderId::OpenCode => ("anomaly_symbol", ANOMALY_SYMBOL),
    };
    symbol(ctx, name, png)
}

/// The logo of a provider's CLI, the avatar of its messages.
pub(crate) fn cli_symbol(ctx: &egui::Context, provider: ProviderId) -> Option<egui::TextureHandle> {
    let (name, png) = match provider {
        ProviderId::Claude => ("claude_code_symbol", CLAUDE_CODE_SYMBOL),
        ProviderId::Antigravity => ("antigravity_symbol", ANTIGRAVITY_SYMBOL),
        ProviderId::Codex => ("openai_symbol", OPENAI_SYMBOL),
        ProviderId::OpenCode => ("opencode_symbol", OPENCODE_SYMBOL),
    };
    symbol(ctx, name, png)
}

/// A symbol as a texture, decoded and uploaded on first use.
fn symbol(ctx: &egui::Context, name: &'static str, png: &[u8]) -> Option<egui::TextureHandle> {
    let id = egui::Id::unique(("symbol_texture", name));
    if let Some(tex) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(tex);
    }
    let icon = eframe::icon_data::from_png_bytes(png)
        .map_err(|e| tracing::warn!("{name} not loaded: {e}"))
        .ok()?;
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [icon.width as usize, icon.height as usize],
        &icon.rgba,
    );
    let tex = ctx.load_texture(name, image, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    Some(tex)
}

#[cfg(test)]
mod symbol_tests {
    use xade_core::ProviderId;

    #[test]
    fn every_company_and_cli_symbol_decodes_into_its_own_cached_texture() {
        let ctx = egui::Context::default();
        let mut ids = std::collections::HashSet::new();
        for p in [
            ProviderId::Claude,
            ProviderId::Antigravity,
            ProviderId::Codex,
            ProviderId::OpenCode,
        ] {
            for load in [super::company_symbol, super::cli_symbol] {
                let tex = load(&ctx, p).expect("symbol");
                assert_eq!(tex.size(), [80, 80]);
                let again = load(&ctx, p).expect("cached second call");
                assert_eq!(tex.id(), again.id(), "cached, not reloaded");
                ids.insert(tex.id());
            }
        }
        // Claude, Antigravity and OpenCode have distinct company + CLI marks; Codex shares OpenAI's.
        assert_eq!(ids.len(), 7);
        assert_eq!(
            super::symbol_tint(ProviderId::OpenCode),
            egui::Color32::WHITE
        );
        assert_eq!(super::symbol_tint(ProviderId::Codex), crate::theme::text());
        assert_eq!(super::symbol_tint(ProviderId::Claude), egui::Color32::WHITE);
    }
}

#[cfg(test)]
mod probe {
    use egui::{Id, Rect, Ui};

    /// What a row allocated and what it actually painted (visible part, in screen space).
    #[derive(Clone, Copy, Debug)]
    pub struct Row {
        pub allocated: Rect,
        pub painted: Rect,
    }

    fn id() -> Id {
        Id::unique("conversation_probe")
    }

    pub fn start(ui: &Ui) -> usize {
        ui.ctx()
            .graphics(|g| g.get(ui.layer_id()).map_or(0, |l| l.all_entries().len()))
    }

    pub fn finish(ui: &Ui, from: usize, allocated: Rect) {
        let painted = ui.ctx().graphics(|g| {
            let mut r = Rect::NOTHING;
            if let Some(l) = g.get(ui.layer_id()) {
                for cs in l.all_entries().skip(from) {
                    r = r.union(cs.shape.visual_bounding_rect());
                }
            }
            r
        });
        ui.data_mut(|d| {
            d.get_temp_mut_or_default::<Vec<Row>>(id())
                .push(Row { allocated, painted })
        });
    }

    pub fn take(ctx: &egui::Context) -> Vec<Row> {
        ctx.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<Vec<Row>>(id())))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use egui::collapsing_header::CollapsingState;
    use time::OffsetDateTime;
    use xade_core::{Effort, ProviderId, Session, SessionId, UsageTotals, WorkspaceId};

    fn session() -> Session {
        let now = OffsetDateTime::now_utc();
        Session {
            origin: xade_core::SessionOrigin::default(),
            id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            provider_id: ProviderId::Claude,
            provider_session_id: None,
            title: "t".into(),
            preview_snippet: String::new(),
            model_id: "opus".into(),
            effort: Effort::Default,
            sent_model_id: String::new(),
            sent_effort: Effort::Default,
            auto_mode: false,
            usage: UsageTotals::default(),
            created_at: now,
            updated_at: now,
            multi_session_id: None,
            agent_handle: None,
        }
    }

    fn msg(role: Role, text: &str, tool: Option<(&str, &str)>, payload: Option<&str>) -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            session_id: SessionId::new(),
            seq: 0,
            role,
            content_text: text.into(),
            thinking_text: None,
            tool_name: tool.map(|t| t.0.to_string()),
            tool_use_id: tool.map(|t| t.1.to_string()),
            tool_payload_json: payload.map(str::to_string),
            is_error: false,
            created_at: OffsetDateTime::now_utc(),
            provider_id: None,
            sender_handle: None,
        }
    }

    #[test]
    fn an_assistant_message_uses_its_own_provider_for_the_avatar() {
        let mut m = msg(Role::Assistant, "hi", None, None);
        assert_eq!(
            avatar_provider(&m, ProviderId::Claude),
            ProviderId::Claude,
            "none means the session's"
        );
        m.provider_id = Some(ProviderId::Antigravity);
        assert_eq!(
            avatar_provider(&m, ProviderId::Claude),
            ProviderId::Antigravity
        );
    }

    fn nasty(n: usize) -> String {
        let mut out = String::new();
        for i in 0..n {
            match i % 10 {
                0 => out.push_str(&format!("\u{2500}\u{2500}\u{2500} FAILED TESTS ({i}) \u{2500}\u{2500}\u{2500}\n")),
                1 => out.push_str(&format!("\u{2715} [WEB] [LISTINGS] PMQA-{i} - applying the filter and refreshing the page will redirect to the SEO-formatted variant (see line {i})\n")),
                2 => out.push_str("  attempt 1: Error: expect(locator).toHaveText(expected) failed\n"),
                3 => out.push_str("    attempt 1: Locator: locator('h1[class*=\"vendor-search-title\"]')\n"),
                4 => out.push('\n'),
                5 => out.push_str("[Pipeline] // withEnv\n[Pipeline] }\n"),
                6 => out.push_str("```\n"),
                7 => out.push_str("| a | b |\n"),
                8 => out.push_str("- item <b>x</b> *emph\n"),
                _ => out.push_str(&format!("{i} \u{2713} done (37.6s)\n")),
            }
        }
        out
    }

    fn log(n: usize) -> String {
        (0..n)
            .map(|i| format!("{i} \u{2713} [LISTINGS] PMQA-{i} - after logging in, the user can complete and submit an RFQ with VRM and view the conversation (37.6s)"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn vm(messages: Vec<Message>) -> SessionVm {
        SessionVm {
            session: session(),
            cwd: "/tmp".into(),
            status: SessionStatus::Idle,
            queued: Default::default(),
            messages,
            live_text: String::new(),
            thinking_tokens: None,
            pending_permission: None,
            input: String::new(),
            error: None,
            md_cache: CommonMarkCache::default(),
            path_links: Default::default(),
            diff: None,
            changes: Default::default(),
            settings_pending: false,
            turn_base: None,
            turn_snapshotting: false,
            turn_files: Default::default(),
            sent_model: String::new(),
            sent_effort: xade_core::Effort::Default,
            collapsed: false,
            window_rect: None,
            apply_rect: None,
            sync: Default::default(),
            mention: Default::default(),
            attach_note: None,
            focus_input: false,
            subagent_msgs: Vec::new(),
            view_subagents: false,
            fork_preparing: false,
            renaming: None,
            scroll_to_end: false,
        }
    }

    fn transcript() -> Vec<Message> {
        vec![
            msg(Role::User, "hello", None, None),
            msg(Role::Assistant, "first **reply**\n\n- a\n- b", None, None),
            msg(Role::User, &nasty(200), None, None),
            msg(Role::Assistant, "after the paste", None, None),
            msg(
                Role::ToolCall,
                "",
                Some(("Bash", "t1")),
                Some(r#"{"command":"ls"}"#),
            ),
            msg(Role::ToolResult, &log(300), Some(("Bash", "t1")), None),
            msg(Role::Assistant, "after the tool", None, None),
            msg(Role::User, "how is it going?", None, None),
            msg(Role::Assistant, "```\n{}\n```", None, None),
        ]
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Cards {
        Collapsed,
        Open,
        /// Open with the full tool output instead of the first 4 KiB.
        OpenAll,
    }

    const WIDTH: f32 = 700.0;
    /// Tall enough that nothing scrolls, so painted extents are not clipped away.
    const HEIGHT: f32 = 30000.0;

    /// Renders the transcript for a few frames and returns the rows of the last one.
    fn render(vm: &mut SessionVm, cards: Cards) -> Vec<probe::Row> {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut rows = Vec::new();
        for _ in 0..4 {
            if cards != Cards::Collapsed {
                for m in vm.messages.iter().filter(|m| m.role == Role::ToolCall) {
                    let id = egui::Id::unique(("tool_card", m.id));
                    let mut st = CollapsingState::load_with_default_open(&ctx, id, true);
                    st.set_open(true);
                    st.store(&ctx);
                    if cards == Cards::OpenAll {
                        ctx.data_mut(|d| d.insert_temp(id.with("all"), true));
                    }
                }
            }
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(WIDTH, HEIGHT),
                )),
                ..Default::default()
            };
            let out = ctx.run_pass(input, |ctx| {
                egui::Window::new("w")
                    .title_bar(false)
                    .fixed_size([WIDTH - 20.0, HEIGHT - 1000.0])
                    .show(ctx, |ui| show(ui, vm));
            });
            out.drop_without_applying_deltas();
            rows = probe::take(&ctx);
        }
        rows
    }

    fn assert_stacked(rows: &[probe::Row], expected: usize) {
        assert_eq!(rows.len(), expected);
        for (i, r) in rows.iter().enumerate() {
            let (a, p) = (r.allocated, r.painted);
            assert!(
                p.max.y <= a.max.y + 1.0 && (!p.is_positive() || p.min.y >= a.min.y - 1.0),
                "row {i} paints outside its rect: {r:?}"
            );
            assert!(
                a.max.x <= WIDTH && p.max.x <= WIDTH,
                "row {i} wider than the window: {r:?}"
            );
        }
        for (i, w) in rows.windows(2).enumerate() {
            assert!(
                w[1].allocated.min.y >= w[0].allocated.max.y,
                "row {} starts above the bottom of row {i}: {:?} {:?}",
                i + 1,
                w[1],
                w[0]
            );
        }
    }

    #[test]
    fn rows_stack_without_overlap_whatever_the_card_state() {
        let mut heights = Vec::new();
        for cards in [Cards::Collapsed, Cards::Open, Cards::OpenAll] {
            let rows = render(&mut vm(transcript()), cards);
            assert_stacked(&rows, 9);
            heights.push(rows[4].allocated.height());
        }
        assert!(heights[1] > heights[0] + 100.0, "{heights:?}");
        assert!(heights[2] > heights[1] * 4.0, "{heights:?}");
    }

    #[test]
    fn a_wide_table_wraps_inside_the_chat_window() {
        let messages = vec![
            msg(Role::User, "run the checks", None, None),
            msg(
                Role::Assistant,
                &crate::widgets::markdown::wide_table(),
                None,
                None,
            ),
        ];
        let rows = render(&mut vm(messages), Cards::Collapsed);
        assert_stacked(&rows, 2);
        let table = rows[1];
        // One long line would be a few rows tall; wrapped into columns it takes many.
        assert!(
            table.allocated.height() > 250.0,
            "the table did not wrap: {table:?}"
        );
        assert!(table.painted.max.x <= WIDTH, "{table:?}");
    }

    #[test]
    fn consecutive_tool_calls_group_unless_a_code_change_or_text_splits_them() {
        let call = |name: &str, id: &str| msg(Role::ToolCall, "", Some((name, id)), Some("{}"));
        let result = |name: &str, id: &str| msg(Role::ToolResult, "ok", Some((name, id)), None);
        let messages = vec![
            msg(Role::User, "go", None, None),
            call("Bash", "a"),
            result("Bash", "a"),
            call("Read", "b"),
            result("Read", "b"),
            msg(Role::Assistant, "text", None, None),
            call("Bash", "c"),
            result("Bash", "c"),
            call("Edit", "d"),
            result("Edit", "d"),
            call("Bash", "e"),
            call("Bash", "f"),
        ];
        assert_eq!(tool_runs(&messages), [(1, 4), (10, 11)]);
    }

    #[test]
    fn a_group_is_one_collapsed_line_even_with_a_failure_inside() {
        let mut messages = vec![msg(Role::User, "go", None, None)];
        for (i, failed) in [false, true, false].into_iter().enumerate() {
            let id = format!("t{i}");
            let input = format!(r#"{{"command":"step {i}"}}"#);
            messages.push(msg(Role::ToolCall, "", Some(("Bash", &id)), Some(&input)));
            let mut r = msg(Role::ToolResult, "out", Some(("Bash", &id)), None);
            r.is_error = failed;
            messages.push(r);
        }
        let rows = render(&mut vm(messages), Cards::Collapsed);
        assert_stacked(&rows, 2);
        assert!(rows[1].allocated.height() < 60.0, "{:?}", rows[1]);
    }

    #[test]
    fn a_long_user_message_shows_a_head_until_expanded() {
        let mut v = vm(vec![msg(Role::User, &log(200), None, None)]);
        let short = render(&mut v, Cards::Collapsed)[0].allocated.height();
        let mut v = vm(vec![msg(Role::User, &log(3), None, None)]);
        let three = render(&mut v, Cards::Collapsed)[0].allocated.height();
        let mut v = vm(vec![msg(Role::User, &log(2), None, None)]);
        let two = render(&mut v, Cards::Collapsed)[0].allocated.height();
        assert!(
            (short - three).abs() < 1.0,
            "200 lines {short} collapse to the same head as 3 lines {three}"
        );
        assert!(
            two < three,
            "2 lines {two} fit whole, no toggle; 3 lines {three}"
        );
        let m = msg(Role::User, &log(200), None, None);
        let id = egui::Id::unique(("user_expanded", m.id));
        let mut v = vm(vec![m]);
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        ctx.data_mut(|d| d.insert_temp(id, true));
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(WIDTH, HEIGHT),
            )),
            ..Default::default()
        };
        for _ in 0..3 {
            ctx.run_pass(input.clone(), |ctx| {
                egui::Window::new("w")
                    .title_bar(false)
                    .fixed_size([WIDTH - 20.0, HEIGHT - 1000.0])
                    .show(ctx, |ui| show(ui, &mut v));
            })
            .drop_without_applying_deltas();
        }
        let all = probe::take(&ctx).last().copied().unwrap().allocated;
        assert!(
            all.height() > short * 4.0,
            "expanded {} vs {short}",
            all.height()
        );
    }

    #[test]
    fn unbroken_long_tokens_stay_inside_the_window() {
        let token = "x".repeat(3000);
        let mut v = vm(vec![
            msg(Role::User, &token, None, None),
            msg(Role::Assistant, &token, None, None),
            msg(Role::Assistant, &format!("```\n{token}\n```"), None, None),
        ]);
        assert_stacked(&render(&mut v, Cards::Collapsed), 3);
    }

    /// A turn editing a.rs twice and b.rs once, each call with a result.
    fn edit_turn() -> Vec<Message> {
        let call = |id: &str, file: &str| {
            let input = format!(r#"{{"file_path":"{file}","old_string":"a","new_string":"b"}}"#);
            [
                msg(Role::ToolCall, "", Some(("Edit", id)), Some(&input)),
                msg(Role::ToolResult, "ok", Some(("Edit", id)), Some("{}")),
            ]
        };
        let mut t = vec![msg(Role::User, "go", None, None)];
        for (id, file) in [
            ("e1", "/tmp/a.rs"),
            ("e2", "/tmp/b.rs"),
            ("e3", "/tmp/a.rs"),
        ] {
            t.extend(call(id, file));
        }
        t.push(msg(Role::Assistant, "done", None, None));
        t
    }

    #[test]
    fn a_finished_turn_ends_with_a_changes_summary_but_a_running_one_does_not() {
        let mut v = vm(edit_turn());
        let rows = render(&mut v, Cards::Collapsed);
        assert_stacked(&rows, 9);
        assert!(rows[8].allocated.height() > 30.0, "{:?}", rows[8]);
        assert_eq!(v.changes.get(&v.messages, &v.cwd).len(), 2);
        let mut v = vm(edit_turn());
        v.status = SessionStatus::Streaming;
        assert_stacked(&render(&mut v, Cards::Collapsed), 8);
    }

    #[test]
    fn a_turn_that_edited_through_bash_still_ends_with_a_changes_summary() {
        let tf = |name: &str| crate::turn_files::TurnFile {
            path: PathBuf::from(format!("/tmp/{name}")),
            added: 2,
            removed: 1,
            old: "a\n".into(),
            new: "b\n".into(),
        };
        let mut v = vm(vec![
            msg(Role::User, "fix", None, None),
            msg(Role::Assistant, "done", None, None),
        ]);
        let anchor = v.messages[1].id;
        assert_stacked(&render(&mut v, Cards::Collapsed), 2);
        let files = vec![tf("a.rs"), tf("b.rs")];
        v.turn_files
            .insert(anchor, crate::turn_files::TurnChanges { files });
        let rows = render(&mut v, Cards::Collapsed);
        assert_stacked(&rows, 3);
        assert!(rows[2].allocated.height() > 30.0, "{:?}", rows[2]);
        v.open_turn_file(anchor, 1);
        let d = v.diff.as_ref().unwrap();
        assert_eq!(d.data.path, Path::new("/tmp/b.rs"));
        assert_eq!((d.data.added, d.data.removed), (1, 1));

        let mut v = vm(vec![
            msg(Role::User, "fix", None, None),
            msg(Role::Assistant, "done", None, None),
        ]);
        let anchor = v.messages[1].id;
        v.turn_files.insert(
            anchor,
            crate::turn_files::TurnChanges {
                files: vec![tf("a.rs")],
            },
        );
        assert_stacked(&render(&mut v, Cards::Collapsed), 3);
    }

    #[test]
    fn opening_a_file_from_the_summary_merges_its_edits_read_only() {
        let mut v = vm(edit_turn());
        let calls = v.changes.get(&v.messages, &v.cwd)[0].calls.clone();
        v.open_changes(&calls);
        let d = v.diff.as_ref().unwrap();
        assert!(d.tool_use_id.is_empty());
        assert_eq!(d.data.path, Path::new("/tmp/a.rs"));
        let seps = d
            .data
            .rows
            .iter()
            .filter(|r| matches!(r, crate::diff::Row::Sep(_)));
        assert_eq!(seps.count(), 2);
    }

    /// Renders the chat in a 400x300 area for a few frames and returns the top of the text
    /// containing `needle` (`None` when culled), so a test sees what is on screen.
    fn y_of(
        ctx: &egui::Context,
        vm: &mut SessionVm,
        events: Vec<egui::Event>,
        needle: &str,
    ) -> Option<f32> {
        fn find(s: &egui::Shape, needle: &str) -> Option<f32> {
            match s {
                egui::Shape::Text(t) if t.galley.text().contains(needle) => Some(t.pos.y),
                egui::Shape::Vec(v) => v.iter().find_map(|c| find(c, needle)),
                _ => None,
            }
        }
        let mut found = None;
        // Frames settle: the scroll offset and the sticky state take one or two to apply.
        for i in 0..4 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(400.0, 300.0),
                )),
                events: if i == 0 { events.clone() } else { vec![] },
                // Time must pass for egui's scroll animation to run.
                time: Some(ctx.cumulative_pass_nr() as f64 * 0.5),
                ..Default::default()
            };
            let out = ctx.run_ui(input, |ui| {
                show(ui, vm);
            });
            found = out.shapes.iter().find_map(|c| find(&c.shape, needle));
            out.drop_without_applying_deltas();
        }
        found
    }

    #[test]
    fn a_bubble_and_its_tail_stay_inside_the_width_on_either_side() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        for tail in [Tail::Left, Tail::Right] {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(400.0, 300.0),
                )),
                ..Default::default()
            };
            let mut seen = None;
            let out = ctx.run_ui(input, |ui| {
                let area = ui.max_rect();
                bubble(ui, tail, Color32::BLACK, Color32::WHITE, |ui| {
                    ui.label("hello");
                });
                seen = Some((area, ui.min_rect()));
            });
            out.drop_without_applying_deltas();
            let (area, used) = seen.expect("ran");
            assert!(
                area.contains_rect(used),
                "{tail:?}: {used:?} outside {area:?}"
            );
            if tail == Tail::Left {
                // The indent sits on the right, empty; on the other side it is counted as used.
                assert!(area.right() - used.right() > 0.0, "room opposite the tail");
            }
        }
    }

    #[test]
    fn sending_jumps_to_the_bottom_and_keeps_following_new_messages() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let rows: Vec<_> = (0..40)
            .map(|i| msg(Role::Assistant, &format!("filler row {i}"), None, None))
            .collect();
        let mut v = vm(rows);
        let visible = |y: Option<f32>| y.is_some_and(|y| (0.0..300.0).contains(&y));
        let push =
            |v: &mut SessionVm, text: &str| v.messages.push(msg(Role::User, text, None, None));

        push(&mut v, "zzlast");
        assert!(
            visible(y_of(&ctx, &mut v, vec![], "zzlast")),
            "starts at the end"
        );

        // The reader scrolls up, which turns the automatic scroll off.
        let wheel = vec![
            egui::Event::PointerMoved(egui::pos2(200.0, 150.0)),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                source: egui::MouseWheelSource::Wheel,
                delta: egui::vec2(0.0, 5000.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        assert!(
            !visible(y_of(&ctx, &mut v, wheel, "zzlast")),
            "scrolled away"
        );
        push(&mut v, "zzcontrol");
        assert!(
            !visible(y_of(&ctx, &mut v, vec![], "zzcontrol")),
            "without a send, a new message does not pull the view down"
        );

        // A send lands on the newest message...
        push(&mut v, "zzsent");
        v.scroll_to_end = true;
        assert!(
            visible(y_of(&ctx, &mut v, vec![], "zzsent")),
            "jumps to the bottom"
        );
        assert!(!v.scroll_to_end, "the request is consumed");
        // ...and the automatic scroll is back on: the reply is followed.
        push(&mut v, "zzreply");
        assert!(
            visible(y_of(&ctx, &mut v, vec![], "zzreply")),
            "follows again"
        );
    }

    #[test]
    fn a_message_time_is_clock_only_and_its_hover_adds_the_date() {
        let at = OffsetDateTime::from_unix_timestamp(1_795_703_528).unwrap(); // Thu 26 Nov 2026, 14:32:08 UTC
        let utc = time_labels(at, &chrono::Utc).unwrap();
        assert_eq!(
            utc,
            ("14:32".to_string(), "Thu 26 Nov 2026, 14:32:08".to_string())
        );
        let plus2 = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        let (clock, full) = time_labels(at, &plus2).unwrap();
        assert_eq!(
            (clock.as_str(), full.as_str()),
            ("16:32", "Thu 26 Nov 2026, 16:32:08")
        );
    }

    #[test]
    fn user_and_assistant_messages_show_their_time_on_a_line_above_at_the_right() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let rows = vec![
            msg(Role::User, "zzuser", None, None),
            msg(Role::Assistant, "zzassistant", None, None),
        ];
        // Both rows are built within the same minute unless the clock rolls over mid-test.
        let (clock, _) = time_labels(rows[0].created_at, &chrono::Local).unwrap();
        assert_eq!(clock.len(), 5, "24-hour HH:MM: {clock}");
        let mut v = vm(rows);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(500.0, 600.0),
            )),
            ..Default::default()
        };
        // (text, rect) of every text shape.
        fn texts(s: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
            match s {
                egui::Shape::Text(t) => out.push((
                    t.galley.text().to_string(),
                    egui::Rect::from_min_size(t.pos, t.galley.size()),
                )),
                egui::Shape::Vec(v) => v.iter().for_each(|c| texts(c, out)),
                _ => {}
            }
        }
        let mut shapes = Vec::new();
        for _ in 0..2 {
            let out = ctx.run_ui(input.clone(), |ui| {
                show(ui, &mut v);
            });
            shapes.clear();
            out.shapes.iter().for_each(|c| texts(&c.shape, &mut shapes));
            out.drop_without_applying_deltas();
        }
        let mut times: Vec<_> = shapes.iter().filter(|(t, _)| *t == clock).collect();
        times.sort_by(|a, b| a.1.top().total_cmp(&b.1.top()));
        assert_eq!(times.len(), 2, "one time per message: {clock}");
        let body = |needle: &str| shapes.iter().find(|(t, _)| t.contains(needle)).unwrap().1;
        for ((_, time), needle) in times.iter().zip(["zzuser", "zzassistant"]) {
            let text = body(needle);
            assert!(time.right() > 480.0, "at the right edge: {time:?}");
            assert!(
                time.bottom() <= text.top(),
                "above its message: {time:?} / {text:?}"
            );
        }
        // Each line belongs to its own message: the second time is below the first message.
        assert!(times[1].1.top() > body("zzuser").bottom());
    }

    #[test]
    fn split_error_keeps_short_lines_inline_and_folds_multiline_or_long_errors() {
        let short = "agy exited unexpectedly (SIGBUS, signal 7)";
        assert_eq!(split_error(short), (Cow::Borrowed(short), None));

        let multi = "agy exited unexpectedly (exit code 1)\nstack frame 1\nstack frame 2";
        assert_eq!(
            split_error(multi),
            (
                Cow::Borrowed("agy exited unexpectedly (exit code 1)"),
                Some("stack frame 1\nstack frame 2")
            )
        );

        assert_eq!(
            split_error("only one line\n   \n"),
            (Cow::Borrowed("only one line"), None)
        );

        let long = "é".repeat(ERROR_SUMMARY_CHARS + 20);
        let (summary, details) = split_error(&long);
        assert_eq!(summary.chars().count(), ERROR_SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));
        assert_eq!(details, Some(long.as_str()));
    }

    #[test]
    fn multiline_error_starts_collapsed_in_conversation() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut v = vm(vec![]);
        v.error = Some("agy exited unexpectedly (exit code 1)\nhidden stderr line".into());
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(500.0, 400.0),
            )),
            ..Default::default()
        };
        fn collect_texts(s: &egui::Shape, out: &mut Vec<String>) {
            match s {
                egui::Shape::Text(t) => out.push(t.galley.text().to_string()),
                egui::Shape::Vec(v) => v.iter().for_each(|c| collect_texts(c, out)),
                _ => {}
            }
        }
        let mut texts = Vec::new();
        for _ in 0..2 {
            let out = ctx.run_ui(input.clone(), |ui| {
                show(ui, &mut v);
            });
            texts.clear();
            out.shapes
                .iter()
                .for_each(|c| collect_texts(&c.shape, &mut texts));
            out.drop_without_applying_deltas();
        }
        assert!(
            texts
                .iter()
                .any(|t| t.contains("agy exited unexpectedly (exit code 1)")),
            "{texts:?}"
        );
        assert!(texts.iter().any(|t| t == "Details"), "{texts:?}");
        assert!(
            !texts.iter().any(|t| t.contains("hidden stderr line")),
            "details must start collapsed: {texts:?}"
        );
    }
}
