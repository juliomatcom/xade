//! The files a fork hands to its provider: `<fork id>.transcript.md` (the history),
//! `<fork id>.snapshot.md` (a state summary the source provider wrote) and `<fork id>.resume.md`
//! (the preamble that tells the new provider to continue, not discuss the hand-off), in one
//! private temp folder. Never pruned except when the session is deleted. `<fork id>.throwaway` marks
//! the snapshot turn's conversation until it is deleted, and outlives its session's files.

use std::io::Write;
use std::path::{Path, PathBuf};

use xade_core::{ProviderId, SessionId};

/// The per-user folder, `<temp dir>/xade-forks-<uid>` (0700) on unix. `None` when it exists but
/// is a link or belongs to someone else: a fork then hands its history over inline.
pub(crate) fn default_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        let dir = std::env::temp_dir().join(format!("xade-forks-{uid}"));
        private_dir(&dir, uid).then_some(dir)
    }
    #[cfg(not(unix))]
    Some(std::env::temp_dir().join("xade-forks"))
}

/// Creates `dir` (0700) or accepts an existing real folder owned by `uid`, tightening its mode.
#[cfg(unix)]
fn private_dir(dir: &Path, uid: u32) -> bool {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            tracing::warn!(error = %e, "fork folder not created; hand-offs go inline");
            return false;
        }
    }
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "fork folder not readable; hand-offs go inline");
            return false;
        }
    };
    if !meta.is_dir() || meta.uid() != uid {
        tracing::warn!("fork folder is a link or not ours; hand-offs go inline");
        return false;
    }
    if meta.mode() & 0o077 != 0 {
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(error = %e, "fork folder not private; hand-offs go inline");
            return false;
        }
    }
    true
}

pub(crate) fn transcript_path(dir: &Path, id: SessionId) -> PathBuf {
    dir.join(format!("{}.transcript.md", id.0))
}

pub(crate) fn snapshot_path(dir: &Path, id: SessionId) -> PathBuf {
    dir.join(format!("{}.snapshot.md", id.0))
}

pub(crate) fn resume_path(dir: &Path, id: SessionId) -> PathBuf {
    dir.join(format!("{}.resume.md", id.0))
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

const MARKER_EXT: &str = ".throwaway";

fn marker_path(dir: &Path, id: SessionId) -> PathBuf {
    dir.join(format!("{}{MARKER_EXT}", id.0))
}

fn provider_name(p: ProviderId) -> &'static str {
    match p {
        ProviderId::Claude => "claude",
        ProviderId::Antigravity => "antigravity",
    }
}

fn parse_provider(s: &str) -> Option<ProviderId> {
    [ProviderId::Claude, ProviderId::Antigravity]
        .into_iter()
        .find(|p| provider_name(*p) == s)
}

/// Records a snapshot turn's conversation (`<provider id> <provider session id>`) so a later
/// start can delete it if this run never does.
pub(crate) fn write_marker(
    dir: &Path,
    id: SessionId,
    provider: ProviderId,
    psid: &str,
) -> std::io::Result<()> {
    write_private(
        &marker_path(dir, id),
        &format!("{} {psid}", provider_name(provider)),
    )
}

/// Removes a marker once its conversation is deleted; a missing one is fine.
pub(crate) fn remove_marker(dir: &Path, id: SessionId) {
    if let Err(e) = std::fs::remove_file(marker_path(dir, id)) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error = %e, "could not remove a throwaway marker");
        }
    }
}

/// A leftover snapshot conversation: its marker file, provider and provider session id.
pub(crate) struct Leftover {
    pub marker: PathBuf,
    pub provider: Option<ProviderId>,
    pub psid: String,
}

/// Every marker in `dir`; one that cannot be parsed is skipped.
pub(crate) fn read_markers(dir: &Path) -> Vec<Leftover> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for e in entries.flatten() {
        if !e.file_name().to_string_lossy().ends_with(MARKER_EXT) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        if let Some((provider, psid)) = text.trim().split_once(' ') {
            found.push(Leftover {
                marker: e.path(),
                provider: parse_provider(provider),
                psid: psid.to_string(),
            });
        }
    }
    found
}

/// Whether `text` opens with an `@` mention of a file in `dir`: how a snapshot turn's own
/// conversation shows in the provider's history.
pub(crate) fn mentions_dir(dir: &Path, text: &str) -> bool {
    let (text, dir) = (text.trim_start(), dir.display());
    text.starts_with(&format!("@{dir}/")) || text.starts_with(&format!("@\"{dir}/"))
}

