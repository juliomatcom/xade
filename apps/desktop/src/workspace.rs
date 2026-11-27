//! Workspace view: one `egui::Window` per open session or editor, auto-tiled into a grid.

use std::collections::HashMap;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{Id, Order, Rect};
use tokio::sync::mpsc::UnboundedSender;
use xade_core::{AgentProvider, MultiSessionId, ProviderId, SessionId};
use xade_runtime::ManagerCmd;

use crate::app::{App, SessionVm};
use crate::config::SendMode;
use crate::editor::{self, EditorId, EditorVm};
use crate::guide::{self, GuideVm};
use crate::multi_view::{self, MultiSessionVm};
use crate::session_view;
use crate::theme;
use crate::widgets::diff_modal::{self, ModalAction};

const GAP: f32 = 8.0;
const DEFAULT_SIZE: [f32; 2] = [520.0, 640.0];
const CASCADE: f32 = 28.0;
/// Smallest tile `tile_rects` makes while the area allows it.
const MIN_TILE: [f32; 2] = [380.0, 300.0];
/// How far a window may drift from its tile before auto-tiling turns off.
const MOVE_TOLERANCE: f32 = 3.0;

/// An open window: a session, multi-session, editor or the guide, tiled alike in open order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum WindowKind {
    Session(SessionId),
    MultiSession(MultiSessionId),
    Editor(EditorId),
    Guide,
}

impl WindowKind {
    pub fn session(self) -> Option<SessionId> {
        match self {
            WindowKind::Session(id) => Some(id),
            WindowKind::MultiSession(_) | WindowKind::Editor(_) | WindowKind::Guide => None,
        }
    }
}

pub fn win_id(kind: WindowKind) -> Id {
    Id::unique(kind)
}

/// A window's state while it is taken out of `App` for drawing.
enum Taken {
    Session(Box<SessionVm>),
    Multi {
        vm: Box<MultiSessionVm>,
        members: HashMap<SessionId, SessionVm>,
    },
    Editor(Box<EditorVm>),
    Guide(GuideVm),
}

type Geom<'a> = (&'a mut bool, &'a mut Option<Rect>, &'a mut Option<Rect>);

impl Taken {
    /// `(collapsed, window_rect, apply_rect)` of any window kind.
    fn geom(&mut self) -> Geom<'_> {
        match self {
            Taken::Session(vm) => (&mut vm.collapsed, &mut vm.window_rect, &mut vm.apply_rect),
            Taken::Multi { vm, .. } => (&mut vm.collapsed, &mut vm.window_rect, &mut vm.apply_rect),
            Taken::Editor(vm) => (&mut vm.collapsed, &mut vm.window_rect, &mut vm.apply_rect),
            Taken::Guide(vm) => (&mut vm.collapsed, &mut vm.window_rect, &mut vm.apply_rect),
        }
    }
}

pub(crate) fn geom_of(app: &mut App, kind: WindowKind) -> Option<Geom<'_>> {
    match kind {
        WindowKind::Session(id) => app
            .sessions
            .get_mut(&id)
            .map(|vm| (&mut vm.collapsed, &mut vm.window_rect, &mut vm.apply_rect)),
        WindowKind::MultiSession(id) => app
            .multi_sessions
            .get_mut(&id)
            .map(|vm| (&mut vm.collapsed, &mut vm.window_rect, &mut vm.apply_rect)),
        WindowKind::Editor(id) => app
            .editors
            .map
            .get_mut(&id)
            .map(|vm| (&mut vm.collapsed, &mut vm.window_rect, &mut vm.apply_rect)),
        WindowKind::Guide => app
            .guide
            .as_mut()
            .map(|vm| (&mut vm.collapsed, &mut vm.window_rect, &mut vm.apply_rect)),
    }
}

/// What session views need from `App` while their `SessionVm` is borrowed out of it.
pub struct Env<'a> {
    pub cmd_tx: &'a UnboundedSender<ManagerCmd>,
    pub providers: &'a [Arc<dyn AgentProvider>],
    /// Providers a session can be forked to: every available one.
    pub fork_targets: &'a [ProviderId],
    pub vscode: Option<&'a Path>,
    pub maximized: Option<SessionId>,
    pub send_mode: SendMode,
    /// When true, plain Enter sends the prompt and Shift+Enter inserts a newline.
    pub send_on_enter: bool,
    /// The text box breaks long words anywhere (`input_break_all`).
    pub break_all: bool,
    /// `<ide_opened_file>` tag for the file the user last had open, sent ahead of prompts.
    pub ide_context: Option<&'a str>,
    /// This window is the focused one: its header is highlighted.
    pub focused: bool,
}

/// Auto-tiling state: windows are re-tiled while `auto` and the set of windows or the area changes.
pub struct Tiling {
    pub auto: bool,
    ids: Vec<WindowKind>,
    area: Rect,
}

impl Default for Tiling {
    fn default() -> Self {
        Tiling {
            auto: true,
            ids: Vec::new(),
            area: Rect::NOTHING,
        }
    }
}

impl Tiling {
    /// The top bar's Tile button.
    pub fn retile(&mut self) {
        self.auto = true;
        self.ids.clear();
    }
}

