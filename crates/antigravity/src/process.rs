//! One `agy acp` child per session: spawn, a stdout/stderr reader each, and one task that owns the
//! child and its stdin, speaks JSON-RPC and turns what comes back into `ProviderEvent`s.

use std::collections::{BTreeSet, HashMap, VecDeque};
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
const NOT_SIGNED_IN: &str =
    "Antigravity is not signed in. Run `agy` in a terminal once to sign in.";

type StderrTail = Arc<Mutex<VecDeque<String>>>;
type Events = UnboundedSender<ProviderEvent>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Spawns the child and its tasks. Must be called inside a tokio runtime. Nothing is sent to agy
/// but `initialize` until the first prompt: the conversation is created lazily.
pub fn spawn_session(binary: &Path, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError> {
    // The environment is left exactly as it is.
    let mut child = Command::new(binary)
        .arg("acp")
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
        Agent::new(cfg, stdin, ev_tx),
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

/// Forwards each stdout line. No length cap (tool outputs can be large); a line that is not UTF-8
/// is skipped.
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
                tracing::warn!(error = %e, "reading agy stdout failed");
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
                tracing::warn!(error = %e, "reading agy stderr failed");
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct PermOption {
    id: String,
    kind: String,
}

fn parse_options(params: &Value) -> Vec<PermOption> {
    params
        .get("options")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|o| {
                    Some(PermOption {
                        id: o.get("optionId")?.as_str()?.to_string(),
                        kind: o.get("kind").and_then(Value::as_str)?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `outcome` for a permission request: allow picks `allow_once`, `allow_always`, then any
/// `allow*`; deny picks `reject_once`, `reject_always`, then any `reject*`; none fitting is `cancelled`.
fn choose_outcome(options: &[PermOption], allow: bool) -> Value {
    let (prefix, order) = if allow {
        ("allow", ["allow_once", "allow_always"])
    } else {
        ("reject", ["reject_once", "reject_always"])
    };
    let pick = order
        .iter()
        .find_map(|kind| options.iter().find(|o| o.kind == *kind))
        .or_else(|| options.iter().find(|o| o.kind.starts_with(prefix)));
    match pick {
        Some(o) => json!({"outcome": {"outcome": "selected", "optionId": o.id}}),
        None => cancelled(),
    }
}

fn cancelled() -> Value {
    json!({"outcome": {"outcome": "cancelled"}})
}

/// The reply line to a permission request. `id` is the request's original JSON value, so a string
/// id stays a string.
fn permission_reply(id: &Value, options: &[PermOption], allow: bool) -> String {
    rpc::response_ok(id, choose_outcome(options, allow))
}

/// `thought_level` value for an effort. `Default` and `Max` (agy has no `max`) set nothing.
fn thought_level(effort: Effort) -> Option<&'static str> {
    match effort {
        Effort::Low => Some("low"),
        Effort::Medium => Some("medium"),
        Effort::High => Some("high"),
        Effort::XHigh => Some("xhigh"),
        Effort::Default | Effort::Max => None,
    }
}

/// `(currentValue, values)` of the config option `id` in a `configOptions` array.
fn config_option<'a>(options: &'a Value, id: &str) -> (Option<&'a str>, Vec<&'a str>) {
    let Some(opt) = options.as_array().and_then(|a| {
        a.iter()
            .find(|o| o.get("id").and_then(Value::as_str) == Some(id))
    }) else {
        return (None, vec![]);
    };
    let current = opt.get("currentValue").and_then(Value::as_str);
    let values = opt
        .get("options")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|o| o.get("value").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    (current, values)
}

/// User-facing text for a failed `session/new` or `session/resume`.
fn session_error(err: &RpcError, resuming: bool) -> String {
    if err.code == -32000 || err.message.to_lowercase().contains("auth") {
        NOT_SIGNED_IN.to_string()
    } else if resuming {
        format!(
            "Could not resume the Antigravity conversation: {}",
            err.message
        )
    } else {
        format!("Antigravity could not start a session: {}", err.message)
    }
}

// ---- protocol state -----------------------------------------------------------------------

enum Pending {
    Initialize,
    Session,
    SetConfig { id: &'static str, value: String },
    Prompt,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Handshaking,
    /// Initialized; no conversation yet (created on the first prompt).
    NeedSession,
    /// `session/new` or `session/resume` sent, or its config options being set.
    CreatingSession,
    Ready,
    Closing,
}

struct Agent {
    cfg: SpawnConfig,
    stdin: Option<ChildStdin>,
    events: Events,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    outstanding: BTreeSet<u64>,
    /// Prompts received before the session is ready.
    queued: VecDeque<String>,
    phase: Phase,
    session_id: Option<String>,
    auto: bool,
    interrupted: bool,
    /// Permission requests shown to the user: key -> (original JSON id, options).
    permissions: HashMap<String, (Value, Vec<PermOption>)>,
    translator: Translator,
    /// Latest `configOptions` from agy.
    options: Value,
    /// Config options still to set, in order: the model first (it can reset the thinking level).
    stages: VecDeque<&'static str>,
}

impl Agent {
    fn new(cfg: SpawnConfig, stdin: ChildStdin, events: Events) -> Self {
        Self {
            auto: cfg.auto_mode,
            cfg,
            stdin: Some(stdin),
            events,
            next_id: 1,
            pending: HashMap::new(),
            outstanding: BTreeSet::new(),
            queued: VecDeque::new(),
            phase: Phase::Handshaking,
            session_id: None,
            interrupted: false,
            permissions: HashMap::new(),
            translator: Translator::new(),
            options: Value::Null,
            stages: VecDeque::new(),
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

    async fn write(&mut self, line: String) {
        let Some(w) = self.stdin.as_mut() else {
            debug!("agy stdin is closed; dropping a frame");
            return;
        };
        let res = match w.write_all(line.as_bytes()).await {
            Ok(()) => w.flush().await,
            Err(e) => Err(e),
        };
        if let Err(e) = res {
            self.stdin = None;
            self.emit(ProviderEvent::Error(format!("write to agy failed: {e}")));
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
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
        });
        self.request("initialize", params, Pending::Initialize)
            .await;
    }

    /// Reports an error that ends the session: queued prompts fail as one turn, then the task shuts
    /// the child down and emits `Exited`.
    fn fail(&mut self, msg: String) {
        self.emit(ProviderEvent::Error(msg));
        if !self.queued.is_empty() {
            self.queued.clear();
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
                    if self.phase == Phase::NeedSession {
                        self.begin_session().await;
                    }
                }
            }
            SessionCommand::Interrupt => self.interrupt().await,
            SessionCommand::RespondPermission { request_id, allow } => {
                // Unknown: already answered or cancelled.
                if let Some((id, options)) = self.permissions.remove(&request_id) {
                    self.write(permission_reply(&id, &options, allow)).await;
                }
            }
            SessionCommand::SetAutoMode(on) => {
                self.auto = on;
                if on {
                    let pending: Vec<_> = self.permissions.drain().collect();
                    for (_, (id, options)) in pending {
                        self.write(permission_reply(&id, &options, true)).await;
                    }
                }
            }
            SessionCommand::Shutdown => {}
        }
    }

    async fn interrupt(&mut self) {
        if self.phase != Phase::Ready && !self.queued.is_empty() {
            // Nothing was sent yet; without this the UI would stay "Streaming".
            self.queued.clear();
            self.emit(ProviderEvent::TurnComplete {
                usage: UsageTotals::default(),
                is_error: false,
                interrupted: true,
            });
        } else if !self.outstanding.is_empty() {
            self.interrupted = true;
            // ACP: the client answers every pending request when it cancels.
            let pending: Vec<_> = self.permissions.drain().collect();
            for (_, (id, _)) in pending {
                self.write(rpc::response_ok(&id, cancelled())).await;
            }
            if let Some(sid) = self.session_id.clone() {
                self.write(rpc::notification(
                    "session/cancel",
                    json!({"sessionId": sid}),
                ))
                .await;
            }
        }
    }

    async fn begin_session(&mut self) {
        self.phase = Phase::CreatingSession;
        let cwd = std::path::absolute(&self.cfg.cwd).unwrap_or_else(|_| self.cfg.cwd.clone());
        let cwd = cwd.to_string_lossy().into_owned();
        match self.cfg.resume.clone() {
            Some(sid) => {
                let params = json!({"sessionId": sid, "cwd": cwd, "mcpServers": []});
                self.request("session/resume", params, Pending::Session)
                    .await;
            }
            None => {
                let params = json!({"cwd": cwd, "mcpServers": []});
                self.request("session/new", params, Pending::Session).await;
            }
        }
    }

    async fn send_prompt(&mut self, text: String) {
        let Some(sid) = self.session_id.clone() else {
            return;
        };
        if self.outstanding.is_empty() {
            self.interrupted = false;
        }
        let params = json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]});
        let id = self
            .request("session/prompt", params, Pending::Prompt)
            .await;
        self.outstanding.insert(id);
    }

    async fn on_line(&mut self, line: &str) {
        match rpc::parse_line(line) {
            Some(Incoming::Response { id, result }) => {
                if let Some(pending) = id
                    .as_u64()
                    .and_then(|id| self.pending.remove(&id).map(|p| (id, p)))
                {
                    self.on_response(pending.0, pending.1, result).await;
                }
            }
            Some(Incoming::Notification { method, params }) => {
                if method != "session/update" {
                    return;
                }
                let Some(sid) = self.session_id.as_deref() else {
                    return;
                };
                if let Some(update) = rpc::update_for(&params, sid) {
                    let evs = self.translator.on_update(update);
                    self.emit_all(evs);
                }
            }
            Some(Incoming::Request { id, method, params }) => {
                if method == "session/request_permission" {
                    self.on_permission(id, &params).await;
                } else {
                    // Never leave a request unanswered: agy would block on it.
                    self.write(rpc::method_not_found(&id)).await;
                }
            }
            None => debug!("skipping an unparsable stdout line"),
        }
    }

    async fn on_permission(&mut self, id: Value, params: &Value) {
        let options = parse_options(params);
        let ours = params.get("sessionId").and_then(Value::as_str) == self.session_id.as_deref();
        if !ours || (self.interrupted && !self.outstanding.is_empty()) {
            // Not ours, or raised after Stop (the cancel is already on its way): never shown, and
            // never auto-allowed.
            self.write(rpc::response_ok(&id, cancelled())).await;
        } else if self.auto {
            self.write(permission_reply(&id, &options, true)).await;
        } else {
            let key = rpc::id_key(&id);
            let evs = self.translator.on_permission_request(key.clone(), params);
            self.permissions.insert(key, (id, options));
            self.emit_all(evs);
        }
    }

    async fn on_response(&mut self, id: u64, pending: Pending, result: Result<Value, RpcError>) {
        match pending {
            Pending::Initialize => match result {
                Ok(r) if r.get("protocolVersion").and_then(Value::as_u64) == Some(1) => {
                    self.phase = Phase::NeedSession;
                    if !self.queued.is_empty() {
                        self.begin_session().await;
                    }
                }
                Ok(_) => self.fail("Unsupported Antigravity ACP version".into()),
                Err(e) => {
                    let msg = if session_error(&e, false) == NOT_SIGNED_IN {
                        NOT_SIGNED_IN.to_string()
                    } else {
                        format!("Antigravity failed to initialize: {}", e.message)
                    };
                    self.fail(msg);
                }
            },
            Pending::Session => match result {
                Ok(r) => self.on_session_ready(&r).await,
                Err(e) => {
                    let msg = session_error(&e, self.cfg.resume.is_some());
                    self.fail(msg);
                }
            },
            Pending::SetConfig { id: config, value } => {
                match result {
                    Ok(r) => {
                        if let Some(opts) = r.get("configOptions").filter(|o| o.is_array()) {
                            self.options = opts.clone();
                        }
                    }
                    Err(e) => {
                        let what = if config == "model" {
                            "model"
                        } else {
                            "thinking level"
                        };
                        self.emit(ProviderEvent::Error(format!(
                            "Antigravity rejected {what} {value}: {}",
                            e.message
                        )));
                    }
                }
                self.next_config().await;
            }
            Pending::Prompt => {
                self.outstanding.remove(&id);
                let last = self.outstanding.is_empty();
                let evs = match result {
                    Ok(r) => self.translator.on_prompt_result(&r, last, self.interrupted),
                    Err(e) => self.translator.on_prompt_error(&e.message, last),
                };
                if last {
                    self.permissions.clear();
                }
                self.emit_all(evs);
            }
        }
    }

    async fn on_session_ready(&mut self, result: &Value) {
        let sid = result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| self.cfg.resume.clone());
        let Some(sid) = sid else {
            self.fail("Antigravity returned no session id".into());
            return;
        };
        self.options = result.get("configOptions").cloned().unwrap_or(Value::Null);
        let model_id = if self.cfg.model_id.is_empty() {
            config_option(&self.options, "model")
                .0
                .unwrap_or_default()
                .to_string()
        } else {
            self.cfg.model_id.clone()
        };
        self.session_id = Some(sid.clone());
        self.emit(ProviderEvent::Started {
            provider_session_id: sid,
            model_id,
        });
        self.stages = VecDeque::from(["model", "thought_level"]);
        self.next_config().await;
    }

    /// Sends the next config change that is needed, or finishes setup. The model always goes
    /// first: it can reset the thinking level, whose valid values depend on the model.
    async fn next_config(&mut self) {
        while let Some(stage) = self.stages.pop_front() {
            let want = match stage {
                "model" => Some(self.cfg.model_id.as_str()).filter(|m| !m.is_empty()),
                _ => thought_level(self.cfg.effort),
            };
            let Some(want) = want else { continue };
            let (current, values) = config_option(&self.options, stage);
            let known = stage == "model" || values.contains(&want);
            if current == Some(want) || !known {
                continue;
            }
            let Some(sid) = self.session_id.clone() else {
                return;
            };
            let params = json!({"sessionId": sid, "configId": stage, "value": want});
            let pending = Pending::SetConfig {
                id: stage,
                value: want.to_string(),
            };
            self.request("session/set_config_option", params, pending)
                .await;
            return;
        }
        self.phase = Phase::Ready;
        while let Some(text) = self.queued.pop_front() {
            self.send_prompt(text).await;
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

/// Command loop; owns the child until `Exited` is emitted.
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
    // The child is reaped, however it ended: whatever it started in its group must not outlive
    // the session.
    killpg(p.pid, libc::SIGKILL);

    // Let the readers drain what the child wrote before we report the exit.
    while let Ok(Some(line)) = timeout(SIGTERM_WAIT, p.line_rx.recv()).await {
        agent.on_line(&line).await;
    }
    for reader in [&mut p.reader, &mut p.err_reader] {
        match timeout(SIGTERM_WAIT, reader).await {
            Ok(Err(e)) => tracing::warn!(error = %e, "agy reader task failed"),
            Err(_) => tracing::warn!("agy reader task did not finish"),
            Ok(Ok(())) => {}
        }
    }

    // Whatever text the user saw streaming is kept.
    let rest = agent.translator.flush_text();
    agent.emit_all(rest);
    let code = status.and_then(|st| st.code());
    if !requested && code != Some(0) {
        let tail: Vec<String> = lock(&p.stderr_tail).iter().cloned().collect();
        agent.emit(ProviderEvent::Error(xade_core::unexpected_exit_message(
            "agy", status, &tail,
        )));
    }
    agent.emit(ProviderEvent::Exited { code });
}

/// Close stdin, wait 3 s, SIGTERM the group, wait 2 s, SIGKILL. No `session/close`: it is not
/// needed and a dead pipe would make it fail.
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

/// Signals the child's process group; an empty group (ESRCH) is fine and ignored.
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

    fn opts(list: &[(&str, &str)]) -> Vec<PermOption> {
        list.iter()
            .map(|(id, kind)| PermOption {
                id: (*id).into(),
                kind: (*kind).into(),
            })
            .collect()
    }

    fn chosen(o: &Value) -> Option<&str> {
        o.pointer("/outcome/optionId").and_then(Value::as_str)
    }

    #[test]
    fn allow_prefers_once_then_always_then_any_allow() {
        let both = opts(&[
            ("a", "allow_always"),
            ("o", "allow_once"),
            ("d", "reject_once"),
        ]);
        assert_eq!(chosen(&choose_outcome(&both, true)), Some("o"));
        let always = opts(&[("d", "reject_once"), ("a", "allow_always")]);
        assert_eq!(chosen(&choose_outcome(&always, true)), Some("a"));
        let other = opts(&[("d", "reject_once"), ("x", "allow_for_session")]);
        assert_eq!(chosen(&choose_outcome(&other, true)), Some("x"));
    }

    #[test]
    fn deny_prefers_once_then_always_then_any_reject() {
        let list = opts(&[
            ("a", "allow_once"),
            ("r", "reject_always"),
            ("d", "reject_once"),
        ]);
        assert_eq!(chosen(&choose_outcome(&list, false)), Some("d"));
        let always = opts(&[("a", "allow_once"), ("r", "reject_always")]);
        assert_eq!(chosen(&choose_outcome(&always, false)), Some("r"));
        let other = opts(&[("a", "allow_once"), ("x", "reject_forever")]);
        assert_eq!(chosen(&choose_outcome(&other, false)), Some("x"));
    }

    #[test]
    fn no_fitting_option_is_cancelled() {
        let only_allow = opts(&[("a", "allow_once")]);
        assert_eq!(choose_outcome(&only_allow, false), cancelled());
        assert_eq!(choose_outcome(&[], true), cancelled());
    }

    #[test]
    fn options_parse_from_the_request_and_skip_malformed_ones() {
        let p = json!({"options": [
            {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
            {"name": "no id", "kind": "allow_once"},
            {"optionId": "deny", "kind": "reject_once"},
        ]});
        assert_eq!(
            parse_options(&p),
            opts(&[("allow", "allow_once"), ("deny", "reject_once")])
        );
        assert!(parse_options(&json!({})).is_empty());
    }

    #[test]
    fn permission_replies_keep_the_request_id_type() {
        let o = opts(&[("allow", "allow_once"), ("deny", "reject_once")]);
        for id in [json!("1"), json!(42)] {
            let v: Value = serde_json::from_str(&permission_reply(&id, &o, true)).expect("json");
            assert_eq!(v["id"], id);
            assert_eq!(v["result"]["outcome"]["outcome"], "selected");
            assert_eq!(v["result"]["outcome"]["optionId"], "allow");
        }
        let v: Value =
            serde_json::from_str(&permission_reply(&json!("7"), &o, false)).expect("json");
        assert_eq!(v["result"]["outcome"]["optionId"], "deny");
    }

    #[test]
    fn effort_maps_to_thought_levels_and_max_to_none() {
        assert_eq!(thought_level(Effort::Default), None);
        assert_eq!(thought_level(Effort::Low), Some("low"));
        assert_eq!(thought_level(Effort::Medium), Some("medium"));
        assert_eq!(thought_level(Effort::High), Some("high"));
        assert_eq!(thought_level(Effort::XHigh), Some("xhigh"));
        assert_eq!(thought_level(Effort::Max), None);
    }

    #[test]
    fn config_option_reads_current_and_values() {
        let o = json!([
            {"id": "model", "currentValue": "m1", "options": [{"value": "m1"}, {"value": "m2"}]},
            {"id": "thought_level", "currentValue": "high", "options": [{"value": "low"}, {"value": "high"}]},
        ]);
        assert_eq!(config_option(&o, "model"), (Some("m1"), vec!["m1", "m2"]));
        assert_eq!(
            config_option(&o, "thought_level"),
            (Some("high"), vec!["low", "high"])
        );
        assert_eq!(config_option(&o, "mode"), (None, vec![]));
        assert_eq!(config_option(&Value::Null, "model"), (None, vec![]));
    }

    #[test]
    fn session_errors_read_for_the_user() {
        let e = |code, message: &str| RpcError {
            code,
            message: message.into(),
        };
        assert_eq!(
            session_error(&e(-32000, "Authentication required"), false),
            NOT_SIGNED_IN
        );
        assert_eq!(
            session_error(&e(-32603, "token auth expired"), true),
            NOT_SIGNED_IN
        );
        assert_eq!(
            session_error(&e(-32603, "Failed to read session x"), true),
            "Could not resume the Antigravity conversation: Failed to read session x"
        );
        assert_eq!(
            session_error(&e(-32603, "boom"), false),
            "Antigravity could not start a session: boom"
        );
    }
}
