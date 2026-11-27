//! Drives `CodexProvider` against a fake `codex` (tests/fake_codex.py) that replays a
//! recorded fixture (`fixtures/`) or a synthetic one (`tests/fake/`). No network, no tokens.

#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::time::timeout;
use xade_codex::CodexProvider;
use xade_core::{Effort, ProviderError, ProviderEvent, SessionCommand, SessionHandle, SpawnConfig};

const WAIT: Duration = Duration::from_secs(20);

struct Fake {
    _dir: tempfile::TempDir,
    log: PathBuf,
    provider: CodexProvider,
}

fn fake_with_path(path: String) -> Fake {
    let root = env!("CARGO_MANIFEST_DIR");
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("in.log");
    let bin = dir.path().join("codex");
    std::fs::write(dir.path().join("fixture"), path).expect("fixture path");
    std::os::unix::fs::symlink(format!("{root}/tests/fake_codex.py"), &bin).expect("symlink");
    Fake {
        _dir: dir,
        log,
        provider: CodexProvider::with_binary(bin),
    }
}

/// `fixture` is a recorded one (`fixtures/<name>`) or, with a `fake/` prefix, a synthetic one.
fn fake(fixture: &str) -> Fake {
    let root = env!("CARGO_MANIFEST_DIR");
    let path = match fixture.strip_prefix("fake/") {
        Some(name) => format!("{root}/tests/fake/{name}.ndjson"),
        None => format!("{root}/fixtures/{fixture}.ndjson"),
    };
    fake_with_path(path)
}

