//! One `opencode acp` child per session: spawn, a stdout/stderr reader each, and one task that owns
//! the child and its stdin, speaks JSON-RPC and turns what comes back into `ProviderEvent`s.
//!
//! One `session/prompt` runs at a time; a prompt sent meanwhile steers it. OpenCode can also work
//! with no prompt running (a background subagent reporting back), and nothing marks the end of
//! that: such a "synthetic" turn is ended after a quiet period.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::{sleep_until, timeout, Instant};
use tracing::debug;
use xade_core::{
    Effort, ProviderError, ProviderEvent, SessionCommand, SessionHandle, SpawnConfig, UsageTotals,
};

use crate::rpc::{self, Incoming, RpcError};
use crate::translate::Translator;

const STDERR_LINES: usize = 50;
const STDIN_CLOSE_WAIT: Duration = Duration::from_secs(3);
const SIGTERM_WAIT: Duration = Duration::from_secs(2);
/// Shorter grace when Stop recycles the child to kill a background subagent.
const RECYCLE_WAIT: Duration = Duration::from_millis(500);
/// How long a turn with no prompt behind it must stay silent to count as over.
pub(crate) const QUIET_PERIOD: Duration = Duration::from_secs(5);

const CONFIG_ENV: &str = "OPENCODE_CONFIG_CONTENT";
/// What Auto OFF makes OpenCode ask about. Reading (and `external_directory`, which a fork's
/// hand-off needs) stays as the user configured it.
const ASK_ACTIONS: [&str; 8] = [
    "shell",
    "edit",
    "write",
    "patch",
    "webfetch",
    "websearch",
    "execute",
    "generate_image",
];
/// Built-in agents whose own rules would otherwise come after ours.
const ASK_AGENTS: [&str; 3] = ["build", "explore", "general"];

const UPDATE_REQUIRED: &str = "OpenCode 2.0 or newer is required; please update OpenCode.";
const NOT_SIGNED_IN: &str =
    "OpenCode is not signed in. Run `opencode auth login` in a terminal to sign in.";

type StderrTail = Arc<Mutex<VecDeque<String>>>;
type Events = UnboundedSender<ProviderEvent>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Spawned {
    child: Child,
    pid: Option<u32>,
    stdin: ChildStdin,
    line_rx: UnboundedReceiver<String>,
    stderr_tail: StderrTail,
    reader: tokio::task::JoinHandle<()>,
    err_reader: tokio::task::JoinHandle<()>,
}

fn spawn_child(
    binary: &Path,
    cwd: &Path,
    auto_off_env: Option<&str>,
) -> Result<Spawned, ProviderError> {
    let mut command = Command::new(binary);
    command
        .arg("acp")
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    if let Some(config) = auto_off_env {
        command.env(CONFIG_ENV, config);
    }
    let mut child = command
        .spawn()
        .map_err(|e| ProviderError::Spawn(e.to_string()))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(ProviderError::Spawn("child stdio unavailable".into()));
    };
    let pid = child.id();
    let (line_tx, line_rx) = mpsc::unbounded_channel();
    let stderr_tail: StderrTail = Arc::default();
    let reader = tokio::spawn(read_stdout(stdout, line_tx));
    let err_reader = tokio::spawn(read_stderr(stderr, stderr_tail.clone()));
    Ok(Spawned {
        child,
        pid,
        stdin,
        line_rx,
        stderr_tail,
        reader,
        err_reader,
    })
}

