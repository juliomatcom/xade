//! End-to-end tests against the real `codex app-server`. They spend API tokens, so each is
//! `#[ignore]` and also skips unless `XADE_E2E_CODEX=1`:
//!
//! `XADE_E2E_CODEX=1 cargo test -p xade-codex --test e2e -- --ignored --test-threads=1`
//!
//! Every test runs in a fresh tempdir outside `$HOME` with `gpt-6-luna` / `Effort::Low`, and
//! deletes every thread it created through `delete_session`, even on failure.

#![allow(clippy::expect_used)]

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use tokio::time::timeout;
use xade_codex::CodexProvider;
use xade_core::{Effort, ProviderEvent, SessionCommand, SessionHandle, SpawnConfig};

const MODEL: &str = "gpt-6-luna";
const WAIT: Duration = Duration::from_secs(180);

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn enabled() -> bool {
    std::env::var("XADE_E2E_CODEX").is_ok_and(|v| v == "1")
}

macro_rules! e2e {
    () => {
        if !enabled() {
            eprintln!("skipped: set XADE_E2E_CODEX=1");
            return;
        }
    };
}

/// Returns a sandbox parent directory guaranteed to be outside `$HOME`.
fn sandbox_parent() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let tmp = std::env::temp_dir();
    if home.as_ref().is_some_and(|h| tmp.starts_with(h)) {
        PathBuf::from("/tmp")
    } else {
        tmp
    }
}

struct Ctx {
    provider: CodexProvider,
    repo: tempfile::TempDir,
    ids: RefCell<BTreeSet<String>>,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl Ctx {
    async fn new() -> Ctx {
        let serial = SERIAL.lock().await;
        let provider = CodexProvider::new();
        provider
            .preflight()
            .expect("codex is installed and signed in");
        assert!(
            provider.models().iter().any(|m| m.id == MODEL),
            "{MODEL} missing from `model/list`"
        );
        let repo = tempfile::tempdir_in(sandbox_parent()).expect("tempdir outside $HOME");
        let git = Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .expect("git");
        assert!(git.success());
        Ctx {
            provider,
            repo,
            ids: RefCell::default(),
            _serial: serial,
        }
    }

    fn spawn(&self, auto: bool, resume: Option<&str>) -> SessionHandle {
        self.provider
            .spawn(SpawnConfig {
                cwd: self.repo.path().to_path_buf(),
                model_id: MODEL.into(),
                effort: Effort::Low,
                auto_mode: auto,
                resume: resume.map(String::from),
            })
            .expect("spawn")
    }

    async fn until(
        &self,
        h: &mut SessionHandle,
        stop: impl Fn(&ProviderEvent) -> bool,
    ) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        let res = timeout(WAIT, async {
            while let Some(ev) = h.events.recv().await {
                if let ProviderEvent::Started {
                    provider_session_id,
                    ..
                } = &ev
                {
                    self.ids.borrow_mut().insert(provider_session_id.clone());
                }
                let done = stop(&ev);
                out.push(ev);
                if done {
                    return true;
                }
            }
            false
        })
        .await;
        assert!(res == Ok(true), "no matching event ({res:?}); got {out:?}");
        out
    }

    async fn turn(&self, h: &mut SessionHandle, text: &str) -> Vec<ProviderEvent> {
        prompt(h, text);
        let evs = self.until(h, is_turn).await;
        assert_eq!(turns(&evs), [(false, false)], "{evs:?}");
        evs
    }

    async fn shutdown(&self, h: &mut SessionHandle) {
        send(h, SessionCommand::Shutdown);
        let evs = self
            .until(h, |e| matches!(e, ProviderEvent::Exited { .. }))
            .await;
        assert!(
            matches!(evs.last(), Some(ProviderEvent::Exited { code: Some(0) })),
            "{evs:?}"
        );
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        let ids = self.ids.borrow().clone();
        for id in &ids {
            if let Err(e) = self.provider.delete_session(id) {
                eprintln!("could not delete codex thread {id}: {e}");
            }
        }
    }
}

fn send(h: &SessionHandle, c: SessionCommand) {
    h.commands.send(c).expect("send");
}

fn prompt(h: &SessionHandle, text: &str) {
    send(h, SessionCommand::SendPrompt(text.into()));
}

fn is_turn(e: &ProviderEvent) -> bool {
    matches!(e, ProviderEvent::TurnComplete { .. })
}

fn is_permission(e: &ProviderEvent) -> bool {
    matches!(e, ProviderEvent::PermissionRequest { .. })
}

fn turns(evs: &[ProviderEvent]) -> Vec<(bool, bool)> {
    evs.iter()
        .filter_map(|e| match e {
            ProviderEvent::TurnComplete {
                is_error,
                interrupted,
                ..
            } => Some((*is_error, *interrupted)),
            _ => None,
        })
        .collect()
}

