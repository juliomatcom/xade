//! Quick-open popup (Cmd+P): fuzzy search over every session; Enter opens the selection.

use egui::{Align, Key, Layout, Modifiers, RichText};
use xade_store::SessionCard;

use crate::app::App;
use crate::home;
use crate::theme;

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
    // Consumed before the search field draws, which would otherwise take Up/Down/Enter.
    let (down, up, enter) = ctx.input_mut(|i| {
        (
            i.consume_key(Modifiers::NONE, Key::ArrowDown),
            i.consume_key(Modifiers::NONE, Key::ArrowUp),
            i.consume_key(Modifiers::NONE, Key::Enter),
        )
    });
    let last = st.results.len().saturating_sub(1);
    let moved = down || up;
    if down {
        st.selected = (st.selected + 1).min(last);
    }
    if up {
        st.selected = st.selected.saturating_sub(1);
    }
    st.selected = st.selected.min(last);
    let mut open = enter
        .then(|| st.results.get(st.selected).map(|c| c.id))
        .flatten();
    let mut changed = false;
    let resp = egui::Modal::new(egui::Id::unique("session_picker")).show(ctx, |ui| {
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
        if st.results.is_empty() {
            ui.label(RichText::new("No sessions found.").color(theme::TEXT_MUTED));
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
                        open = Some(card.id);
                    }
                }
            });
    });
    if open.is_some() || resp.should_close() {
        if let Some(id) = open {
            app.open_session(id);
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

fn row(app: &App, ui: &mut egui::Ui, card: &SessionCard, selected: bool) -> egui::Response {
    let fill = if selected {
        theme::BG_HOVER
    } else {
        theme::BG_RAISED
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
                        .color(theme::TEXT_MUTED),
                );
                if app.open_windows.contains(&card.id) {
                    ui.label(RichText::new("open").small().color(theme::OK));
                }
                ui.label(
                    RichText::new(app.workspace_name(card.workspace_id))
                        .small()
                        .color(theme::TEXT_MUTED),
                );
                home::origin_badge(ui, card.origin);
                let title = if card.title.is_empty() {
                    "New session"
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
            id: xade_core::SessionId::new(),
            workspace_id: xade_core::WorkspaceId::new(),
            title: title.into(),
            preview_snippet: String::new(),
            excerpt: None,
            origin: xade_core::SessionOrigin::default(),
            updated_at: now,
            model_id: String::new(),
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
        let (first, second) = (cards[0].id, cards[1].id);
        app.open_windows.push(first);
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
        assert_eq!(app.focused, Some(second));
        // Esc closes without opening.
        app.picker = Some(PickerState::default());
        frame(&mut app, vec![]);
        frame(&mut app, vec![key(Key::Escape)]);
        assert!(app.picker.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
