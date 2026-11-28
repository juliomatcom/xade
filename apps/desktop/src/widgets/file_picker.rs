//! Quick-open popup (Cmd+P): fuzzy match by name or path over the files of the selected
//! workspace, or of every listed workspace when none is selected; Enter opens the selection in
//! Xade's editor.

use std::path::PathBuf;

use egui::{Align, Key, Layout, Modifiers, RichText};

use crate::app::App;
use crate::mention;
use crate::theme;

/// Rows ranked and listed; the list scrolls.
const SHOWN: usize = 100;
/// Shorter queries match too much to be useful, so nothing is listed for them.
const MIN_CHARS: usize = 2;

struct Root {
    name: String,
    path: PathBuf,
}

pub struct FilePicker {
    roots: Vec<Root>,
    /// A single workspace is selected, so rows need no workspace label.
    scoped: bool,
    pub query: String,
    /// `None` while the folders are still being listed.
    listed: Option<Listed>,
    /// Indexes into `listed.paths`.
    matches: Vec<usize>,
    /// Files and folders matching the query in all, not only the rows listed.
    file_total: usize,
    dir_total: usize,
    /// The query `matches` was ranked for; `None` forces a re-rank.
    ranked_for: Option<String>,
    selected: usize,
}

/// Every file of every root: paths relative to their root, and which root each belongs to;
/// plus the folders those files sit in.
#[derive(Default)]
struct Listed {
    paths: Vec<String>,
    owner: Vec<usize>,
    dirs: Vec<String>,
}

impl FilePicker {
    pub fn new(roots: Vec<(String, PathBuf)>, scoped: bool) -> Self {
        let empty = roots.is_empty();
        let mut picker = FilePicker {
            roots: roots
                .into_iter()
                .map(|(name, path)| Root { name, path })
                .collect(),
            scoped,
            query: String::new(),
            listed: None,
            matches: Vec::new(),
            file_total: 0,
            dir_total: 0,
            ranked_for: None,
            selected: 0,
        };
        if empty {
            picker.listed = Some(Listed::default());
        }
        picker
    }

    pub fn has_roots(&self, roots: &[PathBuf]) -> bool {
        self.roots.len() == roots.len() && self.roots.iter().zip(roots).all(|(r, p)| r.path == *p)
    }

    /// One list per root, in root order.
    pub fn set_files(&mut self, files: Vec<Vec<String>>) {
        let mut listed = Listed::default();
        for (i, list) in files.into_iter().enumerate() {
            listed.owner.extend(std::iter::repeat_n(i, list.len()));
            // A folder seen once has all its parents seen too.
            let mut seen = std::collections::HashSet::new();
            for p in &list {
                let mut dir = p.as_str();
                while let Some((parent, _)) = dir.rsplit_once('/') {
                    if !seen.insert(parent) {
                        break;
                    }
                    dir = parent;
                }
            }
            listed.dirs.extend(seen.into_iter().map(str::to_string));
            listed.paths.extend(list);
        }
        self.listed = Some(listed);
        self.ranked_for = None;
    }

    fn rank(&mut self) {
        if self.ranked_for.as_deref() == Some(self.query.as_str()) {
            return;
        }
        let query = self.query.trim();
        (self.matches, self.file_total, self.dir_total) = match &self.listed {
            Some(l) if self.long_enough() => {
                let (rows, files) = mention::rank_counted(&l.paths, query, SHOWN);
                (rows, files, mention::rank_counted(&l.dirs, query, 0).1)
            }
            _ => (Vec::new(), 0, 0),
        };
        self.ranked_for = Some(self.query.clone());
        self.selected = 0;
    }

    fn long_enough(&self) -> bool {
        self.query.trim().chars().count() >= MIN_CHARS
    }

    /// "161 files, 12 folders match", with a nudge when the list is cut.
    fn summary(&self) -> String {
        let (f, d) = (self.file_total, self.dir_total);
        let plural =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut text = format!(
            "{}, {} match",
            plural(f, "file", "files"),
            plural(d, "folder", "folders")
        );
        if f > self.matches.len() {
            text.push_str(&format!(
                " \u{2014} showing the best {}, type more to narrow",
                self.matches.len()
            ));
        }
        text
    }

    fn title(&self) -> String {
        match (self.roots.as_slice(), self.scoped) {
            ([], _) => "Open file".into(),
            ([r], _) => format!("Open file in {}", r.name),
            _ => format!("Open file in all workspaces ({})", self.roots.len()),
        }
    }

