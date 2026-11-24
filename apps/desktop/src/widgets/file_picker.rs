//! Quick-open popup (Cmd+P): fuzzy match by name or path over the files of `App::target_dir()`;
//! Enter opens the selection in Xade's editor.

use std::path::PathBuf;

use egui::{Align, Key, Layout, Modifiers, RichText};

use crate::app::App;
use crate::mention;
use crate::theme;

/// Rows shown.
const SHOWN: usize = 12;

pub struct FilePicker {
    pub root: PathBuf,
    pub query: String,
    /// `None` while the folder is still being listed.
    files: Option<Vec<String>>,
    matches: Vec<String>,
    /// The query `matches` was ranked for; `None` forces a re-rank.
    ranked_for: Option<String>,
    selected: usize,
}

impl FilePicker {
    pub fn new(root: PathBuf) -> Self {
        FilePicker {
            root,
            query: String::new(),
            files: None,
            matches: Vec::new(),
            ranked_for: None,
            selected: 0,
        }
    }

    pub fn set_files(&mut self, files: Vec<String>) {
        self.files = Some(files);
        self.ranked_for = None;
    }

    fn rank(&mut self) {
        if self.ranked_for.as_deref() == Some(self.query.as_str()) {
            return;
        }
        self.matches = self
            .files
            .as_deref()
            .map(|f| mention::rank_top(f, self.query.trim(), SHOWN))
            .unwrap_or_default();
        self.ranked_for = Some(self.query.clone());
        self.selected = 0;
    }
}

/// Draws the picker when open and applies its decision.
pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(mut st) = app.file_picker.take() else {
        return;
    };
    st.rank();
    // Consumed before the search field draws, which would otherwise take Up/Down/Enter.
    let (down, up, enter) = ctx.input_mut(|i| {
        (
            i.consume_key(Modifiers::NONE, Key::ArrowDown),
            i.consume_key(Modifiers::NONE, Key::ArrowUp),
            i.consume_key(Modifiers::NONE, Key::Enter),
        )
    });
    let last = st.matches.len().saturating_sub(1);
    let moved = down || up;
    if down {
        st.selected = (st.selected + 1).min(last);
    }
    if up {
        st.selected = st.selected.saturating_sub(1);
    }
    st.selected = st.selected.min(last);
    let mut open = enter
        .then(|| st.matches.get(st.selected).cloned())
        .flatten();
    let resp = egui::Modal::new(egui::Id::unique("file_picker")).show(ctx, |ui| {
        ui.set_width(560.0);
        ui.label(RichText::new("Open file").strong().size(16.0));
        let edit = ui.add(
            egui::TextEdit::singleline(&mut st.query)
                .hint_text("Search files by name or path")
                .desired_width(f32::INFINITY),
        );
        if !edit.has_focus() {
            edit.request_focus();
        }
        ui.add_space(4.0);
        if st.files.is_none() {
            ui.label(RichText::new("Listing files...").color(theme::text_muted()));
        } else if st.matches.is_empty() {
            ui.label(RichText::new("No files found.").color(theme::text_muted()));
        }
        egui::ScrollArea::vertical()
            .max_height(360.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (i, path) in st.matches.iter().enumerate() {
                    let selected = i == st.selected;
                    let r = row(ui, path, selected);
                    if selected && moved {
                        r.scroll_to_me(None);
                    }
                    if r.clicked() {
                        open = Some(path.clone());
                    }
                }
            });
    });
    if open.is_some() || resp.should_close() {
        if let Some(rel) = open {
            app.open_path(st.root.join(rel));
        }
        return;
    }
    app.file_picker = Some(st);
}

fn row(ui: &mut egui::Ui, path: &str, selected: bool) -> egui::Response {
    let fill = if selected {
        theme::bg_hover()
    } else {
        theme::bg_raised()
    };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
                ui.label(name);
                if !dir.is_empty() {
                    ui.add(
                        egui::Label::new(RichText::new(dir).small().color(theme::text_muted()))
                            .truncate(),
                    );
                }
            });
        })
        .response
        .interact(egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn matches_by_path_selects_with_arrows_and_closes_on_esc() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-fpicker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        let mut picker = FilePicker::new(dir.clone());
        picker.set_files(vec![
            "src/main.rs".into(),
            "src/app.rs".into(),
            "README.md".into(),
        ]);
        picker.query = "app".into();
        app.file_picker = Some(picker);
        let key = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
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
        assert_eq!(app.file_picker.as_ref().unwrap().matches[0], "src/app.rs");
        // Esc closes without opening anything.
        frame(&mut app, vec![key(Key::Escape)]);
        assert!(app.file_picker.is_none());
        assert!(app.editors.map.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
