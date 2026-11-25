//! Copy / Quote popup that follows a text selection in a chat message.

use egui::{Id, Pos2, Rect};

use crate::app::App;
use crate::icons;

const SETTLED: &str = "xade_selection_settled";
const TEXT_BOX: &str = "xade_text_box_focused";

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
                // The popup reads it next frame; egui repaints only on input, so without this
                // it would wait for the next mouse move.
                ctx.request_repaint();
            }
        }
    }
}

/// Called while an editable text box (prompt, file) has focus, so its selections are not offered
/// the popup. Code blocks in messages are focusable text edits too and must still get it.
pub fn mark_text_box(ctx: &egui::Context) {
    let frame = ctx.cumulative_frame_nr();
    ctx.data_mut(|d| d.insert_temp(Id::unique(TEXT_BOX), frame));
}

fn in_text_box(ctx: &egui::Context) -> bool {
    let frame = ctx.cumulative_frame_nr();
    ctx.data(|d| d.get_temp::<u64>(Id::unique(TEXT_BOX))) == Some(frame)
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
        let in_text_box = in_text_box(ctx);
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

    /// Drags from the first paragraph into the second and returns what egui reported.
    fn drag_report(text: &str) -> Option<String> {
        use egui::{Event, PointerButton, Pos2, RawInput};
        let ctx = egui::Context::default();
        ctx.add_plugin(SelectionWatcher);
        let mut cache = crate::widgets::markdown::new_cache();
        let mut release_repaint = false;
        let mut run = |events: Vec<Event>| -> bool {
            let mut input = RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    Pos2::ZERO,
                    egui::vec2(600.0, 400.0),
                )),
                ..Default::default()
            };
            input.events = events;
            let out = ctx.run_ui(input, |ui| {
                crate::widgets::markdown::markdown(ui, &mut cache, 1, text);
            });
            let repaint = out
                .viewport_output
                .values()
                .any(|v| v.repaint_delay.is_zero());
            out.drop_without_applying_deltas();
            repaint
        };
        let btn = |pos: Pos2, pressed: bool| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        let (a, b) = (Pos2::new(12.0, 8.0), Pos2::new(60.0, 70.0));
        run(vec![]);
        run(vec![Event::PointerMoved(a)]);
        run(vec![btn(a, true)]);
        run(vec![Event::PointerMoved(b)]);
        run(vec![Event::PointerMoved(Pos2::new(61.0, 71.0))]);
        release_repaint |= run(vec![btn(Pos2::new(61.0, 71.0), false)]);
        assert!(
            release_repaint,
            "the release frame must ask for another frame"
        );
        run(vec![]);
        run(vec![]);
        ctx.data_mut(|d| d.remove_temp::<String>(Id::unique(SETTLED)))
    }

    #[test]
    fn a_drag_over_one_paragraph_is_reported() {
        assert!(drag_report("alpha beta gamma delta epsilon").is_some());
    }

    #[test]
    fn a_drag_across_paragraphs_is_reported() {
        let got = drag_report("alpha beta gamma\n\ndelta epsilon zeta\n\neta theta iota");
        assert!(got.is_some(), "{got:?}");
    }

    #[test]
    fn a_drag_across_a_list_and_code_is_reported() {
        let got = drag_report("- one\n- two\n- three\n\n```\ncode here\nmore\n```\n\nend");
        assert!(got.is_some(), "{got:?}");
    }
}