/// Spawns the child and its tasks. Must be called inside a tokio runtime. Nothing is sent to
/// OpenCode but `initialize` until the first prompt: the session is created lazily.
///
/// With Auto OFF the child also gets permission rules in `OPENCODE_CONFIG_CONTENT`; with Auto ON
/// its environment is left as it is, so the user's own rules (a `deny` above all) apply.
pub fn spawn_session(
    binary: &Path,
    cfg: SpawnConfig,
    quiet: Duration,
) -> Result<SessionHandle, ProviderError> {
    let auto_off_env = if !cfg.auto_mode {
        let existing = std::env::var_os(CONFIG_ENV)
            .map(|v| v.into_string().map_err(|_| bad_config("it is not UTF-8")))
            .transpose()?;
        Some(auto_off_config(existing.as_deref())?)
    } else {
        None
    };
    let spawned = spawn_child(binary, &cfg.cwd, auto_off_env.as_deref())?;

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(run(
        Agent::new(cfg, quiet, spawned.stdin, ev_tx),
        Process {
            binary: binary.to_path_buf(),
            auto_off_env,
            child: spawned.child,
            pid: spawned.pid,
            cmd_rx,
            line_rx: spawned.line_rx,
            stderr_tail: spawned.stderr_tail,
            reader: spawned.reader,
            err_reader: spawned.err_reader,
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
                tracing::warn!(error = %e, "reading opencode stdout failed");
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
                tracing::warn!(error = %e, "reading opencode stderr failed");
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

fn bad_config(why: &str) -> ProviderError {
    ProviderError::Protocol(format!(
        "cannot turn Auto off: {CONFIG_ENV} is set and {why}"
    ))
}

/// `existing` (the user's own `OPENCODE_CONFIG_CONTENT`) with an `ask` rule appended for every
/// guarded action, at the top level and for each built-in agent. The inline config is merged last
/// and the last matching rule wins, so only `ask` is ever added (never `allow`). With Auto OFF a
/// user's `deny` rule for a guarded action therefore becomes an `ask` prompt rather than a silent
/// block, and this gated process never auto-allows requests even if Auto is toggled on mid-turn
/// (the idle respawn starts a clean process where user `deny` rules apply again). Anything that
/// cannot be extended safely is an error, never silently dropped.
fn auto_off_config(existing: Option<&str>) -> Result<String, ProviderError> {
    let mut config: Value = match existing.map(str::trim).filter(|s| !s.is_empty()) {
        Some(text) => serde_json::from_str(text).map_err(|_| bad_config("it is not valid JSON"))?,
        None => json!({}),
    };
    let rules = ASK_ACTIONS.map(|a| json!({"action": a, "resource": "*", "effect": "ask"}));
    let append = |scope: &mut Map<String, Value>| {
        scope
            .entry("permissions")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .map(|list| list.extend(rules.iter().cloned()))
            .ok_or_else(|| bad_config("a `permissions` in it is not a list"))
    };
    let root = config
        .as_object_mut()
        .ok_or_else(|| bad_config("it is not a JSON object"))?;
    append(root)?;
    let agents = root
        .entry("agents")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| bad_config("`agents` in it is not an object"))?;
    for name in ASK_AGENTS {
        let agent = agents
            .entry(name)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| bad_config("an agent in it is not an object"))?;
        append(agent)?;
    }
    Ok(config.to_string())
}

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

/// The `outcome` for a permission request. Allowing picks `allow_once`, then any `allow*`: Xade
/// never saves a standing approval while it can avoid it. Denying picks `reject_once`, then any
/// `reject*`; none fitting is `cancelled`.
fn choose_outcome(options: &[PermOption], allow: bool) -> Value {
    let (prefix, once) = if allow {
        ("allow", "allow_once")
    } else {
        ("reject", "reject_once")
    };
    let pick = options
        .iter()
        .find(|o| o.kind == once)
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

/// The `effort` option value. `Default` is a value too: a resumed session keeps its last effort.
fn effort_value(effort: Effort) -> &'static str {
    effort.cli_value().unwrap_or("default")
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

/// The major number of `agentInfo.version`.
fn major_version(version: &str) -> Option<u64> {
    version
        .trim_start_matches('v')
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// Why an `initialize` result cannot be used. Version 1 reads another permission key, so it would
/// run ungated with Auto OFF: an unknown version is refused too.
fn unsupported(init: &Value) -> Option<&'static str> {
    if init.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
        return Some("Unsupported OpenCode ACP version");
    }
    let major = init
        .pointer("/agentInfo/version")
        .and_then(Value::as_str)
        .and_then(major_version);
    (!major.is_some_and(|m| m >= 2)).then_some(UPDATE_REQUIRED)
}

/// User-facing text for a failed `initialize`, `session/new` or `session/resume`.
fn session_error(err: &RpcError, resuming: bool) -> String {
    if err.code == -32000 || err.message.to_lowercase().contains("auth") {
        NOT_SIGNED_IN.to_string()
    } else if resuming {
        format!(
            "Could not resume the OpenCode conversation: {}",
            err.message
        )
    } else {
        format!("OpenCode could not start a session: {}", err.message)
    }
}

// ---- protocol state -----------------------------------------------------------------------

enum Pending {
    Initialize,
    Session,
    SetConfig { id: &'static str, value: String },
    Prompt,
    Steer { text: String, epoch: u64 },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Handshaking,
    /// Initialized; no session yet (created on the first prompt).
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
    phase: Phase,
    session_id: Option<String>,
    auto: bool,
    translator: Translator,
    /// Latest `configOptions` from OpenCode.
    options: Value,
    /// Config options still to set, in order: the model first (it decides the effort levels).
    stages: VecDeque<&'static str>,

    /// The `session/prompt` request that is running.
    prompt: Option<u64>,
    /// Its answer, kept until every steer sent into it was answered: a refused steer must follow
    /// as the next prompt of the same turn.
    answer: Option<Result<Value, RpcError>>,
    steers: usize,
    /// Bumped by Stop, so a steer answered later never starts anything.
    steer_epoch: u64,
    /// Refused steers waiting to run ahead of any later-queued prompts, in their own order.
    refused_steers: VecDeque<String>,
    /// Prompts waiting: for the session, or for the running prompt to end.
    queued: VecDeque<String>,
    /// Stop was asked of the running prompt.
    interrupted: bool,
    /// Stop was asked since the last prompt was sent: permission requests that still arrive with
    /// no prompt running are cancelled.
    stopped: bool,
    /// Permission requests shown to the user: key -> (original JSON id, options).
    permissions: HashMap<String, (Value, Vec<PermOption>)>,

    /// A turn with no prompt behind it is in progress.
    synthetic: bool,
    /// Whether this child process has seen a subagent call or child-session frame: `session/cancel`
    /// cannot stop background subagents, so Stop kills and recycles the child when set.
    saw_subagent: bool,
    /// Set by Stop when the child must be killed and replaced with a fresh `acp` child resuming the
    /// same session.
    needs_recycle: bool,
    /// Tool calls that updates reported as unfinished. A permission request adds none: its call
    /// may never get an update.
    running: HashSet<String>,
    quiet: Duration,
    /// When the synthetic turn ends if nothing else arrives.
    deadline: Option<Instant>,
}

impl Agent {
    fn new(cfg: SpawnConfig, quiet: Duration, stdin: ChildStdin, events: Events) -> Self {
        Self {
            auto: cfg.auto_mode,
            cfg,
            stdin: Some(stdin),
            events,
            next_id: 1,
            pending: HashMap::new(),
            phase: Phase::Handshaking,
            session_id: None,
            translator: Translator::new(),
            options: Value::Null,
            stages: VecDeque::new(),
            prompt: None,
            answer: None,
            steers: 0,
            steer_epoch: 0,
            refused_steers: VecDeque::new(),
            queued: VecDeque::new(),
            interrupted: false,
            stopped: false,
            permissions: HashMap::new(),
            synthetic: false,
            saw_subagent: false,
            needs_recycle: false,
            running: HashSet::new(),
            quiet,
            deadline: None,
        }
    }

    fn reset_for_recycle(&mut self, resume_id: Option<String>, stdin: ChildStdin) {
        self.cfg.resume = resume_id;
        self.stdin = Some(stdin);
        self.next_id = 1;
        self.pending.clear();
        self.phase = Phase::Handshaking;
        self.session_id = None;
        self.options = Value::Null;
        self.stages.clear();
        self.prompt = None;
        self.answer = None;
        self.steers = 0;
        self.steer_epoch += 1;
        self.interrupted = false;
        self.stopped = false;
        self.permissions.clear();
        self.end_synthetic();
        self.saw_subagent = false;
    }

    fn has_queued(&self) -> bool {
        !self.refused_steers.is_empty() || !self.queued.is_empty()
    }

    fn pop_queued(&mut self) -> Option<String> {
        self.refused_steers
            .pop_front()
            .or_else(|| self.queued.pop_front())
    }

    fn clear_queued(&mut self) {
        self.refused_steers.clear();
        self.queued.clear();
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

    /// Whether the UI is waiting for a `TurnComplete`.
    fn mid_turn(&self) -> bool {
        self.prompt.is_some() || self.synthetic || self.has_queued()
    }

    async fn write(&mut self, line: String) {
        let Some(w) = self.stdin.as_mut() else {
            debug!("opencode stdin is closed; dropping a frame");
            return;
        };
        let res = match w.write_all(line.as_bytes()).await {
            Ok(()) => w.flush().await,
            Err(e) => Err(e),
        };
        if let Err(e) = res {
            self.stdin = None;
            self.emit(ProviderEvent::Error(format!(
                "write to opencode failed: {e}"
            )));
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
            "clientInfo": {"name": "xade", "version": env!("CARGO_PKG_VERSION")},
        });
        self.request("initialize", params, Pending::Initialize)
            .await;
    }

    /// Reports an error that ends the session: waiting prompts fail as one turn, then the task
    /// shuts the child down and emits `Exited`.
    fn fail(&mut self, msg: String) {
        self.emit(ProviderEvent::Error(msg));
        if self.mid_turn() {
            self.clear_queued();
            self.prompt = None;
            self.synthetic = false;
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
                    self.send(text).await;
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
                // A process spawned with Auto OFF carries injected `ask` rules that turned user
                // `deny` rules into prompts: never auto-allow in it. The actor's idle respawn
                // brings a clean environment where user `deny` rules apply again.
                if !self.cfg.auto_mode {
                    return;
                }
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

    /// A prompt for the ready session: starts one, steers the running one, or waits behind a
    /// Stop or an earlier waiting prompt.
    async fn send(&mut self, text: String) {
        if self.prompt.is_none() {
            self.start_prompt(text).await;
        } else if self.interrupted || self.answer.is_some() || self.has_queued() {
            self.queued.push_back(text);
        } else {
            let Some(sid) = self.session_id.clone() else {
                return;
            };
            let params = json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]});
            self.steers += 1;
            let epoch = self.steer_epoch;
            self.request("_session/steering", params, Pending::Steer { text, epoch })
                .await;
        }
    }

    async fn start_prompt(&mut self, text: String) {
        if self.closing() {
            return;
        }
        let Some(sid) = self.session_id.clone() else {
            return;
        };
        if self.synthetic {
            // The new prompt takes the turn over; what was said so far stays its own block.
            self.end_synthetic();
            let evs = self.translator.flush_text();
            self.emit_all(evs);
        }
        self.interrupted = false;
        self.stopped = false;
        let params = json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]});
        let id = self
            .request("session/prompt", params, Pending::Prompt)
            .await;
        self.prompt = Some(id);
    }

    async fn cancel_permissions(&mut self) {
        let pending: Vec<_> = self.permissions.drain().collect();
        for (_, (id, _)) in pending {
            self.write(rpc::response_ok(&id, cancelled())).await;
        }
    }

    async fn interrupt(&mut self) {
        if self.phase != Phase::Ready {
            if self.has_queued() {
                // Nothing was sent yet; without this the UI would stay "Streaming".
                self.clear_queued();
                self.emit(ProviderEvent::TurnComplete {
                    usage: UsageTotals::default(),
                    is_error: false,
                    interrupted: true,
                });
            }
            return;
        }
        if self.prompt.is_none() && !self.synthetic {
            return;
        }
        self.clear_queued();
        self.steer_epoch += 1;
        self.steers = 0;
        self.stopped = true;
        // `session/cancel` only cancels the active ACP prompt fiber: it cannot stop a background
        // subagent or a no-prompt follow-up. Kill and recycle the child instead, keeping this
        // `SessionHandle` alive and resuming the same session on the next prompt.
        if self.synthetic || self.saw_subagent {
            self.permissions.clear();
            self.prompt = None;
            self.answer = None;
            self.interrupted = false;
            self.end_synthetic();
            self.saw_subagent = false;
            let evs = self.translator.finish_turn(true);
            self.emit_all(evs);
            self.needs_recycle = true;
            return;
        }
        // ACP: the client answers every pending request when it cancels.
        self.cancel_permissions().await;
        if let Some(sid) = self.session_id.clone() {
            self.write(rpc::notification(
                "session/cancel",
                json!({"sessionId": sid}),
            ))
            .await;
        }
        self.interrupted = true;
        // The prompt had already answered and was only waiting for a steer.
        if let Some(answer) = self.answer.take() {
            self.finish_prompt(answer).await;
        }
    }

    /// The running prompt answered and no steer is outstanding.
    async fn finish_prompt(&mut self, result: Result<Value, RpcError>) {
        self.prompt = None;
        let interrupted = std::mem::take(&mut self.interrupted);
        let clean = !interrupted
            && result.as_ref().is_ok_and(|r| {
                matches!(
                    r.get("stopReason").and_then(Value::as_str),
                    None | Some("end_turn")
                )
            });
        let chain = self.has_queued() && !self.closing();
        // After a clean end only a background subagent can still be waiting for an answer;
        // otherwise OpenCode has given up on whatever it asked.
        if !clean {
            self.cancel_permissions().await;
        }
        let waiting = !self.permissions.is_empty() && !chain;
        let last = !chain && !waiting;
        let evs = match result {
            Ok(r) => self.translator.on_prompt_result(&r, last, interrupted),
            Err(e) => self.translator.on_prompt_error(&e.message, last),
        };
        self.running.clear();
        self.emit_all(evs);
        if chain {
            if let Some(next) = self.pop_queued() {
                self.start_prompt(next).await;
            }
        } else if waiting {
            // The turn stays open: ending it would drop the request from the UI.
            self.synthetic = true;
        }
    }

    fn end_synthetic(&mut self) {
        self.synthetic = false;
        self.deadline = None;
        self.running.clear();
    }

    /// Restarts the quiet period of a synthetic turn; it does not run while OpenCode waits for
    /// us or a tool is known to be working.
    fn rearm(&mut self) {
        self.deadline = (self.synthetic && self.permissions.is_empty() && self.running.is_empty())
            .then(|| Instant::now() + self.quiet);
    }

    fn on_quiet(&mut self) {
        self.end_synthetic();
        let evs = self.translator.finish_turn(false);
        self.emit_all(evs);
    }

    /// Emits translated events; output with no prompt running opens a synthetic turn.
    fn emit_turn(&mut self, evs: Vec<ProviderEvent>) {
        let output = evs.iter().any(|e| {
            matches!(
                e,
                ProviderEvent::TextDelta(_)
                    | ProviderEvent::ThinkingProgress { .. }
                    | ProviderEvent::AssistantText { .. }
                    | ProviderEvent::ToolUse { .. }
                    | ProviderEvent::PermissionRequest { .. }
            )
        });
        if output && self.phase == Phase::Ready && self.prompt.is_none() {
            self.synthetic = true;
        }
        self.emit_all(evs);
    }

    fn track(&mut self, update: &Value) {
        if !matches!(
            update.get("sessionUpdate").and_then(Value::as_str),
            Some("tool_call" | "tool_call_update")
        ) {
            return;
        }
        let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
            return;
        };
        match update.get("status").and_then(Value::as_str) {
            Some("pending" | "in_progress") => {
                self.running.insert(id.to_string());
            }
            Some(_) => {
                self.running.remove(id);
            }
            None => {}
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

    async fn on_line(&mut self, line: &str) {
        match rpc::parse_line(line) {
            Some(Incoming::Response { id, result }) => {
                if let Some((id, pending)) = id
                    .as_u64()
                    .and_then(|id| self.pending.remove(&id).map(|p| (id, p)))
                {
                    self.on_response(id, pending, result).await;
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
                    if is_subagent_update(update) {
                        self.saw_subagent = true;
                    }
                    let evs = self.translator.on_update(update);
                    self.emit_turn(evs);
                    // After `emit_turn`, which may open a turn. An update trailing one is not
                    // tracked: nothing may ever finish it, and it would hold the next turn open.
                    if self.prompt.is_some() || self.synthetic {
                        self.track(update);
                    }
                }
            }
            Some(Incoming::Request { id, method, params }) => {
                if method == "session/request_permission" {
                    self.on_permission(id, &params).await;
                } else {
                    // Never leave a request unanswered: OpenCode would block on it.
                    self.write(rpc::method_not_found(&id)).await;
                }
            }
            None => debug!("skipping an unparsable stdout line"),
        }
    }

    async fn on_permission(&mut self, id: Value, params: &Value) {
        if self.closing() {
            return;
        }
        let options = parse_options(params);
        let ours = params.get("sessionId").and_then(Value::as_str) == self.session_id.as_deref();
        if ours
            && params
                .pointer("/toolCall/_meta/opencode~1child-session")
                .is_some()
        {
            self.saw_subagent = true;
        }
        let after_stop = if self.prompt.is_some() {
            self.interrupted
        } else {
            self.stopped
        };
        if !ours || after_stop {
            // Not ours, or raised after Stop: never shown, and never auto-allowed.
            self.write(rpc::response_ok(&id, cancelled())).await;
        } else if self.auto {
            self.write(permission_reply(&id, &options, true)).await;
        } else {
            let key = rpc::id_key(&id);
            let evs = self.translator.on_permission_request(key.clone(), params);
            self.permissions.insert(key, (id, options));
            self.emit_turn(evs);
        }
    }

    async fn on_response(&mut self, id: u64, pending: Pending, result: Result<Value, RpcError>) {
        match pending {
            Pending::Initialize => match result {
                Ok(r) => match unsupported(&r) {
                    Some(why) => self.fail(why.into()),
                    None => {
                        self.phase = Phase::NeedSession;
                        if self.has_queued() {
                            self.begin_session().await;
                        }
                    }
                },
                Err(e) => self.fail(session_error(&e, false)),
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
                        let msg = format!("OpenCode rejected {config} {value}: {}", e.message);
                        if config == "mode" {
                            // Another agent's own rules could run tools without asking.
                            return self.fail(msg);
                        }
                        self.emit(ProviderEvent::Error(msg));
                    }
                }
                self.next_config().await;
            }
            Pending::Prompt => {
                if self.prompt != Some(id) {
                    return;
                }
                if self.steers > 0 {
                    self.answer = Some(result);
                } else {
                    self.finish_prompt(result).await;
                }
            }
            Pending::Steer { text, epoch } => {
                if epoch != self.steer_epoch {
                    return;
                }
                self.steers = self.steers.saturating_sub(1);
                let injected = result
                    .as_ref()
                    .is_ok_and(|r| r.get("outcome").and_then(Value::as_str) == Some("injected"));
                if !injected {
                    debug!("steering refused; sending it as the next prompt");
                    if self.prompt.is_some() {
                        // Keep refused steers ahead of any prompts queued after them, preserving
                        // their own arrival order.
                        self.refused_steers.push_back(text);
                    } else {
                        self.start_prompt(text).await;
                    }
                }
                if self.steers == 0 {
                    if let Some(answer) = self.answer.take() {
                        self.finish_prompt(answer).await;
                    }
                }
            }
        }
    }

    async fn on_session_ready(&mut self, result: &Value) {
        // `session/resume` answers without the id.
        let sid = result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| self.cfg.resume.clone());
        let Some(sid) = sid else {
            self.fail("OpenCode returned no session id".into());
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
        self.stages = VecDeque::from(["model", "effort", "mode"]);
        self.next_config().await;
    }

    /// Sends the next config change that is needed, or finishes setup. The model goes first: the
    /// valid efforts depend on it.
    async fn next_config(&mut self) {
        while let Some(stage) = self.stages.pop_front() {
            let want = match stage {
                "model" => Some(self.cfg.model_id.as_str()).filter(|m| !m.is_empty()),
                "effort" => Some(effort_value(self.cfg.effort)),
                // With Auto OFF only the `build` agent is known to ask: a custom default agent
                // may carry its own `allow` rules.
                _ => (!self.cfg.auto_mode).then_some("build"),
            };
            let Some(want) = want else { continue };
            let (current, values) = config_option(&self.options, stage);
            let known = stage == "model" || values.contains(&want);
            if stage == "mode" && current != Some(want) && !known {
                self.fail(format!(
                    "OpenCode did not offer `{want}` mode; refusing to run ungated with Auto OFF"
                ));
                return;
            }
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
        if let Some(first) = self.pop_queued() {
            self.start_prompt(first).await;
        }
    }
}

