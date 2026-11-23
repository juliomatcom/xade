//! `ClaudeProvider`: model list, binary resolution, preflight, spawn.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{RwLock, RwLockReadGuard};
use std::time::Duration;

use serde::Deserialize;
use xade_core::{
    AgentProvider, Effort, ImportedSession, ModelOption, ProviderError, ProviderId, RunningSession,
    SessionHandle, SpawnConfig,
};

use crate::models::{fetch_catalog, Catalog};
use crate::process::spawn_session;
use crate::transcript;

const MODELS_TIMEOUT: Duration = Duration::from_secs(15);
const FALLBACK_DEFAULT: &str = "sonnet";

pub struct ClaudeProvider {
    binary: Option<PathBuf>,
    /// Static aliases until `preflight` replaces them with the CLI's versioned list.
    catalog: RwLock<Catalog>,
    /// Email from the last auth check; display only, never logged or persisted.
    account: RwLock<Option<String>>,
}

fn fallback_catalog() -> Catalog {
    let models = [
        ("fable", "Fable"),
        ("opus", "Opus"),
        ("sonnet", "Sonnet"),
        ("haiku", "Haiku"),
    ]
    .into_iter()
    .map(|(id, label)| ModelOption {
        id: id.into(),
        label: label.into(),
        description: String::new(),
        effort_levels: vec![
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
        ],
    })
    .collect();
    Catalog {
        models,
        default_model: FALLBACK_DEFAULT.into(),
    }
}

impl Default for ClaudeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeProvider {
    /// Resolves the `claude` binary now: `$PATH`, then the usual install locations.
    pub fn new() -> Self {
        Self::with_optional_binary(resolve_binary())
    }

    pub fn with_binary(path: PathBuf) -> Self {
        Self::with_optional_binary(Some(path))
    }

    fn with_optional_binary(binary: Option<PathBuf>) -> Self {
        Self {
            binary,
            catalog: RwLock::new(fallback_catalog()),
            account: RwLock::new(None),
        }
    }

    fn catalog(&self) -> RwLockReadGuard<'_, Catalog> {
        self.catalog.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn binary(&self) -> Option<&Path> {
        self.binary.as_deref()
    }
}

/// GUI apps on macOS lack the shell `$PATH`, so also check the known install locations.
fn resolve_binary() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".claude/local"));
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(home.join(".local/bin"));
    } else {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
    }
    dirs.into_iter()
        .map(|d| d.join("claude"))
        .find(|p| p.is_file())
}

/// No `Debug`: the account fields must never reach a log.
#[derive(Deserialize)]
struct AuthStatus {
    #[serde(rename = "loggedIn", default)]
    logged_in: bool,
    #[serde(default)]
    email: Option<String>,
}

/// `Err` when logged out or unparsable; `Ok(label)` is the email when non-empty.
fn parse_auth_status(stdout: &[u8]) -> Result<Option<String>, ProviderError> {
    let auth: AuthStatus =
        serde_json::from_slice(stdout).map_err(|_| ProviderError::NotLoggedIn)?;
    if !auth.logged_in {
        return Err(ProviderError::NotLoggedIn);
    }
    Ok(auth
        .email
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty()))
}

impl AgentProvider for ClaudeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Claude
    }

    fn display_name(&self) -> &'static str {
        "Claude"
    }

    fn models(&self) -> Vec<ModelOption> {
        self.catalog().models.clone()
    }

    fn default_model(&self) -> String {
        self.catalog().default_model.clone()
    }

    fn effort_levels(&self) -> Vec<Effort> {
        vec![
            Effort::Default,
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
        ]
    }

    fn account_label(&self) -> Option<String> {
        self.account
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn preflight(&self) -> Result<(), ProviderError> {
        let set_account = |v: Option<String>| {
            *self.account.write().unwrap_or_else(|e| e.into_inner()) = v;
        };
        set_account(None);
        let binary = self.binary.as_ref().ok_or(ProviderError::NotInstalled)?;
        // The output carries account details: read only `loggedIn` and `email`
        // (the latter for display), never log them.
        let out = Command::new(binary)
            .args(["auth", "status", "--json"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| ProviderError::Spawn(e.to_string()))?;
        if !out.status.success() {
            return Err(ProviderError::NotLoggedIn);
        }
        set_account(parse_auth_status(&out.stdout)?);
        // Best effort: on failure the static aliases stay.
        match fetch_catalog(binary, MODELS_TIMEOUT) {
            Some(c) => *self.catalog.write().unwrap_or_else(|e| e.into_inner()) = c,
            None => tracing::debug!("model list unavailable; keeping static aliases"),
        }
        Ok(())
    }

    fn spawn(&self, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError> {
        let binary = self.binary.as_ref().ok_or(ProviderError::NotInstalled)?;
        spawn_session(binary, cfg)
    }

    fn import_sessions(&self) -> Vec<ImportedSession> {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return Vec::new();
        };
        let scan = transcript::scan_projects(&home.join(".claude/projects"));
        let st = &scan.stats;
        if st.read_errors > 0 {
            tracing::warn!(
                read_errors = st.read_errors,
                "some Claude transcripts were unreadable"
            );
        }
        tracing::info!(
            files = st.files,
            parsed = st.parsed,
            skipped = st.skipped,
            bad_lines = st.bad_lines,
            "history scanned"
        );
        scan.sessions
    }

    fn running_sessions(&self) -> Vec<RunningSession> {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return Vec::new();
        };
        crate::running::scan_running(&home.join(".claude/sessions"))
    }

    fn delete_session(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        let home =
            std::env::var_os("HOME").ok_or_else(|| ProviderError::Protocol("no HOME".into()))?;
        delete_transcript(
            &Path::new(&home).join(".claude/projects"),
            provider_session_id,
        )
    }
}

