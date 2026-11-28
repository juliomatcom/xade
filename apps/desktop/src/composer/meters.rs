//! Context window ring and the token and usage text.

use egui::RichText;
use xade_core::UsageTotals;

use crate::theme;

const DONUT_SIZE: f32 = 16.0;
const DONUT_STROKE: f32 = 3.0;

/// Share of the context window in use; `None` while the window size is unknown.
fn context_fraction(u: &UsageTotals) -> Option<f32> {
    (u.context_window_max > 0).then(|| {
        (u.context_tokens_used as f64 / u.context_window_max as f64).clamp(0.0, 1.0) as f32
    })
}

/// Points along a clockwise arc from the top covering `fraction` of the circle.
fn arc_points(center: egui::Pos2, radius: f32, fraction: f32) -> Vec<egui::Pos2> {
    let steps = ((fraction * 64.0).ceil() as usize).max(1);
    (0..=steps)
        .map(|i| {
            let angle = -std::f32::consts::FRAC_PI_2
                + std::f32::consts::TAU * fraction * i as f32 / steps as f32;
            center + radius * egui::vec2(angle.cos(), angle.sin())
        })
        .collect()
}

/// The context ring and token totals, right-aligned in the rest of the row; the text is cut with
/// "…" when narrow (never a second row) and shows in full on hover.
pub(crate) fn session_stats(ui: &mut egui::Ui, u: &UsageTotals) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        context_donut(ui, u);
        let stats = usage_text(u);
        ui.add(
            egui::Label::new(RichText::new(&stats).small().color(theme::text_muted())).truncate(),
        )
        .on_hover_text(stats);
    });
}

/// The context window as a small ring; the numbers show on hover.
fn context_donut(ui: &mut egui::Ui, u: &UsageTotals) {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(DONUT_SIZE, DONUT_SIZE), egui::Sense::hover());
    let center = rect.center();
    let radius = (DONUT_SIZE - DONUT_STROKE) / 2.0;
    let painter = ui.painter();
    painter.circle_stroke(
        center,
        radius,
        egui::Stroke::new(DONUT_STROKE, theme::border_strong()),
    );
    if let Some(f) = context_fraction(u).filter(|f| *f > 0.0) {
        painter.add(egui::Shape::line(
            arc_points(center, radius, f),
            egui::Stroke::new(DONUT_STROKE, theme::usage_color(f)),
        ));
    }
    resp.on_hover_text(context_text(u));
}

pub(super) fn fmt_tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

fn context_text(u: &UsageTotals) -> String {
    if u.context_window_max == 0 {
        if u.context_tokens_used == 0 {
            return "ctx —".into();
        }
        return format!("ctx {} / ?", fmt_tokens(u.context_tokens_used));
    }
    let pct = u.context_tokens_used as f64 / u.context_window_max as f64 * 100.0;
    format!(
        "ctx {} / {} ({pct:.0}%)",
        fmt_tokens(u.context_tokens_used),
        fmt_tokens(u.context_window_max)
    )
}

fn usage_text(u: &UsageTotals) -> String {
    format!(
        "in {} · out {} · cache {}",
        fmt_tokens(u.input_tokens),
        fmt_tokens(u.output_tokens),
        fmt_tokens(u.cache_read_tokens),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn context_fraction_is_none_without_a_window_and_capped_at_one() {
        let mut u = UsageTotals {
            context_tokens_used: 500,
            ..Default::default()
        };
        assert_eq!(context_fraction(&u), None);
        u.context_window_max = 1000;
        assert_eq!(context_fraction(&u), Some(0.5));
        u.context_tokens_used = 5000;
        assert_eq!(context_fraction(&u), Some(1.0));
        assert_eq!(theme::usage_color(0.0), theme::ok(), "empty is green");
        assert_eq!(theme::usage_color(1.0), theme::danger(), "full is red");
    }

    #[test]
    fn the_arc_starts_at_the_top_and_goes_clockwise() {
        let c = egui::pos2(10.0, 10.0);
        let quarter = arc_points(c, 5.0, 0.25);
        let (first, last) = (quarter[0], *quarter.last().unwrap());
        assert!((first - egui::pos2(10.0, 5.0)).length() < 0.01, "{first:?}");
        assert!((last - egui::pos2(15.0, 10.0)).length() < 0.01, "{last:?}");
    }

    #[test]
    fn meters_format_compactly() {
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(12_345), "12.3k");
        assert_eq!(fmt_tokens(2_500_000), "2.5M");
        let u = UsageTotals {
            context_tokens_used: 50_000,
            context_window_max: 200_000,
            input_tokens: 1_200,
            output_tokens: 300,
            cache_read_tokens: 5_000,
            cost_usd: 0.1234,
        };
        assert_eq!(context_text(&u), "ctx 50.0k / 200.0k (25%)");
        assert_eq!(usage_text(&u), "in 1.2k · out 300 · cache 5.0k");
        assert_eq!(context_text(&UsageTotals::default()), "ctx —");
        let imported = UsageTotals {
            context_tokens_used: 42_000,
            ..UsageTotals::default()
        };
        assert_eq!(context_text(&imported), "ctx 42.0k / ?");
    }
}