/// Takes only the lines after `meta {"restart": true}` from a two-process fixture (e.g. `resume`).
fn fake_after_restart(fixture: &str) -> Fake {
    let root = env!("CARGO_MANIFEST_DIR");
    let full =
        std::fs::read_to_string(format!("{root}/fixtures/{fixture}.ndjson")).expect("fixture");
    let after: Vec<&str> = full
        .lines()
        .skip_while(|l| !l.contains(r#""restart": true"#) && !l.contains(r#""restart":true"#))
        .skip(1)
        .collect();
    assert!(!after.is_empty(), "{fixture} has no restart marker");
    let dir = tempfile::tempdir().expect("tempdir");
    let slice = dir.path().join("slice.ndjson");
    std::fs::write(&slice, after.join("\n") + "\n").expect("write slice");
    let log = dir.path().join("in.log");
    let bin = dir.path().join("codex");
    std::fs::write(dir.path().join("fixture"), slice.to_string_lossy().as_ref())
        .expect("fixture path");
    std::os::unix::fs::symlink(format!("{root}/tests/fake_codex.py"), &bin).expect("symlink");
    Fake {
        _dir: dir,
        log,
        provider: CodexProvider::with_binary(bin),
    }
}

impl Fake {
    fn spawn(
        &self,
        model: &str,
        effort: Effort,
        auto: bool,
        resume: Option<&str>,
    ) -> SessionHandle {
        self.provider
            .spawn(SpawnConfig {
                cwd: std::env::temp_dir(),
                model_id: model.into(),
                effort,
                auto_mode: auto,
                resume: resume.map(String::from),
            })
            .expect("spawn")
    }

    fn sent(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("logged line"))
            .collect()
    }

    fn methods(&self) -> Vec<String> {
        self.sent()
            .iter()
            .filter_map(|f| f["method"].as_str().map(String::from))
            .collect()
    }
}

async fn until(h: &mut SessionHandle, stop: impl Fn(&ProviderEvent) -> bool) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    timeout(WAIT, async {
        while let Some(ev) = h.events.recv().await {
            let done = stop(&ev);
            out.push(ev);
            if done {
                return;
            }
        }
        panic!("event channel closed; got {out:?}");
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; got {out:?}"));
    out
}

fn is_turn(e: &ProviderEvent) -> bool {
    matches!(e, ProviderEvent::TurnComplete { .. })
}

fn is_exit(e: &ProviderEvent) -> bool {
    matches!(e, ProviderEvent::Exited { .. })
}

fn send(h: &SessionHandle, c: SessionCommand) {
    h.commands.send(c).expect("send");
}

fn prompt(h: &SessionHandle, text: &str) {
    send(h, SessionCommand::SendPrompt(text.into()));
}

async fn shutdown(h: &mut SessionHandle) -> Vec<ProviderEvent> {
    send(h, SessionCommand::Shutdown);
    until(h, is_exit).await
}

fn errors(evs: &[ProviderEvent]) -> Vec<&str> {
    evs.iter()
        .filter_map(|e| match e {
            ProviderEvent::Error(m) => Some(m.as_str()),
            _ => None,
        })
        .collect()
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

async fn wait_for_log(f: &Fake, method: &str) {
    let start = Instant::now();
    while !f.methods().iter().any(|m| m == method) {
        assert!(start.elapsed() < WAIT, "{method} never sent");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn start_and_turn_sends_handshake_then_turn_start() {
    let f = fake("simple_turn");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    prompt(&h, "hello");
    let mut evs = until(&mut h, is_turn).await;
    evs.extend(shutdown(&mut h).await);

    assert!(matches!(
        &evs[0],
        ProviderEvent::Started { provider_session_id, model_id }
            if provider_session_id.len() == 36 && model_id == "gpt-6-luna"
    ));
    assert!(evs.iter().any(|e| matches!(e, ProviderEvent::TextDelta(_))));
    assert!(evs
        .iter()
        .any(|e| matches!(e, ProviderEvent::AssistantText { text } if text == "pong")));
    assert_eq!(turns(&evs), [(false, false)]);
    assert!(matches!(
        evs.last(),
        Some(ProviderEvent::Exited { code: Some(0) })
    ));
    assert_eq!(
        f.methods(),
        ["initialize", "initialized", "thread/start", "turn/start"]
    );
    let sent = f.sent();
    assert_eq!(sent[2]["params"]["approvalPolicy"], "untrusted");
    assert_eq!(sent[2]["params"]["sandbox"], "workspace-write");
    assert_eq!(sent[3]["params"]["effort"], "low");
    assert_eq!(sent[3]["params"]["summary"], "concise");
    assert_eq!(sent[3]["params"]["approvalPolicy"], "untrusted");
    assert_eq!(sent[3]["params"]["input"][0]["text"], "hello");
}

#[tokio::test]
async fn effort_default_omits_effort_field_in_turn_start() {
    let f = fake("simple_turn");
    let mut h = f.spawn("gpt-6-luna", Effort::Default, true, None);
    prompt(&h, "hello");
    until(&mut h, is_turn).await;
    shutdown(&mut h).await;
    let sent = f.sent();
    assert_eq!(sent[2]["params"]["approvalPolicy"], "never");
    assert!(sent[3]["params"].get("effort").is_none());
    assert_eq!(sent[3]["params"]["approvalPolicy"], "never");
}

#[tokio::test]
async fn no_prompt_creates_no_thread() {
    let f = fake("simple_turn");
    let mut h = f.spawn("gpt-6-luna", Effort::Default, false, None);
    wait_for_log(&f, "initialized").await;
    let evs = shutdown(&mut h).await;
    assert_eq!(evs.len(), 1);
    assert!(matches!(evs[0], ProviderEvent::Exited { code: Some(0) }));
    assert_eq!(f.methods(), ["initialize", "initialized"]);
}

#[tokio::test]
async fn approval_allow_replies_with_numeric_id_and_accept() {
    let f = fake("tool_allow");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    prompt(&h, "run it");
    let evs = until(&mut h, |e| {
        matches!(e, ProviderEvent::PermissionRequest { .. })
    })
    .await;
    let Some(ProviderEvent::PermissionRequest {
        request_id,
        tool_name,
        ..
    }) = evs.last()
    else {
        panic!("no permission request");
    };
    assert_eq!((request_id.as_str(), tool_name.as_str()), ("0", "Bash"));
    send(
        &h,
        SessionCommand::RespondPermission {
            request_id: "0".into(),
            allow: true,
        },
    );
    let rest = until(&mut h, is_turn).await;
    assert!(rest.iter().any(|e| matches!(
        e,
        ProviderEvent::ToolResult {
            is_error: false,
            ..
        }
    )));
    shutdown(&mut h).await;
    let reply = f
        .sent()
        .into_iter()
        .find(|m| m["id"] == 0 && m.get("method").is_none())
        .expect("reply");
    assert_eq!(reply["result"]["decision"], "accept");
}

#[tokio::test]
async fn approval_deny_replies_decline_and_completes_turn() {
    let f = fake("tool_deny");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    prompt(&h, "run it");
    until(&mut h, |e| {
        matches!(e, ProviderEvent::PermissionRequest { .. })
    })
    .await;
    send(
        &h,
        SessionCommand::RespondPermission {
            request_id: "0".into(),
            allow: false,
        },
    );
    let rest = until(&mut h, is_turn).await;
    assert!(rest
        .iter()
        .any(|e| matches!(e, ProviderEvent::ToolResult { is_error: true, .. })));
    assert_eq!(turns(&rest), [(false, false)]);
    shutdown(&mut h).await;
    let reply = f
        .sent()
        .into_iter()
        .find(|m| m["id"] == 0 && m.get("method").is_none())
        .expect("reply");
    assert_eq!(reply["result"]["decision"], "decline");
}

#[tokio::test]
async fn file_change_approval_works_and_auto_toggle_accepts_pending() {
    let f = fake("edit");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    prompt(&h, "create hello.txt");
    let evs = until(&mut h, |e| {
        matches!(e, ProviderEvent::PermissionRequest { .. })
    })
    .await;
    assert!(matches!(
        evs.last(),
        Some(ProviderEvent::PermissionRequest { tool_name, .. }) if tool_name == "Write"
    ));
    // Toggling Auto ON while an approval is pending accepts it immediately.
    send(&h, SessionCommand::SetAutoMode(true));
    let rest = until(&mut h, is_turn).await;
    assert_eq!(turns(&rest), [(false, false)]);
    shutdown(&mut h).await;
    let reply = f
        .sent()
        .into_iter()
        .find(|m| m["id"] == 0 && m.get("method").is_none())
        .expect("reply");
    assert_eq!(reply["result"]["decision"], "accept");
}

#[tokio::test]
async fn interrupt_mid_stream_sends_turn_interrupt_and_keeps_text() {
    let f = fake("interrupt");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    prompt(&h, "write essay");
    until(&mut h, |e| matches!(e, ProviderEvent::TextDelta(_))).await;
    send(&h, SessionCommand::Interrupt);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    assert!(evs
        .iter()
        .any(|e| matches!(e, ProviderEvent::AssistantText { text } if text.starts_with('#'))));
    shutdown(&mut h).await;
    assert!(f.methods().iter().any(|m| m == "turn/interrupt"));
}

#[tokio::test]
async fn a_prompt_right_after_stop_never_steers_the_dying_turn() {
    let f = fake("interrupt");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    prompt(&h, "write essay");
    until(&mut h, |e| matches!(e, ProviderEvent::TextDelta(_))).await;
    send(&h, SessionCommand::Interrupt);
    prompt(&h, "next question");
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    shutdown(&mut h).await;
    assert!(
        !f.methods().iter().any(|m| m == "turn/steer"),
        "{:?}",
        f.methods()
    );
}

#[tokio::test]
async fn interrupt_with_pending_permission_sends_only_turn_interrupt() {
    let f = fake("interrupt_pending_permission");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    prompt(&h, "run it");
    until(&mut h, |e| {
        matches!(e, ProviderEvent::PermissionRequest { .. })
    })
    .await;
    send(&h, SessionCommand::Interrupt);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    shutdown(&mut h).await;
    assert_eq!(
        f.methods(),
        [
            "initialize",
            "initialized",
            "thread/start",
            "turn/start",
            "turn/interrupt"
        ]
    );
    // No approval reply was sent for id 0.
    assert!(!f
        .sent()
        .iter()
        .any(|m| m["id"] == 0 && m.get("method").is_none()));
}

#[tokio::test]
async fn interrupt_before_thread_exists_completes_without_creating_thread() {
    let f = fake("fake/early_interrupt");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    prompt(&h, "hello");
    send(&h, SessionCommand::Interrupt);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    shutdown(&mut h).await;
    assert!(!f.methods().iter().any(|m| m == "thread/start"));
}

#[tokio::test]
async fn interrupt_before_turn_start_returns_cancels_once_turn_id_arrives() {
    let f = fake("fake/interrupt_during_turn_start");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    prompt(&h, "hello");
    wait_for_log(&f, "turn/start").await;
    send(&h, SessionCommand::Interrupt);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    shutdown(&mut h).await;
    assert_eq!(
        f.methods(),
        [
            "initialize",
            "initialized",
            "thread/start",
            "turn/start",
            "turn/interrupt"
        ]
    );
}

#[tokio::test]
async fn approval_arriving_after_stop_is_declined_and_never_shown() {
    let f = fake("fake/late_permission");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    prompt(&h, "run");
    wait_for_log(&f, "turn/start").await;
    // Wait briefly so turn/started sets active_turn_id before Interrupt.
    tokio::time::sleep(Duration::from_millis(100)).await;
    send(&h, SessionCommand::Interrupt);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    assert!(!evs
        .iter()
        .any(|e| matches!(e, ProviderEvent::PermissionRequest { .. })));
    shutdown(&mut h).await;
    let reply = f
        .sent()
        .into_iter()
        .find(|m| m["id"] == 7 && m.get("method").is_none())
        .expect("late reply");
    assert_eq!(reply["result"]["decision"], "decline");
}

#[tokio::test]
async fn steer_sends_turn_steer_and_ends_with_one_turn_complete() {
    let f = fake("steer");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    prompt(&h, "count");
    until(&mut h, |e| matches!(e, ProviderEvent::TextDelta(_))).await;
    prompt(&h, "stop and say steered");
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, false)]);
    assert!(evs
        .iter()
        .any(|e| matches!(e, ProviderEvent::AssistantText { text } if text == "steered")));
    shutdown(&mut h).await;
    assert_eq!(
        f.methods(),
        [
            "initialize",
            "initialized",
            "thread/start",
            "turn/start",
            "turn/steer"
        ]
    );
}

#[tokio::test]
async fn steer_error_falls_back_to_a_fresh_turn_start() {
    let f = fake("fake/steer_fallback");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    prompt(&h, "first");
    until(&mut h, |e| matches!(e, ProviderEvent::TextDelta(_))).await;
    prompt(&h, "second");
    let evs1 = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs1), [(false, false)]);
    let evs2 = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs2), [(false, false)]);
    assert!(evs2
        .iter()
        .any(|e| matches!(e, ProviderEvent::AssistantText { text } if text == "two")));
    shutdown(&mut h).await;
    assert_eq!(
        f.methods(),
        [
            "initialize",
            "initialized",
            "thread/start",
            "turn/start",
            "turn/steer",
            "turn/start"
        ]
    );
}

