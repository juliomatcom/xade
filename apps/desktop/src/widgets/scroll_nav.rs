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
    }

    /// Scrolls to the content's origin (`ui.cursor().min` read before adding any content) if asked.
    pub fn jump_top(self, ui: &mut egui::Ui, content_top: egui::Pos2) {
        if self.top {
            let rect = Rect::from_min_size(content_top, Vec2::splat(1.0));
            // Unanimated, like the Team feed's jump to the end.
            ui.scroll_to_rect_animation(
                rect,
                Some(egui::Align::TOP),
                egui::style::ScrollAnimation::none(),
            );
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
            .interact(rect, id, Sense::click())
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
    }

    impl Rig {
        fn new() -> Self {
            Rig {
                ctx: egui::Context::default(),
                inner: Rect::NOTHING,
                offset: 0.0,
                frames: 0,
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
                            ui.allocate_space(Vec2::new(100.0, 2000.0));
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
}
