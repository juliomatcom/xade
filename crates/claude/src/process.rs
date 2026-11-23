//! One `claude` child process per session: spawn, three tasks, shutdown.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::timeout;
use tracing::debug;
use xade_core::{ProviderError, ProviderEvent, SessionCommand, SessionHandle, SpawnConfig};

use crate::frames::{parse_line, ControlRequest, InFrame, OutFrame};
use crate::translate::Translator;

const STDERR_LINES: usize = 50;
const STDIN_CLOSE_WAIT: Duration = Duration::from_secs(3);
const SIGTERM_WAIT: Duration = Duration::from_secs(2);
const DENY_MESSAGE: &str = "Denied by user";

type Stdin = Arc<tokio::sync::Mutex<Option<ChildStdin>>>;
/// `request_id` -> original tool input, echoed as `updatedInput` on a manual allow.
type Pending = Arc<Mutex<HashMap<String, Value>>>;
type StderrTail = Arc<Mutex<VecDeque<String>>>;
type Events = UnboundedSender<ProviderEvent>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn build_args(cfg: &SpawnConfig) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
        "--model",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(cfg.model_id.clone());
    if let Some(e) = cfg.effort.cli_value() {
        args.extend(["--effort".to_string(), e.to_string()]);
    }
    if let Some(id) = &cfg.resume {
        args.extend(["--resume".to_string(), id.clone()]);
    }
    args
}

/// Spawns the child and its tasks. Must be called inside a tokio runtime.
pub fn spawn_session(binary: &Path, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError> {
    let mut child = Command::new(binary)
        .args(build_args(&cfg))
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
    let stdin: Stdin = Arc::new(tokio::sync::Mutex::new(Some(stdin)));
    let auto = Arc::new(AtomicBool::new(cfg.auto_mode));
    let interrupted = Arc::new(AtomicBool::new(false));
    let pending: Pending = Arc::default();
    let stderr_tail: StderrTail = Arc::default();

    let translator = Translator::new(interrupted.clone());
    let reader = tokio::spawn(read_stdout(
        stdout,
        translator,
        ev_tx.clone(),
        stdin.clone(),
        auto.clone(),
        pending.clone(),
    ));
    let err_reader = tokio::spawn(read_stderr(stderr, stderr_tail.clone()));
    tokio::spawn(run(Session {
        child,
        pid,
        cmd_rx,
        events: ev_tx,
        stdin,
        auto,
        interrupted,
        pending,
        stderr_tail,
        reader,
        err_reader,
    }));

    Ok(SessionHandle {
        commands: cmd_tx,
        events: ev_rx,
    })
}

async fn send(stdin: &Stdin, frame: &OutFrame) -> io::Result<()> {
    let mut line = frame.0.to_string();
    line.push('\n');
    let mut guard = stdin.lock().await;
    let Some(w) = guard.as_mut() else {
        return Err(io::Error::from(io::ErrorKind::BrokenPipe));
    };
    w.write_all(line.as_bytes()).await?;
    w.flush().await
}

async fn read_stdout(
    stdout: ChildStdout,
    mut translator: Translator,
    events: Events,
    stdin: Stdin,
    auto: Arc<AtomicBool>,
    pending: Pending,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let frame = match parse_line(&line) {
            Ok(f) => f,
            Err(e) => {
                debug!("skipping unparsable frame: {e}");
                continue;
            }
        };
        if let InFrame::ControlRequest {
            request_id,
            request: ControlRequest::CanUseTool { input, .. },
        } = &frame
        {
            if auto.load(Ordering::SeqCst) {
                if let Err(e) = send(&stdin, &OutFrame::allow(request_id, input.clone())).await {
                    debug!("auto-allow write failed: {e}");
                }
                continue;
            }
            lock(&pending).insert(request_id.clone(), input.clone());
        }
        for ev in translator.on_frame(frame) {
            let _ = events.send(ev);
        }
    }
}

async fn read_stderr(stderr: ChildStderr, tail: StderrTail) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut t = lock(&tail);
        if t.len() == STDERR_LINES {
            t.pop_front();
        }
        t.push_back(line);
    }
}

