//! `AntigravityProvider`: binary resolution, model catalog, preflight, spawn, delete.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{RwLock, RwLockReadGuard};
use std::time::Duration;

use xade_core::{
    AgentProvider, Effort, ModelOption, ProviderError, ProviderId, QuotaGroup, SessionHandle,
    SpawnConfig,
};

use crate::models::{mentions_auth, parse_models, run_agy, run_models, Fetch};
use crate::process::spawn_session;
use crate::usage::parse_usage;

const MODELS_TIMEOUT: Duration = Duration::from_secs(15);
const USAGE_TIMEOUT: Duration = Duration::from_secs(20);

pub struct AntigravityProvider {
    binary: Option<PathBuf>,
    /// Empty until `preflight` fills it from `agy models`.
    catalog: RwLock<Vec<ModelOption>>,
}

impl Default for AntigravityProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AntigravityProvider {
    /// Resolves the `agy` binary now: `$PATH`, then the usual install locations.
    pub fn new() -> Self {
        let dirs = candidate_dirs(
            std::env::var_os("PATH"),
            std::env::var_os("HOME").map(PathBuf::from),
        );
        Self::with_optional_binary(first_binary(&dirs))
    }

    pub fn with_binary(path: PathBuf) -> Self {
        Self::with_optional_binary(Some(path))
    }

    fn with_optional_binary(binary: Option<PathBuf>) -> Self {
        Self {
            binary,
            catalog: RwLock::new(Vec::new()),
        }
    }

    fn catalog(&self) -> RwLockReadGuard<'_, Vec<ModelOption>> {
        self.catalog.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn binary(&self) -> Option<&Path> {
        self.binary.as_deref()
    }
}