fn said(evs: &[ProviderEvent]) -> String {
    evs.iter()
        .filter_map(|e| match e {
            ProviderEvent::AssistantText { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase()
}

fn children() -> Vec<u32> {
    let out = Command::new("pgrep")
        .args(["-P", &std::process::id().to_string()])
        .output()
        .expect("pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

async fn wait_children(want: usize) -> Vec<u32> {
    let start = Instant::now();
    loop {
        let c = children();
        if c.len() == want || start.elapsed() > Duration::from_secs(10) {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn a_simple_turn() {
    e2e!();
    let ctx = Ctx::new().await;
    let mut h = ctx.spawn(false, None);
    let evs = ctx.turn(&mut h, "Reply with exactly one word: pong").await;
    assert!(matches!(&evs[0], ProviderEvent::Started { model_id, .. } if model_id == MODEL));
    assert!(evs.iter().any(|e| matches!(e, ProviderEvent::TextDelta(_))));
    assert!(said(&evs).contains("pong"), "{evs:?}");
    let ProviderEvent::TurnComplete { usage, .. } = evs.last().expect("events") else {
        panic!("no turn complete");
    };
    assert!(
        usage.output_tokens > 0 && usage.context_window_max > 0,
        "{usage:?}"
    );
    ctx.shutdown(&mut h).await;
    assert!(wait_children(0).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn auto_off_gates_a_command_and_allow_runs_it() {
    e2e!();
    let ctx = Ctx::new().await;
    let mut h = ctx.spawn(false, None);
    prompt(
        &h,
        "Run the shell command `touch made_by_e2e.txt` now. Do not ask me anything.",
    );
    let evs = ctx.until(&mut h, is_permission).await;
    let Some(ProviderEvent::PermissionRequest {
        request_id,
        tool_name,
        ..
    }) = evs.last()
    else {
        panic!("no permission request");
    };
    assert_eq!(tool_name, "Bash");
    assert!(!ctx.repo.path().join("made_by_e2e.txt").exists());
    send(
        &h,
        SessionCommand::RespondPermission {
            request_id: request_id.clone(),
            allow: true,
        },
    );
    let rest = ctx.until(&mut h, is_turn).await;
    assert_eq!(turns(&rest), [(false, false)], "{rest:?}");
    assert!(ctx.repo.path().join("made_by_e2e.txt").exists());
    ctx.shutdown(&mut h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn resume_in_a_new_process_keeps_context() {
    e2e!();
    let ctx = Ctx::new().await;
    let mut h = ctx.spawn(false, None);
    let evs = ctx
        .turn(
            &mut h,
            "Remember the secret word tangerine. Reply with just: ok",
        )
        .await;
    let ProviderEvent::Started {
        provider_session_id,
        ..
    } = &evs[0]
    else {
        panic!("no Started");
    };
    let sid = provider_session_id.clone();
    ctx.shutdown(&mut h).await;
    wait_children(0).await;

    let mut h2 = ctx.spawn(false, Some(&sid));
    let evs = ctx
        .turn(&mut h2, "What was the secret word? Reply with one word.")
        .await;
    assert!(
        matches!(&evs[0], ProviderEvent::Started { provider_session_id, .. } if *provider_session_id == sid),
        "{evs:?}"
    );
    assert!(said(&evs).contains("tangerine"), "{evs:?}");
    ctx.shutdown(&mut h2).await;
}

/// Free, read-only live check: imports non-Xade threads from the real Codex CLI and verifies
/// via a separate `thread/list` call that no returned thread has an `originator` starting with
/// `"xade"`.
#[test]
#[ignore]
fn import_sessions_live_skips_xade_threads() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    e2e!();
    let provider = CodexProvider::new();
    provider
        .preflight()
        .expect("codex is installed and signed in");
    let imported = provider.import_sessions();

    // Cross-check every imported thread id against raw `thread/list` metadata.
    let bin = provider.binary().expect("codex binary");
    let mut child = Command::new(bin)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn app-server");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    fn rpc(
        stdin: &mut std::process::ChildStdin,
        stdout: &mut BufReader<std::process::ChildStdout>,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let req =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(stdin, "{req}").expect("write");
        stdin.flush().expect("flush");
        let mut line = String::new();
        loop {
            line.clear();
            let n = stdout.read_line(&mut line).expect("read");
            assert!(n > 0, "eof waiting for {method}");
            let v: serde_json::Value = serde_json::from_str(&line).expect("json");
            if v.get("id").and_then(serde_json::Value::as_u64) == Some(id)
                && v.get("method").is_none()
            {
                return v;
            }
        }
    }
    rpc(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        serde_json::json!({"clientInfo": {"name": "xade", "title": "Xade", "version": "0.0.0"}}),
    );
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc": "2.0", "method": "initialized"})
    )
    .expect("initialized");
    stdin.flush().expect("flush");

    let mut by_id = std::collections::HashMap::new();
    let mut cursor: Option<String> = None;
    let mut next_id = 2u64;
    loop {
        let mut params = serde_json::json!({
            "limit": 100,
            "sourceKinds": ["cli", "vscode", "exec", "appServer"],
        });
        if let Some(c) = &cursor {
            params["cursor"] = serde_json::json!(c);
        }
        let resp = rpc(&mut stdin, &mut stdout, next_id, "thread/list", params);
        next_id += 1;
        if let Some(arr) = resp
            .pointer("/result/data")
            .and_then(serde_json::Value::as_array)
        {
            for t in arr {
                if let Some(id) = t.get("id").and_then(serde_json::Value::as_str) {
                    by_id.insert(id.to_string(), t.clone());
                }
            }
        }
        cursor = resp
            .pointer("/result/nextCursor")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from);
        if cursor.is_none() {
            break;
        }
    }
    drop(stdin);
    let _ = child.wait();

    for s in &imported {
        assert!(!s.provider_session_id.is_empty());
        assert!(!s.cwd.as_os_str().is_empty());
        assert!(
            !s.messages.is_empty(),
            "empty session {}",
            s.provider_session_id
        );
        let raw = by_id
            .get(&s.provider_session_id)
            .expect("imported thread exists in thread/list");
        let originator = raw
            .get("originator")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(
            !originator.starts_with("xade"),
            "xade-originated thread {} leaked into import (originator={originator:?})",
            s.provider_session_id
        );
        eprintln!(
            "imported codex session: id={} originator={originator:?} title={:?} model={:?} messages={}",
            s.provider_session_id,
            s.title,
            s.model_id,
            s.messages.len()
        );
    }
    assert!(
        !imported.is_empty(),
        "expected at least one non-Xade Codex thread on disk"
    );
}
