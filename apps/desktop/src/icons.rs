//! Button glyphs; every constant must exist in egui's default fonts (checked by `glyphs_exist`).
//! The emoji fonts need egui's `monochrome_emoji_fonts` feature (set in `Cargo.toml`).

pub const NEW_SESSION: &str = "\u{2795}";
pub const ADD_DIR: &str = "\u{1F4C1}";
pub const TILE: &str = "\u{2B1B}";
pub const HOME: &str = "\u{1F3E0}";
pub const WORKSPACE: &str = "\u{1F5A5}";
pub const SEND: &str = "\u{1F528}";
pub const STOP: &str = "\u{23F9}";
pub const CLOSE: &str = "\u{2716}";
pub const COLLAPSE: &str = "\u{1F5D5}";
pub const MAXIMIZE: &str = "\u{1F5D6}";
pub const RESTORE: &str = "\u{1F5D7}";
pub const APPROVE: &str = "\u{2714}";
pub const DENY: &str = "\u{1F6AB}";
pub const ACCEPT: &str = "\u{2714}";
pub const CANCEL: &str = "\u{2716}";
pub const VSCODE: &str = "\u{2197}";
pub const REVIEW: &str = "\u{1F50D}";
pub const RETRY: &str = "\u{1F504}";
pub const PREV: &str = "\u{23F4}";
pub const NEXT: &str = "\u{23F5}";
pub const AUTO: &str = "\u{1F680}";
pub const IMPORT: &str = "\u{1F4E5}";
pub const OPEN_SESSION: &str = "\u{1F4C2}";
pub const SHORTCUTS: &str = "\u{2328}";
pub const SETTINGS: &str = "\u{2699}";
pub const WARNING: &str = "\u{26A0}";
pub const COPY: &str = "\u{1F4CB}";
pub const BUG: &str = "\u{1F41B}";
pub const TRASH: &str = "\u{1F5D1}";
pub const FOLDER: &str = ADD_DIR;
pub const FOLDER_OPEN: &str = OPEN_SESSION;
pub const FILE: &str = "\u{1F4C4}";
pub const COLLAPSED: &str = "\u{23F5}";
pub const EXPANDED: &str = "\u{23F7}";

/// `"{icon} {label}"`.
pub fn with(icon: &str, label: &str) -> String {
    format!("{icon} {label}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_exist() {
        let all = [
            NEW_SESSION,
            ADD_DIR,
            TILE,
            HOME,
            WORKSPACE,
            SEND,
            STOP,
            CLOSE,
            COLLAPSE,
            MAXIMIZE,
            RESTORE,
            APPROVE,
            DENY,
            ACCEPT,
            CANCEL,
            VSCODE,
            REVIEW,
            RETRY,
            PREV,
            NEXT,
            AUTO,
            IMPORT,
            OPEN_SESSION,
            SHORTCUTS,
            SETTINGS,
            WARNING,
            COPY,
            BUG,
            TRASH,
            FILE,
            COLLAPSED,
            EXPANDED,
        ];
        let ctx = egui::Context::default();
        // Fonts are installed on the first pass.
        ctx.run_pass(egui::RawInput::default(), |_| {})
            .drop_without_applying_deltas();
        let missing: Vec<String> = ctx.fonts_mut(|f| {
            all.iter()
                .filter(|g| !f.has_glyphs(&egui::FontId::proportional(13.5), g))
                .map(|g| format!("U+{:04X}", g.chars().next().map_or(0, |c| c as u32)))
                .collect()
        });
        assert!(missing.is_empty(), "missing glyphs: {missing:?}");
    }
}
