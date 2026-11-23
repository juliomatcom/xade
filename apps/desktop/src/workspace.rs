//! Workspace view: one `egui::Window` per open session, auto-tiled into a grid.

use std::path::Path;
use std::sync::Arc;

use egui::{Id, Order, Rect};
use tokio::sync::mpsc::UnboundedSender;
use xade_core::{AgentProvider, SessionId};
use xade_runtime::ManagerCmd;

use crate::app::App;
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

/// What session views need from `App` while their `SessionVm` is borrowed out of it.
pub struct Env<'a> {
    pub cmd_tx: &'a UnboundedSender<ManagerCmd>,
    pub providers: &'a [Arc<dyn AgentProvider>],
    pub vscode: Option<&'a Path>,
    pub maximized: Option<SessionId>,
}

/// Auto-tiling state: windows are re-tiled while `auto` and the set of windows or the area changes.
pub struct Tiling {
    pub auto: bool,
    ids: Vec<SessionId>,
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
    let ids: Vec<SessionId> = app
        .open_windows
        .iter()
        .copied()
        .filter(|id| app.sessions.contains_key(id))
        .collect();
    if ids.is_empty() {
        ui.label(egui::RichText::new("Opening session…").color(theme::TEXT_MUTED));
        return;
    }
    let tiles = layout(
        area,
        ids.len(),
        app.maximized
            .and_then(|m| ids.iter().position(|id| *id == m)),
    );
    let retile = app.tiling.auto
        && (app.tiling.ids != ids
            || differs(app.tiling.area, area)
            || app.tiling.area.is_negative());
    if retile {
        app.tiling.ids = ids.clone();
        app.tiling.area = area;
        for (id, (rect, collapsed)) in ids.iter().zip(&tiles) {
            if let Some(vm) = app.sessions.get_mut(id) {
                vm.apply_rect = Some(*rect);
                vm.collapsed = *collapsed;
            }
        }
    }

    let mut closes = Vec::new();
    let mut toggle_max = None;
    let mut user_moved = false;
    let mut pressed = None;
    let mut prev_min = area.min;
    let pointer_down = ctx.input(|i| i.pointer.any_pressed());
    let press_layer = ctx
        .pointer_interact_pos()
        .filter(|_| pointer_down)
        .and_then(|p| ctx.layer_id_at(p));
    for (i, id) in ids.iter().enumerate() {
        let Some(mut vm) = app.sessions.remove(id) else {
            continue;
        };
        let env = Env {
            cmd_tx: &app.cmd_tx,
            providers: &app.providers,
            vscode: app.vscode.as_deref(),
            maximized: app.maximized,
        };
        let applying = vm.apply_rect.take();
        let default_pos = if i > 0 {
            prev_min + egui::vec2(CASCADE, CASCADE)
        } else {
            prev_min
        };
        let win = window(*id, vm.collapsed, area, applying, default_pos);
        let mut out = session_view::ViewOut::default();
        let resp = win.show(&ctx, |ui| out = session_view::show(ui, &mut vm, &env));
        if let Some(resp) = &resp {
            let rect = resp.response.rect;
            prev_min = rect.min;
            if applying.is_none() && !vm.collapsed {
                vm.window_rect = Some(rect);
                if app.tiling.auto && !retile && differs(rect, tiles[i].0) {
                    user_moved = true;
                }
            }
            if press_layer == Some(resp.response.layer_id) {
                pressed = Some(*id);
            }
        }
        if out.maximize {
            toggle_max = Some(*id);
        } else if out.collapse && app.maximized.is_some_and(|m| m != *id) {
            // Expanding a minimized window while another is maximized hands the maximize over.
            toggle_max = Some(*id);
        } else if out.collapse {
            let rect = vm.window_rect.unwrap_or(tiles[i].0);
            if vm.collapsed {
                vm.apply_rect = Some(rect);
            } else {
                vm.apply_rect = Some(Rect::from_min_size(
                    rect.min,
                    egui::vec2(rect.width(), session_view::HEADER_HEIGHT),
                ));
            }
            vm.collapsed = !vm.collapsed;
        }
        if out.close {
            closes.push(*id);
        }
        diff_modal_for(&ctx, *id, &mut vm, &env);
        app.sessions.insert(*id, vm);
    }