#[tokio::test]
async fn resume_sends_thread_resume_immediately_with_exclude_turns() {
    let f = fake_after_restart("resume");
    let mut h = f.spawn(
        "gpt-6-luna",
        Effort::Low,
        false,
        Some("01a21c06-7939-72e2-83a4-242afac400f4"),
    );
    until(&mut h, |e| matches!(e, ProviderEvent::Started { .. })).await;
    prompt(&h, "which word?");
    let evs = until(&mut h, is_turn).await;
    assert!(evs
        .iter()
        .any(|e| matches!(e, ProviderEvent::AssistantText { text } if text == "walrus")));
    shutdown(&mut h).await;
    assert_eq!(
        f.methods(),
        ["initialize", "initialized", "thread/resume", "turn/start"]
    );
    let sent = f.sent();
    assert_eq!(sent[2]["params"]["excludeTurns"], true);
    assert_eq!(
        sent[2]["params"]["threadId"],
        "01a21c06-7939-72e2-83a4-242afac400f4"
    );
}

#[tokio::test]
async fn bad_resume_reports_error_and_exits() {
    let f = fake("bad_resume");
    let mut h = f.spawn(
        "gpt-6-luna",
        Effort::Low,
        false,
        Some("00000000-0000-0000-0000-000000000000"),
    );
    prompt(&h, "hello");
    let evs = until(&mut h, is_exit).await;
    let errs = errors(&evs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].starts_with("Could not resume the Codex thread:"));
    assert_eq!(turns(&evs), [(true, false)]);
    assert_eq!(f.methods(), ["initialize", "initialized", "thread/resume"]);
}

