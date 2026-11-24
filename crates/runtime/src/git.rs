//! Git status of a directory via the user's `git`; read-only, no library.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitState {
    Repo {
        branch: Option<String>,
        /// Short sha when HEAD is detached.
        detached_sha: Option<String>,
        has_upstream: bool,
        ahead: u32,
        behind: u32,
        staged: u32,
        modified: u32,
        untracked: u32,
        /// Files with changes (untracked included) and their parent folders, as absolute
        /// paths once `git_status` has rebased them onto the repo root.
        changed: HashSet<PathBuf>,
    },
    NotARepo,
    GitMissing,
}

/// Runs `git status --porcelain=v2 --branch`; never takes the index lock.
pub async fn git_status(dir: &Path) -> GitState {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match timeout(Duration::from_secs(2), cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => {
            let mut state = parse_porcelain_v2(&String::from_utf8_lossy(&out.stdout));
            if let GitState::Repo { changed, .. } = &mut state {
                if !changed.is_empty() {
                    if let Some(top) = toplevel(dir).await {
                        *changed = changed.iter().map(|p| top.join(p)).collect();
                    }
                }
            }
            state
        }
        Ok(Ok(_)) => GitState::NotARepo,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => GitState::GitMissing,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "git status failed");
            GitState::GitMissing
        }
        Err(_) => {
            tracing::warn!("git status timed out");
            GitState::NotARepo
        }
    }
}

pub async fn toplevel(dir: &Path) -> Option<PathBuf> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let out = timeout(Duration::from_secs(2), cmd.output())
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.trim_end_matches('\n');
    (out.status.success() && !line.is_empty()).then(|| PathBuf::from(line))
}

/// The path of a porcelain v2 entry line; renames keep the new path.
fn entry_path(line: &str) -> Option<&str> {
    let (fields, tab) = match line.as_bytes().first()? {
        b'1' => (9, false),
        b'2' => (10, true),
        b'u' => (11, false),
        _ => return None,
    };
    let path = line.splitn(fields, ' ').nth(fields - 1)?;
    Some(if tab { path.split('\t').next()? } else { path })
}

fn mark_changed(set: &mut HashSet<PathBuf>, rel: &str) {
    let mut p = PathBuf::from(rel.trim_end_matches('/'));
    while !p.as_os_str().is_empty() && set.insert(p.clone()) {
        p.pop();
    }
}

pub fn parse_porcelain_v2(out: &str) -> GitState {
    let mut oid = None;
    let mut head = None;
    let (mut has_upstream, mut ahead, mut behind) = (false, 0, 0);
    let (mut staged, mut modified, mut untracked) = (0, 0, 0);
    let mut changed = HashSet::new();
    for line in out.lines() {
        if let Some(h) = line.strip_prefix("# branch.oid ") {
            oid = Some(h.to_string());
        } else if let Some(h) = line.strip_prefix("# branch.head ") {
            head = Some(h.to_string());
        } else if line.starts_with("# branch.upstream ") {
            has_upstream = true;
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            let mut it = ab.split_whitespace();
            ahead = count(it.next(), '+');
            behind = count(it.next(), '-');
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let mut xy = line[2..].chars();
            staged += u32::from(xy.next().is_some_and(|c| c != '.'));
            modified += u32::from(xy.next().is_some_and(|c| c != '.'));
        } else if line.starts_with("u ") {
            modified += 1;
        } else if line.starts_with("? ") {
            untracked += 1;
        }
        if let Some(path) = entry_path(line).or_else(|| line.strip_prefix("? ")) {
            mark_changed(&mut changed, path);
        }
    }
    let detached = head.as_deref() == Some("(detached)");
    GitState::Repo {
        branch: head.filter(|_| !detached),
        detached_sha: oid
            .filter(|_| detached)
            .map(|s| s.chars().take(7).collect()),
        has_upstream,
        ahead,
        behind,
        staged,
        modified,
        untracked,
        changed,
    }
}

