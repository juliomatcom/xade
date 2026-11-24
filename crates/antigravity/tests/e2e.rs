//! End-to-end tests against the real `agy`. They spend a little quota, so each is `#[ignore]` and
//! also skips unless `XADE_E2E_AGY=1`:
//!
//! `XADE_E2E_AGY=1 cargo test -p xade-antigravity --test e2e -- --ignored --test-threads=1`
//!
//! Every test runs in a fresh git repo under the temp dir with `gemini-3.8-flash`, and deletes the
//! conversations it created through `delete_session`, even when it fails.

#![allow(clippy::expect_used)]

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use tokio::time::timeout;
use xade_antigravity::AntigravityProvider;
use xade_core::{AgentProvider, Effort, ProviderEvent, SessionCommand, SessionHandle, SpawnConfig};

const MODEL: &str = "gemini-3.8-flash";
const WAIT: Duration = Duration::from_secs(180);
/// A command whose output (42) cannot be read off the command line.
const RUN_42: &str =
    "Use your shell tool to run exactly this command: echo $((6*7)) . Then tell me its output.";

/// One at a time: the tests count this process's children.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn enabled() -> bool {
    std::env::var("XADE_E2E_AGY").is_ok_and(|v| v == "1")
}

macro_rules! e2e {
    () => {
        if !enabled() {
            eprintln!("skipped: set XADE_E2E_AGY=1");
            return;
        }
    };
}

