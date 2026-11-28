//! Circled up/down arrows over a chat's scroll area, shown only for the direction that has more
//! to scroll to. A click leaves a request in egui memory that the next frame's area consumes
//! (`take` before building it), so the jump lands inside the area and keeps `stick_to_bottom`.

use egui::{Id, Rect, Sense, Stroke, Vec2};

use crate::theme;

const DIAMETER: f32 = 28.0;
/// Right gap clears the floating scroll bar.
const INSET: Vec2 = Vec2::new(22.0, 14.0);
/// Slack for sub-pixel offsets.
const EDGE: f32 = 2.0;

/// A jump asked for last frame.
#[derive(Clone, Copy, Default)]
pub struct Jump {
    pub top: bool,
    pub bottom: bool,
}

impl Jump {
    /// A chat's scroll area; the jumps happen inside it (`jump_top`, and the caller's
    /// `scroll_to_cursor` for the bottom) because only an in-area request unsticks
    /// `stick_to_bottom`.
    pub fn area(self, id_salt: impl egui::AsIdSalt) -> egui::ScrollArea {
        egui::ScrollArea::vertical()
            .id_salt(id_salt)
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            // Unanimated: egui's scroll-to animation clears `scroll_stuck_to_end` until the target
            // settles (even with `ScrollAnimation::none()`), so output arriving on the next frame
            // would leave the chat unstuck.
            .animated(false)
    }

    /// Scrolls to the content's origin (`ui.cursor().min` read before adding any content) if asked.
    pub fn jump_top(self, ui: &mut egui::Ui, content_top: egui::Pos2) {
        if self.top {
            let rect = Rect::from_min_size(content_top, Vec2::splat(1.0));
            ui.scroll_to_rect(rect, Some(egui::Align::TOP));
        }
    }
}

fn key(scroll_id: Id) -> Id {
    scroll_id.with("scroll_nav")
}

/// Takes the pending jump; call before building the scroll area. `scroll_id` is any id the
/// caller keeps per area and also gives to `show`.
pub fn take(ctx: &egui::Context, scroll_id: Id) -> Jump {
    ctx.data_mut(|d| d.remove_temp::<(bool, bool)>(key(scroll_id)))
        .map(|(top, bottom)| Jump { top, bottom })
        .unwrap_or_default()
}

/// Whether there is content above and below the viewport.
fn away(offset: f32, viewport: f32, content: f32) -> (bool, bool) {
    (offset > EDGE, offset + viewport < content - EDGE)
}

/// Draws the arrows in the bottom-right corner of the area's viewport; call right after `show`.
pub fn show<R>(ui: &egui::Ui, out: &egui::scroll_area::ScrollAreaOutput<R>, nav_id: Id) {
    let (up, down) = away(
        out.state.clamped_offset().y,
        out.inner_rect.height(),
        out.content_size.y,
    );
    let mut jump = (false, false);
    let mut corner = out.inner_rect.right_bottom() - INSET;
    for (visible, is_up) in [(down, false), (up, true)] {
        if !visible {
            continue;
        }
        let rect = Rect::from_min_size(corner - Vec2::splat(DIAMETER), Vec2::splat(DIAMETER));
        let id = out.id.with(("scroll_nav", is_up));
        let resp = ui
            // Senses drags too: egui hands a click-only widget's drag to what is behind it, and a
            // press that wanders over chat text would select it and open the Copy / Quote popup.
            .interact(rect, id, Sense::click_and_drag())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(if is_up {
                "Scroll to top"
            } else {
                "Scroll to bottom"
            });
        paint(ui, rect, is_up, resp.hovered());
        if resp.clicked() {
            jump = if is_up { (true, false) } else { (false, true) };
        }
        corner.y -= DIAMETER + 8.0;
    }
    if jump != (false, false) {
        let key = key(nav_id);
        ui.ctx().data_mut(|d| d.insert_temp(key, jump));
        ui.ctx().request_repaint();
    }
}

