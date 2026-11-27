//! `CodexProvider`: binary resolution, short app-server preflight (`account/read` + `model/list`),
//! session spawn, and `thread/delete`.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use xade_core::{
    AgentProvider, Effort, ModelOption, ProviderError, ProviderId, QuotaGroup, SessionHandle,
    SpawnConfig,
};

use crate::models::{
    fallback_models, parse_account, parse_models, AccountStatus, FALLBACK_MODEL_ID,
};
use crate::process::spawn_session;
use crate::rpc::{self, Incoming, RpcError};
use crate::usage::parse_rate_limits;

const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);
const QUOTA_TIMEOUT: Duration = Duration::from_secs(10);
const DELETE_TIMEOUT: Duration = Duration::from_secs(10);
const REAP_WAIT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Catalog {
    models: Vec<ModelOption>,
    default_model: Option<String>,
    /// Display-only account label from `account/read`; never logged or persisted.
    account_label: Option<String>,
}

pub struct CodexProvider {
    binary: Option<PathBuf>,
    catalog: RwLock<Catalog>,
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexProvider {
    /// Resolves the `codex` binary now: `$PATH`, then standard user/system install dirs.
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
            catalog: RwLock::new(Catalog::default()),
        }
    }

    fn catalog(&self) -> RwLockReadGuard<'_, Catalog> {
        self.catalog.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn binary(&self) -> Option<&Path> {
        self.binary.as_deref()
    }

    pub fn display_name(&self) -> &'static str {
        "Codex"
    }

    pub fn models(&self) -> Vec<ModelOption> {
        let cat = self.catalog();
        if cat.models.is_empty() {
            fallback_models()
        } else {
            cat.models.clone()
        }
    }

    pub fn default_model(&self) -> String {
        let cat = self.catalog();
        cat.default_model
            .clone()
            .or_else(|| cat.models.first().map(|m| m.id.clone()))
            .unwrap_or_else(|| FALLBACK_MODEL_ID.to_string())
    }

    pub fn effort_levels(&self) -> Vec<Effort> {
        vec![
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
        ]
    }

    pub fn account_label(&self) -> Option<String> {
        self.catalog().account_label.clone()
    }

    /// Runs one short `codex app-server` process: `initialize`, `initialized`, `account/read`,
    /// `model/list`, then closes stdin.
    pub fn preflight(&self) -> Result<(), ProviderError> {
        let result = self.run_preflight();
        if result.is_err() {
            self.catalog
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .account_label = None;
        }
        result
    }

    fn run_preflight(&self) -> Result<(), ProviderError> {
        let binary = self.binary.as_ref().ok_or(ProviderError::NotInstalled)?;
        let mut srv = ShortServer::spawn(binary, PREFLIGHT_TIMEOUT)?;
        let acct_res = srv.request("account/read", json!({}))?;
        let models_res = srv.request("model/list", json!({}))?;
        srv.finish();

        let acct = acct_res.map_err(|e| ProviderError::Protocol(e.message))?;
        let label = match parse_account(&acct) {
            AccountStatus::NotLoggedIn => {
                self.catalog
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .account_label = None;
                return Err(ProviderError::NotLoggedIn);
            }
            AccountStatus::LoggedIn(label) => label,
        };
        let (models, default_model) = match models_res {
            Ok(v) => parse_models(&v),
            Err(e) => {
                tracing::debug!(error = %e.message, "codex model/list failed; using fallback");
                (Vec::new(), None)
            }
        };
        let mut cat = self.catalog.write().unwrap_or_else(|e| e.into_inner());
        cat.account_label = label;
        if !models.is_empty() {
            cat.models = models;
            cat.default_model = default_model;
        }
        Ok(())
    }

    /// Queries `account/rateLimits/read` in one short `codex app-server` process.
    /// Any error (API-key accounts returning `-32600`, timeout, not signed in) logs at `debug`
    /// and yields an empty list so API-key users never see a quota error banner.
    pub fn quota(&self) -> Result<Vec<QuotaGroup>, ProviderError> {
        let Some(binary) = self.binary.as_ref() else {
            return Ok(Vec::new());
        };
        let mut srv = match ShortServer::spawn(binary, QUOTA_TIMEOUT) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, "codex quota spawn failed");
                return Ok(Vec::new());
            }
        };
        let res = srv.request("account/rateLimits/read", json!({}));
        srv.finish();
        match res {
            Ok(Ok(val)) => Ok(parse_rate_limits(&val)),
            Ok(Err(e)) => {
                tracing::debug!(code = e.code, error = %e.message, "codex rate limits unavailable");
                Ok(Vec::new())
            }
            Err(e) => {
                tracing::debug!(error = %e, "codex quota request failed");
                Ok(Vec::new())
            }
        }
    }

    pub fn spawn(&self, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError> {
        let binary = self.binary.as_ref().ok_or(ProviderError::NotInstalled)?;
        let supported = {
            let cat = self.catalog();
            cat.models
                .iter()
                .find(|m| m.id == cfg.model_id)
                .map(|m| m.effort_levels.clone())
        };
        spawn_session(binary, cfg, supported)
    }

    /// Deletes a persisted Codex thread via `thread/delete`, waiting for the server response
    /// before closing stdin. Errors containing "no rollout" count as already deleted (`Ok`).
    pub fn delete_session(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        let plain = !provider_session_id.is_empty()
            && provider_session_id.len() <= 128
            && provider_session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !plain {
            return Err(ProviderError::Protocol(format!(
                "invalid thread id {provider_session_id:?}"
            )));
        }
        let binary = self.binary.as_ref().ok_or(ProviderError::NotInstalled)?;
        let mut srv = ShortServer::spawn(binary, DELETE_TIMEOUT)?;
        let res = srv.request("thread/delete", json!({"threadId": provider_session_id}))?;
        srv.finish();
        match res {
            Ok(_) => Ok(()),
            Err(e) if e.message.to_lowercase().contains("no rollout") => Ok(()),
            Err(e) => Err(ProviderError::Protocol(e.message)),
        }
    }
}