/// Where `agy` may live, in lookup order: `$PATH`, the default install `~/.gemini/bin`, then the
/// usual user and system bin dirs (GUI apps on macOS lack the shell `$PATH`).
fn candidate_dirs(path_var: Option<OsString>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path_var
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = &home {
        dirs.push(home.join(".gemini/bin"));
        dirs.push(home.join(".local/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs
}

/// A short reason for a failed `agy models`: the first non-empty output line, bounded.
fn failure_reason(stderr: &str, stdout: &str) -> String {
    let line = [stderr, stdout]
        .iter()
        .flat_map(|t| t.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no output");
    let short: String = line.chars().take(120).collect();
    format!("agy models failed: {short}")
}

fn first_binary(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join("agy")).find(|p| p.is_file())
}

impl AgentProvider for AntigravityProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Antigravity
    }

    fn display_name(&self) -> &'static str {
        "Antigravity"
    }

    fn models(&self) -> Vec<ModelOption> {
        self.catalog().clone()
    }

    fn default_model(&self) -> String {
        self.catalog()
            .first()
            .map(|m| m.id.clone())
            .unwrap_or_default()
    }

    fn effort_levels(&self) -> Vec<Effort> {
        vec![Effort::Low, Effort::Medium, Effort::High, Effort::XHigh]
    }

    fn preflight(&self) -> Result<(), ProviderError> {
        let binary = self.binary.as_ref().ok_or(ProviderError::NotInstalled)?;
        match run_models(binary, MODELS_TIMEOUT) {
            Fetch::SpawnFailed(e) => Err(ProviderError::Spawn(e)),
            Fetch::TimedOut => Err(ProviderError::Protocol("agy models timed out".into())),
            Fetch::Done {
                success,
                stdout,
                stderr,
            } => {
                let models = if success {
                    parse_models(&stdout)
                } else {
                    Vec::new()
                };
                if !models.is_empty() {
                    *self.catalog.write().unwrap_or_else(|e| e.into_inner()) = models;
                    return Ok(());
                }
                if mentions_auth(&stdout) || mentions_auth(&stderr) {
                    tracing::debug!(success, "agy models gave no model list");
                    return Err(ProviderError::NotLoggedIn);
                }
                if !success {
                    // Not a login problem (e.g. offline): say what agy said, not "not signed in".
                    return Err(ProviderError::Protocol(failure_reason(&stderr, &stdout)));
                }
                Err(ProviderError::Protocol(
                    "agy models listed no models".into(),
                ))
            }
        }
    }

    fn quota(&self) -> Result<Vec<QuotaGroup>, ProviderError> {
        let binary = self.binary.as_ref().ok_or(ProviderError::NotInstalled)?;
        match run_agy(
            binary,
            &["-p", "/usage", "--output-format", "json"],
            USAGE_TIMEOUT,
        ) {
            Fetch::SpawnFailed(e) => Err(ProviderError::Spawn(e)),
            Fetch::TimedOut => Err(ProviderError::Protocol("agy /usage timed out".into())),
            Fetch::Done {
                success, stdout, ..
            } => match success.then(|| parse_usage(&stdout)).flatten() {
                Some(groups) => Ok(groups),
                None => Err(ProviderError::Protocol(
                    "agy /usage gave no quota report".into(),
                )),
            },
        }
    }

    fn spawn(&self, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError> {
        let binary = self.binary.as_ref().ok_or(ProviderError::NotInstalled)?;
        spawn_session(binary, cfg)
    }

    fn delete_session(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        let Some(home) = std::env::var_os("HOME") else {
            return Ok(());
        };
        delete_conversation(
            &Path::new(&home).join(".gemini/antigravity-cli"),
            provider_session_id,
        )
    }
}

/// Removes one conversation under `root` (`~/.gemini/antigravity-cli`): `conversations/<id>.db` and
/// its `-wal`/`-shm` siblings, `annotations/<id>.pbtxt` and the `brain/<id>` directory. Never
/// touches agy's own `conversation_summaries.db`. A symlink is removed itself, never followed.
/// The id must be a plain token so it can never address anything outside `root`; missing files
/// are fine.
fn delete_conversation(root: &Path, id: &str) -> Result<(), ProviderError> {
    let plain = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !plain {
        return Err(ProviderError::Protocol(format!(
            "invalid session id {id:?}"
        )));
    }
    let targets = [
        root.join(format!("conversations/{id}.db")),
        root.join(format!("conversations/{id}.db-wal")),
        root.join(format!("conversations/{id}.db-shm")),
        root.join(format!("annotations/{id}.pbtxt")),
        root.join(format!("brain/{id}")),
    ];
    for path in targets {
        remove_entry(&path).map_err(|e| ProviderError::Protocol(e.to_string()))?;
    }
    Ok(())
}

fn remove_entry(path: &Path) -> std::io::Result<()> {
    let res = match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    };
    match res {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn binary_lookup_order_is_path_then_install_dirs() {
        let dirs = candidate_dirs(
            Some(OsString::from("/a:/b")),
            Some(PathBuf::from("/home/u")),
        );
        let want: Vec<PathBuf> = [
            "/a",
            "/b",
            "/home/u/.gemini/bin",
            "/home/u/.local/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
        ]
        .map(PathBuf::from)
        .to_vec();
        assert_eq!(dirs, want);
        let none = candidate_dirs(None, None);
        assert_eq!(
            none,
            [
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin")
            ]
        );
    }

    #[test]
    fn the_first_dir_holding_agy_wins() {
        let root = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            root.path().join("a"),
            root.path().join("b"),
            root.path().join("c"),
        );
        for d in [&a, &b, &c] {
            std::fs::create_dir_all(d).unwrap();
        }
        // A directory named `agy` is not a binary.
        std::fs::create_dir_all(a.join("agy")).unwrap();
        std::fs::write(b.join("agy"), "").unwrap();
        std::fs::write(c.join("agy"), "").unwrap();
        assert_eq!(
            first_binary(&[a.clone(), b.clone(), c.clone()]),
            Some(b.join("agy"))
        );
        assert_eq!(first_binary(&[a, root.path().join("missing")]), None);
        assert_eq!(first_binary(&[]), None);
    }

    #[test]
    fn a_missing_binary_is_not_installed() {
        let p = AntigravityProvider::with_optional_binary(None);
        assert_eq!(p.preflight(), Err(ProviderError::NotInstalled));
        assert_eq!(p.default_model(), "");
        assert!(p.models().is_empty());
        assert_eq!(p.id(), ProviderId::Antigravity);
        assert!(matches!(
            p.spawn(SpawnConfig {
                cwd: ".".into(),
                model_id: String::new(),
                effort: Effort::Default,
                auto_mode: false,
                resume: None,
            }),
            Err(ProviderError::NotInstalled)
        ));
    }

    /// Writing an executable while another test thread forks makes its exec fail with "text file
    /// busy", so tests that write and run one take this lock.
    static EXEC: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn fake_agy(root: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("agy");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn preflight_fills_the_catalog_from_agy_models() {
        let _exec = EXEC.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let bin = fake_agy(
            root.path(),
            "echo spinner >&2; printf 'a-low\\tA (Low)\\na-high\\tA (High)\\nb\\tB\\n'",
        );
        let p = AntigravityProvider::with_binary(bin);
        assert_eq!(p.preflight(), Ok(()));
        let ids: Vec<_> = p.models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(p.default_model(), "a");
    }

    #[test]
    fn quota_runs_agy_usage_as_json_and_parses_the_groups() {
        let _exec = EXEC.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let json = r#"{"command":{"data":{"groups":[{"name":"G","buckets":[{"window":"5h","remaining_fraction":0.5}]}]}}}"#;
        let bin = fake_agy(
            root.path(),
            &format!("[ \"$1 $2 $3 $4\" = '-p /usage --output-format json' ] && echo '{json}'"),
        );
        let groups = AntigravityProvider::with_binary(bin).quota().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].windows[0].utilization_pct, 50.0);
        let bad = fake_agy(root.path(), "echo nope");
        assert!(AntigravityProvider::with_binary(bad).quota().is_err());
    }

    #[test]
    fn preflight_failures_are_classified() {
        let _exec = EXEC.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let run = |body: &str| {
            let dir = tempfile::tempdir_in(root.path()).unwrap();
            AntigravityProvider::with_binary(fake_agy(dir.path(), body)).preflight()
        };
        // A failure that does not mention login (offline) is not "not signed in".
        assert_eq!(
            run("echo 'dial tcp: network is unreachable' >&2; exit 1"),
            Err(ProviderError::Protocol(
                "agy models failed: dial tcp: network is unreachable".into()
            ))
        );
        assert!(matches!(run("exit 1"), Err(ProviderError::Protocol(_))));
        assert_eq!(
            run("echo 'please log in first' >&2; exit 1"),
            Err(ProviderError::NotLoggedIn)
        );
        assert_eq!(
            run("echo 'Please sign in' >&2"),
            Err(ProviderError::NotLoggedIn)
        );
        assert!(matches!(run("true"), Err(ProviderError::Protocol(_))));
    }

    #[test]
    fn delete_removes_one_conversation_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        for d in [
            "conversations",
            "annotations",
            "brain/sid/sub",
            "brain/other",
        ] {
            std::fs::create_dir_all(r.join(d)).unwrap();
        }
        let gone = [
            "conversations/sid.db",
            "conversations/sid.db-wal",
            "conversations/sid.db-shm",
            "annotations/sid.pbtxt",
            "brain/sid/sub/x",
        ];
        let kept = [
            "conversations/other.db",
            "conversations/sid2.db",
            "annotations/other.pbtxt",
            "brain/other/x",
            "conversation_summaries.db",
        ];
        for f in gone.iter().chain(&kept) {
            std::fs::write(r.join(f), "x").unwrap();
        }
        delete_conversation(r, "sid").unwrap();
        for f in gone {
            assert!(!r.join(f).exists(), "{f}");
        }
        assert!(!r.join("brain/sid").exists());
        for f in kept {
            assert!(r.join(f).exists(), "{f}");
        }
        // Already gone, and a root that never existed, are fine.
        delete_conversation(r, "sid").unwrap();
        delete_conversation(&r.join("missing"), "sid").unwrap();
    }

    #[test]
    fn delete_rejects_ids_that_are_not_plain_tokens() {
        let root = tempfile::tempdir().unwrap();
        let victim = root.path().join("victim");
        std::fs::write(&victim, "x").unwrap();
        for bad in [
            "",
            "..",
            "../victim",
            "a/b",
            "a\\b",
            ".",
            "x y",
            "a\0b",
            &"a".repeat(129),
        ] {
            assert!(delete_conversation(root.path(), bad).is_err(), "{bad:?}");
        }
        assert!(victim.exists());
    }

    #[test]
    fn delete_never_follows_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep"), "x").unwrap();
        let outside_file = outside.path().join("file");
        std::fs::write(&outside_file, "x").unwrap();
        std::fs::create_dir_all(r.join("brain")).unwrap();
        std::fs::create_dir_all(r.join("conversations")).unwrap();
        std::os::unix::fs::symlink(outside.path(), r.join("brain/sid")).unwrap();
        std::os::unix::fs::symlink(&outside_file, r.join("conversations/sid.db")).unwrap();

        delete_conversation(r, "sid").unwrap();
        assert!(std::fs::symlink_metadata(r.join("brain/sid")).is_err());
        assert!(std::fs::symlink_metadata(r.join("conversations/sid.db")).is_err());
        assert!(outside.path().join("keep").exists() && outside_file.exists());
    }
}