/// Removes `<projects>/*/<id>.jsonl` and the `<id>` directory (subagent transcripts) beside it.
/// The id must be a plain token so it can never address anything outside `projects`; missing
/// files are fine.
fn delete_transcript(projects: &Path, id: &str) -> Result<(), ProviderError> {
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
    let io = |e: std::io::Error| ProviderError::Protocol(e.to_string());
    let ignore_missing = |r: std::io::Result<()>| match r {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(e)),
        _ => Ok(()),
    };
    let entries = match std::fs::read_dir(projects) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io(e)),
    };
    for entry in entries.flatten() {
        // `file_type` does not follow symlinks: a linked project dir is never entered.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let dir = entry.path();
        ignore_missing(std::fs::remove_file(dir.join(format!("{id}.jsonl"))))?;
        let sub = dir.join(id);
        match std::fs::symlink_metadata(&sub) {
            Ok(m) if m.is_dir() => ignore_missing(std::fs::remove_dir_all(&sub))?,
            Ok(_) => ignore_missing(std::fs::remove_file(&sub))?,
            Err(e) => ignore_missing(Err(e))?,
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn account_label_is_the_email_only() {
        let label = |j: &str| parse_auth_status(j.as_bytes());
        assert_eq!(
            label(r#"{"loggedIn":true,"orgName":"Org","email":"a@b.c"}"#).unwrap(),
            Some("a@b.c".into())
        );
        assert_eq!(label(r#"{"loggedIn":true,"orgName":"Org"}"#).unwrap(), None);
        assert_eq!(label(r#"{"loggedIn":true,"email":"  "}"#).unwrap(), None);
        assert_eq!(label(r#"{"loggedIn":true}"#).unwrap(), None);
        assert!(matches!(
            label(r#"{"loggedIn":false,"email":"a@b.c"}"#),
            Err(ProviderError::NotLoggedIn)
        ));
        assert!(label("not json").is_err());
    }

    #[test]
    fn delete_removes_transcript_and_subagent_dir_only() {
        let root = tempfile::tempdir().unwrap();
        let projects = root.path().join("projects");
        let (a, b) = (projects.join("-tmp-a"), projects.join("-tmp-b"));
        std::fs::create_dir_all(a.join("sid/subagents")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("sid.jsonl"), "{}").unwrap();
        std::fs::write(a.join("sid/subagents/x.jsonl"), "{}").unwrap();
        std::fs::write(a.join("other.jsonl"), "{}").unwrap();
        std::fs::write(b.join("sid.jsonl"), "{}").unwrap();
        let outside = root.path().join("sid.jsonl");
        std::fs::write(&outside, "{}").unwrap();

        delete_transcript(&projects, "sid").unwrap();
        assert!(!a.join("sid.jsonl").exists() && !a.join("sid").exists());
        assert!(!b.join("sid.jsonl").exists());
        assert!(a.join("other.jsonl").exists() && outside.exists());
        // Already gone is fine.
        delete_transcript(&projects, "sid").unwrap();
        delete_transcript(&root.path().join("missing"), "sid").unwrap();
    }

    #[test]
    fn delete_rejects_ids_that_are_not_plain_tokens() {
        let root = tempfile::tempdir().unwrap();
        let projects = root.path().join("projects");
        std::fs::create_dir_all(projects.join("p")).unwrap();
        let victim = root.path().join("victim.jsonl");
        std::fs::write(&victim, "{}").unwrap();
        for bad in ["", "..", "../victim", "a/b", "a\\b", ".", "x y", "a\0b"] {
            assert!(delete_transcript(&projects, bad).is_err(), "{bad:?}");
        }
        assert!(victim.exists());
    }
}
