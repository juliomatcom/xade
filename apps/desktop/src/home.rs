//! Home view and the shell panels it shares with the workspace view (top bar, sidebar).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Align, Layout, RichText};
use time::OffsetDateTime;
use xade_core::{QuotaKind, QuotaWindow, SessionOrigin, SessionStatus, Workspace, WorkspaceId};
use xade_runtime::git::GitState;
use xade_store::SessionCard;

use crate::app::{App, View};
use crate::file_tree::{self, TreeAction};
use crate::keymap::{self, Action};
use crate::{icons, running, theme};

/// The home grid is `GRID` x `GRID` tiles per page.
const GRID: usize = 3;
pub const PAGE_SIZE: u32 = (GRID * GRID) as u32;
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);
const CARD_GAP: f32 = 12.0;
const CARD_PAD: f32 = 12.0;
const ROW_GAP: f32 = 4.0;
const SIDEBAR_DEFAULT: f32 = 220.0;
const SIDEBAR_MIN: f32 = 160.0;
/// Below this inner width the import button shows a short label.
const IMPORT_FULL_LABEL_WIDTH: f32 = 190.0;

#[derive(Default)]
pub struct HomeState {
    pub workspaces: Vec<Workspace>,
    /// `None` is "All Workspaces".
    pub scope: Option<WorkspaceId>,
    pub search: String,
    pub search_changed_at: Option<Instant>,
    /// Zero-based.
    pub page: u32,
    pub cards: Vec<SessionCard>,
    pub total: u64,
    /// Bumped per query; stale results are dropped.
    pub generation: u64,
    pub preflight_error: Option<String>,
    /// `None` lists every origin.
    pub origin: Option<SessionOrigin>,
    pub importing: bool,
    /// `(imported, updated)` of the last finished import.
    pub import_result: Option<(u32, u32)>,
}

/// Room the provider dropdown and the right-hand cluster (buttons, quota meters) take; the
/// top-bar test fails if they outgrow it.
const TOP_BAR_RESERVED: f32 = 108.0 + 704.0 + 90.0;

/// The search field shrinks with the space left of `TOP_BAR_RESERVED`.
fn search_width(available: f32) -> f32 {
    (available - TOP_BAR_RESERVED).clamp(160.0, 320.0)
}

/// The Workspace / Home toggle, boxed and centered in the top bar so the two read as one control.
fn view_toggle(app: &mut App, ui: &mut egui::Ui, area: egui::Rect) {
    let id = egui::Id::unique("view_toggle_width");
    let width = ui.data(|d| d.get_temp::<f32>(id)).unwrap_or(190.0);
    // The gap between the clusters as of the previous frame; centered on the bar when it
    // fits there, else on the gap.
    let (lo, hi) = ui
        .data(|d| d.get_temp::<(f32, f32)>(egui::Id::unique("top_bar_gap")))
        .map_or((area.left(), area.right()), |(l, r)| (l + 12.0, r - 12.0));
    let half = width / 2.0;
    let x = if lo + half <= hi - half {
        area.center().x.clamp(lo + half, hi - half)
    } else {
        (lo + hi) / 2.0
    };
    let rect = egui::Rect::from_center_size(
        egui::pos2(x, area.center().y),
        egui::vec2(width, area.height()),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let used = egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, theme::BORDER_STRONG))
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(4, 2))
        .show(&mut child, |ui| {
            let to_ws = ui
                .selectable_label(
                    app.view == View::Workspace,
                    icons::with(icons::WORKSPACE, "Workspace"),
                )
                .clicked();
            let to_home = ui
                .selectable_label(app.view == View::Home, icons::with(icons::HOME, "Home"))
                .on_hover_text(format!(
                    "Home / Workspace ({})",
                    keymap::shortcut_text(Action::ToggleView)
                ))
                .clicked();
            if to_home {
                app.view = View::Home;
            } else if to_ws && !app.open_windows.is_empty() {
                app.view = View::Workspace;
            }
        })
        .response
        .rect
        .width();
    ui.data_mut(|d| d.insert_temp(id, used));
}

