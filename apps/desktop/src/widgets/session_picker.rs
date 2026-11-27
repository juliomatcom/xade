//! Quick-open popup (Cmd+Shift+P): fuzzy search over every session; Enter opens the selection.

use egui::{Align, Key, Layout, Modifiers, RichText};
use xade_store::SessionCard;

use crate::app::App;
use crate::editor::EditorId;
use crate::home;
use crate::running;
use crate::theme;
use crate::workspace::WindowKind;

/// Rows the picker asks the reader for.
pub const PAGE_SIZE: u32 = 20;

#[derive(Default)]
pub struct PickerState {
    pub query: String,
    pub results: Vec<SessionCard>,
    pub selected: usize,
}

/// Draws the picker when open and applies its decision.
pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(mut st) = app.picker.take() else {
        return;
    };
    // Open files that match the query are listed under the sessions.
    let needle = st.query.trim().to_lowercase();
    let mut files: Vec<(EditorId, String)> = app
        .open_windows
        .iter()
        .filter_map(|k| match k {
            WindowKind::Editor(id) => app.editors.map.get(id).map(|vm| (*id, vm.title())),
            WindowKind::Session(_) | WindowKind::MultiSession(_) | WindowKind::Guide => None,
        })
        .filter(|(_, name)| name.to_lowercase().contains(&needle))
        .collect();
    files.sort_by_key(|(id, _)| id.0);
    // Consumed before the search field draws, which would otherwise take Up/Down/Enter.
    let (down, up, enter) = ctx.input_mut(|i| {
        (
            i.consume_key(Modifiers::NONE, Key::ArrowDown),
            i.consume_key(Modifiers::NONE, Key::ArrowUp),
            i.consume_key(Modifiers::NONE, Key::Enter),
        )
    });
    let sessions = st.results.len();
    let last = (sessions + files.len()).saturating_sub(1);
    let moved = down || up;
    if down {
        st.selected = (st.selected + 1).min(last);
    }
    if up {
        st.selected = st.selected.saturating_sub(1);
    }
    st.selected = st.selected.min(last);
    let mut open = enter
        .then(|| st.results.get(st.selected).cloned())
        .flatten();
    let mut open_file = (enter && st.selected >= sessions)
        .then(|| files.get(st.selected - sessions).map(|f| f.0))
        .flatten();
    let mut changed = false;
    let resp = crate::widgets::modal(ctx, egui::Id::unique("session_picker")).show(ctx, |ui| {
        ui.set_width(560.0);
        ui.label(RichText::new("Open session").strong().size(16.0));
        let edit = ui.add(
            egui::TextEdit::singleline(&mut st.query)
                .hint_text("Search all sessions")
                .desired_width(f32::INFINITY),
        );
        if !edit.has_focus() {
            edit.request_focus();
        }
        changed = edit.changed();
        ui.add_space(4.0);
        if st.results.is_empty() && files.is_empty() {
            ui.label(RichText::new("No sessions found.").color(theme::text_muted()));
        }
        egui::ScrollArea::vertical()
            .max_height(360.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (i, card) in st.results.iter().enumerate() {
                    let selected = i == st.selected;
                    let r = row(app, ui, card, selected);
                    if selected && moved {
                        r.scroll_to_me(None);
                    }
                    if r.clicked() {
                        open = Some(card.clone());
                    }
                }
                for (i, (id, name)) in files.iter().enumerate() {
                    let selected = sessions + i == st.selected;
                    let r = file_row(ui, name, selected);
                    if selected && moved {
                        r.scroll_to_me(None);
                    }
                    if r.clicked() {
                        open_file = Some(*id);
                    }
                }
            });
    });
    if open.is_some() || open_file.is_some() || resp.should_close() {
        if let Some(card) = open {
            app.request_open(&card, false);
        } else if let Some(id) = open_file {
            app.focus(WindowKind::Editor(id));
        }
        return;
    }
    app.picker = Some(st);
    if changed {
        if let Some(p) = &mut app.picker {
            p.selected = 0;
        }
        app.refresh_picker();
    }
}

