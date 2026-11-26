//! The scrolling transcript of one session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use egui::RichText;
use egui_commonmark::CommonMarkCache;
use xade_core::{Delivery, Message, ProviderId, Role, SessionStatus};

use crate::app::SessionVm;
use crate::theme;
use crate::widgets::changes_summary;
use crate::widgets::markdown::markdown;
use crate::widgets::path_links::PathLinks;
use crate::widgets::tool_card::{self, CardStatus};

/// Draws the transcript; returns a file path the user clicked.
pub fn show(ui: &mut egui::Ui, vm: &mut SessionVm) -> Option<PathBuf> {
    let find_mark = crate::find::mark(
        ui.ctx(),
        crate::workspace::WindowKind::Session(vm.session.id),
    );
    let provider = vm.session.provider_id;
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
    egui::ScrollArea::vertical()
        .id_salt("conversation")
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
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
                                    let clicked = with_time(ui, m, |ui| {
                                        assistant(ui, avatar_provider(m, provider), |ui| {
                                            path_links.markdown(ui, md_cache, cwd, id, text)
                                        })
                                    });
                                    open = open.take().or(clicked);
                                }
                                Role::ToolCall => {
                                    let (result, card_status) = outcome(m);
                                    let awaiting = pending_permission.as_ref().is_some_and(|p| {
                                        m.tool_use_id.as_deref() == Some(&*p.tool_use_id)
                                    });
                                    if tool_card::show(
                                        ui,
                                        md_cache,
                                        m,
                                        result,
                                        card_status,
                                        awaiting,
                                    ) {
                                        review = m.tool_use_id.clone();
                                    }
                                }
                                Role::System => {
                                    ui.label(
                                        RichText::new(&m.content_text).color(theme::text_muted()),
                                    );
                                }
                                Role::ToolResult => {}
                            });
                            if let Some(mark) = &find_mark {
                                // Only the current message is tinted, not every one that matches.
                                if mark.matches.get(mark.current) == Some(&i) {
                                    ui.painter().rect_filled(
                                        rect.expand(3.0),
                                        4.0,
                                        theme::find_block(),
                                    );
                                    let nth =
                                        mark.current - mark.matches.partition_point(|&x| x < i);
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
                                    if let Some(n) =
                                        changes_summary::show(ui, m.id, cwd, &list, source)
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
                        ui.label(RichText::new(&*e).color(theme::danger()));
                    }
                });
            // After a send: land on the end, which also re-arms `stick_to_bottom`.
            if std::mem::take(scroll_to_end) {
                ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
            }
        });
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

fn user_bubble(
    ui: &mut egui::Ui,
    cache: &mut CommonMarkCache,
    links: &mut PathLinks,
    cwd: &Path,
    m: &Message,
) -> Option<PathBuf> {
    let mut clicked = None;
    let indent = ui.available_width() * 0.15;
    let total = m.content_text.lines().count();
    let long = total > USER_PREVIEW_LINES;
    let open_id = egui::Id::unique(("user_expanded", m.id));
    let open = long && ui.data(|d| d.get_temp::<bool>(open_id)).unwrap_or(false);
    ui.horizontal_top(|ui| {
        ui.add_space(indent);
        egui::Frame::new()
            .fill(theme::bg_raised())
            .stroke(egui::Stroke::new(1.0, theme::border_light()))
            .corner_radius(8)
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // The row is a horizontal layout; the bubble's own content stacks vertically.
                ui.vertical(|ui| {
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
                    clicked = links.markdown(ui, cache, cwd, m.id, shown);
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
                });
            });
    });
    clicked
}

/// Whose avatar an assistant message gets: its own provider (copied fork history), else the
/// session's.
fn avatar_provider(m: &Message, session: ProviderId) -> ProviderId {
    m.provider_id.unwrap_or(session)
}

/// The assistant avatar beside `body`, which draws the text.
fn assistant(
    ui: &mut egui::Ui,
    provider: ProviderId,
    body: impl FnOnce(&mut egui::Ui) -> Option<PathBuf>,
) -> Option<PathBuf> {
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
        if let Some(tex) = cli_symbol(ui.ctx(), provider) {
            egui::Image::new(&tex).paint_at(ui, rect);
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

/// The logo of the company behind a provider, for window headers and the top bar.
pub(crate) fn company_symbol(
    ctx: &egui::Context,
    provider: ProviderId,
) -> Option<egui::TextureHandle> {
    let (name, png) = match provider {
        ProviderId::Claude => ("anthropic_symbol", ANTHROPIC_SYMBOL),
        ProviderId::Antigravity => ("google_symbol", GOOGLE_SYMBOL),
    };
    symbol(ctx, name, png)
}

/// The logo of a provider's CLI, the avatar of its messages.
pub(crate) fn cli_symbol(ctx: &egui::Context, provider: ProviderId) -> Option<egui::TextureHandle> {
    let (name, png) = match provider {
        ProviderId::Claude => ("claude_code_symbol", CLAUDE_CODE_SYMBOL),
        ProviderId::Antigravity => ("antigravity_symbol", ANTIGRAVITY_SYMBOL),
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
        for p in [ProviderId::Claude, ProviderId::Antigravity] {
            for load in [super::company_symbol, super::cli_symbol] {
                let tex = load(&ctx, p).expect("symbol");
                assert_eq!(tex.size(), [80, 80]);
                let again = load(&ctx, p).expect("cached second call");
                assert_eq!(tex.id(), again.id(), "cached, not reloaded");
                ids.insert(tex.id());
            }
        }
        assert_eq!(
            ids.len(),
            4,
            "company and CLI logos differ for every provider"
        );
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
}