pub fn top_bar(app: &mut App, ui: &mut egui::Ui) {
    let frame = egui::Frame::new()
        .fill(theme::BG_APP)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .inner_margin(egui::Margin::symmetric(12, 8));
    egui::Panel::top("top_bar").frame(frame).show(ui, |ui| {
        let area = ui.max_rect();
        view_toggle(app, ui, area);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Xade").strong().size(15.0));
            scope_dropdown(app, ui);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut app.home.search)
                    .hint_text("Fuzzy search sessions (try typos)")
                    .desired_width(search_width(ui.available_width())),
            );
            if resp.changed() {
                app.home.search_changed_at = Some(Instant::now());
            }
            provider_dropdown(ui);
            let left_end = ui.min_rect().right();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .button(icons::SHORTCUTS)
                    .on_hover_text(format!(
                        "Keyboard shortcuts ({})",
                        keymap::shortcut_text(Action::Shortcuts)
                    ))
                    .clicked()
                {
                    app.keymaps_open = true;
                }
                let can_tile = app.view == View::Workspace && !app.open_windows.is_empty();
                if ui
                    .add_enabled(
                        can_tile,
                        egui::Button::new(icons::with(icons::TILE, "Tile")),
                    )
                    .on_hover_text("Re-tile windows into a grid")
                    .clicked()
                {
                    app.tiling.retile();
                }
                let enabled = app.home.scope.is_some() && app.home.preflight_error.is_none();
                let new = ui
                    .add_enabled(
                        enabled,
                        egui::Button::new(
                            RichText::new(icons::with(icons::NEW_SESSION, "New Session"))
                                .color(theme::TEXT),
                        )
                        .fill(theme::ACCENT),
                    )
                    .on_disabled_hover_text("Select a workspace in the sidebar first");
                let new = new.on_hover_text(format!(
                    "New session ({})",
                    keymap::shortcut_text(Action::NewSession)
                ));
                if new.clicked() {
                    app.new_session();
                }
                if ui
                    .button(icons::OPEN_SESSION)
                    .on_hover_text(format!(
                        "Open session ({})",
                        keymap::shortcut_text(Action::OpenPicker)
                    ))
                    .clicked()
                {
                    app.run_action(Action::OpenPicker);
                }
                for kind in [QuotaKind::SevenDay, QuotaKind::FiveHour] {
                    quota_meter(ui, kind, app.quota.iter().find(|q| q.kind == kind));
                }
                let right_start = ui.min_rect().left();
                ui.data_mut(|d| {
                    d.insert_temp(egui::Id::unique("top_bar_gap"), (left_end, right_start))
                });
            });
        });
    });
}

fn scope_dropdown(app: &mut App, ui: &mut egui::Ui) {
    let current = app
        .home
        .scope
        .map(|id| app.workspace_name(id))
        .unwrap_or_else(|| "All Workspaces".into());
    let mut picked = None;
    egui::ComboBox::from_id_salt("scope")
        .selected_text(current)
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(app.home.scope.is_none(), "All Workspaces")
                .clicked()
            {
                picked = Some(None);
            }
            for w in app.home.workspaces.iter().filter(|w| w.is_open) {
                let sel = app.home.scope == Some(w.id);
                if ui.selectable_label(sel, &w.display_name).clicked() {
                    picked = Some(Some(w.id));
                }
            }
        });
    if let Some(scope) = picked {
        set_scope(app, scope);
    }
}

fn set_scope(app: &mut App, scope: Option<WorkspaceId>) {
    if app.home.scope != scope {
        app.home.scope = scope;
        app.home.page = 0;
        app.refresh_cards();
    }
}

fn provider_dropdown(ui: &mut egui::Ui) {
    egui::ComboBox::from_id_salt("provider")
        .selected_text("Claude")
        .show_ui(ui, |ui| {
            let _ = ui.selectable_label(true, "Claude");
            ui.add_enabled(false, egui::Button::new("Antigravity"))
                .on_disabled_hover_text("Coming soon");
        });
}

/// Always drawn; `q` is `None` until the first quota report arrives.
fn quota_meter(ui: &mut egui::Ui, kind: QuotaKind, q: Option<&QuotaWindow>) {
    let label = match kind {
        QuotaKind::FiveHour => "5h Session Use",
        QuotaKind::SevenDay => "Weekly Quota",
    };
    // A fixed-size slot: inside the right-to-left top bar, an unsized `vertical`
    // lets both meters land on the same rect.
    let size = egui::vec2(150.0, ui.available_height());
    let now = OffsetDateTime::now_utc();
    ui.allocate_ui_with_layout(size, Layout::top_down(Align::Min), |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).small().color(theme::TEXT_MUTED));
            let used = q.map_or("\u{2014}".to_string(), |q| {
                format!("{:.0}% Used", quota_pct(q, now))
            });
            ui.label(RichText::new(used).small().color(theme::TEXT_MUTED));
        });
        let fraction = q.map_or(0.0, |q| (quota_pct(q, now) / 100.0).clamp(0.0, 1.0));
        ui.add(
            egui::ProgressBar::new(fraction)
                .desired_width(140.0)
                .desired_height(4.0)
                .fill(theme::usage_color(fraction)),
        );
        if let Some(text) = q.and_then(|q| reset_text(q, now)) {
            ui.label(RichText::new(text).small().color(theme::TEXT_MUTED));
        }
    });
    ui.add_space(8.0);
}

/// A window whose reset time has passed holds no usage any more.
fn quota_pct(q: &QuotaWindow, now: OffsetDateTime) -> f32 {
    if q.resets_at.is_some_and(|at| at <= now) {
        0.0
    } else {
        q.utilization_pct
    }
}