/// `n` equal-width, full-height columns inside `area`.
fn columns(area: Rect, n: usize) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }
    let w = (area.width() - GAP * (n as f32 - 1.0)) / n as f32;
    (0..n)
        .map(|i| {
            let x = area.left() + i as f32 * (w + GAP);
            Rect::from_min_size(egui::pos2(x, area.top()), egui::vec2(w, area.height()))
        })
        .collect()
}

/// Preferred column count for `n` windows: 1x1, 2x1, 3x1, 2x2, 3x2, 3x3, then a square-ish grid.
fn base_cols(n: usize) -> usize {
    match n {
        0 | 1 => 1,
        2 => 2,
        3 => 3,
        4 => 2,
        5..=9 => 3,
        _ => (n as f32).sqrt().ceil() as usize,
    }
}

/// `n` tiles filling `area` as a grid, none narrower than `MIN_TILE[0]` or shorter than
/// `MIN_TILE[1]`. Columns drop first when the area is narrow; when even that cannot fit, tiles
/// keep their minimum height and the rows overlap evenly (the last row ends at the bottom).
pub fn tile_rects(area: Rect, n: usize) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }
    let gaps = |k: usize| GAP * (k as f32 - 1.0);
    let max_cols = (((area.width() + GAP) / (MIN_TILE[0] + GAP)).floor() as usize).clamp(1, n);
    let rows_of = |cols: usize| n.div_ceil(cols);
    let mut cols = base_cols(n).min(max_cols);
    // Too short for the rows: spend spare width on more columns.
    while cols < max_cols
        && rows_of(cols) as f32 * MIN_TILE[1] + gaps(rows_of(cols)) > area.height()
    {
        cols += 1;
    }
    let rows = rows_of(cols);
    let h = ((area.height() - gaps(rows)) / rows as f32)
        .max(MIN_TILE[1])
        .min(area.height());
    let stride = if rows > 1 {
        ((area.height() - h) / (rows - 1) as f32).min(h + GAP)
    } else {
        0.0
    };
    (0..n)
        .map(|i| {
            let (row, col) = (i / cols, i % cols);
            // The last row may hold fewer tiles; they share the full width.
            let in_row = if row + 1 == rows {
                n - row * cols
            } else {
                cols
            };
            let w = (area.width() - gaps(in_row)) / in_row as f32;
            let min = egui::pos2(
                area.left() + col as f32 * (w + GAP),
                area.top() + row as f32 * stride,
            );
            Rect::from_min_size(min, egui::vec2(w, h))
        })
        .collect()
}

/// Rect and collapsed flag per window. A maximized window fills the area below a strip of the
/// other windows' collapsed headers.
pub fn layout(area: Rect, n: usize, maximized: Option<usize>) -> Vec<(Rect, bool)> {
    let Some(m) = maximized.filter(|m| *m < n) else {
        return tile_rects(area, n)
            .into_iter()
            .map(|r| (r, false))
            .collect();
    };
    if n == 1 {
        return vec![(area, false)];
    }
    let strip = Rect::from_min_size(
        area.min,
        egui::vec2(area.width(), session_view::HEADER_HEIGHT),
    );
    let mut heads = columns(strip, n - 1).into_iter();
    let body = Rect::from_min_max(egui::pos2(area.left(), strip.bottom() + GAP), area.max);
    (0..n)
        .map(|i| {
            if i == m {
                (body, false)
            } else {
                (heads.next().unwrap_or(strip), true)
            }
        })
        .collect()
}

