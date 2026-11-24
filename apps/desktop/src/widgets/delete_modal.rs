//! Confirm modal before deleting a session forever.

use egui::RichText;
use xade_core::SessionId;
use xade_runtime::ManagerCmd;

use crate::app::App;
use crate::theme;

/// Minimum width of the modal's content.
const MIN_WIDTH: f32 = 480.0;

/// A session waiting for the user to confirm its deletion.
pub struct PendingDelete {
    pub id: SessionId,
    pub title: String,
}

/// Draws the modal when one is pending; Delete sends the command, Cancel or Esc drops it.
pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(p) = app.pending_delete.as_ref() else {
        return;
    };
    let title = if p.title.is_empty() {
        "New session"
    } else {
        &p.title
    };
    let body = format!(
        "\u{201c}{title}\u{201d} will be removed from Xade and its Claude Code transcript will \
         be deleted. This can't be undone."
    );
    let (mut delete, mut cancel) = (false, false);
    let resp =
        crate::widgets::modal(ctx, egui::Id::unique("confirm_delete_session")).show(ctx, |ui| {
            ui.set_min_width(MIN_WIDTH);
            ui.label(RichText::new("Delete session forever?").strong().size(18.0));
            ui.add_space(6.0);
            ui.label(body);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                delete = ui
                    .add(
                        egui::Button::new(RichText::new("Delete").color(theme::bg_app()))
                            .fill(theme::danger()),
                    )
                    .clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
    if delete {
        if let Some(p) = app.pending_delete.take() {
            let _ = app
                .cmd_tx
                .send(ManagerCmd::DeleteSession { session_id: p.id });
        }
    } else if cancel || resp.should_close() {
        app.pending_delete = None;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn modal_names_the_session_and_escape_cancels() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-del-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        let (tx, mut cmds) = tokio::sync::mpsc::unbounded_channel();
        app.cmd_tx = tx;
        app.pending_delete = Some(PendingDelete {
            id: SessionId::new(),
            title: "Fix the parser".into(),
        });
        let frame = |app: &mut App, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 700.0),
                )),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_pass(input, |ctx| show(app, ctx));
            out.textures_delta.clear();
            out
        };
        frame(&mut app, vec![]);
        let out = frame(&mut app, vec![]);
        fn texts(s: &egui::Shape, out: &mut String) {
            match s {
                egui::Shape::Text(t) => out.push_str(t.galley.text()),
                egui::Shape::Vec(v) => v.iter().for_each(|s| texts(s, out)),
                _ => {}
            }
        }
        let mut all = String::new();
        out.shapes.iter().for_each(|c| texts(&c.shape, &mut all));
        assert!(all.contains("Delete session forever?") && all.contains("Fix the parser"));
        assert!(all.contains("can't be undone") && all.contains("Cancel"));
        assert!(app.pending_delete.is_some());
        let esc = egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        frame(&mut app, vec![esc]);
        assert!(app.pending_delete.is_none() && cmds.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