#[tokio::test]
async fn unsupported_server_request_gets_32601() {
    let f = fake("fake/unknown_request");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    prompt(&h, "hello");
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, false)]);
    shutdown(&mut h).await;
    let reply = f
        .sent()
        .into_iter()
        .find(|m| m["id"] == 99 && m.get("method").is_none())
        .expect("reply to 99");
    assert_eq!(reply["error"]["code"], -32601);
}

#[tokio::test]
async fn crash_mid_turn_emits_error_turn_complete_and_exited() {
    let f = fake("fake/crash_mid_turn");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    prompt(&h, "hello");
    let evs = until(&mut h, is_exit).await;
    assert!(evs
        .iter()
        .any(|e| matches!(e, ProviderEvent::AssistantText { text } if text == "partial")));
    assert!(errors(&evs).iter().any(|m| m.contains("boom")));
    assert_eq!(turns(&evs), [(true, false)]);
    assert!(matches!(
        evs.last(),
        Some(ProviderEvent::Exited { code: Some(3) })
    ));
}

#[tokio::test]
async fn hanging_child_is_terminated_within_five_seconds() {
    let f = fake("fake/hang");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    wait_for_log(&f, "initialized").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let start = Instant::now();
    let evs = shutdown(&mut h).await;
    assert!(start.elapsed() >= Duration::from_secs(3));
    assert!(start.elapsed() < Duration::from_secs(7));
    assert!(errors(&evs).is_empty());
    assert!(matches!(
        evs.last(),
        Some(ProviderEvent::Exited { code: None })
    ));
}