fn reset_text(q: &QuotaWindow, now: OffsetDateTime) -> Option<String> {
    let at = q.resets_at?;
    Some(if at <= now {
        "reset".into()
    } else {
        format!("Resets in {}", duration_text((at - now).whole_seconds()))
    })
}

fn duration_text(secs: i64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

pub fn sidebar(app: &mut App, ui: &mut egui::Ui) {
    let frame = egui::Frame::new()
        .fill(theme::BG_SIDEBAR)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .inner_margin(egui::Margin::same(12));
    egui::Panel::left("sidebar")
        .frame(frame)
        .default_size(SIDEBAR_DEFAULT)
        .size_range(SIDEBAR_MIN..=(ui.available_width() * 0.4).max(SIDEBAR_MIN))
        .resizable(true)
        .show(ui, |ui| {
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                if ui
                    .add_sized(
                        [ui.available_width(), 28.0],
                        egui::Button::new(icons::with(icons::ADD_DIR, "Add Directory")).truncate(),
                    )
                    .clicked()
                {
                    app.add_directory();
                }
                import_button(app, ui);
                // bottom_up: later items sit higher, so the workspace list below takes all
                // remaining height.
                ui.add_space(8.0);
                ui.separator();
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    heading(ui, "Workspaces");
                    if ui
                        .selectable_label(app.home.scope.is_none(), "All Workspaces")
                        .clicked()
                    {
                        set_scope(app, None);
                    }
                    let rows: Vec<(WorkspaceId, PathBuf, String, usize)> = app
                        .home
                        .workspaces
                        .iter()
                        .filter(|w| w.is_open)
                        .map(|w| {
                            let name = w.display_name.clone();
                            (w.id, w.root_path.clone(), name, app.live_count(w.id))
                        })
                        .collect();
                    // Only expanded folders are walked and off-screen rows are not painted, so
                    // uneven row heights rule out `show_rows`.
                    let mut actions = Vec::new();
                    let mut close = None;
                    egui::ScrollArea::vertical()
                        .id_salt("workspaces")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (id, root, name, live) in &rows {
                                let chevron = if app.tree.is_expanded(root) {
                                    icons::EXPANDED
                                } else {
                                    icons::COLLAPSED
                                };
                                let name = if *live > 0 {
                                    format!("{name}  ({live})")
                                } else {
                                    name.clone()
                                };
                                let selected = app.home.scope == Some(*id);
                                let text = icons::with(chevron, &name);
                                let resp =
                                    ui.add(egui::Button::selectable(selected, text).truncate());
                                if resp.clicked() {
                                    set_scope(app, Some(*id));
                                    actions.push(TreeAction::Toggle(root.clone()));
                                }
                                resp.context_menu(|ui| {
                                    if ui.button("Close workspace").clicked() {
                                        close = Some(*id);
                                        ui.close();
                                    }
                                });
                                let changed = match app.git.get(root) {
                                    Some(GitState::Repo { changed, .. }) => Some(changed),
                                    _ => None,
                                };
                                file_tree::show(ui, &app.tree, root, 1, changed, &mut actions);
                            }
                        });
                    for action in actions {
                        app.tree_action(action);
                    }
                    if let Some(id) = close {
                        app.close_workspace(id);
                    }
                });
            });
        });
}

fn import_button(app: &mut App, ui: &mut egui::Ui) {
    let enabled = !app.home.importing && app.home.preflight_error.is_none();
    let narrow = ui.available_width() < IMPORT_FULL_LABEL_WIDTH;
    let label = if narrow {
        "Import"
    } else {
        "Import from Claude Code"
    };
    let btn = egui::Button::new(icons::with(icons::IMPORT, label)).truncate();
    let resp = ui
        .add_enabled_ui(enabled, |ui| {
            ui.add_sized([ui.available_width(), 28.0], btn)
        })
        .inner;
    let resp = if narrow {
        resp.on_hover_text("Import from Claude Code")
    } else {
        resp
    };
    if resp.clicked() {
        app.start_import();
    }
    if app.home.importing {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                RichText::new("Importing\u{2026}")
                    .small()
                    .color(theme::TEXT_MUTED),
            );
        });
    } else if let Some((imported, updated)) = app.home.import_result {
        ui.label(
            RichText::new(format!("Imported {imported}, updated {updated}"))
                .small()
                .color(theme::TEXT_MUTED),
        );
    }
}

fn origin_label(o: SessionOrigin) -> &'static str {
    match o {
        SessionOrigin::Xade => "Xade",
        SessionOrigin::ClaudeCode => "Claude Code",
    }
}

