//! The open session and file windows, saved to `windows.txt` beside `settings.json` so a plain
//! launch reopens them (tiled, in the same order). One window per line: `session <uuid>` or
//! `file <path>`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use xade_core::SessionId;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Saved {
    Session(SessionId),
    File(PathBuf),
}

/// Where the list lives, next to the settings file.
pub fn path(settings: &Path) -> PathBuf {
    settings.with_file_name("windows.txt")
}

/// The saved list; a missing file is an empty list and an unreadable line is skipped.
pub fn load(path: &Path) -> Vec<Saved> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| match line.split_once(' ')? {
            ("session", id) => Some(Saved::Session(SessionId(id.parse().ok()?))),
            ("file", p) => Some(Saved::File(PathBuf::from(p))),
            _ => None,
        })
        .collect()
}

/// The newest list not yet written, by file; a write task drains it, so writes land in order
/// and a burst of changes costs one write.
static PENDING: Mutex<Vec<(PathBuf, Vec<Saved>)>> = Mutex::new(Vec::new());

/// Writes `windows` on the blocking pool, newest list last.
pub fn save_later(rt: &tokio::runtime::Handle, path: PathBuf, windows: Vec<Saved>) {
    {
        let mut pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
        pending.retain(|(p, _)| *p != path);
        pending.push((path, windows));
    }
    rt.spawn_blocking(|| {
        let mut pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
        for (path, windows) in pending.drain(..) {
            if let Err(e) = save(&path, &windows) {
                tracing::warn!(error = %e, "could not save the open windows");
            }
        }
    });
}

pub fn save(path: &Path, windows: &[Saved]) -> std::io::Result<()> {
    let mut text = String::new();
    for w in windows {
        match w {
            Saved::Session(id) => text.push_str(&format!("session {}\n", id.0)),
            Saved::File(p) => {
                // A path with a line break cannot be stored on one line.
                let p = p.to_string_lossy();
                if !p.contains('\n') {
                    text.push_str(&format!("file {p}\n"));
                }
            }
        }
    }
    crate::editor::write_atomic(path, &text)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_in_order_and_reads_bad_files_as_empty() {
        let dir = std::env::temp_dir().join(format!("xade-wins-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = path(&dir.join("settings.json"));
        assert!(load(&file).is_empty());
        let list = vec![
            Saved::Session(SessionId(uuid::Uuid::new_v4())),
            Saved::File(PathBuf::from("/tmp/a b.rs")),
        ];
        save(&file, &list).unwrap();
        assert_eq!(load(&file), list);
        std::fs::write(&file, "garbage\nsession nope\nfile /x\n").unwrap();
        assert_eq!(load(&file), vec![Saved::File(PathBuf::from("/x"))]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