fn differs(a: Rect, b: Rect) -> bool {
    (a.min - b.min).abs().max_elem() > MOVE_TOLERANCE
        || (a.size() - b.size()).abs().max_elem() > MOVE_TOLERANCE
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let area = ui.available_rect_before_wrap();
    let kinds: Vec<WindowKind> = app
        .open_windows
        .iter()
        .copied()
        .filter(|k| app.window_exists(*k))
        .collect();
    if kinds.is_empty() {
        ui.label(egui::RichText::new("Opening session…").color(theme::text_muted()));
        return;
    }
    let tiles = layout(
        area,
        kinds.len(),
        app.maximized
            .and_then(|m| kinds.iter().position(|k| *k == m)),
    );
    let retile = app.tiling.auto
        && (app.tiling.ids != kinds
            || differs(app.tiling.area, area)
            || app.tiling.area.is_negative());
    if retile {
        app.tiling.ids = kinds.clone();
        app.tiling.area = area;
        for (kind, (rect, collapsed)) in kinds.iter().zip(&tiles) {
            if let Some((c, _, apply)) = geom_of(app, *kind) {
                *apply = Some(*rect);
                *c = *collapsed;
            }
        }
    }

    if let Some(WindowKind::Editor(id)) = app.focused {
        app.editors.last_focused = Some(id);
    }
    let ide_context = app.editors.ide_context();
    let mut closes = Vec::new();
    let mut reloads = Vec::new();
    let mut forks = Vec::new();
    let mut renames = Vec::new();
    let mut multi_renames = Vec::new();
    let mut attaches = Vec::new();
    let mut indexes = Vec::new();
    let mut images = Vec::new();
    let mut opens = Vec::new();
    let mut toggle_max = None;
    let mut user_moved = false;
    let mut pressed = None;
    let mut prev_min = area.min;
    let mut errors = Vec::new();
    let pointer_down = ctx.input(|i| i.pointer.any_pressed());
    let press_layer = ctx
        .pointer_interact_pos()
        .filter(|_| pointer_down)
        .and_then(|p| ctx.layer_id_at(p));
    let fork_targets = app.selectable_providers();
    let guide_rows = match app.guide {
        Some(_) => guide::rows(app),
        None => Vec::new(),
    };
    let mut guide_action = None;
    for (i, kind) in kinds.iter().enumerate() {
        let taken = match *kind {
            WindowKind::Session(id) => app
                .sessions
                .remove(&id)
                .map(|vm| Taken::Session(Box::new(vm))),
            WindowKind::MultiSession(id) => app.multi_sessions.remove(&id).map(|vm| {
                let mut members = HashMap::with_capacity(vm.member_ids.len());
                for &sid in &vm.member_ids {
                    if let Some(m) = app.sessions.remove(&sid) {
                        members.insert(sid, m);
                    }
                }
                Taken::Multi {
                    vm: Box::new(vm),
                    members,
                }
            }),
            WindowKind::Editor(id) => app
                .editors
                .map
                .remove(&id)
                .map(|vm| Taken::Editor(Box::new(vm))),
            WindowKind::Guide => app.guide.take().map(Taken::Guide),
        };
        let Some(mut taken) = taken else {
            continue;
        };
        let show_whitespace = app.config.show_whitespace;
        let focused = app.focused == Some(*kind);
        let env = Env {
            cmd_tx: &app.cmd_tx,
            providers: &app.providers,
            fork_targets: &fork_targets,
            vscode: app.vscode.as_deref(),
            maximized: app.maximized.and_then(WindowKind::session),
            send_mode: app.config.send_while_running,
            send_on_enter: app.config.send_on_enter,
            break_all: app.config.input_break_all,
            ide_context: ide_context.as_deref(),
            focused,
        };
        let is_maximized = app.maximized == Some(*kind);
        let collapsed = *taken.geom().0;
        let applying = taken.geom().2.take();
        let default_pos = if i > 0 {
            prev_min + egui::vec2(CASCADE, CASCADE)
        } else {
            prev_min
        };
        let fill = match kind {
            WindowKind::Session(_) | WindowKind::MultiSession(_) | WindowKind::Guide => {
                theme::bg_window()
            }
            WindowKind::Editor(_) => theme::bg_file(),
        };
        let win = window(win_id(*kind), fill, collapsed, area, applying, default_pos);
        let mut out = session_view::ViewOut::default();
        let shown = catch_unwind(AssertUnwindSafe(|| {
            win.show(&ctx, |ui| match &mut taken {
                Taken::Session(vm) => out = session_view::show(ui, vm, &env),
                Taken::Multi { vm, members } => {
                    out = multi_view::show(ui, vm, members, &env, is_maximized);
                }
                Taken::Editor(vm) => {
                    let o = editor::show(ui, vm, is_maximized, focused, show_whitespace);
                    out = o.view;
                    errors.extend(o.error);
                }
                Taken::Guide(vm) => {
                    out = guide::show(ui, vm, &guide_rows, is_maximized, focused);
                }
            })
        }));
        let resp = match shown {
            Ok(resp) => resp,
            Err(payload) => {
                // The window's state was taken out of `app`; put it back before the frame guard
                // reports the panic, or the session would vanish from the UI.
                match (taken, *kind) {
                    (Taken::Session(vm), WindowKind::Session(id)) => {
                        app.sessions.insert(id, *vm);
                    }
                    (Taken::Multi { vm, members }, WindowKind::MultiSession(id)) => {
                        app.sessions.extend(members);
                        app.multi_sessions.insert(id, *vm);
                    }
                    (Taken::Editor(vm), WindowKind::Editor(id)) => {
                        app.editors.map.insert(id, *vm);
                    }
                    (Taken::Guide(vm), WindowKind::Guide) => app.guide = Some(vm),
                    _ => {}
                }
                resume_unwind(payload)
            }
        };
        let mut collapsed = collapsed;
        let mut window_rect = *taken.geom().1;
        if let Some(resp) = &resp {
            let rect = resp.response.rect;
            prev_min = rect.min;
            if applying.is_none() && !collapsed {
                window_rect = Some(rect);
                if app.tiling.auto && !retile && differs(rect, tiles[i].0) {
                    user_moved = true;
                }
            }
            if press_layer == Some(resp.response.layer_id) {
                pressed = Some(*kind);
            }
        }
        let mut apply_rect = None;
        if out.maximize {
            toggle_max = Some(*kind);
        } else if out.collapse && app.maximized.is_some_and(|m| m != *kind) {
            // Expanding a minimized window while another is maximized hands the maximize over.
            toggle_max = Some(*kind);
        } else if out.collapse {
            let rect = window_rect.unwrap_or(tiles[i].0);
            apply_rect = Some(if collapsed {
                rect
            } else {
                Rect::from_min_size(
                    rect.min,
                    egui::vec2(rect.width(), session_view::HEADER_HEIGHT),
                )
            });
            collapsed = !collapsed;
        }
        if out.close {
            closes.push(*kind);
        }
        if let (true, WindowKind::Session(id)) = (out.reload, *kind) {
            reloads.push(id);
        }
        if let (Some(to), WindowKind::Session(id)) = (out.fork, *kind) {
            forks.push((id, to));
        }
        if let (Some(title), WindowKind::Session(id)) = (out.rename.as_deref(), *kind) {
            renames.push((id, title.to_string()));
        }
        if let (Some(title), WindowKind::MultiSession(id)) = (out.rename.take(), *kind) {
            multi_renames.push((id, title));
        }
        if let (true, WindowKind::Session(id)) = (out.attach, *kind) {
            attaches.push(id);
        }
        if let (Some(img), WindowKind::Session(id)) = (out.pasted_image.take(), *kind) {
            images.push((id, img));
        }
        if let (true, WindowKind::Session(id)) = (out.index_files, *kind) {
            indexes.push(id);
        }
        opens.extend(out.open_file.take());
        guide_action = guide_action.or(out.guide);
        {
            let (c, w, a) = taken.geom();
            *c = collapsed;
            *w = window_rect;
            if apply_rect.is_some() {
                *a = apply_rect;
            }
        }
        match taken {
            Taken::Session(vm) => {
                if let WindowKind::Session(id) = *kind {
                    app.sessions.insert(id, *vm);
                    if let Some(vm) = app.sessions.get_mut(&id) {
                        let env = Env {
                            cmd_tx: &app.cmd_tx,
                            providers: &app.providers,
                            fork_targets: &fork_targets,
                            vscode: app.vscode.as_deref(),
                            maximized: app.maximized.and_then(WindowKind::session),
                            send_mode: app.config.send_while_running,
                            send_on_enter: app.config.send_on_enter,
                            break_all: app.config.input_break_all,
                            ide_context: ide_context.as_deref(),
                            focused,
                        };
                        opens.extend(diff_modal_for(&ctx, id, vm, &env));
                    }
                }
            }
            Taken::Multi { vm, members } => {
                if let WindowKind::MultiSession(id) = *kind {
                    let ordered_ids = vm.member_ids.clone();
                    let show_diff = !vm.collapsed;
                    app.sessions.extend(members);
                    app.multi_sessions.insert(id, *vm);
                    if show_diff {
                        // Draw at most one diff modal per frame in deterministic roster order.
                        for sid in ordered_ids {
                            if app.sessions.get(&sid).is_some_and(|m| m.diff.is_some()) {
                                if let Some(m) = app.sessions.get_mut(&sid) {
                                    let env = Env {
                                        cmd_tx: &app.cmd_tx,
                                        providers: &app.providers,
                                        fork_targets: &fork_targets,
                                        vscode: app.vscode.as_deref(),
                                        maximized: app.maximized.and_then(WindowKind::session),
                                        send_mode: app.config.send_while_running,
                                        send_on_enter: app.config.send_on_enter,
                                        break_all: app.config.input_break_all,
                                        ide_context: ide_context.as_deref(),
                                        focused,
                                    };
                                    opens.extend(diff_modal_for(&ctx, sid, m, &env));
                                }
                                break;
                            }
                        }
                    }
                }
            }
            Taken::Editor(vm) => {
                if let WindowKind::Editor(id) = *kind {
                    app.editors.map.insert(id, *vm);
                }
            }
            Taken::Guide(vm) => app.guide = Some(vm),
        }
    }
    if let Some(e) = errors.pop() {
        app.report(crate::errors::ErrorReport::new(
            "Could not reload the file",
            e,
        ));
    }

    if let Some(kind) = toggle_max {
        app.maximized = if app.maximized == Some(kind) {
            None
        } else {
            Some(kind)
        };
        app.tiling.retile();
        app.raise = Some(kind);
    }
    if user_moved && toggle_max.is_none() {
        app.tiling.auto = false;
    }
    if let Some(kind) = pressed {
        app.focused = Some(kind);
    }
    if let Some(kind) = app.raise.take() {
        ctx.move_to_top(egui::LayerId::new(Order::Middle, win_id(kind)));
    }
    for kind in closes {
        match kind {
            WindowKind::Session(id) => app.close_session(id),
            WindowKind::MultiSession(id) => app.close_multi_session(id),
            WindowKind::Editor(id) => app.request_close_editor(id),
            WindowKind::Guide => app.close_guide(),
        }
    }
    match guide_action {
        Some(guide::GuideAction::CheckAgain) => app.recheck_providers(),
        Some(guide::GuideAction::OpenFolder) => app.open_chosen_folder(),
        None => {}
    }
    for (id, title) in multi_renames {
        app.rename_multi_session(id, title);
    }
    for (id, img) in images {
        app.attach_image(id, img);
    }
    for id in indexes {
        app.index_files(id);
    }
    for id in attaches {
        app.attach_dialog(id);
    }
    for id in reloads {
        app.reload_session(id);
    }
    for (id, to) in forks {
        app.fork_session(id, to);
    }
    for (id, title) in renames {
        app.rename_session(id, title);
    }
    for path in opens {
        app.open_path(path);
    }
}