/// Small outlined tag naming where a session was created.
pub fn origin_badge(ui: &mut egui::Ui, origin: SessionOrigin) {
    let color = match origin {
        SessionOrigin::Xade => theme::ACCENT,
        SessionOrigin::ClaudeCode => theme::CLAUDE,
    };
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(6, 0))
        .show(ui, |ui| {
            ui.label(RichText::new(origin_label(origin)).small().color(color));
        });
}

fn origin_filter(app: &mut App, ui: &mut egui::Ui) {
    let mut picked = None;
    ui.horizontal(|ui| {
        for (label, value) in [
            ("All", None),
            ("Xade", Some(SessionOrigin::Xade)),
            ("Claude Code", Some(SessionOrigin::ClaudeCode)),
        ] {
            if ui
                .selectable_label(app.home.origin == value, label)
                .clicked()
            {
                picked = Some(value);
            }
        }
    });
    if let Some(origin) = picked.filter(|o| *o != app.home.origin) {
        app.home.origin = origin;
        app.home.page = 0;
        app.refresh_cards();
    }
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).small().color(theme::TEXT_MUTED));
}

pub fn grid(app: &mut App, ui: &mut egui::Ui) {
    if let Some(err) = app.home.preflight_error.clone() {
        egui::Frame::new()
            .fill(theme::BG_RAISED)
            .stroke(egui::Stroke::new(1.0, theme::DANGER))
            .corner_radius(6)
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(format!("Claude is not available: {err}")).color(theme::DANGER),
                );
                if ui.button(icons::with(icons::RETRY, "Retry")).clicked() {
                    app.retry_preflight();
                }
            });
        ui.add_space(12.0);
    }
    origin_filter(app, ui);
    let pages = app.home.total.div_ceil(u64::from(PAGE_SIZE)).max(1);
    egui::Panel::bottom("pager")
        .frame(egui::Frame::new())
        .show_separator_line(false)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let page = app.home.page;
                let mut next_page = None;
                if ui
                    .add_enabled(
                        page > 0,
                        egui::Button::new(icons::with(icons::PREV, "Prev")),
                    )
                    .clicked()
                {
                    next_page = Some(page.saturating_sub(1));
                }
                ui.label(
                    RichText::new(format!("Page {} of {}", page + 1, pages))
                        .color(theme::TEXT_MUTED),
                );
                if ui
                    .add_enabled(
                        u64::from(page) + 1 < pages,
                        egui::Button::new(icons::with(icons::NEXT, "Next")),
                    )
                    .clicked()
                {
                    next_page = Some(page + 1);
                }
                if let Some(p) = next_page {
                    app.home.page = p;
                    app.refresh_cards();
                }
            });
        });
    ui.add_space(CARD_GAP);
    let area = ui.available_rect_before_wrap();
    let mut open: Option<SessionCard> = None;
    let mut delete: Option<SessionCard> = None;
    if app.home.cards.is_empty() {
        let msg = if !app.home.search.trim().is_empty() {
            "No sessions match your search."
        } else if !app.home.workspaces.iter().any(|w| w.is_open) {
            "Add a directory to get started."
        } else {
            "No sessions yet. Select a workspace and start a new session."
        };
        ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
            ui.label(RichText::new(msg).color(theme::TEXT_MUTED));
        });
    }
    for (card, rect) in app.home.cards.iter().zip(tile_rects(area)) {
        match card_ui(app, ui, card, rect) {
            Some(CardClick::Open) => open = Some(card.clone()),
            Some(CardClick::Delete) => delete = Some(card.clone()),
            None => {}
        }
    }
    if let Some(card) = delete {
        app.request_delete(&card);
    }
    if let Some(card) = open {
        // Cmd+click keeps Home and just adds the window.
        if ui.input(|i| i.modifiers.command) {
            app.request_open(&card, true);
        } else {
            app.request_open(&card, false);
        }
    }
}

/// The 3x3 tile rects (row-major) that exactly fill `area` with `CARD_GAP` between them.
fn tile_rects(area: egui::Rect) -> impl Iterator<Item = egui::Rect> {
    let n = GRID as f32;
    let w = ((area.width() - CARD_GAP * (n - 1.0)) / n).max(0.0);
    let h = ((area.height() - CARD_GAP * (n - 1.0)) / n).max(0.0);
    (0..GRID * GRID).map(move |i| {
        let (col, row) = ((i % GRID) as f32, (i / GRID) as f32);
        let min = area.min + egui::vec2(col * (w + CARD_GAP), row * (h + CARD_GAP));
        egui::Rect::from_min_size(min, egui::vec2(w, h))
    })
}

/// Wrapped text of at most `max_rows` rows, ellipsized, never wider than `width`.
fn clipped_text(
    painter: &egui::Painter,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    width: f32,
    max_rows: usize,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), font, color, width);
    job.wrap.max_rows = max_rows;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('\u{2026}');
    painter.layout_job(job)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardClick {
    Open,
    Delete,
}

