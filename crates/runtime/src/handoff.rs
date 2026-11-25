//! The files a fork hands to its provider: `<fork id>.transcript.md` (the history) and
//! `<fork id>.snapshot.md` (a state summary the source provider wrote), in one private
//! temp folder. Never pruned except when the session is deleted.

use std::io::Write;
use std::path::{Path, PathBuf};

use xade_core::SessionId;

/// `<temp dir>/xade-forks`.
pub(crate) fn default_dir() -> PathBuf {
    std::env::temp_dir().join("xade-forks")
}

pub(crate) fn transcript_path(dir: &Path, id: SessionId) -> PathBuf {
    dir.join(format!("{}.transcript.md", id.0))
}

pub(crate) fn snapshot_path(dir: &Path, id: SessionId) -> PathBuf {
    dir.join(format!("{}.snapshot.md", id.0))
}

/// Creates (or replaces) `path`, and its folder when missing; private to the user on unix.
pub(crate) fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        let mut dirs = std::fs::DirBuilder::new();
        dirs.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut dirs, 0o700);
        dirs.create(dir)?;
    }
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
    file.open(path)?.write_all(text.as_bytes())
}

/// The `@path` mention the CLI reads, quoted when the path has spaces (as the composer does).
pub(crate) fn mention(path: &Path) -> String {
    let p = path.display();
    if p.to_string().contains(' ') {
        format!("@\"{p}\"")
    } else {
        format!("@{p}")
    }
}

/// Removes every file of session `id`; a missing folder or file is fine.
pub(crate) fn remove_all(dir: &Path, id: SessionId) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{}.", id.0);
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().starts_with(&prefix) {
            if let Err(err) = std::fs::remove_file(e.path()) {
                tracing::warn!(error = %err, "could not remove a fork hand-off file");
            }
        }
    }
}

/// The first-send hand-off of a session with no provider id yet whose history is `text` (a
/// `transcript`): the snapshot when one exists, else the transcript file, refreshed. If it
/// cannot be written the text itself, so a fork never starts empty. Blocking.
pub(crate) fn first_prompt(dir: &Path, id: SessionId, text: String) -> Option<String> {
    let snapshot = snapshot_path(dir, id);
    if snapshot.is_file() {
        return Some(mention(&snapshot));
    }
    if text.is_empty() {
        return None;
    }
    let path = transcript_path(dir, id);
    match write_private(&path, &text) {
        Ok(()) => Some(mention(&path)),
        Err(e) => {
            tracing::warn!(error = %e, "could not write the fork transcript; sending it inline");
            Some(text)
        }
    }
}
