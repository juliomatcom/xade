//! `@` file mentions in the composer: the session folder's file list, fuzzy ranking and the
//! text replacement. The CLI expands the inserted `@path` itself (see claude-protocol.md).

use std::path::Path;
use std::time::{Duration, Instant};

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Rows shown in the popup.
pub const MAX_SHOWN: usize = 8;
/// The list is rebuilt when the popup opens and it is older than this.
pub const REFRESH: Duration = Duration::from_secs(30);
const MAX_FILES: usize = 20_000;
/// Folders never entered, even where no `.gitignore` names them.
const SKIP_DIRS: [&str; 3] = [".git", "node_modules", "target"];

/// The session folder's files, relative to it.
#[derive(Default)]
pub enum Files {
    #[default]
    Unloaded,
    Loading,
    Ready {
        files: Vec<String>,
        at: Instant,
    },
}

/// Per-session autocomplete state.
#[derive(Default)]
pub struct MentionState {
    pub files: Files,
    /// The query `matches` was ranked for; `None` forces a re-rank.
    ranked_for: Option<String>,
    pub matches: Vec<String>,
    pub selected: usize,
    /// Char index of an `@` the user dismissed with Esc.
    pub dismissed: Option<usize>,
    /// Where the popup was drawn last frame, so a click on it still counts after the input
    /// lost focus.
    pub popup_rect: Option<egui::Rect>,
}

impl MentionState {
    pub fn set_files(&mut self, files: Vec<String>) {
        self.files = Files::Ready {
            files,
            at: Instant::now(),
        };
        self.ranked_for = None;
    }

    /// Whether the list should be (re)built now.
    pub fn needs_index(&self) -> bool {
        match &self.files {
            Files::Unloaded => true,
            Files::Loading => false,
            Files::Ready { at, .. } => at.elapsed() >= REFRESH,
        }
    }

    /// Re-ranks when the query or the list changed; keeps the selection in range.
    pub fn update(&mut self, query: &str) {
        if self.ranked_for.as_deref() == Some(query) {
            return;
        }
        self.matches = match &self.files {
            Files::Ready { files, .. } => rank(files, query),
            _ => Vec::new(),
        };
        self.ranked_for = Some(query.to_string());
        self.selected = 0;
    }

    pub fn step(&mut self, down: bool) {
        let n = self.matches.len();
        if n > 0 {
            self.selected = if down {
                (self.selected + 1) % n
            } else {
                (self.selected + n - 1) % n
            };
        }
    }
}

/// An `@token` being typed at the cursor.
#[derive(Debug, PartialEq)]
pub struct Active {
    /// Char index of the `@`.
    pub start: usize,
    pub query: String,
}

/// The mention ending at the cursor (`cursor` is a char index): an `@` at the start or after
/// whitespace, followed by non-space characters.
pub fn active(input: &str, cursor: usize) -> Option<Active> {
    let before: Vec<char> = input.chars().take(cursor).collect();
    let mut i = before.len();
    while i > 0 {
        i -= 1;
        match before[i] {
            c if c.is_whitespace() => return None,
            '@' => {
                if i > 0 && !before[i - 1].is_whitespace() {
                    return None;
                }
                let query: String = before[i + 1..].iter().collect();
                let query = query.strip_prefix('"').unwrap_or(&query).to_string();
                return (!query.contains('"')).then_some(Active { start: i, query });
            }
            _ => {}
        }
    }
    None
}

/// The best `MAX_SHOWN` files for `query`; the first files when it is empty.
pub fn rank(files: &[String], query: &str) -> Vec<String> {
    rank_top(files, query, MAX_SHOWN)
}

/// The best `limit` files for `query`; the first files when it is empty.
pub fn rank_top(files: &[String], query: &str, limit: usize) -> Vec<String> {
    if query.is_empty() {
        return files.iter().take(limit).cloned().collect();
    }
    let pattern = Pattern::new(
        query,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, &String)> = files
        .iter()
        .filter_map(|f| {
            let score = pattern.score(Utf32Str::new(f, &mut buf), &mut matcher)?;
            Some((score, f))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, f)| f.clone())
        .collect()
}

/// `@path`, quoted when the path has whitespace.
pub fn token(path: &str) -> String {
    if path.contains(char::is_whitespace) {
        format!("@\"{path}\"")
    } else {
        format!("@{path}")
    }
}

/// Replaces the `@query` between `start` and `cursor` (char indices) with the mention of
/// `path` and a space; returns the new text and the cursor after it.
pub fn apply(input: &str, start: usize, cursor: usize, path: &str) -> (String, usize) {
    let chars: Vec<char> = input.chars().collect();
    let (start, cursor) = (start.min(chars.len()), cursor.min(chars.len()));
    let mention = format!("{} ", token(path));
    let mut out: String = chars[..start].iter().collect();
    out.push_str(&mention);
    out.extend(&chars[cursor..]);
    (out, start + mention.chars().count())
}

