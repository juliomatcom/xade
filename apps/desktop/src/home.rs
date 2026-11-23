//! Home view and the shell panels it shares with the workspace view (top bar, sidebar).

use std::time::{Duration, Instant};

use egui::{Align, Layout, RichText};
use time::OffsetDateTime;
use xade_core::{QuotaKind, QuotaWindow, SessionOrigin, SessionStatus, Workspace, WorkspaceId};
use xade_store::SessionCard;

use crate::app::{App, View};
use crate::{icons, theme};

pub const PAGE_SIZE: u32 = 24;
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);
const CARD_HEIGHT: f32 = 132.0;
const CARD_GAP: f32 = 12.0;

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

pub fn top_bar(app: &mut App, ui: &mut egui::Ui) {
    let frame = egui::Frame::new()
        .fill(theme::BG_APP)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .inner_margin(egui::Margin::symmetric(12, 8));
    egui::Panel::top("top_bar").frame(frame).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Xade").strong().size(15.0));
            scope_dropdown(app, ui);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut app.home.search)
                    .hint_text("Fuzzy search sessions (try typos)")
                    .desired_width(320.0),
            );
            if resp.changed() {
                app.home.search_changed_at = Some(Instant::now());
            }
            provider_dropdown(ui);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let can_tile = app.view == View::Workspace && !app.open_windows.is_empty();
                if ui
                    .add_enabled(
                        can_tile,
                        egui::Button::new(icons::with(icons::TILE, "Tile")),
                    )
                    .on_hover_text("Re-tile windows into equal columns")
                    .clicked()
                {
                    app.tiling.retile();
                }
                let to_ws = ui
                    .selectable_label(
                        app.view == View::Workspace,
                        icons::with(icons::WORKSPACE, "Workspace"),
                    )
                    .clicked();
                let to_home = ui
                    .selectable_label(app.view == View::Home, icons::with(icons::HOME, "Home"))
                    .clicked();
                if to_home {
                    app.view = View::Home;
                } else if to_ws && !app.open_windows.is_empty() {
                    app.view = View::Workspace;
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
                if new.clicked() {
                    app.new_session();
                }
                for kind in [QuotaKind::SevenDay, QuotaKind::FiveHour] {
                    quota_meter(ui, kind, app.quota.iter().find(|q| q.kind == kind));
                }
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
            for w in &app.home.workspaces {
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
        .default_size(210.0)
        .resizable(false)
        .show(ui, |ui| {
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                if ui
                    .add_sized(
                        [ui.available_width(), 28.0],
                        egui::Button::new(icons::with(icons::ADD_DIR, "Add Directory")),
                    )
                    .clicked()
                {
                    app.add_directory();
                }
                import_button(app, ui);
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    heading(ui, "Workspaces");
                    if ui
                        .selectable_label(app.home.scope.is_none(), "All Workspaces")
                        .clicked()
                    {
                        set_scope(app, None);
                    }
                    let rows: Vec<(WorkspaceId, String, usize)> = app
                        .home
                        .workspaces
                        .iter()
                        .map(|w| (w.id, w.display_name.clone(), app.live_count(w.id)))
                        .collect();
                    for (id, name, live) in rows {
                        let text = if live > 0 {
                            format!("{name}  ({live})")
                        } else {
                            name
                        };
                        if ui
                            .selectable_label(app.home.scope == Some(id), text)
                            .clicked()
                        {
                            set_scope(app, Some(id));
                        }
                    }
                    ui.add_space(16.0);
                    heading(ui, "Active Providers");
                    ui.label(RichText::new("Claude").color(theme::CLAUDE));
                });
            });
        });
}

fn import_button(app: &mut App, ui: &mut egui::Ui) {
    let enabled = !app.home.importing && app.home.preflight_error.is_none();
    let btn = egui::Button::new(icons::with(icons::IMPORT, "Import from Claude Code"));
    if ui
        .add_enabled_ui(enabled, |ui| {
            ui.add_sized([ui.available_width(), 28.0], btn)
        })
        .inner
        .clicked()
    {
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
    if app.home.cards.is_empty() {
        let msg = if !app.home.search.trim().is_empty() {
            "No sessions match your search."
        } else if app.home.workspaces.is_empty() {
            "Add a directory to get started."
        } else {
            "No sessions yet. Select a workspace and start a new session."
        };
        ui.label(RichText::new(msg).color(theme::TEXT_MUTED));
    }
    let mut open = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let cols = ((ui.available_width() / 320.0) as usize).clamp(1, 4);
            let width = (ui.available_width() - CARD_GAP * (cols as f32 - 1.0)) / cols as f32;
            for row in app.home.cards.chunks(cols) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = CARD_GAP;
                    for card in row {
                        if card_ui(app, ui, card, width) {
                            open = Some(card.id);
                        }
                    }
                });
            }
        });
    if let Some(id) = open {
        app.open_session(id);
    }
}

/// Draws one card; returns true when clicked.
fn card_ui(app: &App, ui: &mut egui::Ui, card: &SessionCard, width: f32) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, CARD_HEIGHT), egui::Sense::click());
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
    let inner = rect.shrink(12.0);
    let status = app
        .sessions
        .get(&card.id)
        .map_or(SessionStatus::Closed, |vm| vm.status.clone());
    let excerpt = card.excerpt.as_deref().unwrap_or(&card.preview_snippet);
    let excerpt: String = excerpt.split_whitespace().collect::<Vec<_>>().join(" ");
    ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
        ui.shrink_clip_rect(inner);
        ui.spacing_mut().item_spacing.y = 4.0;
        let title = if card.title.is_empty() {
            "New session"
        } else {
            &card.title
        };
        ui.add(
            egui::Label::new(RichText::new(title).strong())
                .truncate()
                .selectable(false),
        );
        ui.horizontal(|ui| {
            origin_badge(ui, card.origin);
            ui.label(RichText::new("Claude").small().color(theme::CLAUDE));
            ui.label(
                RichText::new(app.workspace_name(card.workspace_id))
                    .small()
                    .color(theme::TEXT_MUTED),
            );
        });
        ui.add(
            egui::Label::new(RichText::new(excerpt).small().color(theme::TEXT_MUTED))
                .selectable(false),
        );
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(relative_time(card.updated_at))
                    .small()
                    .color(theme::TEXT_MUTED),
            );
            let (text, color) = status_badge(&status);
            ui.label(RichText::new(text).small().color(color));
        });
    });
    resp.clicked()
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

fn relative_time(t: OffsetDateTime) -> String {
    let secs = (OffsetDateTime::now_utc() - t).whole_seconds().max(0);
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
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
}