/// Side of the trash button in a card's bottom-right corner.
const TRASH_SIZE: f32 = 22.0;

/// Draws one card inside `rect`, clipped to it; returns what was clicked.
fn card_ui(
    app: &App,
    ui: &mut egui::Ui,
    card: &SessionCard,
    rect: egui::Rect,
) -> Option<CardClick> {
    let resp = ui
        .interact(
            rect,
            ui.make_persistent_id(("card", card.id)),
            egui::Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let fill = if resp.hovered() {
        theme::BG_HOVER
    } else {
        theme::BG_RAISED
    };
    ui.painter().rect(
        rect,
        6,
        fill,
        egui::Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Inside,
    );
    let inner = rect.shrink(CARD_PAD);
    let open = || resp.clicked().then_some(CardClick::Open);
    if inner.width() <= 0.0 || inner.height() <= 0.0 {
        return open();
    }
    let running = app.running_for(card);
    if let Some(r) = running {
        let dot = egui::Rect::from_center_size(
            egui::pos2(
                rect.right() - CARD_PAD / 2.0 - 1.0,
                rect.top() + CARD_PAD / 2.0 + 1.0,
            ),
            egui::Vec2::splat(CARD_PAD),
        );
        ui.interact(
            dot,
            ui.make_persistent_id(("run_dot", card.id)),
            egui::Sense::hover(),
        )
        .on_hover_text(running::tooltip(r));
        running::paint_dot(ui, dot.center(), 4.0, r.state);
    }
    let status = app
        .sessions
        .get(&card.id)
        .map_or(SessionStatus::Closed, |vm| vm.status.clone());
    let excerpt = card.excerpt.as_deref().unwrap_or(&card.preview_snippet);
    let excerpt: String = excerpt.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = if card.title.is_empty() {
        "New session"
    } else {
        &card.title
    };
    let painter = ui.painter().with_clip_rect(inner);
    let small = egui::TextStyle::Small.resolve(ui.style());
    let line_h = painter
        .layout_no_wrap("Ag".into(), small.clone(), theme::TEXT_MUTED)
        .size()
        .y;

    // Top-down: title, badge row, excerpt; the time/status row is pinned to the bottom.
    let title = clipped_text(
        &painter,
        title,
        egui::TextStyle::Body.resolve(ui.style()),
        ui.visuals().strong_text_color(),
        inner.width(),
        2,
    );
    let mut y = inner.top();
    painter.galley(egui::pos2(inner.left(), y), title.clone(), theme::TEXT);
    y += title.size().y + ROW_GAP;

    let badge_h = line_h + 4.0;
    // Only Xade-origin sessions can be deleted; the button sits in the bottom-right corner.
    let can_delete = card.origin == SessionOrigin::Xade;
    let trash_w = if can_delete { TRASH_SIZE + 4.0 } else { 0.0 };
    let bottom = egui::Rect::from_min_max(
        egui::pos2(inner.left(), (inner.bottom() - line_h).max(inner.top())),
        egui::pos2((inner.right() - trash_w).max(inner.left()), inner.bottom()),
    );
    let badges = egui::Rect::from_min_size(
        egui::pos2(inner.left(), y),
        egui::vec2(inner.width(), badge_h),
    );
    y += badge_h + ROW_GAP;
    let rows = ((bottom.top() - ROW_GAP - y) / line_h).floor();
    if rows >= 1.0 && !excerpt.is_empty() {
        let g = clipped_text(
            &painter,
            &excerpt,
            small,
            theme::TEXT_MUTED,
            inner.width(),
            rows as usize,
        );
        painter.galley(egui::pos2(inner.left(), y), g, theme::TEXT_MUTED);
    }

    let row = |ui: &mut egui::Ui, r: egui::Rect, content: &mut dyn FnMut(&mut egui::Ui)| {
        if r.top() >= inner.bottom() {
            return;
        }
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(r)
                .layout(Layout::left_to_right(Align::Center)),
            |ui| {
                ui.shrink_clip_rect(inner);
                ui.style_mut().interaction.selectable_labels = false;
                content(ui);
            },
        );
    };
    row(ui, badges, &mut |ui| {
        origin_badge(ui, card.origin);
        ui.label(RichText::new("Claude").small().color(theme::CLAUDE));
        ui.add(
            egui::Label::new(
                RichText::new(app.workspace_name(card.workspace_id))
                    .small()
                    .color(theme::TEXT_MUTED),
            )
            .truncate(),
        );
    });
    row(ui, bottom, &mut |ui| {
        ui.label(
            RichText::new(relative_time(card.updated_at))
                .small()
                .color(theme::TEXT_MUTED),
        );
        if running.is_none() {
            ui.label(running::cache_text(card.updated_at));
        }
        let (text, color) = status_badge(&status);
        ui.label(RichText::new(text).small().color(color));
    });
    if can_delete {
        let trash = egui::Rect::from_min_size(
            egui::pos2(
                inner.right() - TRASH_SIZE,
                inner.bottom() - TRASH_SIZE + 4.0,
            ),
            egui::Vec2::splat(TRASH_SIZE),
        );
        // Added after the card so it takes the click before the tile's open handler.
        let tr = ui.interact(
            trash,
            ui.make_persistent_id(("trash", card.id)),
            egui::Sense::click(),
        );
        let (color, tip) = match running {
            Some(r) => (
                theme::TEXT_MUTED.gamma_multiply(0.4),
                format!("Running in {} \u{2014} close it there first", r.location),
            ),
            None if tr.hovered() => (theme::DANGER, "Delete forever".to_string()),
            None => (theme::TEXT_MUTED, "Delete forever".to_string()),
        };
        ui.painter().text(
            trash.center(),
            egui::Align2::CENTER_CENTER,
            icons::TRASH,
            egui::FontId::proportional(14.0),
            color,
        );
        let tr = tr.on_hover_text(tip);
        if tr.clicked() {
            return running.is_none().then_some(CardClick::Delete);
        }
    }
    open()
}