fn count(s: Option<&str>, sign: char) -> u32 {
    s.and_then(|s| s.strip_prefix(sign))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OID: &str = "# branch.oid 0123456789abcdef0123456789abcdef01234567\n";

    fn repo(
        branch: Option<&str>,
        sha: Option<&str>,
        up: bool,
        ab: (u32, u32),
        counts: (u32, u32, u32),
    ) -> GitState {
        GitState::Repo {
            branch: branch.map(String::from),
            detached_sha: sha.map(String::from),
            has_upstream: up,
            ahead: ab.0,
            behind: ab.1,
            staged: counts.0,
            modified: counts.1,
            untracked: counts.2,
            changed: HashSet::new(),
        }
    }

    fn with_changed(mut state: GitState, files: &[&str]) -> GitState {
        if let GitState::Repo { changed, .. } = &mut state {
            *changed = files.iter().map(PathBuf::from).collect();
        }
        state
    }

    #[test]
    fn clean_with_upstream() {
        let out =
            format!("{OID}# branch.head main\n# branch.upstream origin/main\n# branch.ab +0 -0\n");
        assert_eq!(
            parse_porcelain_v2(&out),
            repo(Some("main"), None, true, (0, 0), (0, 0, 0))
        );
    }

    #[test]
    fn dirty_counts() {
        let out = format!(
            "{OID}# branch.head main\n\
             1 M. N... 100644 100644 100644 aaa bbb staged.rs\n\
             1 .M N... 100644 100644 100644 aaa bbb modified.rs\n\
             1 MM N... 100644 100644 100644 aaa bbb both.rs\n\
             2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\told.rs\n\
             u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.rs\n\
             ? new.txt\n? other.txt\n! ignored.log\n"
        );
        assert_eq!(
            parse_porcelain_v2(&out),
            with_changed(
                repo(Some("main"), None, false, (0, 0), (3, 3, 2)),
                &[
                    "staged.rs",
                    "modified.rs",
                    "both.rs",
                    "new.rs",
                    "conflict.rs",
                    "new.txt",
                    "other.txt"
                ]
            )
        );
    }

    #[test]
    fn nested_paths_mark_their_parent_folders() {
        let out = format!("{OID}# branch.head main\n1 .M N... 100644 100644 100644 a b src/ui/a b.rs\n? docs/x.md\n");
        assert_eq!(
            parse_porcelain_v2(&out),
            with_changed(
                repo(Some("main"), None, false, (0, 0), (0, 1, 1)),
                &["src", "src/ui", "src/ui/a b.rs", "docs", "docs/x.md"]
            )
        );
    }

    #[test]
    fn detached_head() {
        let out = format!("{OID}# branch.head (detached)\n");
        assert_eq!(
            parse_porcelain_v2(&out),
            repo(None, Some("0123456"), false, (0, 0), (0, 0, 0))
        );
    }

    #[test]
    fn no_upstream() {
        let out = format!("{OID}# branch.head feature\n? a\n");
        assert_eq!(
            parse_porcelain_v2(&out),
            with_changed(
                repo(Some("feature"), None, false, (0, 0), (0, 0, 1)),
                &["a"]
            )
        );
    }

    #[test]
    fn ahead_and_behind() {
        let out =
            format!("{OID}# branch.head dev\n# branch.upstream origin/dev\n# branch.ab +3 -12\n");
        assert_eq!(
            parse_porcelain_v2(&out),
            repo(Some("dev"), None, true, (3, 12), (0, 0, 0))
        );
    }

    #[test]
    fn initial_commit() {
        let out = "# branch.oid (initial)\n# branch.head main\n";
        assert_eq!(
            parse_porcelain_v2(out),
            repo(Some("main"), None, false, (0, 0), (0, 0, 0))
        );
    }

    /// A fresh repo with untracked files only (no commit yet) still reports its branch.
    #[tokio::test]
    async fn real_unborn_repo_with_untracked_files_has_a_branch() {
        let dir = std::env::temp_dir().join(format!("xade-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q"])
            .status();
        if init.is_ok_and(|s| s.success()) {
            for f in ["a", "b", "c"] {
                std::fs::write(dir.join(f), "").expect("file");
            }
            match git_status(&dir).await {
                GitState::Repo {
                    branch, untracked, ..
                } => assert!(branch.is_some_and(|b| !b.is_empty()) && untracked == 3),
                other => panic!("{other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