struct Session {
    child: Child,
    pid: Option<u32>,
    cmd_rx: UnboundedReceiver<SessionCommand>,
    events: Events,
    stdin: Stdin,
    auto: Arc<AtomicBool>,
    interrupted: Arc<AtomicBool>,
    pending: Pending,
    stderr_tail: StderrTail,
    reader: tokio::task::JoinHandle<()>,
    err_reader: tokio::task::JoinHandle<()>,
}

/// Command loop; owns the child until `Exited` is emitted.
async fn run(mut s: Session) {
    let mut requested = false;
    let status = loop {
        tokio::select! {
            cmd = s.cmd_rx.recv() => match cmd {
                None | Some(SessionCommand::Shutdown) => {
                    requested = true;
                    break shutdown(&mut s.child, &s.stdin, s.pid).await;
                }
                Some(c) => handle_command(&s, c).await,
            },
            st = s.child.wait() => break st.ok(),
        }
    };

    // Let the readers drain what the child wrote before we report the exit.
    let _ = timeout(SIGTERM_WAIT, &mut s.reader).await;
    let _ = timeout(SIGTERM_WAIT, &mut s.err_reader).await;

    let code = status.and_then(|st| st.code());
    if !requested && code != Some(0) {
        let tail: Vec<String> = lock(&s.stderr_tail).iter().cloned().collect();
        let _ = s.events.send(ProviderEvent::Error(format!(
            "claude exited unexpectedly: {}",
            tail.join("\n")
        )));
    }
    let _ = s.events.send(ProviderEvent::Exited { code });
}

async fn handle_command(s: &Session, cmd: SessionCommand) {
    let frame = match cmd {
        SessionCommand::SendPrompt(text) => {
            s.interrupted.store(false, Ordering::SeqCst);
            OutFrame::user_message(&text)
        }
        SessionCommand::Interrupt => {
            s.interrupted.store(true, Ordering::SeqCst);
            OutFrame::interrupt(&uuid::Uuid::new_v4().to_string())
        }
        SessionCommand::RespondPermission { request_id, allow } => {
            let input = lock(&s.pending).remove(&request_id);
            if allow {
                OutFrame::allow(&request_id, input.unwrap_or_else(|| json!({})))
            } else {
                OutFrame::deny(&request_id, DENY_MESSAGE)
            }
        }
        SessionCommand::SetAutoMode(on) => {
            s.auto.store(on, Ordering::SeqCst);
            return;
        }
        SessionCommand::Shutdown => return,
    };
    if let Err(e) = send(&s.stdin, &frame).await {
        let _ = s
            .events
            .send(ProviderEvent::Error(format!("write to claude failed: {e}")));
    }
}

/// Close stdin, wait 3 s, SIGTERM the group, wait 2 s, SIGKILL.
async fn shutdown(child: &mut Child, stdin: &Stdin, pid: Option<u32>) -> Option<ExitStatus> {
    if let Ok(mut g) = timeout(Duration::from_secs(1), stdin.lock()).await {
        g.take();
    }
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
mod tests {
    use super::*;
    use xade_core::Effort;

    fn cfg(effort: Effort, resume: Option<&str>) -> SpawnConfig {
        SpawnConfig {
            cwd: ".".into(),
            model_id: "haiku".into(),
            effort,
            auto_mode: false,
            resume: resume.map(String::from),
        }
    }

    #[test]
    fn args_default() {
        let a = build_args(&cfg(Effort::Default, None));
        assert!(a.ends_with(&["--model".into(), "haiku".into()]));
        assert!(!a.contains(&"--effort".to_string()));
        assert!(!a.contains(&"--setting-sources".to_string()));
    }

    #[test]
    fn args_effort_and_resume() {
        let a = build_args(&cfg(Effort::XHigh, Some("abc")));
        assert!(a.ends_with(&[
            "--effort".into(),
            "xhigh".into(),
            "--resume".into(),
            "abc".into()
        ]));
    }
}