fn status_badge(s: &SessionStatus) -> (&'static str, egui::Color32) {
    match s {
        SessionStatus::Closed => ("Closed", theme::TEXT_MUTED),
        SessionStatus::Starting => ("Starting", theme::TEXT_MUTED),
        SessionStatus::Idle => ("Idle", theme::OK),
        SessionStatus::Streaming => ("Streaming", theme::ACCENT),
        SessionStatus::NeedsApproval => ("Needs approval", theme::WARN),
        SessionStatus::Error(_) => ("Error", theme::DANGER),
    }
}

pub fn relative_time(t: OffsetDateTime) -> String {
    let secs = (OffsetDateTime::now_utc() - t).whole_seconds().max(0);
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn window(pct: f32, resets_in_secs: Option<i64>, now: OffsetDateTime) -> QuotaWindow {
        QuotaWindow {
            kind: QuotaKind::FiveHour,
            utilization_pct: pct,
            resets_at: resets_in_secs.map(|s| now + time::Duration::seconds(s)),
        }
    }

    #[test]
    fn past_reset_shows_zero_and_reset() {
        let now = OffsetDateTime::now_utc();
        let past = window(80.0, Some(-60), now);
        assert_eq!(quota_pct(&past, now), 0.0);
        assert_eq!(reset_text(&past, now).as_deref(), Some("reset"));
        let future = window(80.0, Some(5400), now);
        assert_eq!(quota_pct(&future, now), 80.0);
        assert_eq!(
            reset_text(&future, now).as_deref(),
            Some("Resets in 1h 30m")
        );
        assert_eq!(reset_text(&window(10.0, None, now), now), None);
    }

    /// `(text rects, frame rects)` the top bar draws at window width `width`.
    fn top_bar_rects(width: f32) -> (Vec<egui::Rect>, Vec<egui::Rect>) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        let now = OffsetDateTime::now_utc();
        app.quota = vec![
            window(80.0, Some(5400), now),
            QuotaWindow {
                kind: QuotaKind::SevenDay,
                ..window(20.0, Some(90_000), now)
            },
        ];
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 760.0),
            )),
            ..Default::default()
        };
        // The second pass settles sizes that depend on the first.
        ctx.run_ui(input(), |ui| top_bar(&mut app, ui))
            .textures_delta
            .clear();
        let mut out = ctx.run_ui(input(), |ui| top_bar(&mut app, ui));
        out.textures_delta.clear();
        let _ = std::fs::remove_dir_all(&dir);
        let (mut texts, mut frames) = (Vec::new(), Vec::new());
        fn collect(
            shape: &egui::Shape,
            width: f32,
            t: &mut Vec<egui::Rect>,
            f: &mut Vec<egui::Rect>,
        ) {
            match shape {
                egui::Shape::Text(s) => t.push(s.galley.rect.translate(s.pos.to_vec2())),
                // The bar's own background spans the window; widget frames never do.
                egui::Shape::Rect(r) if r.rect.width() < width * 0.9 => f.push(r.rect),
                egui::Shape::Vec(v) => v.iter().for_each(|s| collect(s, width, t, f)),
                _ => {}
            }
        }
        for c in &out.shapes {
            collect(&c.shape, width, &mut texts, &mut frames);
        }
        (texts, frames)
    }

    fn assert_no_overlaps(width: f32) {
        let (texts, frames) = top_bar_rects(width);
        assert!(texts.len() > 8, "top bar drew {} texts", texts.len());
        for (i, a) in texts.iter().enumerate() {
            assert!(a.max.x <= width, "text {a:?} leaves the window at {width}");
            for b in &texts[i + 1..] {
                let o = a.intersect(*b);
                assert!(
                    o.width() < 0.5 || o.height() < 0.5,
                    "text overlap {a:?} / {b:?} at width {width}"
                );
            }
        }
        // Frames may nest (a bar inside its track) but never partly cover one another.
        for (i, a) in frames.iter().enumerate() {
            for b in &frames[i + 1..] {
                let o = a.intersect(*b);
                let nested = a.contains_rect(*b) || b.contains_rect(*a);
                assert!(
                    nested || o.width() < 0.5 || o.height() < 0.5,
                    "frame overlap {a:?} / {b:?} at width {width}"
                );
            }
        }
    }

    #[test]
    fn top_bar_has_no_overlaps_from_min_window_width_up() {
        for width in [crate::MIN_WINDOW_SIZE[0], 1400.0, 1600.0, 2000.0, 3000.0] {
            assert_no_overlaps(width);
        }
    }

    fn test_card(provider_session_id: Option<&str>, age_secs: i64) -> SessionCard {
        SessionCard {
            id: xade_core::SessionId::new(),
            workspace_id: xade_core::WorkspaceId::new(),
            title: "t".into(),
            preview_snippet: String::new(),
            excerpt: None,
            origin: SessionOrigin::default(),
            updated_at: OffsetDateTime::now_utc() - time::Duration::seconds(age_secs),
            model_id: String::new(),
            provider_session_id: provider_session_id.map(Into::into),
        }
    }

    /// `(texts, circle count)` drawn for `card` by a headless frame.
    fn render_card(app: &App, ctx: &egui::Context, card: &SessionCard) -> (Vec<String>, usize) {
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let rect = egui::Rect::from_min_size(egui::pos2(20.0, 20.0), egui::vec2(300.0, 180.0));
        let mut out = ctx.run_ui(input(), |ui| {
            card_ui(app, ui, card, rect);
        });
        out.textures_delta.clear();
        let (mut texts, mut circles) = (Vec::new(), 0);
        fn walk(s: &egui::Shape, t: &mut Vec<String>, c: &mut usize) {
            match s {
                egui::Shape::Text(x) => t.push(x.galley.text().to_string()),
                egui::Shape::Circle(_) => *c += 1,
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, t, c)),
                _ => {}
            }
        }
        for c in &out.shapes {
            walk(&c.shape, &mut texts, &mut circles);
        }
        (texts, circles)
    }

    #[test]
    fn card_shows_cache_label_or_running_dot() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-card-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        let idle = test_card(Some("p-idle"), 10 * 60);
        let busy = test_card(Some("p-busy"), 10 * 60);
        let plain = test_card(Some("p-none"), 10 * 60);
        let old = test_card(None, 2 * 3600);
        for (id, state) in [
            ("p-idle", xade_core::RunState::Idle),
            ("p-busy", xade_core::RunState::Busy),
        ] {
            app.running.insert(
                id.into(),
                xade_core::RunningSession {
                    provider_session_id: id.into(),
                    state,
                    location: "VS Code".into(),
                    pid: 1,
                },
            );
        }
        let has = |t: &[String], s: &str| t.iter().any(|x| x.contains(s));

        let (texts, circles) = render_card(&app, &ctx, &plain);
        assert!(has(&texts, "Cache \u{b7} 50m left"), "{texts:?}");
        assert_eq!(circles, 0);
        let (texts, circles) = render_card(&app, &ctx, &old);
        assert!(has(&texts, "Expired") && circles == 0, "{texts:?}");
        for card in [&idle, &busy] {
            let (texts, circles) = render_card(&app, &ctx, card);
            assert_eq!(circles, 1);
            assert!(!has(&texts, "Cache"), "{texts:?}");
        }
        assert_eq!(
            running::tooltip(&app.running["p-busy"]),
            "Running in VS Code \u{b7} busy"
        );

        // Confirm before opening a session running elsewhere; open directly otherwise.
        app.request_open(&plain, false);
        assert!(app.pending_open.is_none());
        app.request_open(&idle, true);
        let p = app.pending_open.take().unwrap();
        assert_eq!(
            (p.id, p.location.as_str(), p.background),
            (idle.id, "VS Code", true)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs headless frames over one card with `events` on the last one; returns the click
    /// and the rect of every text drawn.
    fn click_card(
        app: &App,
        ctx: &egui::Context,
        card: &SessionCard,
        at: Option<egui::Pos2>,
    ) -> (Option<CardClick>, Vec<(String, egui::Rect)>) {
        let rect = egui::Rect::from_min_size(egui::pos2(20.0, 20.0), egui::vec2(300.0, 180.0));
        let frame = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let mut click = None;
            let mut out = ctx.run_ui(input, |ui| click = card_ui(app, ui, card, rect));
            out.textures_delta.clear();
            let mut texts = Vec::new();
            fn walk(s: &egui::Shape, t: &mut Vec<(String, egui::Rect)>) {
                match s {
                    egui::Shape::Text(x) => {
                        t.push((x.galley.text().to_string(), x.visual_bounding_rect()))
                    }
                    egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, t)),
                    _ => {}
                }
            }
            out.shapes.iter().for_each(|c| walk(&c.shape, &mut texts));
            (click, texts)
        };
        let Some(pos) = at else { return frame(vec![]) };
        frame(vec![egui::Event::PointerMoved(pos)]);
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![button(true)]);
        frame(vec![button(false)])
    }

    #[test]
    fn trash_sits_bottom_right_only_on_xade_cards_and_does_not_open_the_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-trash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        let (tx, mut cmds) = tokio::sync::mpsc::unbounded_channel();
        app.cmd_tx = tx;
        let card_rect = egui::Rect::from_min_size(egui::pos2(20.0, 20.0), egui::vec2(300.0, 180.0));
        let trash_of = |texts: &[(String, egui::Rect)]| {
            texts
                .iter()
                .find(|(t, _)| t == icons::TRASH)
                .map(|(_, r)| *r)
        };

        let mut xade = test_card(Some("p-x"), 60);
        xade.origin = SessionOrigin::Xade;
        click_card(&app, &ctx, &xade, None);
        let (_, texts) = click_card(&app, &ctx, &xade, None);
        let trash = trash_of(&texts).expect("trash icon drawn");
        let c = trash.center();
        assert!(
            c.x > card_rect.center().x && c.y > card_rect.center().y,
            "{trash:?} not bottom-right of {card_rect:?}"
        );
        assert!(
            card_rect.right() - trash.right() < 20.0 && card_rect.bottom() - trash.bottom() < 20.0
        );

        // Clicking it asks for confirmation and neither opens the session nor sends a command.
        let (click, _) = click_card(&app, &ctx, &xade, Some(c));
        assert_eq!(click, Some(CardClick::Delete));
        app.request_delete(&xade);
        assert_eq!(app.pending_delete.as_ref().map(|p| p.id), Some(xade.id));
        assert!(app.open_windows.is_empty() && cmds.try_recv().is_err());
        // A click elsewhere on the card still opens it.
        let (click, _) = click_card(&app, &ctx, &xade, Some(card_rect.center()));
        assert_eq!(click, Some(CardClick::Open));

        // Running elsewhere: the button is inert.
        app.running.insert(
            "p-x".into(),
            xade_core::RunningSession {
                provider_session_id: "p-x".into(),
                state: xade_core::RunState::Idle,
                location: "VS Code".into(),
                pid: 1,
            },
        );
        let (click, texts) = click_card(&app, &ctx, &xade, Some(c));
        assert!(trash_of(&texts).is_some());
        assert_eq!(click, None);

        // Claude Code cards have no trash at all.
        let mut cc = test_card(Some("p-cc"), 60);
        cc.origin = SessionOrigin::ClaudeCode;
        let (_, texts) = click_card(&app, &ctx, &cc, None);
        assert!(trash_of(&texts).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tiles_fill_the_area_exactly_at_any_size() {
        for (w, h) in [(700.0, 400.0), (1000.0, 640.0), (2400.0, 1200.0)] {
            let area = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(w, h));
            let tiles: Vec<_> = tile_rects(area).collect();
            assert_eq!(tiles.len(), PAGE_SIZE as usize);
            assert_eq!(tiles[0].min, area.min);
            assert!((tiles[8].max - area.max).length() < 0.01);
            assert!(tiles
                .iter()
                .all(|t| (t.width() - tiles[0].width()).abs() < 0.01));
            assert!(tiles.windows(2).all(|p| !p[0].intersects(p[1])));
        }
    }

    #[test]
    fn sidebar_renders_an_expanded_tree() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-side-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("proj/sub/deep")).unwrap();
        std::fs::write(dir.join("proj/.env"), "").unwrap();
        std::fs::write(dir.join("proj/sub/a.rs"), "").unwrap();
        let ctx = egui::Context::default();
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        let root = dir.join("proj");
        app.home.workspaces = vec![Workspace {
            id: WorkspaceId::new(),
            root_path: root.clone(),
            display_name: "proj".into(),
            last_opened_at: OffsetDateTime::now_utc(),
            is_open: true,
        }];
        for p in [root.clone(), root.join("sub"), root.join("missing")] {
            app.tree.toggle(&p);
            app.tree.loaded(&p, file_tree::read_listing(&p));
        }
        for width in [160.0, 220.0, 400.0] {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width + 400.0, 600.0),
                )),
                ..Default::default()
            };
            let mut full = ctx.run_ui(input, |ui| sidebar(&mut app, ui));
            full.textures_delta.clear();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