    /// The absolute path of match `i`.
    fn path_of(&self, i: usize) -> Option<PathBuf> {
        let l = self.listed.as_ref()?;
        Some(self.roots.get(*l.owner.get(i)?)?.path.join(l.paths.get(i)?))
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
        .then(|| st.matches.get(st.selected).copied())
        .flatten();
    let resp = crate::widgets::modal(ctx, egui::Id::unique("file_picker")).show(ctx, |ui| {
        ui.set_width(560.0);
        ui.label(RichText::new(st.title()).strong().size(16.0));
        let edit = ui.add(
            egui::TextEdit::singleline(&mut st.query)
                .hint_text("Search files by name or path")
                .desired_width(f32::INFINITY),
        );
        if !edit.has_focus() {
            edit.request_focus();
        }
        ui.add_space(4.0);
        if st.roots.is_empty() {
            ui.label(
                RichText::new("No workspace to search. Add one in the sidebar.")
                    .color(theme::text_muted()),
            );
        } else if st.listed.is_none() {
            ui.label(RichText::new("Listing files...").color(theme::text_muted()));
        } else if !st.long_enough() {
            ui.label(
                RichText::new(format!("Type at least {MIN_CHARS} characters."))
                    .color(theme::text_muted()),
            );
        } else if st.matches.is_empty() {
            ui.label(RichText::new("No files or folders match.").color(theme::text_muted()));
        } else {
            ui.label(
                RichText::new(st.summary())
                    .small()
                    .color(theme::text_muted()),
            );
        }
        egui::ScrollArea::vertical()
            .max_height(360.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let Some(listed) = &st.listed else { return };
                for (n, &i) in st.matches.iter().enumerate() {
                    let selected = n == st.selected;
                    let workspace = (!st.scoped && st.roots.len() > 1)
                        .then(|| st.roots[listed.owner[i]].name.as_str());
                    let r = row(ui, &listed.paths[i], workspace, selected);
                    if selected && moved {
                        r.scroll_to_me(None);
                    }
                    if r.clicked() {
                        open = Some(i);
                    }
                }
            });
    });
    if open.is_some() || resp.should_close() {
        if let Some(path) = open.and_then(|i| st.path_of(i)) {
            app.open_path(path);
        }
        return;
    }
    app.file_picker = Some(st);
}

fn row(ui: &mut egui::Ui, path: &str, workspace: Option<&str>, selected: bool) -> egui::Response {
    let fill = if selected {
        theme::bg_hover()
    } else {
        theme::bg_raised()
    };
    egui::Frame::new()
        .fill(fill)
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(name) = workspace {
                    ui.label(RichText::new(name).small().color(theme::text_muted()));
                }
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

    fn picker(roots: &[(&str, &str)], scoped: bool) -> FilePicker {
        FilePicker::new(
            roots
                .iter()
                .map(|(n, p)| (n.to_string(), PathBuf::from(p)))
                .collect(),
            scoped,
        )
    }

    #[test]
    fn files_of_several_roots_rank_together_and_resolve_to_their_own_root() {
        let mut p = picker(&[("one", "/r1"), ("two", "/r2")], false);
        assert!(p.listed.is_none());
        p.set_files(vec![
            vec!["src/main.rs".into()],
            vec!["notes/2026-11-23.md".into(), "src/main.rs".into()],
        ]);
        p.query = "2026".into();
        p.rank();
        assert_eq!(p.matches.len(), 1);
        assert_eq!(
            p.path_of(p.matches[0]),
            Some(PathBuf::from("/r2/notes/2026-11-23.md"))
        );
        p.query = "main".into();
        p.rank();
        let paths: Vec<_> = p.matches.iter().filter_map(|&i| p.path_of(i)).collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("/r1/src/main.rs"),
                PathBuf::from("/r2/src/main.rs")
            ]
        );
        assert_eq!(p.title(), "Open file in all workspaces (2)");
        assert_eq!(picker(&[("one", "/r1")], true).title(), "Open file in one");
    }

    #[test]
    fn counts_files_and_folders_and_waits_for_enough_characters() {
        let mut p = picker(&[("one", "/r1")], true);
        p.set_files(vec![vec![
            "docs/notes/2026-01.md".into(),
            "docs/notes/2026-02.md".into(),
            "docs/2025/a.md".into(),
            "src/main.rs".into(),
        ]]);
        p.query = "2".into();
        p.rank();
        assert!(p.matches.is_empty() && p.file_total == 0 && !p.long_enough());
        p.query = "2026".into();
        p.rank();
        assert_eq!((p.file_total, p.dir_total), (2, 0));
        assert_eq!(p.summary(), "2 files, 0 folders match");
        p.query = "notes".into();
        p.rank();
        assert_eq!((p.file_total, p.dir_total), (2, 1));
        p.query = "docs".into();
        p.rank();
        assert_eq!((p.file_total, p.dir_total), (3, 3));
        p.query = "src".into();
        p.rank();
        assert_eq!(p.summary(), "1 file, 1 folder match");
    }

    #[test]
    fn no_workspace_means_nothing_to_search() {
        let p = picker(&[], false);
        assert!(p.listed.as_ref().is_some_and(|l| l.paths.is_empty()));
        assert!(!p.has_roots(&[PathBuf::from("/x")]));
        assert!(p.has_roots(&[]));
    }

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
        let mut picker = picker(&[("proj", dir.to_str().unwrap())], true);
        picker.set_files(vec![vec![
            "src/main.rs".into(),
            "src/app.rs".into(),
            "README.md".into(),
        ]]);
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
        let st = app.file_picker.as_ref().unwrap();
        assert_eq!(
            st.listed.as_ref().unwrap().paths[st.matches[0]],
            "src/app.rs"
        );
        // Esc closes without opening anything.
        frame(&mut app, vec![key(Key::Escape)]);
        assert!(app.file_picker.is_none());
        assert!(app.editors.map.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