fn file_row(ui: &mut egui::Ui, name: &str, selected: bool) -> egui::Response {
    let fill = if selected {
        theme::bg_hover()
    } else {
        theme::bg_raised()
    };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                crate::chrome::file_badge(ui);
                ui.add(egui::Label::new(name).truncate());
            });
        })
        .response
        .interact(egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn row(app: &App, ui: &mut egui::Ui, card: &SessionCard, selected: bool) -> egui::Response {
    let fill = if selected {
        theme::bg_hover()
    } else {
        theme::bg_raised()
    };
    let inner = egui::Frame::new()
        .fill(fill)
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(home::relative_time(card.updated_at))
                        .small()
                        .color(theme::text_muted()),
                );
                let running = app.running_for(card);
                if running.is_none() && card.multi_id().is_none() {
                    ui.label(running::cache_text(card.updated_at));
                }
                let is_open = match card.target {
                    xade_store::CardTarget::Session(id) => {
                        app.open_windows.contains(&WindowKind::Session(id))
                    }
                    xade_store::CardTarget::Multi(id) => {
                        app.open_windows.contains(&WindowKind::MultiSession(id))
                    }
                };
                if is_open {
                    ui.label(RichText::new("open").small().color(theme::ok()));
                }
                if card.multi_id().is_some() {
                    ui.label(RichText::new("Multi").small().color(theme::accent()));
                }
                ui.label(
                    RichText::new(app.workspace_name(card.workspace_id))
                        .small()
                        .color(theme::text_muted()),
                );
                home::origin_badge(ui, card.origin);
                if let Some(r) = running {
                    let (rect, resp) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    running::paint_dot(ui, rect.center(), 4.0, r.state);
                    resp.on_hover_text(running::tooltip(r));
                }
                let title = if card.title.is_empty() {
                    if card.multi_id().is_some() {
                        "New Multi-Session"
                    } else {
                        "New session"
                    }
                } else {
                    &card.title
                };
                ui.add(egui::Label::new(title).truncate());
            });
        });
    inner
        .response
        .interact(egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn card(title: &str) -> SessionCard {
        let now = time::OffsetDateTime::now_utc();
        SessionCard {
            target: xade_store::CardTarget::Session(xade_core::SessionId::new()),
            workspace_id: xade_core::WorkspaceId::new(),
            title: title.into(),
            preview_snippet: String::new(),
            excerpt: None,
            origin: xade_core::SessionOrigin::default(),
            updated_at: now,
            model_id: String::new(),
            provider_session_id: None,
            provider_id: xade_core::ProviderId::Claude,
            members: Vec::new(),
        }
    }

    #[test]
    fn renders_headless_navigates_and_closes() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-picker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        let cards = vec![
            card("first"),
            card(&"very long title ".repeat(20)),
            card(""),
        ];
        let (first, second) = (
            cards[0].session_id().unwrap(),
            cards[1].session_id().unwrap(),
        );
        app.open_windows.push(WindowKind::Session(first));
        app.picker = Some(PickerState {
            results: cards,
            ..PickerState::default()
        });
        let key = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        let frame = |app: &mut App, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                events,
                ..Default::default()
            };
            ctx.run_pass(input, |ctx| show(app, ctx))
                .drop_without_applying_deltas();
        };
        frame(&mut app, vec![]);
        frame(&mut app, vec![]);
        frame(&mut app, vec![key(Key::ArrowDown)]);
        assert_eq!(app.picker.as_ref().unwrap().selected, 1);
        // Enter opens the selection and closes the picker.
        frame(&mut app, vec![key(Key::Enter)]);
        assert!(app.picker.is_none());
        assert_eq!(app.focused, Some(WindowKind::Session(second)));
        // Esc closes without opening.
        app.picker = Some(PickerState::default());
        frame(&mut app, vec![]);
        frame(&mut app, vec![key(Key::Escape)]);
        assert!(app.picker.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