impl AgentProvider for CodexProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Codex
    }

    fn display_name(&self) -> &'static str {
        CodexProvider::display_name(self)
    }

    fn models(&self) -> Vec<ModelOption> {
        CodexProvider::models(self)
    }

    fn default_model(&self) -> String {
        CodexProvider::default_model(self)
    }

    fn effort_levels(&self) -> Vec<Effort> {
        CodexProvider::effort_levels(self)
    }

    fn preflight(&self) -> Result<(), ProviderError> {
        CodexProvider::preflight(self)
    }

    fn account_label(&self) -> Option<String> {
        CodexProvider::account_label(self)
    }

    fn spawn(&self, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError> {
        CodexProvider::spawn(self, cfg)
    }

    fn quota(&self) -> Result<Vec<QuotaGroup>, ProviderError> {
        CodexProvider::quota(self)
    }

    fn delete_session(&self, provider_session_id: &str) -> Result<(), ProviderError> {
        CodexProvider::delete_session(self, provider_session_id)
    }
}

/// Short-lived synchronous `codex app-server` client used by `preflight` and `delete_session`.
/// Enforces a hard wall-clock deadline and always reaps the child's process group.
struct ShortServer {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    lines: mpsc::Receiver<String>,
    stderr: Option<std::thread::JoinHandle<String>>,
    next_id: u64,
    deadline: Instant,
}

