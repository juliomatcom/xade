//! The one markdown helper every message text goes through.

use std::fmt::Debug;
use std::hash::Hash;

use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

const CODE_BLOCK_OVERSHOOT: f32 = 16.0;
/// Code block theme: the default one paints blocks a bluish gray, lighter than the window.
const CODE_THEME: &str = "xade-code";
const CODE_THEME_FILE: &[u8] = include_bytes!("../../../../assets/xade-code.tmTheme");

/// A markdown cache that knows the code block theme; use it wherever a message is rendered.
pub fn new_cache() -> CommonMarkCache {
    let mut cache = CommonMarkCache::default();
    if let Err(e) = cache.add_syntax_theme_from_bytes(CODE_THEME, CODE_THEME_FILE) {
        tracing::warn!("code theme not loaded: {e}");
    }
    cache
}

/// Renders `text` as markdown. An unclosed fence renders as code to the end of the text.
pub fn markdown(ui: &mut egui::Ui, cache: &mut CommonMarkCache, id: impl Hash + Debug, text: &str) {
    ui.push_id(id, |ui| {
        // Code blocks are text edits that overshoot the width they are given by their frame.
        ui.set_max_width((ui.available_width() - CODE_BLOCK_OVERSHOOT).max(40.0));
        // The code theme sets no background, so blocks take egui's `extreme_bg_color`.
        ui.visuals_mut().extreme_bg_color = crate::theme::bg_terminal();
        CommonMarkViewer::new()
            .syntax_theme_dark(CODE_THEME)
            .show(ui, cache, text);
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
    fn the_code_theme_file_parses() {
        let mut cache = CommonMarkCache::default();
        cache
            .add_syntax_theme_from_bytes(CODE_THEME, CODE_THEME_FILE)
            .expect("assets/xade-code.tmTheme must be a valid tmTheme");
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