/// A temp git repo, the provider, and every conversation id seen; dropping deletes those
/// conversations (a second time after a pause when the test panicked, since a child still
/// shutting down may write its files once more).
struct Ctx {
    provider: AntigravityProvider,
    repo: tempfile::TempDir,
    ids: RefCell<BTreeSet<String>>,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl Ctx {
    async fn new() -> Ctx {
        let serial = SERIAL.lock().await;
        let provider = AntigravityProvider::new();
        provider
            .preflight()
            .expect("agy is installed and signed in");
        assert!(
            provider.models().iter().any(|m| m.id == MODEL),
            "{MODEL} missing from `agy models`"
        );
        let repo = tempfile::tempdir().expect("tempdir");
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

    fn spawn(&self, model: &str, auto: bool, resume: Option<&str>) -> SessionHandle {
        self.provider
            .spawn(SpawnConfig {
                cwd: self.repo.path().to_path_buf(),
                model_id: model.into(),
                effort: Effort::Default,
                auto_mode: auto,
                resume: resume.map(String::from),
            })
            .expect("spawn")
    }

    /// Events up to and including the first that satisfies `stop`.
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

    /// Sends a prompt and returns the events through its `TurnComplete`, which must not be an error.
    async fn turn(&self, h: &mut SessionHandle, text: &str) -> Vec<ProviderEvent> {
        prompt(h, text);
        let evs = self.until(h, is_turn).await;
        assert_eq!(turns(&evs), [(false, false)], "{evs:?}");
        evs
    }

    async fn shutdown(&self, h: &mut SessionHandle) -> Vec<ProviderEvent> {
        send(h, SessionCommand::Shutdown);
        let evs = self
            .until(h, |e| matches!(e, ProviderEvent::Exited { .. }))
            .await;
        assert!(
            matches!(evs.last(), Some(ProviderEvent::Exited { code: Some(0) })),
            "{evs:?}"
        );
        evs
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        let delete = |ids: &BTreeSet<String>| {
            for id in ids {
                if let Err(e) = self.provider.delete_session(id) {
                    eprintln!("could not delete conversation {id}: {e}");
                }
            }
        };
        let ids = self.ids.borrow().clone();
        delete(&ids);
        if std::thread::panicking() {
            std::thread::sleep(Duration::from_secs(4));
            delete(&ids);
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

/// Everything the assistant said, lower-cased.
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

/// `(output, is_error)` of every tool result.
fn results(evs: &[ProviderEvent]) -> Vec<(String, bool)> {
    evs.iter()
        .filter_map(|e| match e {
            ProviderEvent::ToolResult {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

/// Pids of this process's children (the `agy acp` processes; the tests run one at a time).
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

fn conversations_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME"))
        .join(".gemini/antigravity-cli/conversations")
}

fn conversation_files() -> BTreeSet<String> {
    std::fs::read_dir(conversations_dir())
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn a_simple_turn() {
    e2e!();
    let ctx = Ctx::new().await;
    let mut h = ctx.spawn(MODEL, false, None);
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn two_turns_share_one_process_and_context() {
    e2e!();
    let ctx = Ctx::new().await;
    let mut h = ctx.spawn(MODEL, false, None);
    ctx.turn(&mut h, "Remember the word kiwi. Reply with just: ok")
        .await;
    let pids = wait_children(1).await;
    assert_eq!(pids.len(), 1, "{pids:?}");
    let evs = ctx
        .turn(
            &mut h,
            "What word did I ask you to remember? Reply with one word.",
        )
        .await;
    assert!(said(&evs).contains("kiwi"), "{evs:?}");
    assert_eq!(children(), pids, "same process for both turns");
    ctx.shutdown(&mut h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn auto_off_gates_a_command_and_allow_runs_it() {
    e2e!();
    let ctx = Ctx::new().await;
    let mut h = ctx.spawn(MODEL, false, None);
    prompt(&h, RUN_42);
    let evs = ctx.until(&mut h, is_permission).await;
    let Some(ProviderEvent::PermissionRequest {
        request_id,
        tool_name,
        ..
    }) = evs.last()
    else {
        panic!("no request");
    };
    assert_eq!(tool_name, "run_command");
    assert!(results(&evs).is_empty(), "nothing ran before the answer");
    send(
        &h,
        SessionCommand::RespondPermission {
            request_id: request_id.clone(),
            allow: true,
        },
    );
    let rest = ctx.until(&mut h, is_turn).await;
    assert!(
        results(&rest)
            .iter()
            .any(|(o, err)| o.contains("42") && !err),
        "{rest:?}"
    );
    ctx.shutdown(&mut h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn auto_off_deny_blocks_the_command() {
    e2e!();
    let ctx = Ctx::new().await;
    let mut h = ctx.spawn(MODEL, false, None);
    prompt(&h, RUN_42);
    let evs = ctx.until(&mut h, is_permission).await;
    let Some(ProviderEvent::PermissionRequest { request_id, .. }) = evs.last() else {
        panic!("no request");
    };
    send(
        &h,
        SessionCommand::RespondPermission {
            request_id: request_id.clone(),
            allow: false,
        },
    );
    let rest = ctx.until(&mut h, is_turn).await;
    let r = results(&rest);
    assert!(
        !r.is_empty() && r.iter().all(|(o, err)| *err && !o.contains("42")),
        "{r:?}"
    );
    ctx.shutdown(&mut h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn auto_on_runs_a_command_without_asking() {
    e2e!();
    let ctx = Ctx::new().await;
    let mut h = ctx.spawn(MODEL, true, None);
    let evs = ctx.turn(&mut h, RUN_42).await;
    assert!(!evs.iter().any(is_permission), "{evs:?}");
    assert!(
        results(&evs)
            .iter()
            .any(|(o, err)| o.contains("42") && !err),
        "{evs:?}"
    );
    ctx.shutdown(&mut h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn interrupt_then_the_next_prompt_works_in_the_same_process() {
    e2e!();
    let ctx = Ctx::new().await;
    let mut h = ctx.spawn(MODEL, false, None);
    prompt(
        &h,
        "Write the numbers from 1 to 800, one per line, and nothing else.",
    );
    ctx.until(&mut h, |e| {
        matches!(
            e,
            ProviderEvent::TextDelta(_) | ProviderEvent::ThinkingProgress { .. }
        )
    })
    .await;
    let pids = wait_children(1).await;
    assert_eq!(pids.len(), 1);
    send(&h, SessionCommand::Interrupt);
    let evs = ctx.until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)], "{evs:?}");
    let evs = ctx.turn(&mut h, "Reply with exactly one word: pong").await;
    assert!(said(&evs).contains("pong"), "{evs:?}");
    assert_eq!(children(), pids, "the process survived the interrupt");
    ctx.shutdown(&mut h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn a_steer_ends_with_exactly_one_turn_complete() {
    e2e!();
    let ctx = Ctx::new().await;
    let mut h = ctx.spawn(MODEL, true, None);
    prompt(
        &h,
        "Use your shell tool to run exactly: sleep 4; echo done . Then tell me its output.",
    );
    ctx.until(&mut h, |e| matches!(e, ProviderEvent::ToolUse { .. }))
        .await;
    prompt(&h, "Also end your final answer with the word banana.");
    let mut evs = ctx.until(&mut h, is_turn).await;
    // A second TurnComplete would show up shortly after the first.
    let extra = timeout(Duration::from_secs(6), ctx.until(&mut h, is_turn)).await;
    if let Ok(more) = extra {
        evs.extend(more);
    }
    assert_eq!(turns(&evs), [(false, false)], "{evs:?}");
    ctx.shutdown(&mut h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn resume_in_a_new_process_keeps_the_context() {
    e2e!();
    let ctx = Ctx::new().await;
    let mut h = ctx.spawn(MODEL, false, None);
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

    let mut h2 = ctx.spawn(MODEL, false, Some(&sid));
    let evs = ctx
        .turn(&mut h2, "What was the secret word? Reply with one word.")
        .await;
    assert!(
        matches!(&evs[0], ProviderEvent::Started { provider_session_id, model_id } if *provider_session_id == sid && model_id == MODEL),
        "{evs:?}"
    );
    assert!(said(&evs).contains("tangerine"), "{evs:?}");
    ctx.shutdown(&mut h2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn a_bad_model_is_not_fatal() {
    e2e!();
    let ctx = Ctx::new().await;
    let mut h = ctx.spawn("no-such-model-xade", false, None);
    let evs = ctx.turn(&mut h, "Reply with exactly one word: pong").await;
    assert!(
        evs.iter().any(|e| matches!(e, ProviderEvent::Error(m) if m.contains("rejected model no-such-model-xade"))),
        "{evs:?}"
    );
    assert!(said(&evs).contains("pong"), "{evs:?}");
    ctx.shutdown(&mut h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn no_prompt_creates_no_conversation() {
    e2e!();
    let ctx = Ctx::new().await;
    let before = conversation_files();
    let mut h = ctx.spawn(MODEL, false, None);
    assert_eq!(wait_children(1).await.len(), 1);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let evs = ctx.shutdown(&mut h).await;
    assert_eq!(evs.len(), 1, "only Exited: {evs:?}");
    assert_eq!(conversation_files(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn shutdown_leaves_no_agy_child() {
    e2e!();
    let ctx = Ctx::new().await;
    let mut h = ctx.spawn(MODEL, false, None);
    ctx.turn(&mut h, "Reply with exactly one word: pong").await;
    assert_eq!(wait_children(1).await.len(), 1);
    ctx.shutdown(&mut h).await;
    assert!(
        wait_children(0).await.is_empty(),
        "an agy child survived Shutdown"
    );
}