/// Removes every file of session `id` except its throwaway marker (which belongs to a
/// provider conversation that may still exist); a missing folder or file is fine.
pub(crate) fn remove_all(dir: &Path, id: SessionId) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{}.", id.0);
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && !name.ends_with(MARKER_EXT) {
            if let Err(err) = std::fs::remove_file(e.path()) {
                tracing::warn!(error = %err, "could not remove a fork hand-off file");
            }
        }
    }
}

/// The first-send hand-off of a session with no provider id yet whose history is `text` (a
/// `transcript`): the snapshot when one exists, else the transcript file, refreshed, each
/// followed by the fork's resume preamble (when its file exists). If the transcript cannot be
/// written (or there is no folder) the text itself, after the preamble, so a fork never starts
/// empty. Blocking.
pub(crate) fn first_prompt(dir: Option<&Path>, id: SessionId, text: String) -> Option<String> {
    let Some(dir) = dir else {
        return (!text.is_empty()).then_some(text);
    };
    let resume = std::fs::read_to_string(resume_path(dir, id))
        .map(|r| r.trim().to_string())
        .unwrap_or_default();
    let with_resume = |first: String| {
        if resume.is_empty() {
            first
        } else {
            format!("{first}\n{resume}")
        }
    };
    let snapshot = snapshot_path(dir, id);
    if snapshot.is_file() {
        return Some(with_resume(mention(&snapshot)));
    }
    if text.is_empty() {
        return None;
    }
    let path = transcript_path(dir, id);
    match write_private(&path, &text) {
        Ok(()) => Some(with_resume(mention(&path))),
        Err(e) => {
            tracing::warn!(error = %e, "could not write the fork transcript; sending it inline");
            Some(if resume.is_empty() {
                text
            } else {
                format!("{resume}\n\n{text}")
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn the_folder_is_private_and_ours_or_none() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let dir = tmp.path().join("forks");
        assert!(private_dir(&dir, uid));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        // An existing folder of ours is accepted and tightened.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_dir(&dir, uid));
        assert_eq!(mode(&dir), 0o700);
        // Another owner's folder, or a link (even to a folder of ours), is refused.
        assert!(!private_dir(&dir, uid + 1));
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(!private_dir(&link, uid));
    }

    #[test]
    fn mentions_of_the_folder_are_found_quoted_or_not() {
        let dir = Path::new("/tmp/xade-forks-1");
        assert!(mentions_dir(
            dir,
            "@/tmp/xade-forks-1/a.transcript.md\n\nWrite"
        ));
        assert!(mentions_dir(dir, "\n@\"/tmp/xade-forks-1/a.md\" go"));
        assert!(!mentions_dir(dir, "@/tmp/xade-forks-12/a.md"));
        assert!(!mentions_dir(dir, "look at @/tmp/xade-forks-1/a.md"));
    }

    #[test]
    fn markers_round_trip_and_survive_remove_all() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        write_marker(tmp.path(), id, ProviderId::Antigravity, "s 1").unwrap();
        write_private(&snapshot_path(tmp.path(), id), "x").unwrap();
        remove_all(tmp.path(), id);
        assert!(!snapshot_path(tmp.path(), id).exists());
        let found = read_markers(tmp.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].provider, Some(ProviderId::Antigravity));
        assert_eq!(found[0].psid, "s 1");
        remove_marker(tmp.path(), id);
        assert!(read_markers(tmp.path()).is_empty());
    }

    #[test]
    fn the_resume_preamble_follows_the_mention_when_its_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let line = mention(&transcript_path(tmp.path(), id));
        let first = |text: &str| first_prompt(Some(tmp.path()), id, text.into());
        assert_eq!(first("hist"), Some(line.clone()), "no file, no preamble");
        write_private(&resume_path(tmp.path(), id), "Carry on.\n").unwrap();
        assert_eq!(first("hist"), Some(format!("{line}\nCarry on.")));
        write_private(&snapshot_path(tmp.path(), id), "x").unwrap();
        let snap = mention(&snapshot_path(tmp.path(), id));
        assert_eq!(first(""), Some(format!("{snap}\nCarry on.")));
        // The transcript cannot be written: the text goes inline, after the preamble.
        std::fs::remove_file(snapshot_path(tmp.path(), id)).unwrap();
        std::fs::remove_file(transcript_path(tmp.path(), id)).unwrap();
        std::fs::create_dir(transcript_path(tmp.path(), id)).unwrap();
        assert_eq!(first("hist"), Some("Carry on.\n\nhist".to_string()));
    }

    #[test]
    fn without_a_folder_the_history_goes_inline() {
        let id = SessionId::new();
        assert_eq!(first_prompt(None, id, "hi".into()).as_deref(), Some("hi"));
        assert_eq!(first_prompt(None, id, String::new()), None);
    }
}
