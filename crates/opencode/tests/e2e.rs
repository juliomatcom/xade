//! End-to-end tests against the real `opencode`. They spend a little quota, so each is `#[ignore]`
//! and also skips unless `XADE_E2E_OPENCODE=1`:
//!
//! `XADE_E2E_OPENCODE=1 cargo test -p xade-opencode --test e2e -- --ignored --test-threads=1`
//!
//! Every test runs in a fresh git repo under the temp dir on cheap `opencode-go` models, and
//! deletes the sessions it created through `delete_session`, even when it fails. Only processes
//! the test itself started are ever signalled: the user's `opencode serve` is left alone.

#![allow(clippy::expect_used)]

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::time::timeout;
use xade_core::{Effort, ProviderEvent, SessionCommand, SessionHandle, SpawnConfig};
use xade_opencode::OpenCodeProvider;

const MODEL: &str = "opencode-go/glm-5.3-flash";
/// A model that edits through the `patch` tool.
const PATCH_MODEL: &str = "opencode-go/gpt-6-luna";
const WAIT: Duration = Duration::from_secs(180);

/// One at a time: the tests count this process's descendants.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn enabled() -> bool {
    std::env::var("XADE_E2E_OPENCODE").is_ok_and(|v| v == "1")
}

macro_rules! e2e {
    () => {
        if !enabled() {
            eprintln!("skipped: set XADE_E2E_OPENCODE=1");
            return;
        }
    };
}