fn is_subagent_update(update: &Value) -> bool {
    if update.pointer("/_meta/opencode~1child-session").is_some() {
        return true;
    }
    matches!(
        update.get("sessionUpdate").and_then(Value::as_str),
        Some("tool_call" | "tool_call_update")
    ) && matches!(
        update.get("name").and_then(Value::as_str),
        Some("subagent" | "task")
    )
}

// ---- task ---------------------------------------------------------------------------------

struct Process {
    binary: PathBuf,
    auto_off_env: Option<String>,
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
        if agent.needs_recycle {
            agent.needs_recycle = false;
            shutdown_with(
                &mut p.child,
                &mut agent.stdin,
                p.pid,
                RECYCLE_WAIT,
                RECYCLE_WAIT,
            )
            .await;
            killpg(p.pid, libc::SIGKILL);
            p.reader.abort();
            p.err_reader.abort();
            let resume_id = agent
                .session_id
                .clone()
                .or_else(|| agent.cfg.resume.clone());
            match spawn_child(&p.binary, &agent.cfg.cwd, p.auto_off_env.as_deref()) {
                Ok(next) => {
                    p.child = next.child;
                    p.pid = next.pid;
                    p.line_rx = next.line_rx;
                    p.stderr_tail = next.stderr_tail;
                    p.reader = next.reader;
                    p.err_reader = next.err_reader;
                    stdout_open = true;
                    agent.reset_for_recycle(resume_id, next.stdin);
                    agent.initialize().await;
                }
                Err(e) => {
                    agent.emit(ProviderEvent::Error(format!(
                        "OpenCode could not restart after stop: {e}"
                    )));
                    agent.emit(ProviderEvent::Exited { code: None });
                    return;
                }
            }
        }
        let deadline = agent.deadline;
        tokio::select! {
            cmd = p.cmd_rx.recv() => match cmd {
                None | Some(SessionCommand::Shutdown) => {
                    requested = true;
                    break shutdown(&mut p.child, &mut agent.stdin, p.pid).await;
                }
                Some(c) => {
                    agent.on_command(c).await;
                    agent.rearm();
                }
            },
            line = p.line_rx.recv(), if stdout_open => match line {
                Some(l) => {
                    agent.on_line(&l).await;
                    agent.rearm();
                }
                None => stdout_open = false,
            },
            () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                agent.deadline = None;
                agent.on_quiet();
            }
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
            Ok(Err(e)) => tracing::warn!(error = %e, "opencode reader task failed"),
            Err(_) => tracing::warn!("opencode reader task did not finish"),
            Ok(Ok(())) => {}
        }
    }

    let code = status.and_then(|st| st.code());
    let cut_off = !requested && agent.mid_turn();
    // Whatever text the user saw streaming is kept.
    let rest = agent.translator.flush_text();
    agent.emit_all(rest);
    if !requested && (cut_off || code != Some(0)) {
        let tail: Vec<String> = lock(&p.stderr_tail).iter().cloned().collect();
        let msg = xade_core::unexpected_exit_message("opencode", status, &tail);
        if cut_off {
            // Without a `TurnComplete` the UI would stay "Streaming".
            let evs = agent.translator.on_prompt_error(&msg, true);
            agent.emit_all(evs);
        } else {
            agent.emit(ProviderEvent::Error(msg));
        }
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
    shutdown_with(child, stdin, pid, STDIN_CLOSE_WAIT, SIGTERM_WAIT).await
}

