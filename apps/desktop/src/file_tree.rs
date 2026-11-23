//! Sidebar file tree: lazy `read_dir` per expanded folder, cached until it is collapsed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use egui::text::{LayoutJob, TextWrapping};
use egui::{Sense, TextStyle};

use crate::{icons, theme};

/// Entries shown per folder; the rest collapse into a "… N more" row.
pub const MAX_ENTRIES: usize = 2000;
const INDENT: f32 = 14.0;
const CHEVRON_W: f32 = 14.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    /// Shown, never followed into.
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub kind: Kind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// Entries beyond the cap.
    pub more: usize,
}

enum Node {
    Loading,
    Loaded(Listing),
    Failed(String),
}

/// What a click on a row asks the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeAction {
    Toggle(PathBuf),
    Open(PathBuf),
}

/// A folder is expanded exactly when it has a node.
#[derive(Default)]
pub struct FileTree {
    nodes: HashMap<PathBuf, Node>,
}

impl FileTree {
    pub fn is_expanded(&self, path: &Path) -> bool {
        self.nodes.contains_key(path)
    }

    /// Collapses an expanded folder (dropping its cache and its subfolders') and returns
    /// `false`; otherwise marks it loading and returns `true`: the caller reads the directory.
    pub fn toggle(&mut self, path: &Path) -> bool {
        if self.is_expanded(path) {
            self.nodes.retain(|p, _| !p.starts_with(path));
            false
        } else {
            self.nodes.insert(path.to_path_buf(), Node::Loading);
            true
        }
    }

    /// Stores a read result; dropped when the folder was collapsed meanwhile.
    pub fn loaded(&mut self, path: &Path, result: Result<Listing, String>) {
        if let Some(node) = self.nodes.get_mut(path) {
            *node = match result {
                Ok(l) => Node::Loaded(l),
                Err(e) => Node::Failed(e),
            };
        }
    }
}

/// Folders first, then case-insensitive by name; at most `max` entries kept.
pub fn sort_and_cap(mut entries: Vec<Entry>, max: usize) -> Listing {
    entries.sort_by_cached_key(|e| (e.kind != Kind::Dir, e.name.to_lowercase()));
    let more = entries.len().saturating_sub(max);
    entries.truncate(max);
    Listing { entries, more }
}

/// Reads one directory level (blocking). Unreadable entries are skipped.
pub fn read_listing(dir: &Path) -> Result<Listing, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| {
        let s = e.to_string();
        s.split(" (os error").next().unwrap_or(&s).to_string()
    })?;
    let mut unreadable = 0;
    let entries: Vec<Entry> = rd
        .filter_map(|e| e.map_err(|_| unreadable += 1).ok())
        .map(|e| {
            // `DirEntry::file_type` does not follow symlinks.
            let kind = match e.file_type() {
                Ok(t) if t.is_symlink() => Kind::Symlink,
                Ok(t) if t.is_dir() => Kind::Dir,
                _ => Kind::File,
            };
            Entry {
                name: e.file_name().to_string_lossy().into_owned(),
                path: e.path(),
                kind,
            }
        })
        .collect();
    if unreadable > 0 {
        tracing::warn!(dir = %dir.display(), unreadable, "skipped unreadable directory entries");
    }
    Ok(sort_and_cap(entries, MAX_ENTRIES))
}

pub fn click_action(e: &Entry) -> TreeAction {
    match e.kind {
        Kind::Dir => TreeAction::Toggle(e.path.clone()),
        Kind::File | Kind::Symlink => TreeAction::Open(e.path.clone()),
    }
}

/// Draws the children of the expanded folder `dir` at `depth`, walking only expanded folders.
pub fn show(
    ui: &mut egui::Ui,
    tree: &FileTree,
    dir: &Path,
    depth: usize,
    changed: Option<&HashSet<PathBuf>>,
    out: &mut Vec<TreeAction>,
) {
    match tree.nodes.get(dir) {
        None => {}
        Some(Node::Loading) => note(ui, depth, "…"),
        Some(Node::Failed(e)) => note(ui, depth, e),
        Some(Node::Loaded(l)) => {
            for e in &l.entries {
                let open = e.kind == Kind::Dir && tree.is_expanded(&e.path);
                let dirty = changed.is_some_and(|c| c.contains(&e.path));
                if row(ui, depth, e, open, dirty) {
                    out.push(click_action(e));
                }
                if open {
                    show(ui, tree, &e.path, depth + 1, changed, out);
                }
            }
            if l.more > 0 {
                note(ui, depth, &format!("… {} more", l.more));
            }
        }
    }
}

fn row_rect(ui: &mut egui::Ui, sense: Sense) -> (egui::Rect, egui::Response) {
    let size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
    ui.allocate_exact_size(size, sense)
}

