//! Files a turn changed, found by comparing the working tree before and after it. Unlike the
//! per-call view of `widgets::changes_summary`, this sees edits made through Bash too.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use similar::{ChangeTag, TextDiff};
use tokio::process::Command;
use tokio::time::timeout;
use xade_runtime::git::{git_status, toplevel, GitState};

/// Bigger or non-UTF-8 files are left out.
const MAX_BYTES: u64 = 512 * 1024;

/// What the changed files of a repo held at one moment; `None` content = unreadable.
pub struct Snapshot {
    top: PathBuf,
    files: HashMap<PathBuf, Option<String>>,
}

/// What a finished turn changed in the working tree.
#[derive(Clone, Debug, Default)]
pub struct TurnChanges {
    pub files: Vec<TurnFile>,
}

/// One file a turn changed, with both versions for the diff.
#[derive(Clone, Debug)]
pub struct TurnFile {
    pub path: PathBuf,
    pub added: usize,
    pub removed: usize,
    pub old: String,
    pub new: String,
}

fn read_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_BYTES {
        return None;
    }
    String::from_utf8(std::fs::read(path).ok()?).ok()
}

/// The repo's changed files and their content; `None` outside a git repo.
pub async fn snapshot(dir: &Path) -> Option<Snapshot> {
    let GitState::Repo { changed, .. } = git_status(dir).await else {
        return None;
    };
    let top = toplevel(dir).await?;
    // `changed` also lists parent folders; only files matter.
    let files = changed
        .into_iter()
        .filter(|p| p.is_file())
        .map(|p| {
            let text = read_text(&p);
            (p, text)
        })
        .collect();
    Some(Snapshot { top, files })
}

/// The content of `path` in HEAD; empty when it is not there (a new file).
async fn committed(top: &Path, path: &Path) -> String {
    let Ok(rel) = path.strip_prefix(top) else {
        return String::new();
    };
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(top)
        .arg("show")
        .arg(format!("HEAD:{}", rel.display()))
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match timeout(Duration::from_secs(2), cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => String::from_utf8(out.stdout).unwrap_or_default(),
        _ => String::new(),
    }
}

fn count(old: &str, new: &str) -> (usize, usize) {
    let diff = TextDiff::from_lines(old, new);
    diff.iter_all_changes()
        .fold((0, 0), |(a, r), c| match c.tag() {
            ChangeTag::Insert => (a + 1, r),
            ChangeTag::Delete => (a, r + 1),
            ChangeTag::Equal => (a, r),
        })
}

/// Files whose content differs from `base`, by path. A file that was clean at `base` is
/// compared with HEAD; one dirty then, with its content then.
pub async fn since(base: &Snapshot, dir: &Path) -> Vec<TurnFile> {
    let Some(now) = snapshot(dir).await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (path, new) in &now.files {
        let Some(new) = new else { continue };
        let old = match base.files.get(path) {
            Some(Some(old)) => old.clone(),
            Some(None) => continue,
            None => committed(&base.top, path).await,
        };
        if old == *new {
            continue;
        }
        let (added, removed) = count(&old, new);
        out.push(TurnFile {
            path: path.clone(),
            added,
            removed,
            old,
            new: new.clone(),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[tokio::test]
    async fn a_turn_lists_edited_new_and_already_dirty_files_but_not_untouched_ones() {
        let dir = std::env::temp_dir().join(format!("xade-turn-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        git(&dir, &["init", "-q"]);
        for f in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.join(f), "one\ntwo\n").unwrap();
        }
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "init"]);
        // Dirty before the turn: only a change during it counts.
        std::fs::write(dir.join("b.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.join("c.txt"), "one\ntwo\nbefore\n").unwrap();
        let base = snapshot(&dir).await.expect("a repo");

        std::fs::write(dir.join("a.txt"), "one\nTWO\n").unwrap();
        std::fs::write(dir.join("c.txt"), "one\ntwo\nbefore\nafter\n").unwrap();
        std::fs::write(dir.join("new.txt"), "x\ny\nz\n").unwrap();
        let files = since(&base, &dir).await;
        let _ = std::fs::remove_dir_all(&dir);

        let got: Vec<_> = files
            .iter()
            .map(|f| {
                (
                    f.path.file_name().unwrap().to_str().unwrap(),
                    f.added,
                    f.removed,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![("a.txt", 1, 1), ("c.txt", 1, 0), ("new.txt", 3, 0)]
        );
    }

    #[tokio::test]
    async fn outside_a_repo_there_is_no_snapshot() {
        let dir = std::env::temp_dir().join(format!("xade-turn-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(snapshot(&dir).await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
