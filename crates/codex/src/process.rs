//! One `codex app-server` child per session: spawn, a stdout/stderr reader each, and one task
//! that owns the child and its stdin, speaks JSON-RPC 2.0 and turns what comes back into
//! `ProviderEvent`s.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::timeout;
use tracing::debug;
use xade_core::{
    Effort, ProviderError, ProviderEvent, SessionCommand, SessionHandle, SpawnConfig, UsageTotals,
};

use crate::rpc::{self, Incoming, RpcError};
use crate::translate::Translator;

const STDERR_LINES: usize = 50;
const STDIN_CLOSE_WAIT: Duration = Duration::from_secs(3);
const SIGTERM_WAIT: Duration = Duration::from_secs(2);
const NOT_SIGNED_IN: &str = "Codex is not signed in. Run `codex login` in a terminal once.";

type StderrTail = Arc<Mutex<VecDeque<String>>>;
type Events = UnboundedSender<ProviderEvent>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Spawns `codex app-server` and its tasks. Must be called inside a tokio runtime.
/// For a fresh session (`cfg.resume.is_none()`), only `initialize` + `initialized` run until the
/// first prompt arrives (lazy `thread/start`). For a resume, `thread/resume` runs right away.
pub fn spawn_session(
    binary: &Path,
    cfg: SpawnConfig,
    supported_efforts: Option<Vec<Effort>>,
) -> Result<SessionHandle, ProviderError> {
    let mut child = Command::new(binary)
        .arg("app-server")
        .current_dir(&cfg.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .process_group(0)
        .spawn()
        .map_err(|e| ProviderError::Spawn(e.to_string()))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(ProviderError::Spawn("child stdio unavailable".into()));
    };
    let pid = child.id();

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let (line_tx, line_rx) = mpsc::unbounded_channel();
    let stderr_tail: StderrTail = Arc::default();

    let reader = tokio::spawn(read_stdout(stdout, line_tx));
    let err_reader = tokio::spawn(read_stderr(stderr, stderr_tail.clone()));
    tokio::spawn(run(
        Agent::new(cfg, supported_efforts, stdin, ev_tx),
        Process {
            child,
            pid,
            cmd_rx,
            line_rx,
            stderr_tail,
            reader,
            err_reader,
        },
    ));
    Ok(SessionHandle {
        commands: cmd_tx,
        events: ev_rx,
    })
}

/// Forwards each stdout line. No length cap (tool outputs and diffs can be large); non-UTF-8 lines
/// are skipped.
async fn read_stdout(stdout: ChildStdout, lines: UnboundedSender<String>) {
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => break,
            Ok(_) => match std::str::from_utf8(&buf) {
                Ok(line) if !line.trim().is_empty() => {
                    if lines.send(line.to_string()).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => debug!("skipping a non-UTF-8 stdout line"),
            },
            Err(e) => {
                tracing::warn!(error = %e, "reading codex stdout failed");
                break;
            }
        }
    }
}

async fn read_stderr(stderr: ChildStderr, tail: StderrTail) {
    let mut lines = BufReader::new(stderr).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "reading codex stderr failed");
                break;
            }
        };
        let mut t = lock(&tail);
        if t.len() == STDERR_LINES {
            t.pop_front();
        }
        t.push_back(line);
    }
}

// ---- pure helpers -------------------------------------------------------------------------

fn approval_policy(auto: bool) -> &'static str {
    if auto {
        "never"
    } else {
        "untrusted"
    }
}

fn effective_effort(effort: Effort, supported: Option<&[Effort]>) -> Option<&'static str> {
    let value = effort.cli_value()?;
    if let Some(list) = supported {
        if !list.contains(&effort) {
            return None;
        }
    }
    Some(value)
}

fn approval_reply(id: &Value, allow: bool) -> String {
    let decision = if allow { "accept" } else { "decline" };
    rpc::response_ok(id, json!({"decision": decision}))
}

