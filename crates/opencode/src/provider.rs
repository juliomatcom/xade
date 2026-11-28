//! `OpenCodeProvider`: binary resolution, model catalog, preflight, spawn, delete. It reads no
//! OpenCode account or credential, so it has no account label.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock, RwLockReadGuard};
use std::time::Duration;

use xade_core::{
    AgentProvider, Effort, ModelOption, ProviderError, ProviderId, SessionHandle, SpawnConfig,
};

use crate::models::{
    default_model, mentions_auth, parse_models, run_models, run_opencode, Fetch, EFFORT_LEVELS,
};
use crate::process::{spawn_session, QUIET_PERIOD};

const MODELS_TIMEOUT: Duration = Duration::from_secs(15);
const DELETE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct OpenCodeProvider {
    binary: OnceLock<PathBuf>,
    /// Finds a binary that was missing at startup; `None` for a fixed one.
    lookup: Option<fn() -> Option<PathBuf>>,
    /// Empty until `preflight` fills it from `opencode models`.
    catalog: RwLock<Vec<ModelOption>>,
    quiet: Duration,
}

impl Default for OpenCodeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenCodeProvider {
    /// Resolves the `opencode` binary now: `$PATH`, then the usual install locations.
    pub fn new() -> Self {
        Self {
            lookup: Some(resolve_binary),
            ..Self::with_optional_binary(resolve_binary())
        }
    }

    pub fn with_binary(path: PathBuf) -> Self {
        Self::with_optional_binary(Some(path))
    }

    /// Shortens the silence that ends a turn with no prompt behind it, for tests.
    #[doc(hidden)]
    pub fn with_quiet_period(mut self, quiet: Duration) -> Self {
        self.quiet = quiet;
        self
    }

    fn with_optional_binary(binary: Option<PathBuf>) -> Self {
        Self {
            binary: binary.map(OnceLock::from).unwrap_or_default(),
            lookup: None,
            catalog: RwLock::new(Vec::new()),
            quiet: QUIET_PERIOD,
        }
    }

    fn catalog(&self) -> RwLockReadGuard<'_, Vec<ModelOption>> {
        self.catalog.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn binary(&self) -> Option<&Path> {
        self.binary.get().map(PathBuf::as_path)
    }

    /// The binary, looked up again while it is missing, so a CLI installed after startup is
    /// found by the next preflight.
    fn find_binary(&self) -> Option<&Path> {
        if self.binary.get().is_none() {
            if let Some(found) = self.lookup.and_then(|lookup| lookup()) {
                // A concurrent preflight may have set it first; either path is good.
                let _ = self.binary.set(found);
            }
        }
        self.binary()
    }
}

impl AgentProvider for OpenCodeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OpenCode
    }

    fn display_name(&self) -> &'static str {
        "OpenCode"
    }

    fn models(&self) -> Vec<ModelOption> {
        self.catalog().clone()
    }

    fn default_model(&self) -> String {
        default_model(&self.catalog())
    }

    fn effort_levels(&self) -> Vec<Effort> {
        EFFORT_LEVELS.to_vec()
    }

    /// Lists the models, which also fills the catalog. Blocking.
    fn preflight(&self) -> Result<(), ProviderError> {
        let binary = self.find_binary().ok_or(ProviderError::NotInstalled)?;
        match run_models(binary, MODELS_TIMEOUT) {
            Fetch::SpawnFailed(e) => Err(ProviderError::Spawn(e)),
            Fetch::TimedOut => Err(ProviderError::Protocol("opencode models timed out".into())),
            Fetch::Done {
                success: true,
                stdout,
                ..
            } => {
                let models = parse_models(&stdout);
                if models.is_empty() {
                    return Err(ProviderError::Protocol(
                        "opencode models listed no models".into(),
                    ));
                }
                *self.catalog.write().unwrap_or_else(|e| e.into_inner()) = models;
                Ok(())
            }
            Fetch::Done { stdout, stderr, .. } => {
                if mentions_auth(&stdout) || mentions_auth(&stderr) {
                    return Err(ProviderError::NotLoggedIn);
                }
                Err(ProviderError::Protocol(failure_reason(
                    "opencode models",
                    &stderr,
                    &stdout,
                )))
            }
        }
    }

    /// Starts a session process. Must be called inside a tokio runtime.
    fn spawn(&self, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError> {
        let binary = self.binary().ok_or(ProviderError::NotInstalled)?;
        spawn_session(binary, cfg, self.quiet)
    }

    /// Removes OpenCode's copy of a session, its subagent sessions included. Already gone is Ok.
    /// Blocking.
    fn delete_session(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        let id = provider_session_id;
        let plain = !id.is_empty()
            && id.len() <= 128
            // Never read as a flag.
            && !id.starts_with('-')
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !plain {
            return Err(ProviderError::Protocol(format!(
                "invalid session id {id:?}"
            )));
        }
        let binary = self.binary().ok_or(ProviderError::NotInstalled)?;
        let args = ["session", "delete", "--standalone", id];
        match run_opencode(binary, &args, DELETE_TIMEOUT) {
            Fetch::SpawnFailed(e) => Err(ProviderError::Spawn(e)),
            Fetch::TimedOut => Err(ProviderError::Protocol(
                "opencode session delete timed out".into(),
            )),
            Fetch::Done {
                success,
                stdout,
                stderr,
            } => {
                let missing = [&stdout, &stderr]
                    .iter()
                    .any(|t| t.to_lowercase().contains("session not found"));
                if success || missing {
                    return Ok(());
                }
                Err(ProviderError::Protocol(failure_reason(
                    "opencode session delete",
                    &stderr,
                    &stdout,
                )))
            }
        }
    }

    /// Auto OFF is permission rules in the process's environment.
    fn respawn_on_auto_change(&self) -> bool {
        true
    }
}