fn paint(ui: &egui::Ui, rect: Rect, up: bool, hovered: bool) {
    let p = ui.painter();
    let c = rect.center();
    let (fill, line) = if hovered {
        (theme::bg_hover(), theme::text())
    } else {
        (theme::bg_raised(), theme::text_muted())
    };
    p.circle(
        c,
        DIAMETER / 2.0,
        fill,
        Stroke::new(1.5, theme::border_strong()),
    );
    // Arrow: a stem and a chevron head, pointing up or down.
    let s = if up { -1.0 } else { 1.0 };
    let tip = c + Vec2::new(0.0, 5.0 * s);
    let stroke = Stroke::new(1.8, line);
    p.line_segment([c - Vec2::new(0.0, 5.0 * s), tip], stroke);
    p.line_segment([tip, tip + Vec2::new(-4.5, -4.5 * s)], stroke);
    p.line_segment([tip, tip + Vec2::new(4.5, -4.5 * s)], stroke);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_follow_the_scroll_position() {
        // Short content: nothing to scroll to.
        assert_eq!(away(0.0, 400.0, 300.0), (false, false));
        // Top: only down. Middle: both. Bottom: only up.
        assert_eq!(away(0.0, 400.0, 1000.0), (false, true));
        assert_eq!(away(200.0, 400.0, 1000.0), (true, true));
        assert_eq!(away(600.0, 400.0, 1000.0), (true, false));
        // Sub-pixel drift at the edges does not show an arrow.
        assert_eq!(away(0.5, 400.0, 1000.0), (false, true));
        assert_eq!(away(599.5, 400.0, 1000.0), (true, false));
    }

    /// Drives a tall chat-like area headlessly: returns the scroll offset after each pass.
    struct Rig {
        ctx: egui::Context,
        inner: Rect,
        offset: f32,
        frames: u32,
        /// `TextSelectionSettled` outputs seen, which open the Copy / Quote popup.
        settled: usize,
    }

    impl Rig {
        fn new() -> Self {
            Rig {
                ctx: egui::Context::default(),
                inner: Rect::NOTHING,
                offset: 0.0,
                frames: 0,
                settled: 0,
            }
        }

        fn pass(&mut self, events: Vec<egui::Event>) {
            let screen = Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(400.0, 300.0));
            self.frames += 1;
            let input = egui::RawInput {
                screen_rect: Some(screen),
                time: Some(f64::from(self.frames) * 0.05),
                events,
                ..Default::default()
            };
            let (mut inner, mut offset) = (self.inner, self.offset);
            let out = self.ctx.run_pass(input, |ctx| {
                egui::Window::new("w")
                    .title_bar(false)
                    .fixed_size([380.0, 280.0])
                    .show(ctx, |ui| {
                        let nav_id = ui.make_persistent_id("chat");
                        let nav = take(ui.ctx(), nav_id);
                        let out = nav.area("chat").show(ui, |ui| {
                            let top = ui.cursor().min;
                            for _ in 0..100 {
                                ui.label("words to select in a chat message ".repeat(12));
                            }
                            nav.jump_top(ui, top);
                            if nav.bottom {
                                ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
                            }
                        });
                        show(ui, &out, nav_id);
                        inner = out.inner_rect;
                        offset = out.state.clamped_offset().y;
                    });
            });
            self.settled += out
                .platform_output
                .commands
                .iter()
                .filter(|c| matches!(c, egui::OutputCommand::TextSelectionSettled(_)))
                .count();
            out.drop_without_applying_deltas();
            self.inner = inner;
            self.offset = offset;
        }

        fn click(&mut self, pos: egui::Pos2) {
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.pass(vec![egui::Event::PointerMoved(pos)]);
            self.pass(vec![button(true)]);
            self.pass(vec![button(false)]);
            for _ in 0..40 {
                self.pass(vec![]);
            }
        }

        /// Center of the nth arrow from the bottom-right corner (0 = lowest).
        fn arrow(&self, nth: f32) -> egui::Pos2 {
            self.inner.right_bottom()
                - INSET
                - Vec2::splat(DIAMETER / 2.0)
                - Vec2::new(0.0, nth * (DIAMETER + 8.0))
        }
    }

    #[test]
    fn arrows_jump_to_the_top_and_back_to_the_bottom() {
        let mut rig = Rig::new();
        for _ in 0..4 {
            rig.pass(vec![]);
        }
        let bottom = rig.offset;
        assert!(bottom > 1000.0, "a stuck chat starts at the bottom");

        // At the bottom only "up" is drawn, in the lowest slot.
        rig.click(rig.arrow(0.0));
        assert_eq!(rig.offset, 0.0, "up jumps to the top");

        // At the top only "down" is drawn, in the lowest slot.
        rig.click(rig.arrow(0.0));
        assert_eq!(rig.offset, bottom, "down jumps back to the bottom");
    }

    #[test]
    fn pressing_an_arrow_never_selects_the_chat_text_under_it() {
        let mut rig = Rig::new();
        for _ in 0..4 {
            rig.pass(vec![]);
        }
        let pos = rig.arrow(0.0);
        // A double-click selects a word of the label beneath, as does a short drag.
        rig.click(pos);
        rig.click(pos);
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let moved = pos + Vec2::new(-14.0, 0.0);
        rig.pass(vec![egui::Event::PointerMoved(pos), button(pos, true)]);
        rig.pass(vec![egui::Event::PointerMoved(moved)]);
        rig.pass(vec![button(moved, false)]);
        for _ in 0..5 {
            rig.pass(vec![]);
        }
        assert_eq!(
            rig.settled, 0,
            "an arrow press must not open the selection popup"
        );
    }

    #[test]
    fn control_dragging_over_text_settles_a_selection() {
        let mut rig = Rig::new();
        for _ in 0..4 {
            rig.pass(vec![]);
        }
        let a = egui::pos2(rig.inner.left() + 20.0, rig.inner.top() + 40.0);
        let b = a + Vec2::new(120.0, 30.0);
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        rig.pass(vec![egui::Event::PointerMoved(a), button(a, true)]);
        rig.pass(vec![egui::Event::PointerMoved(b)]);
        rig.pass(vec![button(b, false)]);
        for _ in 0..5 {
            rig.pass(vec![]);
        }
        assert!(rig.settled > 0, "settled={}", rig.settled);
    }
}
