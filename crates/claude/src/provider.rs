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

#[derive(Deserialize)]
struct AuthStatus {
    #[serde(rename = "loggedIn", default)]
    logged_in: bool,
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

    fn preflight(&self) -> Result<(), ProviderError> {
        let binary = self.binary.as_ref().ok_or(ProviderError::NotInstalled)?;
        // The output carries account details: parse only `loggedIn`, never log it.
        let out = Command::new(binary)
            .args(["auth", "status", "--json"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| ProviderError::Spawn(e.to_string()))?;
        if !out.status.success() {
            return Err(ProviderError::NotLoggedIn);
        }
        match serde_json::from_slice::<AuthStatus>(&out.stdout) {
            Ok(a) if a.logged_in => {}
            _ => return Err(ProviderError::NotLoggedIn),
        }
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
        transcript::scan_projects(&home.join(".claude/projects")).sessions
    }

    fn running_sessions(&self) -> Vec<RunningSession> {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return Vec::new();
        };
        crate::running::scan_running(&home.join(".claude/sessions"))
    }
}