fn paint_text(ui: &egui::Ui, rect: egui::Rect, x: f32, text: &str, color: egui::Color32) {
    let font = TextStyle::Body.resolve(ui.style());
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = TextWrapping::truncate_at_width((rect.right() - x).max(0.0));
    let galley = ui.painter().layout_job(job);
    let pos = egui::pos2(x, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(pos, galley, color);
}

fn note(ui: &mut egui::Ui, depth: usize, text: &str) {
    let (rect, _) = row_rect(ui, Sense::hover());
    if ui.is_rect_visible(rect) {
        let x = rect.left() + INDENT * depth as f32 + CHEVRON_W;
        paint_text(ui, rect, x, text, theme::TEXT_MUTED);
    }
}

/// One entry row; `true` when clicked. Rows scrolled out of view are allocated, not painted.
/// `dirty`: the entry (or a file inside it) has uncommitted changes.
fn row(ui: &mut egui::Ui, depth: usize, e: &Entry, open: bool, dirty: bool) -> bool {
    let (rect, resp) = row_rect(ui, Sense::click());
    if ui.is_rect_visible(rect) {
        if resp.hovered() {
            ui.painter().rect_filled(rect, 3.0, theme::BG_HOVER);
        }
        let x = rect.left() + INDENT * depth as f32;
        let icon = match (e.kind, open) {
            (Kind::Dir, true) => icons::FOLDER_OPEN,
            (Kind::Dir, false) => icons::FOLDER,
            _ => icons::FILE,
        };
        if e.kind == Kind::Dir {
            let chevron = if open {
                icons::EXPANDED
            } else {
                icons::COLLAPSED
            };
            paint_text(ui, rect, x, chevron, theme::TEXT_MUTED);
        }
        paint_text(
            ui,
            rect,
            x + CHEVRON_W,
            &icons::with(icon, &e.name),
            if dirty { theme::WARN } else { theme::TEXT },
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(&e.name)
        .clicked()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: Kind) -> Entry {
        Entry {
            name: name.into(),
            path: PathBuf::from("/r").join(name),
            kind,
        }
    }

    fn names(l: &Listing) -> Vec<&str> {
        l.entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn sorts_folders_first_then_case_insensitive_and_keeps_dotfiles() {
        let l = sort_and_cap(
            vec![
                entry("b.txt", Kind::File),
                entry(".env", Kind::File),
                entry("Zed", Kind::Dir),
                entry("A.md", Kind::File),
                entry(".git", Kind::Dir),
                entry("alpha", Kind::Dir),
                entry("link", Kind::Symlink),
            ],
            MAX_ENTRIES,
        );
        assert_eq!(
            names(&l),
            [".git", "alpha", "Zed", ".env", "A.md", "b.txt", "link"]
        );
        assert_eq!(l.more, 0);
    }

    #[test]
    fn cap_keeps_the_first_entries_and_counts_the_rest() {
        let many: Vec<Entry> = (0..MAX_ENTRIES + 5)
            .map(|i| entry(&format!("f{i:05}"), Kind::File))
            .collect();
        let l = sort_and_cap(many, MAX_ENTRIES);
        assert_eq!((l.entries.len(), l.more), (MAX_ENTRIES, 5));
        assert_eq!(l.entries[0].name, "f00000");
    }

    #[cfg(unix)]
    #[test]
    fn read_listing_handles_nested_dotfile_symlink_and_unreadable_dir() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = std::env::temp_dir().join(format!("xade-tree-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("src/nested/deep.rs"), "").unwrap();
        std::fs::write(root.join(".hidden"), "").unwrap();
        std::fs::write(root.join("a.txt"), "").unwrap();
        symlink(root.join("src"), root.join("to_src")).unwrap();
        let locked = root.join("locked");
        std::fs::create_dir(&locked).unwrap();

        let top = read_listing(&root).unwrap();
        assert_eq!(names(&top), ["locked", "src", ".hidden", "a.txt", "to_src"]);
        let link = top.entries.iter().find(|e| e.name == "to_src").unwrap();
        assert_eq!(link.kind, Kind::Symlink);
        let nested = read_listing(&root.join("src")).unwrap();
        assert_eq!(names(&nested), ["nested"]);
        assert_eq!(
            names(&read_listing(&root.join("src/nested")).unwrap()),
            ["deep.rs"]
        );

        // Skipped when chmod does not take effect (running as root, odd filesystems).
        if std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).is_ok() {
            if let Err(e) = read_listing(&locked) {
                assert!(e.contains("Permission denied"), "{e}");
            }
            let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
        }
        assert!(read_listing(&root.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn toggle_collapses_with_descendants_and_late_results_are_dropped() {
        let mut t = FileTree::default();
        let (root, sub) = (PathBuf::from("/r"), PathBuf::from("/r/sub"));
        assert!(t.toggle(&root) && t.toggle(&sub));
        assert!(!t.toggle(&root));
        assert!(!t.is_expanded(&root) && !t.is_expanded(&sub));
        t.loaded(&root, Ok(Listing::default()));
        assert!(!t.is_expanded(&root));
    }

    #[test]
    fn clicks_toggle_folders_and_open_files() {
        assert_eq!(
            click_action(&entry("d", Kind::Dir)),
            TreeAction::Toggle(PathBuf::from("/r/d"))
        );
        assert_eq!(
            click_action(&entry("f.rs", Kind::File)),
            TreeAction::Open(PathBuf::from("/r/f.rs"))
        );
    }

    /// A real pointer click on the first row of an expanded tree yields its action.
    #[test]
    fn clicking_a_file_row_emits_open() {
        let mut tree = FileTree::default();
        let root = PathBuf::from("/r");
        tree.toggle(&root);
        let file = entry("a.txt", Kind::File);
        tree.loaded(
            &root,
            Ok(Listing {
                entries: vec![file.clone()],
                more: 0,
            }),
        );
        let ctx = egui::Context::default();
        let frame = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(300.0, 200.0),
                )),
                events,
                ..Default::default()
            };
            let mut out = Vec::new();
            let mut full = ctx.run_ui(input, |ui| show(ui, &tree, &root, 0, None, &mut out));
            full.textures_delta.clear();
            out
        };
        let pos = egui::pos2(40.0, 8.0);
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![]);
        frame(vec![egui::Event::PointerMoved(pos)]);
        frame(vec![button(true)]);
        let out = frame(vec![button(false)]);
        assert_eq!(out, vec![TreeAction::Open(file.path)]);
    }
}