fn is_approval_method(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
    )
}

fn thread_error(err: &RpcError, resuming: bool) -> String {
    let lower = err.message.to_lowercase();
    if lower.contains("auth") || lower.contains("login") || lower.contains("sign in") {
        NOT_SIGNED_IN.to_string()
    } else if resuming {
        format!("Could not resume the Codex thread: {}", err.message)
    } else {
        format!("Codex could not start a thread: {}", err.message)
    }
}

// ---- protocol state -----------------------------------------------------------------------

enum Pending {
    Initialize,
    Thread,
    StartTurn,
    Steer { text: String },
    Interrupt,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Handshaking,
    /// Initialized; no thread yet (created lazily on the first prompt).
    NeedThread,
    /// `thread/start` or `thread/resume` in flight.
    StartingThread,
    Ready,
    Closing,
}

struct Agent {
    cfg: SpawnConfig,
    supported_efforts: Option<Vec<Effort>>,
    stdin: Option<ChildStdin>,
    events: Events,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    /// Prompts waiting for the thread to be ready or for `turn/start` to yield a `turn_id`.
    queued: VecDeque<String>,
    phase: Phase,
    thread_id: Option<String>,
    /// `turn/start` sent; awaiting its response (or `turn/started`) for `active_turn_id`.
    turn_starting: bool,
    active_turn_id: Option<String>,
    auto: bool,
    /// Set on `Interrupt` while a turn is active; cleared on `turn/completed`.
    interrupted: bool,
    /// `Interrupt` arrived while `turn_starting` was true; fire `turn/interrupt` once `turn_id` lands.
    interrupt_pending: bool,
    /// Approval requests shown to the user: `id_key(&id)` -> original JSON `id`.
    permissions: HashMap<String, Value>,
    translator: Translator,
}

impl Agent {
    fn new(
        cfg: SpawnConfig,
        supported_efforts: Option<Vec<Effort>>,
        stdin: ChildStdin,
        events: Events,
    ) -> Self {
        Self {
            auto: cfg.auto_mode,
            cfg,
            supported_efforts,
            stdin: Some(stdin),
            events,
            next_id: 1,
            pending: HashMap::new(),
            queued: VecDeque::new(),
            phase: Phase::Handshaking,
            thread_id: None,
            turn_starting: false,
            active_turn_id: None,
            interrupted: false,
            interrupt_pending: false,
            permissions: HashMap::new(),
            translator: Translator::new(),
        }
    }

    fn emit(&self, ev: ProviderEvent) {
        let _ = self.events.send(ev);
    }

    fn emit_all(&self, evs: Vec<ProviderEvent>) {
        for ev in evs {
            self.emit(ev);
        }
    }

    fn closing(&self) -> bool {
        self.phase == Phase::Closing
    }

    fn turn_in_flight(&self) -> bool {
        self.turn_starting || self.active_turn_id.is_some()
    }

    async fn write(&mut self, line: String) {
        let Some(w) = self.stdin.as_mut() else {
            debug!("codex stdin is closed; dropping a frame");
            return;
        };
        let res = match w.write_all(line.as_bytes()).await {
            Ok(()) => w.flush().await,
            Err(e) => Err(e),
        };
        if let Err(e) = res {
            self.stdin = None;
            self.emit(ProviderEvent::Error(format!("write to codex failed: {e}")));
        }
    }