async fn shutdown_with(
    child: &mut Child,
    stdin: &mut Option<ChildStdin>,
    pid: Option<u32>,
    close_wait: Duration,
    term_wait: Duration,
) -> Option<ExitStatus> {
    stdin.take();
    if let Ok(st) = timeout(close_wait, child.wait()).await {
        return st.ok();
    }
    killpg(pid, libc::SIGTERM);
    if let Ok(st) = timeout(term_wait, child.wait()).await {
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

    /// OpenCode's own three options, in its order.
    fn offered() -> Vec<PermOption> {
        opts(&[
            ("once", "allow_once"),
            ("always", "allow_always"),
            ("reject", "reject_once"),
        ])
    }

    #[test]
    fn allow_picks_once_and_deny_picks_reject() {
        assert_eq!(chosen(&choose_outcome(&offered(), true)), Some("once"));
        assert_eq!(chosen(&choose_outcome(&offered(), false)), Some("reject"));
        let reordered = opts(&[("a", "allow_always"), ("o", "allow_once")]);
        assert_eq!(chosen(&choose_outcome(&reordered, true)), Some("o"));
        let always = opts(&[("a", "allow_always"), ("r", "reject_always")]);
        assert_eq!(chosen(&choose_outcome(&always, true)), Some("a"));
        assert_eq!(chosen(&choose_outcome(&always, false)), Some("r"));
    }

    #[test]
    fn no_fitting_option_is_cancelled() {
        assert_eq!(
            choose_outcome(&opts(&[("a", "allow_once")]), false),
            cancelled()
        );
        assert_eq!(choose_outcome(&[], true), cancelled());
    }

    #[test]
    fn options_parse_from_the_request_and_skip_malformed_ones() {
        let p = json!({"options": [
            {"optionId": "once", "name": "Allow once", "kind": "allow_once"},
            {"name": "no id", "kind": "allow_once"},
            {"optionId": "reject", "kind": "reject_once"},
        ]});
        assert_eq!(
            parse_options(&p),
            opts(&[("once", "allow_once"), ("reject", "reject_once")])
        );
        assert!(parse_options(&json!({})).is_empty());
    }

    #[test]
    fn permission_replies_keep_the_request_id_type() {
        for id in [json!("1"), json!(0)] {
            let v: Value =
                serde_json::from_str(&permission_reply(&id, &offered(), true)).expect("json");
            assert_eq!(v["id"], id);
            assert_eq!(v["result"]["outcome"]["outcome"], "selected");
        }
    }

    fn config(existing: Option<&str>) -> Value {
        serde_json::from_str(&auto_off_config(existing).expect("config")).expect("json")
    }

    fn effects(rules: &Value) -> Vec<(String, String, String)> {
        rules
            .as_array()
            .expect("rules")
            .iter()
            .map(|r| {
                let s = |k: &str| r[k].as_str().expect(k).to_string();
                (s("action"), s("resource"), s("effect"))
            })
            .collect()
    }

    #[test]
    fn auto_off_asks_about_every_guarded_action_everywhere_and_allows_nothing() {
        for existing in [None, Some(""), Some("  ")] {
            let c = config(existing);
            let want: Vec<_> = ASK_ACTIONS
                .map(|a| (a.to_string(), "*".to_string(), "ask".to_string()))
                .into();
            assert_eq!(effects(&c["permissions"]), want);
            for agent in ASK_AGENTS {
                assert_eq!(effects(&c["agents"][agent]["permissions"]), want, "{agent}");
            }
            assert!(!c.to_string().contains("allow"));
        }
        let c = config(None);
        let actions: Vec<String> = effects(&c["permissions"])
            .into_iter()
            .map(|r| r.0)
            .collect();
        // Reading stays free, and a fork must read its hand-off outside the project.
        assert!(!actions
            .iter()
            .any(|a| a == "read" || a == "external_directory"));
    }

    #[test]
    fn auto_off_appends_to_the_users_config_after_their_rules() {
        let mine = json!({
            "theme": "x",
            "permissions": [{"action": "shell", "resource": "rm *", "effect": "deny"}],
            "agents": {
                "build": {"model": "m", "permissions": [{"action": "edit", "resource": "*", "effect": "allow"}]},
                "mine": {"description": "d"},
            },
        });
        let c = config(Some(&mine.to_string()));
        assert_eq!(c["theme"], "x");
        assert_eq!(c["agents"]["mine"], json!({"description": "d"}));
        assert_eq!(c["agents"]["build"]["model"], "m");
        for rules in [&c["permissions"], &c["agents"]["build"]["permissions"]] {
            let rules = effects(rules);
            assert_eq!(rules.len(), 1 + ASK_ACTIONS.len());
            assert_ne!(rules[0].2, "ask", "the user's rule stays first");
            assert!(rules[1..].iter().all(|r| r.2 == "ask"));
        }
    }

    #[test]
    fn a_config_that_cannot_be_extended_is_refused() {
        for bad in [
            "{",
            "// comment\n{}",
            "[]",
            "1",
            r#"{"permissions": {}}"#,
            r#"{"agents": []}"#,
            r#"{"agents": {"build": 1}}"#,
            r#"{"agents": {"explore": {"permissions": "ask"}}}"#,
        ] {
            let err = auto_off_config(Some(bad));
            assert!(
                matches!(&err, Err(ProviderError::Protocol(m)) if m.contains(CONFIG_ENV)),
                "{bad}: {err:?}"
            );
        }
    }

    #[test]
    fn every_effort_has_a_value() {
        let values = [
            Effort::Default,
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
        ]
        .map(effort_value);
        assert_eq!(values, ["default", "low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn only_protocol_1_from_opencode_2_or_newer_is_used() {
        let init = |protocol: u64, version: Value| {
            unsupported(&json!({"protocolVersion": protocol, "agentInfo": {"version": version}}))
        };
        for ok in ["2.0.0", "2.4.2", "v3.1", "10.0.0-beta.1"] {
            assert_eq!(init(1, ok.into()), None, "{ok}");
        }
        for old in ["1.14.2", "0.9.0", "local", ""] {
            assert_eq!(init(1, old.into()), Some(UPDATE_REQUIRED), "{old}");
        }
        assert_eq!(init(1, Value::Null), Some(UPDATE_REQUIRED));
        assert_eq!(
            unsupported(&json!({"protocolVersion": 1})),
            Some(UPDATE_REQUIRED)
        );
        assert_eq!(
            init(2, "2.4.2".into()),
            Some("Unsupported OpenCode ACP version")
        );
    }

    #[test]
    fn config_option_reads_current_and_values() {
        let o = json!([
            {"id": "model", "currentValue": "m1", "options": [{"value": "m1"}, {"value": "m2"}]},
        ]);
        assert_eq!(config_option(&o, "model"), (Some("m1"), vec!["m1", "m2"]));
        assert_eq!(config_option(&o, "effort"), (None, vec![]));
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
            session_error(&e(-32602, "Invalid params: session not found: x"), true),
            "Could not resume the OpenCode conversation: Invalid params: session not found: x"
        );
        assert_eq!(
            session_error(&e(-32603, "boom"), false),
            "OpenCode could not start a session: boom"
        );
    }
}