/// One session window; `applying` forces its rect for this frame only.
fn window<'a>(
    id: Id,
    fill: egui::Color32,
    collapsed: bool,
    area: Rect,
    applying: Option<Rect>,
    default_pos: egui::Pos2,
) -> egui::Window<'a> {
    let win = egui::Window::new("session")
        .id(id)
        .title_bar(false)
        // While the size is being forced the window's previous size may still be wrong, and
        // constraining against it would shove the window off its tile.
        .constrain(false)
        .min_size([100.0, if collapsed { 20.0 } else { 80.0 }])
        .resizable(!collapsed)
        .frame(
            egui::Frame::new()
                .fill(fill)
                .stroke(egui::Stroke::new(1.0, theme::border()))
                .corner_radius(8),
        );
    match applying {
        Some(r) => win.current_pos(r.min).fixed_size(r.size()),
        None => win
            .constrain_to(area)
            .default_size(DEFAULT_SIZE)
            .default_pos(default_pos),
    }
}

/// Draws the session's diff modal, when open, and applies its decision.
fn diff_modal_for(
    ctx: &egui::Context,
    id: SessionId,
    vm: &mut crate::app::SessionVm,
    env: &Env,
) -> Option<PathBuf> {
    let diff = vm.diff.as_ref()?;
    let pending = vm
        .pending_permission
        .as_ref()
        .filter(|p| p.tool_use_id == diff.tool_use_id)
        .map(|p| p.request_id.clone());
    let rel = diff
        .data
        .path
        .strip_prefix(&vm.cwd)
        .unwrap_or(&diff.data.path)
        .display()
        .to_string();
    let action = diff_modal::show(
        ctx,
        Id::unique(("diff", id)),
        &diff.data,
        &rel,
        pending.is_some(),
        env.vscode,
    );
    let mut open = None;
    let decision = match action {
        ModalAction::Accept => Some((true, false)),
        ModalAction::AcceptAuto => Some((true, true)),
        ModalAction::Cancel => Some((false, false)),
        ModalAction::Close => {
            vm.diff = None;
            None
        }
        ModalAction::OpenInXade => {
            open = vm.diff.take().map(|d| d.data.path);
            None
        }
        ModalAction::None => None,
    };
    if let (Some((allow, enable_auto)), Some(request_id)) = (decision, pending) {
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
        vm.diff = None;
    }
    open
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{DiffState, PendingPermission, SessionVm};
    use crate::diff::DiffData;
    use egui_commonmark::CommonMarkCache;
    use time::OffsetDateTime;
    use xade_core::{
        Effort, Message, ProviderId, Role, Session, SessionStatus, UsageTotals, WorkspaceId,
    };

    fn msg(role: Role, text: &str, tool: Option<(&str, &str)>, payload: Option<&str>) -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            session_id: SessionId::new(),
            seq: 1,
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

    fn vm() -> SessionVm {
        let now = OffsetDateTime::now_utc();
        let session = Session {
            origin: xade_core::SessionOrigin::default(),
            id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            provider_id: ProviderId::Claude,
            provider_session_id: None,
            title: "Fix the thing".into(),
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
        };
        let edit = r#"{"file_path":"/tmp/nope/a.rs","old_string":"a","new_string":"b"}"#;
        let patch = r#"{"structuredPatch":[{"oldStart":3,"newStart":3,"lines":["-a","+b"]}]}"#;
        SessionVm {
            session,
            cwd: "/tmp/nope".into(),
            status: SessionStatus::NeedsApproval,
            queued: Default::default(),
            messages: vec![
                msg(
                    Role::User,
                    "hello `code`\n\n```rust\nfn x() {}\n```",
                    None,
                    None,
                ),
                msg(
                    Role::Assistant,
                    "Sure:\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```sh\nls",
                    None,
                    None,
                ),
                msg(
                    Role::ToolCall,
                    "",
                    Some(("Bash", "t1")),
                    Some(r#"{"command":"ls"}"#),
                ),
                msg(Role::ToolResult, "a.rs", Some(("Bash", "t1")), None),
                msg(Role::ToolCall, "", Some(("Edit", "t2")), Some(edit)),
                msg(Role::ToolResult, "ok", Some(("Edit", "t2")), Some(patch)),
                msg(
                    Role::ToolCall,
                    "",
                    Some(("Write", "t3")),
                    Some(r#"{"file_path":"/tmp/nope/b.txt","content":"x"}"#),
                ),
            ],
            live_text: "streaming ```".into(),
            thinking_tokens: Some(42),
            pending_permission: Some(PendingPermission {
                request_id: "r".into(),
                tool_use_id: "t3".into(),
                tool_name: "Write".into(),
                input_json: String::new(),
                reviewable: false,
            }),
            input: "draft".into(),
            error: Some("boom".into()),
            md_cache: CommonMarkCache::default(),
            path_links: Default::default(),
            diff: DiffData::build("Edit", edit, Some(patch), Path::new("/tmp/nope")).map(|data| {
                DiffState {
                    tool_use_id: "t2".into(),
                    data,
                }
            }),
            changes: Default::default(),
            settings_pending: true,
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

    fn frame(ctx: &egui::Context, run: impl FnMut(&egui::Context)) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        ctx.run_pass(input, run).drop_without_applying_deltas();
    }

    #[test]
    fn maximized_window_fills_the_area_below_collapsed_headers() {
        let area = Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(900.0, 600.0));
        let l = layout(area, 3, Some(1));
        assert!(l[0].1 && !l[1].1 && l[2].1);
        assert_eq!(l[1].0.right_bottom(), area.right_bottom());
        assert_eq!(l[1].0.left(), area.left());
        assert!(l[1].0.top() >= l[0].0.bottom());
        assert_eq!(layout(area, 1, Some(0)), vec![(area, false)]);
        assert!(layout(area, 2, None).iter().all(|(_, c)| !c));
    }

    /// `(columns, rows)` of a tiling: tiles on the first row, distinct row tops.
    fn shape(tiles: &[Rect]) -> (usize, usize) {
        let cols = tiles.iter().filter(|r| r.top() == tiles[0].top()).count();
        let mut tops: Vec<i32> = tiles.iter().map(|r| r.top().round() as i32).collect();
        tops.dedup();
        (cols, tops.len())
    }

    #[test]
    fn grid_shapes_fill_a_roomy_area() {
        let area = Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(2000.0, 1200.0));
        let want = [
            (1, (1, 1)),
            (2, (2, 1)),
            (3, (3, 1)),
            (4, (2, 2)),
            (5, (3, 2)),
            (6, (3, 2)),
            (7, (3, 3)),
            (9, (3, 3)),
            (10, (4, 3)),
            (17, (5, 4)),
        ];
        for (n, cols_rows) in want {
            let tiles = tile_rects(area, n);
            assert_eq!(shape(&tiles), cols_rows, "n={n}");
            assert!(tiles.iter().all(|r| area.expand(0.01).contains_rect(*r)));
            // Every row spans the area; nothing overlaps.
            for (i, a) in tiles.iter().enumerate() {
                for b in &tiles[i + 1..] {
                    let o = a.intersect(*b);
                    assert!(o.width() < 0.5 || o.height() < 0.5, "n={n}: {a:?} {b:?}");
                }
            }
        }
        let five = tile_rects(area, 5);
        assert!((five[2].right() - area.right()).abs() < 0.01);
        assert!((five[4].right() - area.right()).abs() < 0.01);
        assert!((five[4].bottom() - area.bottom()).abs() < 0.01);
    }

    #[test]
    fn narrow_areas_drop_columns_and_tiny_ones_overlap_rows() {
        let min = |r: &Rect| r.width() >= MIN_TILE[0] - 0.5 && r.height() >= MIN_TILE[1] - 0.5;
        // Room for two 380 px columns only: 3 windows become 2x2 slots, none under the minimum.
        let area = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 800.0));
        let tiles = tile_rects(area, 3);
        assert_eq!(shape(&tiles), (2, 2));
        assert!(tiles.iter().all(min));
        // Short and wide: 4 windows prefer one row over four squashed ones.
        let wide = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1700.0, 500.0));
        let tiles = tile_rects(wide, 4);
        assert_eq!(shape(&tiles), (4, 1));
        assert!(tiles.iter().all(min));
        // One column, not enough height: minimum size kept, rows overlap evenly inside the area.
        let tiny = Rect::from_min_size(egui::pos2(5.0, 5.0), egui::vec2(400.0, 700.0));
        let tiles = tile_rects(tiny, 4);
        assert!(tiles.iter().all(min));
        assert!(tiles.iter().all(|r| tiny.expand(0.01).contains_rect(*r)));
        assert_eq!(tiles[0].top(), tiny.top());
        assert!((tiles[3].bottom() - tiny.bottom()).abs() < 0.01);
        assert!(tiles[1].top() > tiles[0].top());
        // Narrower than a tile: it takes the whole width.
        let thin = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(300.0, 900.0));
        assert_eq!(tile_rects(thin, 1)[0], thin);
    }

    #[test]
    fn tiled_windows_land_on_their_tiles_and_stay_there() {
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
        let area = Rect::from_min_size(egui::pos2(20.0, 50.0), egui::vec2(1200.0, 700.0));
        let tiles = tile_rects(area, 3);
        let mut vms: Vec<SessionVm> = (0..3).map(|_| vm()).collect();
        for f in 0..6 {
            frame(&ctx, |ctx| {
                for (i, vm) in vms.iter_mut().enumerate() {
                    let applying = (f == 0).then_some(tiles[i]);
                    let win = window(
                        win_id(WindowKind::Session(vm.session.id)),
                        theme::bg_window(),
                        false,
                        area,
                        applying,
                        area.min,
                    );
                    let resp = win.show(ctx, |ui| {
                        session_view::show(ui, vm, &env);
                    });
                    let rect = resp.map(|r| r.response.rect).unwrap_or(Rect::NOTHING);
                    if f >= 2 {
                        assert!(
                            !differs(rect, tiles[i]),
                            "frame {f} window {i}: {rect:?} vs {:?}",
                            tiles[i]
                        );
                    }
                }
            });
        }
    }

    #[test]
    fn session_view_renders_banner_modal_and_collapsed_header() {
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
        let mut v = vm();
        let area = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 700.0));
        for f in 0..4 {
            v.collapsed = f == 3;
            frame(&ctx, |ctx| {
                let win = window(
                    win_id(WindowKind::Session(v.session.id)),
                    theme::bg_window(),
                    v.collapsed,
                    area,
                    None,
                    area.min,
                );
                win.show(ctx, |ui| {
                    session_view::show(ui, &mut v, &env);
                });
                let _ = diff_modal_for(ctx, v.session.id, &mut v, &env);
            });
        }
    }

    #[test]
    fn sessions_and_editors_share_the_grid_in_open_order() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-ws-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        let (s1, s2) = (vm(), vm());
        let (id1, id2) = (s1.session.id, s2.session.id);
        app.sessions.insert(id1, s1);
        app.sessions.insert(id2, s2);
        let e = app.editors.create(None, "# hi\n".into());
        app.focus(WindowKind::Session(id1));
        app.focus(WindowKind::Editor(e));
        app.focus(WindowKind::Session(id2));
        let order = [
            WindowKind::Session(id1),
            WindowKind::Editor(e),
            WindowKind::Session(id2),
        ];
        assert_eq!(app.open_windows, order);
        let screen = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
        for _ in 0..6 {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            ctx.run_ui(input, |ui| show(&mut app, ui))
                .drop_without_applying_deltas();
        }
        let tiles = tile_rects(screen, 3);
        let rects = [
            app.sessions[&id1].window_rect,
            app.editors.map[&e].window_rect,
            app.sessions[&id2].window_rect,
        ];
        for (i, r) in rects.iter().enumerate() {
            let r = r.expect("drawn");
            assert!(!differs(r, tiles[i]), "window {i}: {r:?} vs {:?}", tiles[i]);
        }
        // Closing the editor re-tiles the sessions into two tiles.
        app.request_close_editor(e);
        assert_eq!(app.open_windows.len(), 2);
        assert!(app.tiling.auto);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fork_is_offered_only_when_idle_with_history_and_a_target() {
        let both = [ProviderId::Claude, ProviderId::Antigravity];
        let mut v = vm();
        v.status = SessionStatus::Idle;
        assert!(!v.messages.is_empty());
        assert_eq!(session_view::fork_blocker(&v, &both), None);
        assert!(session_view::fork_blocker(&v, &[]).is_some());
        for busy in [
            SessionStatus::Streaming,
            SessionStatus::NeedsApproval,
            SessionStatus::Starting,
            SessionStatus::Error("x".into()),
        ] {
            v.status = busy;
            assert!(session_view::fork_blocker(&v, &both).is_some());
        }
        v.status = SessionStatus::Idle;
        v.messages.clear();
        assert!(session_view::fork_blocker(&v, &both).is_some());
    }

    #[test]
    fn fork_stays_off_while_a_fork_is_preparing() {
        let both = [ProviderId::Claude, ProviderId::Antigravity];
        let mut v = vm();
        v.status = SessionStatus::Idle;
        assert_eq!(session_view::fork_blocker(&v, &both), None);
        v.fork_preparing = true;
        assert!(session_view::fork_blocker(&v, &both)
            .is_some_and(|why| why.starts_with("Preparing fork")));
    }

    #[test]
    fn header_badges_share_one_muted_color_and_sit_right_of_the_title() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        // The badge's color and left edge, and the title's right edge, in one header.
        let probe = |origin: xade_core::SessionOrigin, focused: bool, label: &str| {
            let mut v = vm();
            v.session.origin = origin;
            v.collapsed = true;
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
                focused,
            };
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(600.0, 200.0),
                )),
                ..Default::default()
            };
            let out = ctx.run_ui(input, |ui| {
                session_view::show(ui, &mut v, &env);
            });
            // (color, rect) of the text shape whose text is `needle`.
            fn find(s: &egui::Shape, needle: &str) -> Option<(egui::Color32, Rect)> {
                match s {
                    egui::Shape::Text(t) if t.galley.text().contains(needle) => {
                        let color = t.galley.job.sections.first()?.format.color;
                        Some((color, Rect::from_min_size(t.pos, t.galley.size())))
                    }
                    egui::Shape::Vec(v) => v.iter().find_map(|c| find(c, needle)),
                    _ => None,
                }
            }
            let badge = out.shapes.iter().find_map(|c| find(&c.shape, label));
            let title = out
                .shapes
                .iter()
                .find_map(|c| find(&c.shape, "Fix the thing"));
            out.drop_without_applying_deltas();
            (badge.expect("badge drawn"), title.expect("title drawn"))
        };
        for (origin, label) in [
            (xade_core::SessionOrigin::Xade, "Xade"),
            (xade_core::SessionOrigin::ClaudeCode, "Claude Code"),
            (xade_core::SessionOrigin::Codex, "Codex"),
        ] {
            for focused in [true, false] {
                let ((color, badge), (_, title)) = probe(origin, focused, label);
                assert_eq!(color, theme::text_muted(), "{label} focused={focused}");
                assert!(
                    badge.left() > title.right(),
                    "{label}: badge beside the buttons"
                );
                // Right beside the window buttons, not mid-strip.
                assert!(badge.right() > 400.0, "{label} badge at {badge:?}");
            }
        }
    }

    /// Drives the collapsed header of `v` over frames: click the title, type, press a key.
    fn rename_run(
        origin: xade_core::SessionOrigin,
        steps: impl Fn(egui::Pos2) -> Vec<Vec<egui::Event>>,
    ) -> (SessionVm, Vec<Option<String>>) {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut v = vm();
        v.session.origin = origin;
        v.collapsed = true;
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
            focused: true,
        };
        let mut renames = Vec::new();
        let mut frame = |v: &mut SessionVm, evs: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(600.0, 200.0),
                )),
                events: evs,
                ..Default::default()
            };
            let mut found = None;
            let out = ctx.run_ui(input, |ui| {
                renames.push(session_view::show(ui, v, &env).rename);
            });
            fn find(s: &egui::Shape, needle: &str, at: &mut Option<egui::Pos2>) {
                match s {
                    egui::Shape::Text(t) if t.galley.text().contains(needle) => {
                        *at = Some(egui::Rect::from_min_size(t.pos, t.galley.size()).center());
                    }
                    egui::Shape::Vec(v) => v.iter().for_each(|c| find(c, needle, at)),
                    _ => {}
                }
            }
            out.shapes
                .iter()
                .for_each(|c| find(&c.shape, "Fix the thing", &mut found));
            out.drop_without_applying_deltas();
            found
        };
        // Two frames so the layout settles, then find the title.
        frame(&mut v, vec![]);
        let at = frame(&mut v, vec![]).expect("title drawn");
        for evs in steps(at) {
            frame(&mut v, evs);
            // A settling frame after each step, like real input arriving between frames.
            frame(&mut v, vec![]);
        }
        (v, renames)
    }

    fn click(at: egui::Pos2) -> Vec<egui::Event> {
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        vec![egui::Event::PointerMoved(at), button(true), button(false)]
    }

    #[test]
    fn clicking_an_xade_session_title_starts_editing_it() {
        let (v, _) = rename_run(xade_core::SessionOrigin::Xade, |at| vec![click(at)]);
        assert_eq!(v.renaming.as_deref(), Some("Fix the thing"));
    }

    #[test]
    fn an_imported_session_title_stays_read_only() {
        for origin in [
            xade_core::SessionOrigin::ClaudeCode,
            xade_core::SessionOrigin::Codex,
        ] {
            let (v, renames) = rename_run(origin, |at| vec![click(at)]);
            assert_eq!(v.renaming, None, "{origin:?}");
            assert!(renames.iter().all(Option::is_none), "{origin:?}");
        }
    }

    fn key(k: egui::Key) -> Vec<egui::Event> {
        vec![egui::Event::Key {
            key: k,
            physical_key: Some(k),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]
    }

    fn typed(text: &str) -> Vec<egui::Event> {
        vec![egui::Event::Text(text.into())]
    }

    #[test]
    fn enter_commits_the_typed_title_replacing_the_selected_old_one() {
        let (v, renames) = rename_run(xade_core::SessionOrigin::Xade, |at| {
            vec![click(at), typed("Better name"), key(egui::Key::Enter)]
        });
        assert_eq!(v.renaming, None);
        let got: Vec<_> = renames.into_iter().flatten().collect();
        assert_eq!(got, vec!["Better name".to_string()]);
    }

    #[test]
    fn escape_cancels_the_edit() {
        let (v, renames) = rename_run(xade_core::SessionOrigin::Xade, |at| {
            vec![click(at), typed("Nope"), key(egui::Key::Escape)]
        });
        assert_eq!(v.renaming, None);
        assert!(renames.iter().all(Option::is_none));
    }

    #[test]
    fn clicking_away_commits_the_edit() {
        let (v, renames) = rename_run(xade_core::SessionOrigin::Xade, |at| {
            vec![click(at), typed("Away"), click(egui::pos2(300.0, 150.0))]
        });
        assert_eq!(v.renaming, None);
        let got: Vec<_> = renames.into_iter().flatten().collect();
        assert_eq!(got, vec!["Away".to_string()]);
    }

    /// Whether a drag and double clicks on a session header title would open the Copy / Quote
    /// popup (a settled selection outside a marked text box).
    fn header_selection_settles(origin: xade_core::SessionOrigin) -> bool {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut v = vm();
        v.session.origin = origin;
        v.collapsed = true;
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
            focused: true,
        };
        let mut settled = false;
        let mut at = None;
        let mut frame =
            |v: &mut SessionVm, events: Vec<egui::Event>, at: &mut Option<egui::Pos2>| {
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(600.0, 200.0),
                    )),
                    events,
                    ..Default::default()
                };
                let mut marked = false;
                let out = ctx.run_ui(input, |ui| {
                    session_view::show(ui, v, &env);
                    marked = crate::widgets::selection_menu::in_text_box(ui.ctx());
                });
                // A settled selection the app would turn into the popup: not from a text box.
                settled |= !marked
                    && out
                        .platform_output
                        .commands
                        .iter()
                        .any(|c| matches!(c, egui::OutputCommand::TextSelectionSettled(_)));
                fn find(s: &egui::Shape, at: &mut Option<egui::Pos2>) {
                    match s {
                        egui::Shape::Text(t) if t.galley.text().contains("Fix the thing") => {
                            *at = Some(t.pos);
                        }
                        egui::Shape::Vec(v) => v.iter().for_each(|c| find(c, at)),
                        _ => {}
                    }
                }
                out.shapes.iter().for_each(|c| find(&c.shape, at));
                out.drop_without_applying_deltas();
            };
        frame(&mut v, vec![], &mut at);
        frame(&mut v, vec![], &mut at);
        let start = at.expect("title drawn") + egui::vec2(4.0, 6.0);
        let end = start + egui::vec2(60.0, 0.0);
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        // A drag across the title, then a double click on it.
        frame(
            &mut v,
            vec![egui::Event::PointerMoved(start), button(start, true)],
            &mut at,
        );
        frame(&mut v, vec![egui::Event::PointerMoved(end)], &mut at);
        frame(&mut v, vec![button(end, false)], &mut at);
        for _ in 0..2 {
            frame(
                &mut v,
                vec![button(start, true), button(start, false)],
                &mut at,
            );
        }
        frame(&mut v, vec![], &mut at);
        settled
    }

    #[test]
    fn selecting_the_window_top_bar_text_never_offers_the_selection_menu() {
        for origin in [
            xade_core::SessionOrigin::Xade,
            xade_core::SessionOrigin::ClaudeCode,
            xade_core::SessionOrigin::Codex,
        ] {
            assert!(!header_selection_settles(origin), "{origin:?}");
        }
    }
}