impl ShortServer {
    fn spawn(binary: &Path, timeout: Duration) -> Result<Self, ProviderError> {
        let mut child = Command::new(binary)
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| ProviderError::Spawn(e.to_string()))?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            kill_and_reap(&mut child);
            return Err(ProviderError::Spawn("child stdio unavailable".into()));
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(line) = std::str::from_utf8(&buf) {
                            if !line.trim().is_empty() && tx.send(line.to_string()).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });
        let err_handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut r = stderr;
            let _ = r.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        });
        let mut srv = Self {
            child,
            stdin: Some(stdin),
            lines: rx,
            stderr: Some(err_handle),
            next_id: 1,
            deadline: Instant::now() + timeout,
        };
        let init_params = json!({
            "clientInfo": {
                "name": "xade",
                "title": "Xade",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        match srv.request("initialize", init_params)? {
            Ok(_) => {
                srv.write(&rpc::notification("initialized", None))?;
                Ok(srv)
            }
            Err(e) => Err(ProviderError::Protocol(format!(
                "codex initialize failed: {}",
                e.message
            ))),
        }
    }

    fn write(&mut self, line: &str) -> Result<(), ProviderError> {
        let Some(w) = self.stdin.as_mut() else {
            return Err(ProviderError::Protocol("codex stdin closed".into()));
        };
        w.write_all(line.as_bytes())
            .and_then(|()| w.flush())
            .map_err(|e| ProviderError::Protocol(format!("write to codex failed: {e}")))
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Result<Value, RpcError>, ProviderError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&rpc::request(id, method, params))?;
        loop {
            let Some(rem) = self.deadline.checked_duration_since(Instant::now()) else {
                return Err(ProviderError::Protocol(format!("codex {method} timed out")));
            };
            let line = match self.lines.recv_timeout(rem) {
                Ok(l) => l,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(ProviderError::Protocol(format!("codex {method} timed out")));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let tail = self.take_stderr_snippet();
                    let msg = if tail.is_empty() {
                        format!("codex exited before answering {method}")
                    } else {
                        format!("codex {method} failed: {tail}")
                    };
                    return Err(ProviderError::Protocol(msg));
                }
            };
            match rpc::parse_line(&line) {
                Some(Incoming::Response { id: rid, result }) if rid.as_u64() == Some(id) => {
                    return Ok(result);
                }
                Some(Incoming::Request { id: rid, .. }) => {
                    let _ = self.write(&rpc::method_not_found(&rid));
                }
                _ => {}
            }
        }
    }

    fn take_stderr_snippet(&mut self) -> String {
        self.stdin.take();
        kill_and_reap(&mut self.child);
        let text = self
            .stderr
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        line.chars().take(120).collect()
    }

    fn finish(mut self) {
        self.stdin.take();
        let limit = Instant::now() + REAP_WAIT;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < limit => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) | Err(_) => {
                    kill_and_reap(&mut self.child);
                    break;
                }
            }
        }
        // Always ensure nothing in the child's process group survives.
        // SAFETY: plain syscall on our child's process group (process_group(0)).
        unsafe {
            libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL);
        }
        if let Some(h) = self.stderr.take() {
            let _ = h.join();
        }
    }
}

impl Drop for ShortServer {
    fn drop(&mut self) {
        self.stdin.take();
        kill_and_reap(&mut self.child);
    }
}

fn kill_and_reap(child: &mut Child) {
    // SAFETY: plain syscall; the child leads its own group (process_group(0)).
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Where `codex` may live, in lookup order: `$PATH`, `~/.local/bin`, `/opt/homebrew/bin`,
/// `/usr/local/bin` (GUI apps on macOS lack the shell `$PATH`).
fn candidate_dirs(path_var: Option<OsString>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path_var
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = &home {
        dirs.push(home.join(".local/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs
}

fn first_binary(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join("codex")).find(|p| p.is_file())
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
            "/home/u/.local/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
        ]
        .map(PathBuf::from)
        .to_vec();
        assert_eq!(dirs, want);
        assert_eq!(
            candidate_dirs(None, None),
            [
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
            ]
        );
    }

    #[test]
    fn first_dir_holding_codex_file_wins() {
        let root = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            root.path().join("a"),
            root.path().join("b"),
            root.path().join("c"),
        );
        for d in [&a, &b, &c] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(a.join("codex")).unwrap();
        std::fs::write(b.join("codex"), "").unwrap();
        std::fs::write(c.join("codex"), "").unwrap();
        assert_eq!(
            first_binary(&[a.clone(), b.clone(), c]),
            Some(b.join("codex"))
        );
        assert_eq!(first_binary(&[a, root.path().join("missing")]), None);
    }

    #[test]
    fn missing_binary_is_not_installed_with_static_fallback_model() {
        let p = CodexProvider::with_optional_binary(None);
        assert_eq!(p.id(), ProviderId::Codex);
        assert_eq!(p.preflight(), Err(ProviderError::NotInstalled));
        assert_eq!(p.default_model(), FALLBACK_MODEL_ID);
        assert_eq!(p.models().len(), 1);
        assert_eq!(p.models()[0].id, FALLBACK_MODEL_ID);
        assert_eq!(p.account_label(), None);
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
    fn delete_rejects_invalid_thread_ids() {
        let p = CodexProvider::with_optional_binary(None);
        for bad in ["", "../x", "a/b", "x y", &"a".repeat(129)] {
            assert!(
                matches!(p.delete_session(bad), Err(ProviderError::Protocol(_))),
                "{bad:?}"
            );
        }
    }
}