    if let Some(id) = toggle_max {
        app.maximized = if app.maximized == Some(id) {
            None
        } else {
            Some(id)
        };
        app.tiling.retile();
        app.raise = Some(id);
    }
    if user_moved && toggle_max.is_none() {
        app.tiling.auto = false;
    }
    if let Some(id) = pressed {
        app.focused = Some(id);
    }
    if let Some(id) = app.raise.take() {
        ctx.move_to_top(egui::LayerId::new(Order::Middle, Id::unique(id)));
    }
    for id in closes {
        app.close_session(id);
    }
}

/// One session window; `applying` forces its rect for this frame only.
fn window<'a>(
    id: SessionId,
    collapsed: bool,
    area: Rect,
    applying: Option<Rect>,
    default_pos: egui::Pos2,
) -> egui::Window<'a> {
    let win = egui::Window::new("session")
        .id(Id::unique(id))
        .title_bar(false)
        // While the size is being forced the window's previous size may still be wrong, and
        // constraining against it would shove the window off its tile.
        .constrain(false)
        .min_size([100.0, if collapsed { 20.0 } else { 80.0 }])
        .resizable(!collapsed)
        .frame(
            egui::Frame::new()
                .fill(theme::BG_WINDOW)
                .stroke(egui::Stroke::new(1.0, theme::BORDER))
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
fn diff_modal_for(ctx: &egui::Context, id: SessionId, vm: &mut crate::app::SessionVm, env: &Env) {
    let Some(diff) = &vm.diff else { return };
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
    let decision = match action {
        ModalAction::Accept => Some((true, false)),
        ModalAction::AcceptAuto => Some((true, true)),
        ModalAction::Cancel => Some((false, false)),
        ModalAction::Close => {
            vm.diff = None;
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
            auto_mode: false,
            usage: UsageTotals::default(),
            created_at: now,
            updated_at: now,
        };
        let edit = r#"{"file_path":"/tmp/nope/a.rs","old_string":"a","new_string":"b"}"#;
        let patch = r#"{"structuredPatch":[{"oldStart":3,"newStart":3,"lines":["-a","+b"]}]}"#;
        SessionVm {
            session,
            cwd: "/tmp/nope".into(),
            status: SessionStatus::NeedsApproval,
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
            diff: DiffData::build("Edit", edit, Some(patch), Path::new("/tmp/nope")).map(|data| {
                DiffState {
                    tool_use_id: "t2".into(),
                    data,
                }
            }),
            settings_pending: true,
            collapsed: false,
            window_rect: None,
            apply_rect: None,
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
            vscode: None,
            maximized: None,
        };
        let area = Rect::from_min_size(egui::pos2(20.0, 50.0), egui::vec2(1200.0, 700.0));
        let tiles = tile_rects(area, 3);
        let mut vms: Vec<SessionVm> = (0..3).map(|_| vm()).collect();
        for f in 0..6 {
            frame(&ctx, |ctx| {
                for (i, vm) in vms.iter_mut().enumerate() {
                    let applying = (f == 0).then_some(tiles[i]);
                    let win = window(vm.session.id, false, area, applying, area.min);
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
            vscode: None,
            maximized: None,
        };
        let mut v = vm();
        let area = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 700.0));
        for f in 0..4 {
            v.collapsed = f == 3;
            frame(&ctx, |ctx| {
                let win = window(v.session.id, v.collapsed, area, None, area.min);
                win.show(ctx, |ui| {
                    session_view::show(ui, &mut v, &env);
                });
                diff_modal_for(ctx, v.session.id, &mut v, &env);
            });
        }
    }
}