/// Files under `root`, relative and sorted, skipping what `.gitignore`, `.ignore` and the
/// global excludes name (via the `ignore` crate) plus `SKIP_DIRS`. Blocking.
pub fn list_files(root: &Path) -> Vec<String> {
    let walk = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| !SKIP_DIRS.iter().any(|s| e.file_name() == *s))
        .build();
    let mut files = Vec::new();
    for entry in walk.flatten() {
        if entry.file_type().is_some_and(|t| t.is_file()) {
            if let Ok(rel) = entry.path().strip_prefix(root) {
                files.push(rel.to_string_lossy().into_owned());
            }
        }
        if files.len() >= MAX_FILES {
            break;
        }
    }
    files.sort();
    files
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("xade-mention-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn active_mention_is_the_at_token_before_the_cursor() {
        let a = |s: &str, c: usize| active(s, c);
        assert_eq!(
            a("@", 1),
            Some(Active {
                start: 0,
                query: String::new()
            })
        );
        assert_eq!(a("look at @src/ma", 15).unwrap().query, "src/ma");
        assert_eq!(a("look at @src/ma", 15).unwrap().start, 8);
        // The cursor decides: mid-token only the part before it counts.
        assert_eq!(a("@abcdef", 3).unwrap().query, "ab");
        assert_eq!(a("@\"my", 4).unwrap().query, "my");
        // A space ends the mention; names with spaces are picked from the list, then quoted.
        assert_eq!(a("@my no", 6), None);
        assert_eq!(a("é @é", 4).unwrap().start, 2);
        // Not mentions: an email, a finished token, plain text, a closed quote.
        assert_eq!(a("me@example.com", 14), None);
        assert_eq!(a("@a.rs next", 10), None);
        assert_eq!(a("no mention", 10), None);
        assert_eq!(a("@\"a b\"", 6), None);
    }

    #[test]
    fn ranking_is_fuzzy_prefers_short_paths_and_limits_rows() {
        let files: Vec<String> = [
            "src/main.rs",
            "src/app.rs",
            "docs/main-notes.md",
            "README.md",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(rank(&files, "mainrs")[0], "src/main.rs");
        assert_eq!(rank(&files, "read")[0], "README.md");
        assert!(rank(&files, "zzz").is_empty());
        assert_eq!(rank(&files, "").len(), files.len());
        let many: Vec<String> = (0..50).map(|i| format!("f{i}.rs")).collect();
        assert_eq!(rank(&many, "f").len(), MAX_SHOWN);
    }

    #[test]
    fn apply_replaces_the_token_and_places_the_cursor_after_it() {
        let (text, cursor) = apply("look at @ma and more", 8, 11, "src/main.rs");
        assert_eq!(text, "look at @src/main.rs  and more");
        assert_eq!(cursor, 8 + "@src/main.rs ".chars().count());
        let (text, _) = apply("@x", 0, 2, "my notes.txt");
        assert_eq!(text, "@\"my notes.txt\" ");
        // Multi-byte text before the token keeps char indexing right.
        let (text, cursor) = apply("é @a", 2, 4, "a.rs");
        assert_eq!((text.as_str(), cursor), ("é @a.rs ", 8));
    }

    #[test]
    fn state_reranks_on_new_query_or_files_and_steps_around() {
        let mut s = MentionState::default();
        assert!(s.needs_index());
        s.files = Files::Loading;
        assert!(!s.needs_index());
        s.update("a");
        assert!(s.matches.is_empty());
        s.set_files(vec!["a.rs".into(), "b.rs".into(), "ab.rs".into()]);
        assert!(!s.needs_index());
        s.update("a");
        assert_eq!(s.matches.len(), 2);
        s.step(true);
        assert_eq!(s.selected, 1);
        s.step(true);
        assert_eq!(s.selected, 0);
        s.step(false);
        assert_eq!(s.selected, 1);
        s.update("b");
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn listing_skips_heavy_folders_and_keeps_dotfiles() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/x")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "").unwrap();
        std::fs::write(dir.join("top.txt"), "").unwrap();
        std::fs::write(dir.join(".env.example"), "").unwrap();
        std::fs::write(dir.join("node_modules/x/y.js"), "").unwrap();
        assert_eq!(list_files(&dir), [".env.example", "src/a.rs", "top.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_in_a_git_repo_honors_gitignore() {
        let dir = temp_dir();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(init.status.success());
        std::fs::write(dir.join(".gitignore"), "ignored.txt\nbuild/\n").unwrap();
        std::fs::create_dir_all(dir.join("build")).unwrap();
        std::fs::write(dir.join("build/out.o"), "").unwrap();
        std::fs::write(dir.join("ignored.txt"), "").unwrap();
        std::fs::write(dir.join("tracked.rs"), "").unwrap();
        std::fs::write(dir.join("new file.md"), "").unwrap();
        assert_eq!(
            list_files(&dir),
            [".gitignore", "new file.md", "tracked.rs"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
