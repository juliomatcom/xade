//! Drives `AntigravityProvider::spawn` against a fake `agy` (tests/fake_agy.py) that replays a
//! recorded fixture (`fixtures/`) or a small synthetic one (`tests/fake/`). No network, no quota.

#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::time::timeout;
use xade_antigravity::AntigravityProvider;
use xade_core::{AgentProvider, Effort, ProviderEvent, SessionCommand, SessionHandle, SpawnConfig};

const WAIT: Duration = Duration::from_secs(20);

struct Fake {
    _dir: tempfile::TempDir,
    log: PathBuf,
    provider: AntigravityProvider,
}

/// `fixture` is a recorded one (`fixtures/<name>`) or, with a `fake/` prefix, a synthetic one.
fn fake(fixture: &str) -> Fake {
    let root = env!("CARGO_MANIFEST_DIR");
    let path = match fixture.strip_prefix("fake/") {
        Some(name) => format!("{root}/tests/fake/{name}.ndjson"),
        None => format!("{root}/fixtures/{fixture}.ndjson"),
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("in.log");
    let bin = dir.path().join("agy");
    // Never write an executable here: another test thread may fork while its file is open for
    // writing, and exec then fails with "text file busy". Link the committed stub instead.
    std::fs::write(dir.path().join("fixture"), path).expect("fixture path");
    std::os::unix::fs::symlink(format!("{root}/tests/fake_agy.py"), &bin).expect("symlink");
    Fake {
        _dir: dir,
        log,
        provider: AntigravityProvider::with_binary(bin),
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

    /// Frames the client sent, in order.
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

/// Events up to and including the first that satisfies `stop`.
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

/// Shuts down and returns the events up to `Exited`.
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
async fn a_prompt_queued_during_the_handshake_runs_in_order() {
    let f = fake("simple_turn");
    let mut h = f.spawn("gemini-3.8-flash", Effort::Default, false, None);
    prompt(&h, "hello");
    let mut evs = until(&mut h, is_turn).await;
    evs.extend(shutdown(&mut h).await);

    assert!(matches!(
        &evs[0],
        ProviderEvent::Started { provider_session_id, model_id }
            if provider_session_id.len() == 36 && model_id == "gemini-3.8-flash"
    ));
    assert!(evs.iter().any(|e| matches!(e, ProviderEvent::TextDelta(_))));
    assert!(evs
        .iter()
        .any(|e| matches!(e, ProviderEvent::AssistantText { .. })));
    assert_eq!(turns(&evs), [(false, false)]);
    assert!(matches!(
        evs.last(),
        Some(ProviderEvent::Exited { code: Some(0) })
    ));
    assert_eq!(
        f.methods(),
        [
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/prompt"
        ]
    );
    let sent = f.sent();
    assert_eq!(sent[0]["params"]["protocolVersion"], 1);
    assert_eq!(sent[2]["params"]["configId"], "model");
    assert_eq!(sent[3]["params"]["prompt"][0]["text"], "hello");
}

#[tokio::test]
async fn no_prompt_means_no_conversation() {
    let f = fake("simple_turn");
    let mut h = f.spawn("gemini-3.8-flash", Effort::Default, false, None);
    let evs = shutdown(&mut h).await;
    assert_eq!(evs.len(), 1);
    assert!(matches!(evs[0], ProviderEvent::Exited { code: Some(0) }));
    assert_eq!(f.methods(), ["initialize"], "session/new is lazy");
}

#[tokio::test]
async fn model_then_thinking_level_are_set_before_the_prompt() {
    let f = fake("set_config");
    let mut h = f.spawn("gemini-3.8-flash", Effort::Low, false, None);
    prompt(&h, "x");
    until(&mut h, |e| matches!(e, ProviderEvent::Started { .. })).await;
    wait_for_log(&f, "session/prompt").await;
    shutdown(&mut h).await;
    let sent = f.sent();
    let ids: Vec<_> = sent
        .iter()
        .filter(|f| f["method"] == "session/set_config_option")
        .map(|f| {
            (
                f["params"]["configId"].clone(),
                f["params"]["value"].clone(),
            )
        })
        .collect();
    assert_eq!(
        ids,
        [
            ("model".into(), "gemini-3.8-flash".into()),
            ("thought_level".into(), "low".into())
        ]
    );
    assert_eq!(
        f.methods().last().map(String::as_str),
        Some("session/prompt"),
        "the prompt waits for the config answers"
    );
}

#[tokio::test]
async fn interrupt_before_the_session_is_ready_completes_the_turn() {
    let f = fake("fake/early_interrupt");
    let mut h = f.spawn("", Effort::Default, false, None);
    prompt(&h, "hello");
    send(&h, SessionCommand::Interrupt);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    let rest = shutdown(&mut h).await;
    assert!(errors(&rest).is_empty());
    assert_eq!(
        f.methods(),
        ["initialize"],
        "the dropped prompt never created a session"
    );
}

#[tokio::test]
async fn an_auth_error_on_new_session_is_reported_then_exited() {
    let f = fake("fake/auth_error");
    let mut h = f.spawn("", Effort::Default, false, None);
    prompt(&h, "hello");
    let evs = until(&mut h, is_exit).await;
    assert_eq!(errors(&evs).len(), 1);
    assert!(errors(&evs)[0].contains("Run `agy` in a terminal once to sign in"));
    assert_eq!(
        turns(&evs),
        [(true, false)],
        "the queued prompt fails as a turn"
    );
    assert!(matches!(evs.last(), Some(ProviderEvent::Exited { .. })));
    assert!(!evs
        .iter()
        .any(|e| matches!(e, ProviderEvent::Started { .. })));
}

#[tokio::test]
async fn resuming_an_unknown_conversation_is_a_clear_error() {
    let f = fake("bad_resume");
    let mut h = f.spawn(
        "",
        Effort::Default,
        false,
        Some("33d6d48a-d458-40f8-9917-8bee5fc305b4"),
    );
    prompt(&h, "hello");
    let evs = until(&mut h, is_exit).await;
    let errs = errors(&evs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].starts_with("Could not resume the Antigravity conversation:"));
    assert_eq!(turns(&evs), [(true, false)]);
    assert_eq!(f.methods(), ["initialize", "session/resume"]);
}

#[tokio::test]
async fn an_unsupported_protocol_version_fails_the_session() {
    let f = fake("fake/bad_version");
    let mut h = f.spawn("", Effort::Default, false, None);
    let evs = until(&mut h, is_exit).await;
    assert_eq!(errors(&evs), ["Unsupported Antigravity ACP version"]);
}

#[tokio::test]
async fn an_unknown_model_is_not_fatal() {
    let f = fake("fake/bad_model_turn");
    let mut h = f.spawn("m1", Effort::Default, false, None);
    prompt(&h, "hello");
    let evs = until(&mut h, is_turn).await;
    assert_eq!(
        errors(&evs),
        ["Antigravity rejected model m1: Unknown model ID: m1"]
    );
    assert!(evs
        .iter()
        .any(|e| matches!(e, ProviderEvent::AssistantText { text } if text == "hi")));
    assert_eq!(turns(&evs), [(false, false)]);
    shutdown(&mut h).await;
}

#[tokio::test]
async fn an_unknown_request_from_agy_is_answered_with_its_id() {
    let f = fake("fake/unknown_request");
    let mut h = f.spawn("", Effort::Default, false, None);
    prompt(&h, "hello");
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, false)]);
    shutdown(&mut h).await;
    let reply = f
        .sent()
        .into_iter()
        .find(|f| f["id"] == 99)
        .expect("a reply to request 99");
    assert_eq!(reply["error"]["code"], -32601);
}