/// A temp git repo, the provider, and every session id seen; dropping deletes those sessions.
struct Ctx {
    provider: OpenCodeProvider,
    repo: tempfile::TempDir,
    ids: RefCell<BTreeSet<String>>,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl Ctx {
    async fn new() -> Ctx {
        let serial = SERIAL.lock().await;
        let provider = OpenCodeProvider::new();
        provider.preflight().expect("opencode 2 is installed");
        for model in [MODEL, PATCH_MODEL] {
            assert!(
                provider.models().iter().any(|m| m.id == model),
                "{model} missing from `opencode models`"
            );
        }
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

    fn path(&self, file: &str) -> std::path::PathBuf {
        self.repo.path().join(file)
    }

    /// Writes the repo's own `opencode.json`.
    fn project_config(&self, config: Value) {
        std::fs::write(self.path("opencode.json"), config.to_string()).expect("config");
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

    /// Sends a prompt and returns the events through its `TurnComplete`, answering every
    /// permission request with `allow`.
    async fn turn_answering(
        &self,
        h: &mut SessionHandle,
        text: &str,
        allow: bool,
    ) -> Vec<ProviderEvent> {
        prompt(h, text);
        let mut all = Vec::new();
        loop {
            let evs = self.until(h, |e| is_turn(e) || is_permission(e)).await;
            let request = match evs.last() {
                Some(ProviderEvent::PermissionRequest { request_id, .. }) => {
                    Some(request_id.clone())
                }
                _ => None,
            };
            all.extend(evs);
            let Some(request_id) = request else {
                return all;
            };
            send(h, SessionCommand::RespondPermission { request_id, allow });
        }
    }

    /// A turn that must end well.
    async fn turn(&self, h: &mut SessionHandle, text: &str) -> Vec<ProviderEvent> {
        let evs = self.turn_answering(h, text, true).await;
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
        for id in self.ids.borrow().iter() {
            if let Err(e) = self.provider.delete_session(id) {
                eprintln!("could not delete session {id}: {e}");
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

fn parsed(json: &str) -> Value {
    serde_json::from_str(json).expect("json")
}

/// `(name, input)` of every permission request.
fn requests(evs: &[ProviderEvent]) -> Vec<(&str, Value)> {
    evs.iter()
        .filter_map(|e| match e {
            ProviderEvent::PermissionRequest {
                tool_name,
                input_json,
                ..
            } => Some((tool_name.as_str(), parsed(input_json))),
            _ => None,
        })
        .collect()
}

/// `(output, is_error, structured)` of every tool result.
fn results(evs: &[ProviderEvent]) -> Vec<(&str, bool, Option<Value>)> {
    evs.iter()
        .filter_map(|e| match e {
            ProviderEvent::ToolResult {
                output,
                is_error,
                structured_json,
                ..
            } => Some((
                output.as_str(),
                *is_error,
                structured_json.as_deref().map(parsed),
            )),
            _ => None,
        })
        .collect()
}

/// `(pid, ppid, command line)` of every process.
fn processes() -> Vec<(u32, u32, String)> {
    let out = Command::new("ps")
        .args(["-eo", "pid=,ppid=,stat=,args="])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (pid, ppid) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?);
            // A zombie awaiting its reaper counts as gone.
            if f.next()?.starts_with('Z') {
                return None;
            }
            Some((pid, ppid, f.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

/// Everything this test process started, at any depth, `ps` itself left out.
fn descendants() -> Vec<(u32, String)> {
    let all = processes();
    let mut found = vec![(std::process::id(), String::new())];
    let mut next = 0;
    while next < found.len() {
        let parent = found[next].0;
        found.extend(
            all.iter()
                .filter(|p| p.1 == parent && !p.2.starts_with("ps "))
                .map(|p| (p.0, p.2.clone())),
        );
        next += 1;
    }
    found.split_off(1)
}

/// Waits until none of `pids` is alive.
fn gone(pids: &[(u32, String)]) {
    let start = Instant::now();
    loop {
        let alive: Vec<_> = processes()
            .into_iter()
            .filter(|p| pids.iter().any(|(pid, cmd)| *pid == p.0 && *cmd == p.2))
            .collect();
        if alive.is_empty() {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "still alive: {alive:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[tokio::test]
#[ignore = "spends quota; set XADE_E2E_OPENCODE=1"]
async fn two_turns_in_one_process_keep_context() {
    e2e!();
    let c = Ctx::new().await;
    let mut h = c.spawn(MODEL, false, None);
    c.turn(&mut h, "Remember the word kumquat7. Reply with just OK.")
        .await;
    let evs = c
        .turn(
            &mut h,
            "What word did I ask you to remember? Reply with just the word.",
        )
        .await;
    assert!(said(&evs).contains("kumquat7"), "{evs:?}");
    assert!(matches!(
        evs.last(),
        Some(ProviderEvent::TurnComplete { usage, .. })
            if usage.context_window_max > 0 && usage.context_tokens_used > 0
    ));
    c.shutdown(&mut h).await;
}

/// The project allows `shell` at the top level and for `build`: Auto OFF must still ask.
#[tokio::test]
#[ignore = "spends quota; set XADE_E2E_OPENCODE=1"]
async fn auto_off_asks_even_when_the_project_allows_the_shell() {
    e2e!();
    let c = Ctx::new().await;
    let allow = json!([{"action": "shell", "resource": "*", "effect": "allow"}]);
    c.project_config(json!({"permissions": allow, "agents": {"build": {"permissions": allow}}}));
    let mut h = c.spawn(MODEL, false, None);
    let run = "Use the shell tool to run exactly: touch allowed.txt . Then reply DONE.";
    let evs = c.turn(&mut h, run).await;
    assert!(
        requests(&evs).iter().any(|(name, input)| *name == "Bash"
            && input["command"]
                .as_str()
                .is_some_and(|c| c.contains("allowed.txt"))),
        "{evs:?}"
    );
    assert!(c.path("allowed.txt").exists());

    let run = "Use the shell tool to run exactly: touch denied.txt . If it is refused, stop.";
    let evs = c.turn_answering(&mut h, run, false).await;
    assert!(!requests(&evs).is_empty(), "{evs:?}");
    assert_eq!(turns(&evs), [(false, true)], "a refusal ends the turn");
    assert!(results(&evs).iter().any(|r| r.1), "{evs:?}");
    assert!(!c.path("denied.txt").exists());
    // The process takes the next prompt.
    c.turn(&mut h, "Reply with exactly: still here").await;
    c.shutdown(&mut h).await;
}

/// A custom default agent with its own `allow` would never ask: Auto OFF leaves it for `build`.
#[tokio::test]
#[ignore = "spends quota; set XADE_E2E_OPENCODE=1"]
async fn auto_off_asks_even_with_a_permissive_default_agent() {
    e2e!();
    let c = Ctx::new().await;
    c.project_config(json!({
        "default_agent": "yolo",
        "agents": {"yolo": {
            "mode": "primary",
            "description": "runs anything",
            "permissions": [{"action": "shell", "resource": "*", "effect": "allow"}],
        }},
    }));
    let mut h = c.spawn(MODEL, false, None);
    let evs = c
        .turn(
            &mut h,
            "Use the shell tool to run exactly: touch asked.txt . Then reply DONE.",
        )
        .await;
    assert!(!requests(&evs).is_empty(), "{evs:?}");
    c.shutdown(&mut h).await;
}

/// Auto ON leaves the user's rules alone: a `deny` stays a `deny`, the rest runs unasked.
#[tokio::test]
#[ignore = "spends quota; set XADE_E2E_OPENCODE=1"]
async fn auto_on_never_weakens_a_deny() {
    e2e!();
    let c = Ctx::new().await;
    c.project_config(json!({"permissions": [
        {"action": "shell", "resource": "touch forbidden*", "effect": "deny"},
    ]}));
    let mut h = c.spawn(MODEL, true, None);
    let evs = c
        .turn_answering(
            &mut h,
            "Use the shell tool to run exactly `touch fine.txt`, then as a second call exactly \
             `touch forbidden.txt`. Do not work around a refusal. Then reply DONE.",
            true,
        )
        .await;
    assert!(requests(&evs).is_empty(), "{evs:?}");
    assert!(c.path("fine.txt").exists(), "{evs:?}");
    assert!(!c.path("forbidden.txt").exists(), "{evs:?}");
    assert!(results(&evs).iter().any(|r| r.1), "{evs:?}");
    c.shutdown(&mut h).await;
}

#[tokio::test]
#[ignore = "spends quota; set XADE_E2E_OPENCODE=1"]
async fn writes_and_edits_are_reviewable() {
    e2e!();
    let c = Ctx::new().await;
    std::fs::write(c.path("notes.txt"), "alpha\n").expect("seed");
    let mut h = c.spawn(MODEL, false, None);
    let evs = c
        .turn(
            &mut h,
            "Without the shell: use the write tool to overwrite notes.txt with the single line \
             beta, then use the edit tool to replace beta with gamma. Then reply DONE.",
        )
        .await;
    let names: Vec<&str> = requests(&evs).iter().map(|r| r.0).collect();
    assert!(
        names.contains(&"Write") && names.contains(&"Edit"),
        "{evs:?}"
    );
    assert!(
        results(&evs).iter().any(|r| r
            .2
            .as_ref()
            .is_some_and(|s| { s["type"] == "update" && s["originalFile"] == "alpha\n" })),
        "{evs:?}"
    );
    let text = std::fs::read_to_string(c.path("notes.txt")).expect("notes");
    assert!(text.contains("gamma"), "{text}");
    c.shutdown(&mut h).await;
}

#[tokio::test]
#[ignore = "spends quota; set XADE_E2E_OPENCODE=1"]
async fn a_patch_is_reviewed_with_every_file_it_touches() {
    e2e!();
    let c = Ctx::new().await;
    std::fs::write(c.path("old.txt"), "bye\n").expect("seed");
    let mut h = c.spawn(PATCH_MODEL, false, None);
    let evs = c
        .turn(
            &mut h,
            "Use only your patch tool, in one single patch call: add new.txt containing the line \
             hello and delete old.txt. Then reply DONE.",
        )
        .await;
    let (name, input) = requests(&evs).remove(0);
    assert_eq!(name, "Edit", "{evs:?}");
    let shown = input["structuredPatch"].to_string();
    assert!(
        shown.contains("new.txt") && shown.contains("old.txt"),
        "{shown}"
    );
    assert!(c.path("new.txt").exists() && !c.path("old.txt").exists());
    c.shutdown(&mut h).await;
}

#[tokio::test]
#[ignore = "spends quota; set XADE_E2E_OPENCODE=1"]
async fn a_steer_joins_the_running_turn() {
    e2e!();
    let c = Ctx::new().await;
    let mut h = c.spawn(MODEL, true, None);
    prompt(
        &h,
        "Use the shell tool to run `sleep 5`, then write one sentence about a lighthouse.",
    );
    c.until(&mut h, |e| matches!(e, ProviderEvent::ToolUse { .. }))
        .await;
    prompt(&h, "Also end your answer with the word BANANA.");
    let evs = c.until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, false)], "{evs:?}");
    assert!(said(&evs).contains("banana"), "{evs:?}");
    c.shutdown(&mut h).await;
}

#[tokio::test]
#[ignore = "spends quota; set XADE_E2E_OPENCODE=1"]
async fn stop_mid_tool_kills_it_and_the_next_prompt_works() {
    e2e!();
    let c = Ctx::new().await;
    let mut h = c.spawn(MODEL, true, None);
    prompt(
        &h,
        "Use the shell tool to run exactly `sleep 91` in the foreground. Do nothing else.",
    );
    c.until(&mut h, |e| matches!(e, ProviderEvent::ToolUse { .. }))
        .await;
    let sleeping = || -> Vec<(u32, String)> {
        descendants()
            .into_iter()
            .filter(|(_, cmd)| cmd == "sleep 91")
            .collect()
    };
    let start = Instant::now();
    while sleeping().is_empty() {
        assert!(start.elapsed() < Duration::from_secs(30), "never ran");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let tool = sleeping();
    send(&h, SessionCommand::Interrupt);
    let evs = c.until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)], "{evs:?}");
    gone(&tool);
    let evs = c.turn(&mut h, "Reply with exactly: still alive").await;
    assert!(said(&evs).contains("still alive"), "{evs:?}");
    c.shutdown(&mut h).await;
}

#[tokio::test]
#[ignore = "spends quota; set XADE_E2E_OPENCODE=1"]
async fn a_new_process_resumes_until_the_session_is_deleted() {
    e2e!();
    let c = Ctx::new().await;
    let mut h = c.spawn(MODEL, false, None);
    let evs = c
        .turn(&mut h, "Remember the word persimmon4. Reply with just OK.")
        .await;
    let Some(ProviderEvent::Started {
        provider_session_id: sid,
        ..
    }) = evs.first()
    else {
        panic!("no Started: {evs:?}");
    };
    let sid = sid.clone();
    c.shutdown(&mut h).await;

    let mut h = c.spawn(MODEL, false, Some(&sid));
    let evs = c
        .turn(
            &mut h,
            "What word did I ask you to remember? Reply with just the word.",
        )
        .await;
    assert!(said(&evs).contains("persimmon4"), "{evs:?}");
    c.shutdown(&mut h).await;

    c.provider.delete_session(&sid).expect("delete");
    c.provider.delete_session(&sid).expect("already gone is Ok");
    let mut h = c.spawn(MODEL, false, Some(&sid));
    prompt(&h, "hello");
    let evs = c
        .until(&mut h, |e| matches!(e, ProviderEvent::Exited { .. }))
        .await;
    assert!(
        evs.iter().any(|e| matches!(
            e,
            ProviderEvent::Error(m) if m.starts_with("Could not resume")
        )),
        "{evs:?}"
    );
}

/// A background subagent reports back after the prompt ended; that turn also ends.
#[tokio::test]
#[ignore = "spends quota; set XADE_E2E_OPENCODE=1"]
async fn a_background_subagents_report_is_a_turn_that_ends() {
    e2e!();
    let c = Ctx::new().await;
    let mut h = c.spawn(MODEL, true, None);
    c.turn(
        &mut h,
        "Start a subagent (agent 'general') in the background whose task is to run the shell \
         command `sleep 8; echo bgmark`. Do NOT wait for it: immediately reply with the single \
         word started and end your turn.",
    )
    .await;
    let mut evs = Vec::new();
    while !said(&evs).contains("bgmark") {
        evs.extend(c.until(&mut h, is_turn).await);
    }
    assert!(turns(&evs).iter().all(|t| *t == (false, false)), "{evs:?}");
    c.shutdown(&mut h).await;
}

#[tokio::test]
#[ignore = "spends quota; set XADE_E2E_OPENCODE=1"]
async fn shutdown_leaves_none_of_our_processes() {
    e2e!();
    let c = Ctx::new().await;
    assert!(descendants().is_empty(), "{:?}", descendants());
    let mut h = c.spawn(MODEL, false, None);
    c.turn(&mut h, "Reply with exactly: hi").await;
    let ours = descendants();
    assert!(
        ours.iter().any(|(_, cmd)| cmd.ends_with(" acp")),
        "{ours:?}"
    );
    c.shutdown(&mut h).await;
    gone(&ours);

    // And mid-turn, with a tool running.
    let mut h = c.spawn(MODEL, true, None);
    prompt(
        &h,
        "Use the shell tool to run exactly `sleep 92` in the foreground. Do nothing else.",
    );
    c.until(&mut h, |e| matches!(e, ProviderEvent::ToolUse { .. }))
        .await;
    let start = Instant::now();
    while !descendants().iter().any(|(_, cmd)| cmd == "sleep 92") {
        assert!(start.elapsed() < Duration::from_secs(30), "never ran");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let ours = descendants();
    c.shutdown(&mut h).await;
    gone(&ours);
}

/// An empty folder never prompted in leaves no session behind.
#[tokio::test]
#[ignore = "needs opencode; set XADE_E2E_OPENCODE=1"]
async fn no_prompt_creates_no_session() {
    e2e!();
    let c = Ctx::new().await;
    let mut h = c.spawn(MODEL, false, None);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let evs = c.shutdown(&mut h).await;
    assert_eq!(evs.len(), 1, "{evs:?}");
    assert!(sessions_in(c.repo.path()).is_empty());
}

/// The ids `opencode session list` shows for a folder.
fn sessions_in(dir: &Path) -> Vec<Value> {
    let out = Command::new("opencode")
        .args(["session", "list", "--standalone", "--format", "json"])
        .current_dir(dir)
        .output()
        .expect("opencode session list");
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    match text.trim() {
        "" => Vec::new(),
        json => serde_json::from_str(json).expect("json"),
    }
}