fn resolve_binary() -> Option<PathBuf> {
    first_binary(&candidate_dirs(
        std::env::var_os("PATH"),
        std::env::var_os("HOME").map(PathBuf::from),
    ))
}

/// Where `opencode` may live, in lookup order: `$PATH`, the default install `~/.opencode/bin`,
/// then the usual user and system bin dirs (GUI apps on macOS lack the shell `$PATH`).
fn candidate_dirs(path_var: Option<OsString>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path_var
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = &home {
        dirs.push(home.join(".opencode/bin"));
        dirs.push(home.join(".local/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs
}

/// The binary is run from other folders than ours, so a relative `$PATH` entry is not used.
fn first_binary(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .filter(|d| d.is_absolute())
        .map(|d| d.join("opencode"))
        .find(|p| p.is_file())
}

/// A short reason for a failed command: the first non-empty output line, bounded.
fn failure_reason(command: &str, stderr: &str, stdout: &str) -> String {
    let line = [stderr, stdout]
        .iter()
        .flat_map(|t| t.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no output");
    let short: String = line.chars().take(120).collect();
    format!("{command} failed: {short}")
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
            "/home/u/.opencode/bin",
            "/home/u/.local/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
        ]
        .map(PathBuf::from)
        .to_vec();
        assert_eq!(dirs, want);
        assert_eq!(
            candidate_dirs(None, None),
            ["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from)
        );
    }

    #[test]
    fn the_first_absolute_dir_holding_opencode_wins() {
        let root = tempfile::tempdir().unwrap();
        let [a, b, c] = ["a", "b", "c"].map(|d| root.path().join(d));
        for d in [&a, &b, &c] {
            std::fs::create_dir_all(d).unwrap();
        }
        // A directory named `opencode` is not a binary.
        std::fs::create_dir_all(a.join("opencode")).unwrap();
        std::fs::write(b.join("opencode"), "").unwrap();
        std::fs::write(c.join("opencode"), "").unwrap();
        assert_eq!(
            first_binary(&[a.clone(), b.clone(), c]),
            Some(b.join("opencode"))
        );
        assert_eq!(first_binary(&[a, root.path().join("missing")]), None);
        // This crate's own folder holds no such file; a relative entry is skipped regardless.
        assert_eq!(first_binary(&[PathBuf::from("."), PathBuf::new()]), None);
    }

    #[test]
    fn a_missing_binary_is_not_installed() {
        let p = OpenCodeProvider::with_optional_binary(None);
        assert_eq!(p.preflight(), Err(ProviderError::NotInstalled));
        assert_eq!(p.default_model(), "");
        assert!(p.models().is_empty());
        assert_eq!(p.account_label(), None);
        assert_eq!(p.id(), ProviderId::OpenCode);
        assert!(p.respawn_on_auto_change());
        assert_eq!(p.delete_session("ses_1"), Err(ProviderError::NotInstalled));
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

    #[test]
    fn a_binary_installed_later_is_found_by_the_next_preflight() {
        static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        fn lookup() -> Option<PathBuf> {
            INSTALLED
                .load(std::sync::atomic::Ordering::SeqCst)
                .then(|| PathBuf::from("/nonexistent/opencode"))
        }
        let p = OpenCodeProvider {
            lookup: Some(lookup),
            ..OpenCodeProvider::with_optional_binary(None)
        };
        assert_eq!(p.preflight(), Err(ProviderError::NotInstalled));
        INSTALLED.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(p.preflight(), Err(ProviderError::Spawn(_))));
    }

    #[test]
    fn ids_that_are_not_plain_tokens_are_refused_before_anything_runs() {
        let p = OpenCodeProvider::with_optional_binary(None);
        for bad in [
            "",
            "-x",
            "--standalone",
            "..",
            "a/b",
            "x y",
            "a\0b",
            "ses_1;rm",
            &"a".repeat(129),
        ] {
            assert!(
                matches!(p.delete_session(bad), Err(ProviderError::Protocol(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_failure_reason_is_one_bounded_line() {
        let reason = failure_reason("opencode models", "\n  boom \nmore", "out");
        assert_eq!(reason, "opencode models failed: boom");
        let long = failure_reason("c", "", &"é".repeat(500));
        assert_eq!(long.chars().count(), "c failed: ".len() + 120);
        assert_eq!(failure_reason("c", " ", ""), "c failed: no output");
    }
}
