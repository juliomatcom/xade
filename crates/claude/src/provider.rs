//! `ClaudeProvider`: model list, binary resolution, preflight, spawn.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;
use xade_core::{
    AgentProvider, Effort, ModelOption, ProviderError, ProviderId, SessionHandle, SpawnConfig,
};

use crate::process::spawn_session;

pub struct ClaudeProvider {
    binary: Option<PathBuf>,
}

impl Default for ClaudeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeProvider {
    /// Resolves the `claude` binary now: `$PATH`, then the usual install locations.
    pub fn new() -> Self {
        Self {
            binary: resolve_binary(),
        }
    }

    pub fn with_binary(path: PathBuf) -> Self {
        Self { binary: Some(path) }
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
        [
            ("fable", "Fable"),
            ("opus", "Opus"),
            ("sonnet", "Sonnet"),
            ("haiku", "Haiku"),
        ]
        .into_iter()
        .map(|(id, label)| ModelOption {
            id: id.into(),
            label: label.into(),
        })
        .collect()
    }

    fn default_model(&self) -> String {
        "sonnet".into()
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
            Ok(a) if a.logged_in => Ok(()),
            _ => Err(ProviderError::NotLoggedIn),
        }
    }

    fn spawn(&self, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError> {
        let binary = self.binary.as_ref().ok_or(ProviderError::NotInstalled)?;
        spawn_session(binary, cfg)
    }
}