    async fn request(&mut self, method: &str, params: Value, pending: Pending) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, pending);
        self.write(rpc::request(id, method, params)).await;
        id
    }

    async fn initialize(&mut self) {
        let params = json!({
            "clientInfo": {
                "name": "xade",
                "title": "Xade",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        self.request("initialize", params, Pending::Initialize)
            .await;
    }

    fn fail(&mut self, msg: String) {
        self.emit(ProviderEvent::Error(msg));
        if !self.queued.is_empty() || self.turn_in_flight() {
            self.queued.clear();
            self.turn_starting = false;
            self.active_turn_id = None;
            self.emit(ProviderEvent::TurnComplete {
                usage: UsageTotals::default(),
                is_error: true,
                interrupted: false,
            });
        }
        self.phase = Phase::Closing;
    }

    async fn on_command(&mut self, cmd: SessionCommand) {
        if self.closing() {
            return;
        }
        match cmd {
            SessionCommand::SendPrompt(text) => {
                if self.phase == Phase::Ready {
                    self.send_prompt(text).await;
                } else {
                    self.queued.push_back(text);
                    if self.phase == Phase::NeedThread {
                        self.begin_thread().await;
                    }
                }
            }
            SessionCommand::Interrupt => self.interrupt().await,
            SessionCommand::RespondPermission { request_id, allow } => {
                if let Some(id) = self.permissions.remove(&request_id) {
                    self.write(approval_reply(&id, allow)).await;
                }
            }
            SessionCommand::SetAutoMode(on) => {
                self.auto = on;
                if on {
                    let pending: Vec<_> = self.permissions.drain().collect();
                    for (_, id) in pending {
                        self.write(approval_reply(&id, true)).await;
                    }
                }
            }
            SessionCommand::Shutdown => {}
        }
    }

    async fn interrupt(&mut self) {
        if self.phase != Phase::Ready {
            if !self.queued.is_empty() {
                self.queued.clear();
                self.emit(ProviderEvent::TurnComplete {
                    usage: UsageTotals::default(),
                    is_error: false,
                    interrupted: true,
                });
            }
            return;
        }
        if let Some(turn_id) = self.active_turn_id.clone() {
            self.queued.clear();
            self.interrupted = true;
            self.interrupt_pending = false;
            self.permissions.clear();
            if let Some(tid) = self.thread_id.clone() {
                self.request(
                    "turn/interrupt",
                    json!({"threadId": tid, "turnId": turn_id}),
                    Pending::Interrupt,
                )
                .await;
            }
        } else if self.turn_starting {
            // Wait for `turn/start` response (or `turn/started`) to give us `turn_id`, then
            // immediately send `turn/interrupt`.
            self.queued.clear();
            self.interrupted = true;
            self.interrupt_pending = true;
            self.permissions.clear();
        } else if !self.queued.is_empty() {
            self.queued.clear();
            self.emit(ProviderEvent::TurnComplete {
                usage: UsageTotals::default(),
                is_error: false,
                interrupted: true,
            });
        }
    }

    async fn begin_thread(&mut self) {
        self.phase = Phase::StartingThread;
        let cwd = std::path::absolute(&self.cfg.cwd).unwrap_or_else(|_| self.cfg.cwd.clone());
        let cwd = cwd.to_string_lossy().into_owned();
        let mut params = json!({
            "cwd": cwd,
            "approvalPolicy": approval_policy(self.auto),
            "sandbox": "workspace-write",
        });
        if !self.cfg.model_id.is_empty() {
            params["model"] = json!(self.cfg.model_id);
        }
        match self.cfg.resume.clone() {
            Some(tid) => {
                params["threadId"] = json!(tid);
                params["excludeTurns"] = json!(true);
                self.request("thread/resume", params, Pending::Thread).await;
            }
            None => {
                self.request("thread/start", params, Pending::Thread).await;
            }
        }
    }

    async fn send_prompt(&mut self, text: String) {
        if self.interrupted && self.turn_in_flight() {
            // Never steer into a turn being stopped; `turn/completed` starts it.
            self.queued.push_back(text);
        } else if let Some(turn_id) = self.active_turn_id.clone() {
            let Some(tid) = self.thread_id.clone() else {
                return;
            };
            let params = json!({
                "threadId": tid,
                "expectedTurnId": turn_id,
                "input": [{"type": "text", "text": text}],
            });
            self.request("turn/steer", params, Pending::Steer { text })
                .await;
        } else if self.turn_starting {
            // Hold until `turn/start` gives us `turn_id`, then steer.
            self.queued.push_back(text);
        } else {
            self.start_turn(text).await;
        }
    }

    async fn start_turn(&mut self, text: String) {
        if self.closing() {
            return;
        }
        let Some(tid) = self.thread_id.clone() else {
            return;
        };
        self.turn_starting = true;
        self.interrupted = false;
        self.interrupt_pending = false;
        let mut params = json!({
            "threadId": tid,
            "input": [{"type": "text", "text": text}],
            "approvalPolicy": approval_policy(self.auto),
            "summary": "concise",
        });
        if !self.cfg.model_id.is_empty() {
            params["model"] = json!(self.cfg.model_id);
        }
        if let Some(effort) = effective_effort(self.cfg.effort, self.supported_efforts.as_deref()) {
            params["effort"] = json!(effort);
        }
        self.request("turn/start", params, Pending::StartTurn).await;
    }

    async fn on_line(&mut self, line: &str) {
        match rpc::parse_line(line) {
            Some(Incoming::Response { id, result }) => {
                if let Some(pending) = id
                    .as_u64()
                    .and_then(|id| self.pending.remove(&id).map(|p| (id, p)))
                {
                    self.on_response(pending.1, result).await;
                }
            }
            Some(Incoming::Notification { method, params }) => {
                let Some(tid) = self.thread_id.as_deref() else {
                    return;
                };
                if !rpc::for_thread(&params, tid) {
                    return;
                }
                if method == "turn/started" {
                    if let Some(turn_id) = params.pointer("/turn/id").and_then(Value::as_str) {
                        self.on_turn_id(turn_id.to_string()).await;
                    }
                }
                let evs = self.translator.on_notification(&method, &params);
                self.emit_all(evs);
                if method == "turn/completed" {
                    self.active_turn_id = None;
                    self.turn_starting = false;
                    self.interrupted = false;
                    self.interrupt_pending = false;
                    self.permissions.clear();
                    if let Some(next) = self.queued.pop_front() {
                        self.start_turn(next).await;
                    }
                }
            }
            Some(Incoming::Request { id, method, params }) => {
                if is_approval_method(&method) {
                    self.on_approval(id, &method, &params).await;
                } else {
                    self.write(rpc::method_not_found(&id)).await;
                }
            }
            None => debug!("skipping an unparsable stdout line"),
        }
    }

    async fn on_approval(&mut self, id: Value, method: &str, params: &Value) {
        if self.closing() {
            return;
        }
        let ours = self
            .thread_id
            .as_deref()
            .is_some_and(|tid| rpc::for_thread(params, tid));
        if !ours || self.interrupted {
            self.write(approval_reply(&id, false)).await;
        } else if self.auto {
            self.write(approval_reply(&id, true)).await;
        } else {
            let key = rpc::id_key(&id);
            let evs = self
                .translator
                .on_approval_request(key.clone(), method, params);
            self.permissions.insert(key, id);
            self.emit_all(evs);
        }
    }

    async fn on_turn_id(&mut self, turn_id: String) {
        self.turn_starting = false;
        if self.active_turn_id.as_deref() == Some(turn_id.as_str()) {
            return;
        }
        self.active_turn_id = Some(turn_id.clone());
        if self.interrupt_pending {
            self.interrupt_pending = false;
            if let Some(tid) = self.thread_id.clone() {
                self.request(
                    "turn/interrupt",
                    json!({"threadId": tid, "turnId": turn_id}),
                    Pending::Interrupt,
                )
                .await;
            }
            return;
        }
        while let Some(text) = self.queued.pop_front() {
            self.send_prompt(text).await;
        }
    }

    async fn on_response(&mut self, pending: Pending, result: Result<Value, RpcError>) {
        match pending {
            Pending::Initialize => match result {
                Ok(_) => {
                    self.write(rpc::notification("initialized", None)).await;
                    if self.cfg.resume.is_some() {
                        self.begin_thread().await;
                    } else {
                        self.phase = Phase::NeedThread;
                        if !self.queued.is_empty() {
                            self.begin_thread().await;
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("Codex failed to initialize: {}", e.message);
                    self.fail(msg);
                }
            },
            Pending::Thread => match result {
                Ok(r) => self.on_thread_ready(&r).await,
                Err(e) => {
                    let msg = thread_error(&e, self.cfg.resume.is_some());
                    self.fail(msg);
                }
            },
            Pending::StartTurn => match result {
                Ok(r) => {
                    if let Some(turn_id) = r.pointer("/turn/id").and_then(Value::as_str) {
                        self.on_turn_id(turn_id.to_string()).await;
                    } else {
                        self.turn_starting = false;
                    }
                }
                Err(e) => {
                    self.turn_starting = false;
                    self.active_turn_id = None;
                    let interrupted = self.interrupt_pending || self.interrupted;
                    self.interrupted = false;
                    self.interrupt_pending = false;
                    if !interrupted {
                        self.emit(ProviderEvent::Error(e.message));
                    }
                    self.emit(ProviderEvent::TurnComplete {
                        usage: UsageTotals::default(),
                        is_error: !interrupted,
                        interrupted,
                    });
                    if let Some(next) = self.queued.pop_front() {
                        self.start_turn(next).await;
                    }
                }
            },
            Pending::Steer { text } => {
                if let Err(e) = result {
                    debug!(error = %e.message, "turn/steer rejected; falling back to turn/start");
                    if self.active_turn_id.is_none() && !self.turn_starting {
                        self.start_turn(text).await;
                    } else {
                        self.queued.push_back(text);
                    }
                }
            }
            Pending::Interrupt => {
                if let Err(e) = result {
                    debug!(error = %e.message, "turn/interrupt returned an error");
                }
            }
        }
    }

    async fn on_thread_ready(&mut self, result: &Value) {
        let tid = result
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| self.cfg.resume.clone());
        let Some(tid) = tid else {
            self.fail("Codex returned no thread id".into());
            return;
        };
        let model_id = result
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| self.cfg.model_id.clone());
        self.thread_id = Some(tid.clone());
        self.phase = Phase::Ready;
        self.emit(ProviderEvent::Started {
            provider_session_id: tid,
            model_id,
        });
        if let Some(first) = self.queued.pop_front() {
            self.start_turn(first).await;
        }
    }
}

// ---- task ---------------------------------------------------------------------------------

struct Process {
    child: Child,
    pid: Option<u32>,
    cmd_rx: UnboundedReceiver<SessionCommand>,
    line_rx: UnboundedReceiver<String>,
    stderr_tail: StderrTail,
    reader: tokio::task::JoinHandle<()>,
    err_reader: tokio::task::JoinHandle<()>,
}

async fn run(mut agent: Agent, mut p: Process) {
    agent.initialize().await;
    let mut requested = false;
    let mut stdout_open = true;
    let status = loop {
        if agent.closing() {
            requested = true;
            break shutdown(&mut p.child, &mut agent.stdin, p.pid).await;
        }
        tokio::select! {
            cmd = p.cmd_rx.recv() => match cmd {
                None | Some(SessionCommand::Shutdown) => {
                    requested = true;
                    break shutdown(&mut p.child, &mut agent.stdin, p.pid).await;
                }
                Some(c) => agent.on_command(c).await,
            },
            line = p.line_rx.recv(), if stdout_open => match line {
                Some(l) => agent.on_line(&l).await,
                None => stdout_open = false,
            },
            st = p.child.wait() => break st.ok(),
        }
    };
    agent.stdin = None;
    agent.phase = Phase::Closing;
    killpg(p.pid, libc::SIGKILL);

    while let Ok(Some(line)) = timeout(SIGTERM_WAIT, p.line_rx.recv()).await {
        agent.on_line(&line).await;
    }
    for reader in [&mut p.reader, &mut p.err_reader] {
        match timeout(SIGTERM_WAIT, reader).await {
            Ok(Err(e)) => tracing::warn!(error = %e, "codex reader task failed"),
            Err(_) => tracing::warn!("codex reader task did not finish"),
            Ok(Ok(())) => {}
        }
    }

    let rest = agent.translator.flush_text();
    agent.emit_all(rest);
    let code = status.and_then(|st| st.code());
    let mid_turn = agent.turn_in_flight() || !agent.queued.is_empty();
    if !requested && (code != Some(0) || mid_turn) {
        let tail: Vec<String> = lock(&p.stderr_tail).iter().cloned().collect();
        let msg = xade_core::unexpected_exit_message("codex", status, &tail);
        agent.emit(ProviderEvent::Error(msg));
        if mid_turn {
            agent.emit(ProviderEvent::TurnComplete {
                usage: UsageTotals::default(),
                is_error: true,
                interrupted: false,
            });
        }
    }
    agent.emit(ProviderEvent::Exited { code });
}

/// Close stdin, wait 3 s, SIGTERM the group, wait 2 s, SIGKILL.
async fn shutdown(
    child: &mut Child,
    stdin: &mut Option<ChildStdin>,
    pid: Option<u32>,
) -> Option<ExitStatus> {
    stdin.take();
    if let Ok(st) = timeout(STDIN_CLOSE_WAIT, child.wait()).await {
        return st.ok();
    }
    killpg(pid, libc::SIGTERM);
    if let Ok(st) = timeout(SIGTERM_WAIT, child.wait()).await {
        return st.ok();
    }
    killpg(pid, libc::SIGKILL);
    child.wait().await.ok()
}

fn killpg(pid: Option<u32>, signal: libc::c_int) {
    if let Some(pid) = pid {
        // SAFETY: plain syscall; the child leads its own group (process_group(0)).
        unsafe {
            libc::killpg(pid as libc::pid_t, signal);
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn approval_policy_maps_auto_mode() {
        assert_eq!(approval_policy(false), "untrusted");
        assert_eq!(approval_policy(true), "never");
    }

    #[test]
    fn effort_omitted_for_default_or_unsupported_model_level() {
        assert_eq!(effective_effort(Effort::Default, None), None);
        assert_eq!(effective_effort(Effort::High, None), Some("high"));
        let levels = [Effort::Low, Effort::Medium, Effort::High, Effort::XHigh];
        assert_eq!(effective_effort(Effort::High, Some(&levels)), Some("high"));
        assert_eq!(effective_effort(Effort::Max, Some(&levels)), None);
    }

    #[test]
    fn approval_reply_preserves_numeric_id() {
        let v: Value = serde_json::from_str(&approval_reply(&json!(0), true)).expect("json");
        assert_eq!(v["id"], json!(0));
        assert_eq!(v["result"]["decision"], json!("accept"));
        let v: Value = serde_json::from_str(&approval_reply(&json!(7), false)).expect("json");
        assert_eq!(v["id"], json!(7));
        assert_eq!(v["result"]["decision"], json!("decline"));
    }

    #[test]
    fn thread_error_classifies_auth_resume_and_start() {
        let auth = RpcError {
            code: -32600,
            message: "Authentication required".into(),
        };
        assert_eq!(thread_error(&auth, false), NOT_SIGNED_IN);
        let resume = RpcError {
            code: -32600,
            message: "no rollout found for thread id abc".into(),
        };
        assert!(thread_error(&resume, true).starts_with("Could not resume the Codex thread:"));
        assert!(thread_error(&resume, false).starts_with("Codex could not start a thread:"));
    }
}
