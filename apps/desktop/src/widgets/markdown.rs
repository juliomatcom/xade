//! The one markdown helper every message text goes through.

use std::fmt::Debug;
use std::hash::Hash;

use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

const CODE_BLOCK_OVERSHOOT: f32 = 16.0;

/// Renders `text` as markdown. An unclosed fence renders as code to the end of the text.
pub fn markdown(ui: &mut egui::Ui, cache: &mut CommonMarkCache, id: impl Hash + Debug, text: &str) {
    ui.push_id(id, |ui| {
        // Code blocks are text edits that overshoot the width they are given by their frame.
        ui.set_max_width((ui.available_width() - CODE_BLOCK_OVERSHOOT).max(40.0));
        CommonMarkViewer::new().show(ui, cache, text);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(text: &str) {
        let ctx = egui::Context::default();
        let mut cache = CommonMarkCache::default();
        for _ in 0..2 {
            let out = ctx.run_ui(egui::RawInput::default(), |ui| {
                markdown(ui, &mut cache, 1, text);
            });
            out.drop_without_applying_deltas();
        }
    }

    #[test]
    fn unclosed_fence_does_not_panic() {
        render("Here:\n\n```rust\nfn main() {\n    let x = 1;");
        render("```");
        render("");
    }

    #[test]
    fn table_list_and_inline_code_render() {
        render("# T\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n- one\n- `two`\n\n> quote\n\n[l](https://x.y)");
    }
}
