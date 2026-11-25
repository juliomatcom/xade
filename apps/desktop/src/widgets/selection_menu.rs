//! Copy / Quote popup that follows a text selection in a chat message.

use egui::{Id, Pos2, Rect};

use crate::app::App;
use crate::icons;

const SETTLED: &str = "xade_selection_settled";

/// A selection waiting in the popup: its text and where the pointer let go.
pub struct Pending {
    text: String,
    pos: Pos2,
    /// The popup's rect from the last frame it was drawn.
    rect: Option<Rect>,
}

/// Catches egui's "selection settled" output (labels only) so the app can react to it.
pub struct SelectionWatcher;

impl egui::Plugin for SelectionWatcher {
    fn debug_name(&self) -> &'static str {
        "XadeSelectionWatcher"
    }

    fn output_hook(&mut self, ctx: &egui::Context, output: &mut egui::FullOutput) {
        for cmd in &output.platform_output.commands {
            if let egui::OutputCommand::TextSelectionSettled(text) = cmd {
                ctx.data_mut(|d| d.insert_temp(Id::unique(SETTLED), text.clone()));
            }
        }
    }
}

/// `> ` before every line, ready to type under.
fn quoted(text: &str) -> String {
    let mut out: String = text
        .trim_end()
        .lines()
        .map(|l| {
            if l.is_empty() {
                ">".to_string()
            } else {
                format!("> {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    out.push_str("\n\n");
    out
}

/// Appends a quote to a prompt box's text, on its own paragraph.
pub fn append_quote(input: &mut String, text: &str) {
    if !input.is_empty() && !input.ends_with("\n\n") {
        input.push_str(if input.ends_with('\n') { "\n" } else { "\n\n" });
    }
    input.push_str(&quoted(text));
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    if let Some(text) = ctx.data_mut(|d| d.remove_temp::<String>(Id::unique(SETTLED))) {
        // Selections inside a text box (prompt, file) are not chat text.
        let in_text_box = ctx.memory(|m| m.focused().is_some());
        let pos = ctx.input(|i| i.pointer.latest_pos());
        app.selection_menu = match (in_text_box, pos, app.focused_session()) {
            (false, Some(pos), Some(_)) if !text.trim().is_empty() => Some(Pending {
                text,
                pos,
                rect: None,
            }),
            _ => None,
        };
    }
    let Some(pending) = app.selection_menu.as_mut() else {
        return;
    };
    let pressed_outside = ctx.input(|i| {
        i.pointer.any_pressed()
            && pending
                .rect
                .zip(i.pointer.interact_pos())
                .is_some_and(|(r, p)| !r.contains(p))
    });
    if pressed_outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.selection_menu = None;
        return;
    }
    let (mut copy, mut quote) = (false, false);
    let screen = ctx.content_rect();
    let at = (pending.pos + egui::vec2(6.0, 12.0)).min(screen.max - egui::vec2(170.0, 44.0));
    let area = egui::Area::new(Id::unique("selection_menu"))
        .order(egui::Order::Foreground)
        .fixed_pos(at)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    copy = ui.button(icons::with(icons::COPY, "Copy")).clicked();
                    quote = ui.button("\u{275D} Quote").clicked();
                });
            });
        });
    pending.rect = Some(area.response.rect);
    if copy || quote {
        let text = std::mem::take(&mut pending.text);
        app.selection_menu = None;
        if copy {
            ctx.copy_text(text);
        } else if let Some(vm) = app
            .focused_session()
            .and_then(|id| app.sessions.get_mut(&id))
        {
            append_quote(&mut vm.input, &text);
            vm.focus_input = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_every_line_and_leaves_room_to_type() {
        assert_eq!(quoted("a\n\nb"), "> a\n>\n> b\n\n");
    }

    #[test]
    fn a_quote_starts_its_own_paragraph() {
        let mut s = String::new();
        append_quote(&mut s, "x");
        assert_eq!(s, "> x\n\n");
        let mut s = "hi".to_string();
        append_quote(&mut s, "x");
        assert_eq!(s, "hi\n\n> x\n\n");
        let mut s = "hi\n".to_string();
        append_quote(&mut s, "x");
        assert_eq!(s, "hi\n\n> x\n\n");
    }
}
