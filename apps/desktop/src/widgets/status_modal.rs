//! Status dialog: every registered provider with its preflight state and a Retry.

use egui::RichText;
use xade_core::{ProviderError, ProviderId};

use crate::app::App;
use crate::icons;
use crate::theme;

#[derive(Debug, PartialEq, Eq)]
enum State {
    Ok(Option<String>),
    Checking,
    Failed(String),
}

fn state_of(app: &App, id: ProviderId) -> State {
    if app.home.pending.contains(&id) {
        return State::Checking;
    }
    match app.home.preflight.get(&id) {
        None | Some(Ok(())) => State::Ok(
            app.providers
                .iter()
                .find(|p| p.id() == id)
                .and_then(|p| p.account_label()),
        ),
        Some(Err(ProviderError::NotInstalled)) => State::Failed("Not installed".into()),
        Some(Err(ProviderError::NotLoggedIn)) => State::Failed("Not signed in".into()),
        Some(Err(e)) => State::Failed(e.to_string()),
    }
}

/// Text, dot color and whether Retry applies, for one state.
fn describe(state: &State) -> (String, egui::Color32, bool) {
    match state {
        State::Ok(None) => ("OK".into(), theme::ok(), false),
        State::Ok(Some(label)) => (format!("OK ({label})"), theme::ok(), false),
        State::Checking => ("Checking\u{2026}".into(), theme::text_muted(), false),
        State::Failed(text) => (text.clone(), theme::danger(), true),
    }
}

/// Draws the dialog while `app.status_open`; Close, Esc or a click outside closes it.
pub fn show(app: &mut App, ctx: &egui::Context) {
    if !app.status_open {
        return;
    }
    let rows: Vec<_> = app
        .providers
        .iter()
        .map(|p| (p.id(), p.display_name(), describe(&state_of(app, p.id()))))
        .collect();
    let mut close = false;
    let mut retry = None;
    let resp = crate::widgets::modal(ctx, egui::Id::unique("status")).show(ctx, |ui| {
        ui.set_min_width(360.0);
        ui.label(
            RichText::new(icons::with(icons::PROVIDERS, "Status"))
                .strong()
                .size(20.0),
        );
        ui.add_space(8.0);
        egui::Grid::new("status_grid")
            .num_columns(3)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                for (id, name, (text, color, can_retry)) in &rows {
                    ui.horizontal(|ui| {
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), 4.0, *color);
                        ui.label(*name);
                    });
                    ui.label(RichText::new(text).color(theme::text_muted()));
                    if *can_retry && ui.button(icons::with(icons::RETRY, "Retry")).clicked() {
                        retry = Some(*id);
                    }
                    ui.end_row();
                }
            });
        ui.add_space(12.0);
        close = ui.button("Close").clicked();
    });
    if let Some(id) = retry {
        app.retry_preflight(id);
    }
    if close || resp.should_close() {
        app.status_open = false;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn rows_cover_each_state() {
        let ok = describe(&State::Ok(None));
        assert_eq!((ok.0.as_str(), ok.1, ok.2), ("OK", theme::ok(), false));
        let labelled = describe(&State::Ok(Some("me@x".into())));
        assert_eq!(labelled.0, "OK (me@x)");
        let checking = describe(&State::Checking);
        assert_eq!(checking.0, "Checking\u{2026}");
        assert_eq!((checking.1, checking.2), (theme::text_muted(), false));
        for text in ["Not installed", "boom"] {
            let d = describe(&State::Failed(text.into()));
            assert_eq!((d.0.as_str(), d.1, d.2), (text, theme::danger(), true));
        }
    }

    #[test]
    fn states_come_from_the_preflight_and_the_menu_item_opens_the_dialog() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-status-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        app.providers
            .push(std::sync::Arc::new(crate::app::StubProvider::antigravity()));
        let agy = ProviderId::Antigravity;
        assert_eq!(state_of(&app, agy), State::Ok(None));
        app.home.pending.insert(agy);
        assert_eq!(state_of(&app, agy), State::Checking);
        app.home.pending.clear();
        app.home
            .preflight
            .insert(agy, Err(ProviderError::NotInstalled));
        assert_eq!(state_of(&app, agy), State::Failed("Not installed".into()));
        app.home
            .preflight
            .insert(agy, Err(ProviderError::NotLoggedIn));
        assert_eq!(state_of(&app, agy), State::Failed("Not signed in".into()));
        app.home
            .preflight
            .insert(agy, Err(ProviderError::Spawn("x".into())));
        assert_eq!(
            state_of(&app, agy),
            State::Failed("failed to spawn provider: x".into())
        );

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
        assert!(!app.status_open);
        app.status_open = true;
        frame(&mut app, vec![]);
        frame(&mut app, vec![]);
        assert!(app.status_open);
        frame(
            &mut app,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(!app.status_open);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