fn process_gone(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let stat = String::from_utf8_lossy(&out.stdout);
    stat.trim().is_empty() || stat.trim().starts_with('Z')
}

async fn background_pid(f: &Fake) -> u32 {
    let path = f.log.with_file_name("bg.pid");
    let start = Instant::now();
    loop {
        if let Some(pid) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            return pid;
        }
        assert!(start.elapsed() < WAIT, "the fake never started its sleep");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn assert_reaped_soon(pid: u32) {
    let start = Instant::now();
    while !process_gone(pid) {
        if start.elapsed() > Duration::from_secs(5) {
            // SAFETY: plain syscall on a pid the fake started.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            panic!("process {pid} outlived the session");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn clean_shutdown_kills_background_processes_in_child_group() {
    let f = fake("fake/orphan_clean");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    let pid = background_pid(&f).await;
    assert!(!process_gone(pid));
    shutdown(&mut h).await;
    assert_reaped_soon(pid).await;
}

#[tokio::test]
async fn unexpected_exit_kills_background_processes_in_child_group() {
    let f = fake("fake/orphan_crash");
    let mut h = f.spawn("gpt-6-luna", Effort::Low, false, None);
    let pid = background_pid(&f).await;
    until(&mut h, is_exit).await;
    assert_reaped_soon(pid).await;
}

#[test]
fn preflight_populates_models_default_and_account_label() {
    let f = fake("fake/preflight_ok");
    assert_eq!(f.provider.preflight(), Ok(()));
    assert_eq!(f.provider.account_label().as_deref(), Some("API key"));
    assert_eq!(f.provider.default_model(), "gpt-6.2-sol");
    let ids: Vec<_> = f.provider.models().into_iter().map(|m| m.id).collect();
    assert_eq!(ids, ["gpt-6.2-sol", "gpt-5.5"]);
    assert_eq!(
        f.methods(),
        ["initialize", "initialized", "account/read", "model/list"]
    );
}

#[test]
fn preflight_returns_not_logged_in_when_account_is_null() {
    let f = fake("fake/preflight_not_logged_in");
    assert_eq!(f.provider.preflight(), Err(ProviderError::NotLoggedIn));
    assert_eq!(f.provider.account_label(), None);
}

#[test]
fn delete_session_waits_for_response_and_treats_no_rollout_as_ok() {
    let ok = fake("fake/delete_ok");
    assert_eq!(ok.provider.delete_session("t1"), Ok(()));
    assert_eq!(ok.methods(), ["initialize", "initialized", "thread/delete"]);

    let missing = fake("fake/delete_missing");
    assert_eq!(missing.provider.delete_session("missing"), Ok(()));
}

#[test]
fn quota_returns_empty_on_api_key_error_and_groups_on_success() {
    let err = fake("fake/quota_api_key");
    assert_eq!(err.provider.quota(), Ok(vec![]));
    assert_eq!(
        err.methods(),
        ["initialize", "initialized", "account/rateLimits/read"]
    );

    let ok = fake("fake/quota_ok");
    let groups = ok.provider.quota().expect("quota");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].name, "");
    assert_eq!(groups[0].windows.len(), 2);
    assert_eq!(groups[0].windows[0].utilization_pct, 35.0);
    assert_eq!(groups[0].windows[1].utilization_pct, 12.0);
}

#[test]
fn import_sessions_pages_thread_list_and_skips_xade_ephemeral_and_subagents() {
    let f = fake("fake/import_pages");
    let imported = f.provider.import_sessions();
    assert_eq!(imported.len(), 2);
    assert_eq!(imported[0].provider_session_id, "t-keep-1");
    assert_eq!(imported[0].title, "First kept");
    assert_eq!(imported[0].model_id, "gpt-6.2-sol");
    assert_eq!(imported[0].messages.len(), 2);

    assert_eq!(imported[1].provider_session_id, "t-keep-2");
    assert_eq!(imported[1].title, "second kept prompt");
    assert_eq!(imported[1].model_id, "gpt-6-luna");
    assert_eq!(imported[1].messages.len(), 2);

    assert_eq!(
        f.methods(),
        [
            "initialize",
            "initialized",
            "thread/list",
            "thread/list",
            "thread/read",
            "thread/read",
        ]
    );
    let sent = f.sent();
    assert_eq!(
        sent[2]["params"]["sourceKinds"],
        serde_json::json!(["cli", "vscode", "exec", "appServer"])
    );
    assert!(sent[2]["params"].get("cursor").is_none());
    assert_eq!(sent[3]["params"]["cursor"], "page-2");
    assert_eq!(sent[4]["params"]["threadId"], "t-keep-1");
    assert_eq!(sent[4]["params"]["includeTurns"], true);
    assert_eq!(sent[5]["params"]["threadId"], "t-keep-2");
    assert_eq!(sent[5]["params"]["includeTurns"], true);
}