#[tokio::test]
async fn allowing_a_permission_replies_with_the_string_id() {
    let f = fake("tool_allow");
    let mut h = f.spawn("gemini-3.8-flash", Effort::Default, false, None);
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
    assert_eq!(
        (request_id.as_str(), tool_name.as_str()),
        ("1", "run_command")
    );
    assert!(
        matches!(evs[evs.len() - 2], ProviderEvent::ToolUse { .. }),
        "the tool use precedes its request"
    );
    send(
        &h,
        SessionCommand::RespondPermission {
            request_id: "1".into(),
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
        .find(|f| f["id"] == "1")
        .expect("reply");
    assert_eq!(reply["result"]["outcome"]["outcome"], "selected");
    assert_eq!(reply["result"]["outcome"]["optionId"], "allow");
}

#[tokio::test]
async fn denying_a_permission_picks_the_reject_option() {
    let f = fake("tool_deny");
    let mut h = f.spawn("gemini-3.8-flash", Effort::Default, false, None);
    prompt(&h, "run it");
    until(&mut h, |e| {
        matches!(e, ProviderEvent::PermissionRequest { .. })
    })
    .await;
    send(
        &h,
        SessionCommand::RespondPermission {
            request_id: "1".into(),
            allow: false,
        },
    );
    let rest = until(&mut h, is_turn).await;
    assert!(rest
        .iter()
        .any(|e| matches!(e, ProviderEvent::ToolResult { is_error: true, .. })));
    shutdown(&mut h).await;
    let reply = f
        .sent()
        .into_iter()
        .find(|f| f["id"] == "1")
        .expect("reply");
    assert_eq!(reply["result"]["outcome"]["optionId"], "deny");
}

#[tokio::test]
async fn auto_mode_answers_permissions_without_an_event() {
    let f = fake("tool_allow");
    let mut h = f.spawn("gemini-3.8-flash", Effort::Default, true, None);
    prompt(&h, "run it");
    let evs = until(&mut h, is_turn).await;
    assert!(!evs
        .iter()
        .any(|e| matches!(e, ProviderEvent::PermissionRequest { .. })));
    shutdown(&mut h).await;
    let reply = f
        .sent()
        .into_iter()
        .find(|f| f["id"] == "1")
        .expect("reply");
    assert_eq!(reply["result"]["outcome"]["optionId"], "allow");
}

#[tokio::test]
async fn turning_auto_on_answers_the_pending_request() {
    let f = fake("tool_allow");
    let mut h = f.spawn("gemini-3.8-flash", Effort::Default, false, None);
    prompt(&h, "run it");
    until(&mut h, |e| {
        matches!(e, ProviderEvent::PermissionRequest { .. })
    })
    .await;
    send(&h, SessionCommand::SetAutoMode(true));
    until(&mut h, is_turn).await;
    shutdown(&mut h).await;
    let reply = f
        .sent()
        .into_iter()
        .find(|f| f["id"] == "1")
        .expect("reply");
    assert_eq!(reply["result"]["outcome"]["optionId"], "allow");
}

#[tokio::test]
async fn interrupt_with_a_pending_permission_answers_cancelled_then_cancels() {
    let f = fake("interrupt_pending_permission");
    let mut h = f.spawn("gemini-3.8-flash", Effort::Default, false, None);
    prompt(&h, "run it");
    until(&mut h, |e| {
        matches!(e, ProviderEvent::PermissionRequest { .. })
    })
    .await;
    send(&h, SessionCommand::Interrupt);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    shutdown(&mut h).await;
    let sent = f.sent();
    let n = sent.len();
    assert_eq!(sent[n - 2]["id"], "1");
    assert_eq!(sent[n - 2]["result"]["outcome"]["outcome"], "cancelled");
    assert_eq!(sent[n - 1]["method"], "session/cancel");
    assert!(sent[n - 1].get("id").is_none(), "cancel is a notification");
}

#[tokio::test]
async fn interrupt_keeps_the_process_for_the_next_prompt() {
    let f = fake("interrupt");
    let mut h = f.spawn("gemini-3.8-flash", Effort::Default, false, None);
    prompt(&h, "count");
    until(&mut h, |e| matches!(e, ProviderEvent::TextDelta(_))).await;
    send(&h, SessionCommand::Interrupt);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    assert!(
        evs.iter()
            .any(|e| matches!(e, ProviderEvent::AssistantText { .. })),
        "text streamed before the cancel is kept"
    );
    prompt(&h, "again");
    let evs = until(&mut h, is_turn).await;
    assert_eq!(
        turns(&evs),
        [(false, false)],
        "the interrupt flag is per turn"
    );
    let rest = shutdown(&mut h).await;
    assert!(matches!(
        rest.last(),
        Some(ProviderEvent::Exited { code: Some(0) })
    ));
}

#[tokio::test]
async fn a_steer_ends_with_exactly_one_turn_complete() {
    let f = fake("steer");
    let mut h = f.spawn("gemini-3.8-flash", Effort::Default, false, None);
    prompt(&h, "first");
    until(&mut h, |e| matches!(e, ProviderEvent::ToolUse { .. })).await;
    prompt(&h, "second");
    until(&mut h, |e| {
        matches!(e, ProviderEvent::PermissionRequest { .. })
    })
    .await;
    send(
        &h,
        SessionCommand::RespondPermission {
            request_id: "1".into(),
            allow: true,
        },
    );
    let mut evs = until(&mut h, is_turn).await;
    evs.extend(shutdown(&mut h).await);
    assert_eq!(turns(&evs), [(false, false)]);
}

#[tokio::test]
async fn a_child_that_dies_reports_its_stderr_then_exits() {
    let f = fake("fake/crash");
    let mut h = f.spawn("", Effort::Default, false, None);
    let evs = until(&mut h, is_exit).await;
    let errs = errors(&evs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].contains("boom"), "{errs:?}");
    assert!(matches!(
        evs.last(),
        Some(ProviderEvent::Exited { code: Some(3) })
    ));
}

#[tokio::test]
async fn a_child_that_ignores_stdin_close_is_terminated() {
    let f = fake("fake/hang");
    let mut h = f.spawn("", Effort::Default, false, None);
    // Let the child reach its hang before asking it to stop.
    wait_for_log(&f, "initialize").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let start = Instant::now();
    let evs = shutdown(&mut h).await;
    assert!(
        start.elapsed() >= Duration::from_secs(3),
        "stdin close gets 3 s first"
    );
    assert!(start.elapsed() < Duration::from_secs(7));
    assert!(
        errors(&evs).is_empty(),
        "a requested shutdown is not an error"
    );
    assert!(matches!(
        evs.last(),
        Some(ProviderEvent::Exited { code: None })
    ));
}

#[tokio::test]
async fn dropping_the_handle_shuts_the_child_down() {
    let f = fake("simple_turn");
    let h = f.spawn("gemini-3.8-flash", Effort::Default, false, None);
    let SessionHandle {
        commands,
        mut events,
    } = h;
    drop(commands);
    let evs = timeout(WAIT, async {
        let mut out = Vec::new();
        while let Some(e) = events.recv().await {
            out.push(e);
        }
        out
    })
    .await
    .expect("channel closes");
    assert!(matches!(
        evs.last(),
        Some(ProviderEvent::Exited { code: Some(0) })
    ));
}
