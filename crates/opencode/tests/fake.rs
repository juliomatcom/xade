//! Drives `OpenCodeProvider` against a fake `opencode` (tests/fake_opencode.py) that replays a
//! recorded fixture (`fixtures/`) or a script built here. No network, no quota.

#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::time::timeout;
use xade_core::{
    AgentProvider, Effort, ProviderError, ProviderEvent, SessionCommand, SessionHandle, SpawnConfig,
};
use xade_opencode::OpenCodeProvider;

const WAIT: Duration = Duration::from_secs(20);
/// The quiet period under test: long against the fake's pauses, short against the tests.
const QUIET: Duration = Duration::from_millis(400);
const MODEL: &str = "opencode-go/glm-5.3-flash";
const SID: &str = "ses_fake";

struct Fake {
    dir: tempfile::TempDir,
    provider: OpenCodeProvider,
}

fn fake_with(fixture: impl FnOnce(&std::path::Path) -> PathBuf) -> Fake {
    let root = env!("CARGO_MANIFEST_DIR");
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("opencode");
    // Never write an executable here: another test thread may fork while its file is open for
    // writing, and exec then fails with "text file busy". Link the committed stub instead.
    let fixture = fixture(dir.path());
    std::fs::write(
        dir.path().join("fixture"),
        fixture.to_string_lossy().as_bytes(),
    )
    .expect("fixture path");
    std::os::unix::fs::symlink(format!("{root}/tests/fake_opencode.py"), &bin).expect("symlink");
    Fake {
        provider: OpenCodeProvider::with_binary(bin).with_quiet_period(QUIET),
        dir,
    }
}

/// A recorded fixture.
fn recorded(name: &str) -> Fake {
    let path = format!("{}/fixtures/{name}.ndjson", env!("CARGO_MANIFEST_DIR"));
    fake_with(|_| path.into())
}

/// A script of `{dir, frame}` entries.
fn scripted(entries: Vec<Value>) -> Fake {
    fake_with(|dir| {
        let path = dir.join("script.ndjson");
        let text: Vec<String> = entries.iter().map(Value::to_string).collect();
        std::fs::write(&path, text.join("\n")).expect("script");
        path
    })
}

// ---- script building ------------------------------------------------------------------------

/// Waits for the client's request; `label` names it for the answer.
fn request(method: &str, label: &str) -> Value {
    json!({"dir": "in", "frame": {"method": method, "id": label}})
}

/// Waits for any other line from the client (a notification or a reply).
fn line() -> Value {
    json!({"dir": "in", "frame": {}})
}

fn out(frame: Value) -> Value {
    json!({"dir": "out", "frame": frame})
}

fn meta(frame: Value) -> Value {
    json!({"dir": "meta", "frame": frame})
}

fn pause(seconds: f64) -> Value {
    meta(json!({"sleep": seconds}))
}

fn answer(label: &str, result: Value) -> Value {
    out(json!({"jsonrpc": "2.0", "id": label, "result": result}))
}

fn refuse(label: &str, code: i64, message: &str) -> Value {
    out(json!({"jsonrpc": "2.0", "id": label, "error": {"code": code, "message": message}}))
}

fn options(model: &str, effort: &str, efforts: &[&str], mode: &str) -> Value {
    let list = |v: &[&str]| v.iter().map(|v| json!({"value": v})).collect::<Vec<_>>();
    json!([
        {"id": "model", "currentValue": model, "options": list(&[model])},
        {"id": "effort", "currentValue": effort, "options": list(efforts)},
        {"id": "mode", "currentValue": mode, "options": list(&["build", "plan", "mine"])},
    ])
}

fn initialize(version: &str) -> Vec<Value> {
    vec![
        request("initialize", "init"),
        answer(
            "init",
            json!({"protocolVersion": 1, "agentInfo": {"name": "OpenCode", "version": version}}),
        ),
    ]
}

/// Handshake and a new session on model `m` that needs no config change.
fn ready() -> Vec<Value> {
    let mut s = initialize("2.4.2");
    s.extend([
        request("session/new", "new"),
        answer(
            "new",
            json!({"sessionId": SID,
                   "configOptions": options("m", "default", &["low", "default"], "build")}),
        ),
    ]);
    s
}

fn update(update: Value) -> Value {
    out(json!({"jsonrpc": "2.0", "method": "session/update",
               "params": {"sessionId": SID, "update": update}}))
}

fn says(text: &str) -> Value {
    update(json!({"sessionUpdate": "agent_message_chunk",
                  "content": {"type": "text", "text": text}}))
}

fn tool(id: &str, status: &str) -> Value {
    update(
        json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "name": "shell",
                  "status": status, "rawInput": {"command": "ls"}}),
    )
}

fn asks(id: &str, call: &str) -> Value {
    out(
        json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
            "sessionId": SID,
            "toolCall": {"toolCallId": call, "name": "shell", "status": "pending",
                         "rawInput": {"command": "ls"}},
            "options": [
                {"optionId": "once", "kind": "allow_once"},
                {"optionId": "always", "kind": "allow_always"},
                {"optionId": "reject", "kind": "reject_once"},
            ],
        }}),
    )
}

fn ends(label: &str, stop: &str, input: u64) -> Value {
    answer(
        label,
        json!({"stopReason": stop, "usage": {"inputTokens": input, "outputTokens": 1}}),
    )
}

// ---- driving --------------------------------------------------------------------------------

impl Fake {
    fn config(model: &str, effort: Effort, auto: bool, resume: Option<&str>) -> SpawnConfig {
        SpawnConfig {
            cwd: std::env::temp_dir(),
            model_id: model.into(),
            effort,
            auto_mode: auto,
            resume: resume.map(String::from),
        }
    }

    fn spawn(
        &self,
        model: &str,
        effort: Effort,
        auto: bool,
        resume: Option<&str>,
    ) -> SessionHandle {
        self.provider
            .spawn(Self::config(model, effort, auto, resume))
            .expect("spawn")
    }

    /// A session for a [`ready`] script.
    fn start(&self, auto: bool) -> SessionHandle {
        self.spawn("m", Effort::Default, auto, None)
    }

    fn read(&self, name: &str) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join(name))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("logged line"))
            .collect()
    }

    /// Frames the client sent, in order.
    fn sent(&self) -> Vec<Value> {
        self.read("in.log")
    }

    fn methods(&self) -> Vec<String> {
        self.sent()
            .iter()
            .filter_map(|f| f["method"].as_str().map(String::from))
            .collect()
    }

    /// The prompt texts sent as `method`.
    fn texts(&self, method: &str) -> Vec<String> {
        self.sent()
            .iter()
            .filter(|f| f["method"] == method)
            .filter_map(|f| f["params"]["prompt"][0]["text"].as_str().map(String::from))
            .collect()
    }

    fn configured(&self) -> Vec<(String, String)> {
        self.sent()
            .iter()
            .filter(|f| f["method"] == "session/set_config_option")
            .map(|f| {
                let s = |k: &str| f["params"][k].as_str().expect(k).to_string();
                (s("configId"), s("value"))
            })
            .collect()
    }

    /// The client's reply to the server request `id`.
    fn reply(&self, id: Value) -> Value {
        self.sent()
            .into_iter()
            .find(|f| f["id"] == id && f.get("method").is_none())
            .unwrap_or_else(|| panic!("no reply to {id}"))
    }

    async fn wait_for(&self, method: &str) {
        let start = Instant::now();
        while !self.methods().iter().any(|m| m == method) {
            assert!(start.elapsed() < WAIT, "{method} never sent");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
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

/// Whatever arrives within `time`.
async fn during(h: &mut SessionHandle, time: Duration) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    let _ = timeout(time, async {
        while let Some(ev) = h.events.recv().await {
            out.push(ev);
        }
    })
    .await;
    out
}

fn is_turn(e: &ProviderEvent) -> bool {
    matches!(e, ProviderEvent::TurnComplete { .. })
}

fn is_exit(e: &ProviderEvent) -> bool {
    matches!(e, ProviderEvent::Exited { .. })
}

fn is_permission(e: &ProviderEvent) -> bool {
    matches!(e, ProviderEvent::PermissionRequest { .. })
}

fn is_delta(e: &ProviderEvent) -> bool {
    matches!(e, ProviderEvent::TextDelta(_))
}

fn is_tool_use(e: &ProviderEvent) -> bool {
    matches!(e, ProviderEvent::ToolUse { .. })
}

fn send(h: &SessionHandle, c: SessionCommand) {
    h.commands.send(c).expect("send");
}

fn prompt(h: &SessionHandle, text: &str) {
    send(h, SessionCommand::SendPrompt(text.into()));
}

fn respond(h: &SessionHandle, id: &str, allow: bool) {
    send(
        h,
        SessionCommand::RespondPermission {
            request_id: id.into(),
            allow,
        },
    );
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

fn texts(evs: &[ProviderEvent]) -> Vec<&str> {
    evs.iter()
        .filter_map(|e| match e {
            ProviderEvent::AssistantText { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// ---- handshake and configuration ------------------------------------------------------------

#[tokio::test]
async fn a_prompt_queued_during_the_handshake_runs_after_it() {
    let f = recorded("simple_turn");
    let mut h = f.spawn(MODEL, Effort::Default, true, None);
    prompt(&h, "hello");
    let mut evs = until(&mut h, is_turn).await;
    evs.extend(shutdown(&mut h).await);

    assert!(matches!(
        &evs[0],
        ProviderEvent::Started { provider_session_id, model_id }
            if provider_session_id.starts_with("ses_") && model_id == MODEL
    ));
    assert_eq!(texts(&evs), ["hello fixture"]);
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
    assert_eq!(sent[0]["params"]["clientInfo"]["name"], "xade");
    assert_eq!(sent[1]["params"]["mcpServers"], json!([]));
    assert_eq!(f.texts("session/prompt"), ["hello"]);
    assert_eq!(f.read("runs.log")[0]["argv"], json!(["acp"]));
}

#[tokio::test]
async fn no_prompt_means_no_session() {
    let f = recorded("simple_turn");
    let mut h = f.spawn(MODEL, Effort::Default, true, None);
    f.wait_for("initialize").await;
    let evs = shutdown(&mut h).await;
    assert_eq!(evs.len(), 1);
    assert!(matches!(evs[0], ProviderEvent::Exited { code: Some(0) }));
    assert_eq!(f.methods(), ["initialize"], "session/new is lazy");
}

#[tokio::test]
async fn prompts_queued_before_the_session_run_in_order_as_one_turn() {
    let mut s = ready();
    s.insert(1, pause(0.5));
    s.extend([
        request("session/prompt", "a"),
        says("one"),
        ends("a", "end_turn", 10),
        request("session/prompt", "b"),
        says("two"),
        ends("b", "end_turn", 5),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "first");
    prompt(&h, "second");
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, false)]);
    assert_eq!(texts(&evs), ["one", "two"]);
    assert!(matches!(
        evs.last(),
        Some(ProviderEvent::TurnComplete { usage, .. }) if usage.input_tokens == 15
    ));
    assert_eq!(f.texts("session/prompt"), ["first", "second"]);
    shutdown(&mut h).await;
}

#[tokio::test]
async fn the_model_is_set_before_the_effort_and_both_before_the_prompt() {
    let mut s = initialize("2.4.2");
    s.extend([
        request("session/new", "new"),
        answer(
            "new",
            json!({"sessionId": SID,
                   "configOptions": options("other", "default", &["default"], "build")}),
        ),
        request("session/set_config_option", "model"),
        // The new model brings its own effort levels.
        answer(
            "model",
            json!({"configOptions": options("m", "default", &["high", "default"], "build")}),
        ),
        request("session/set_config_option", "effort"),
        answer(
            "effort",
            json!({"configOptions": options("m", "high", &["high", "default"], "build")}),
        ),
        request("session/prompt", "p"),
        ends("p", "end_turn", 1),
    ]);
    let f = scripted(s);
    let mut h = f.spawn("m", Effort::High, false, None);
    prompt(&h, "x");
    until(&mut h, is_turn).await;
    shutdown(&mut h).await;
    let want = [("model", "m"), ("effort", "high")].map(|(a, b)| (a.to_string(), b.to_string()));
    assert_eq!(f.configured(), want);
    assert_eq!(
        f.methods().last().map(String::as_str),
        Some("session/prompt")
    );
}

#[tokio::test]
async fn an_effort_the_model_lacks_is_not_sent() {
    let mut s = ready();
    s.extend([request("session/prompt", "p"), ends("p", "end_turn", 1)]);
    let f = scripted(s);
    let mut h = f.spawn("m", Effort::Max, false, None);
    prompt(&h, "x");
    let evs = until(&mut h, is_turn).await;
    assert!(errors(&evs).is_empty());
    assert!(f.configured().is_empty());
    shutdown(&mut h).await;
}

#[tokio::test]
async fn resuming_with_the_default_effort_resets_the_sessions_old_one() {
    let mut s = initialize("2.4.2");
    s.extend([
        request("session/resume", "resume"),
        // No session id in the answer; the old effort is still set.
        answer(
            "resume",
            json!({"configOptions": options("m", "low", &["low", "default"], "build")}),
        ),
        request("session/set_config_option", "effort"),
        answer(
            "effort",
            json!({"configOptions": options("m", "default", &["low", "default"], "build")}),
        ),
        request("session/prompt", "p"),
        says("back"),
        ends("p", "end_turn", 1),
    ]);
    let f = scripted(s);
    let mut h = f.spawn("m", Effort::Default, false, Some(SID));
    prompt(&h, "x");
    let evs = until(&mut h, is_turn).await;
    assert!(matches!(
        &evs[0],
        ProviderEvent::Started { provider_session_id, .. } if provider_session_id == SID
    ));
    assert_eq!(
        texts(&evs),
        ["back"],
        "updates are read under the resumed id"
    );
    assert_eq!(
        f.configured(),
        [("effort".to_string(), "default".to_string())]
    );
    assert_eq!(f.sent()[1]["params"]["sessionId"], SID);
    shutdown(&mut h).await;
}

#[tokio::test]
async fn resuming_an_unknown_session_is_a_clear_error() {
    let f = recorded("bad_resume");
    let mut h = f.spawn(
        "",
        Effort::Default,
        true,
        Some("ses_000000000000000000000000000"),
    );
    prompt(&h, "hello");
    let evs = until(&mut h, is_exit).await;
    let errs = errors(&evs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].starts_with("Could not resume the OpenCode conversation:"));
    assert_eq!(turns(&evs), [(true, false)]);
    assert_eq!(f.methods(), ["initialize", "session/resume"]);
}

#[tokio::test]
async fn opencode_1_is_refused_before_any_session() {
    let f = scripted(initialize("1.14.3"));
    let mut h = f.start(false);
    prompt(&h, "hello");
    let evs = until(&mut h, is_exit).await;
    assert_eq!(
        errors(&evs),
        ["OpenCode 2.0 or newer is required; please update OpenCode."]
    );
    assert_eq!(turns(&evs), [(true, false)]);
    assert_eq!(f.methods(), ["initialize"]);
}

#[tokio::test]
async fn an_unknown_model_is_not_fatal() {
    let mut s = ready();
    s.extend([
        request("session/set_config_option", "model"),
        refuse("model", -32602, "Invalid params: model not found: x/y"),
        request("session/prompt", "p"),
        says("hi"),
        ends("p", "end_turn", 1),
    ]);
    let f = scripted(s);
    let mut h = f.spawn("x/y", Effort::Default, false, None);
    prompt(&h, "hello");
    let evs = until(&mut h, is_turn).await;
    assert_eq!(
        errors(&evs),
        ["OpenCode rejected model x/y: Invalid params: model not found: x/y"]
    );
    assert_eq!(texts(&evs), ["hi"]);
    assert_eq!(turns(&evs), [(false, false)]);
    shutdown(&mut h).await;
}

/// A session of `mode`, answering any mode change with `reply`.
fn in_mode(mode: &str, change: Vec<Value>) -> Fake {
    let mut s = initialize("2.4.2");
    s.extend([
        request("session/new", "new"),
        answer(
            "new",
            json!({"sessionId": SID,
                   "configOptions": options("m", "default", &["default"], mode)}),
        ),
    ]);
    s.extend(change);
    s.extend([request("session/prompt", "p"), ends("p", "end_turn", 1)]);
    scripted(s)
}

#[tokio::test]
async fn auto_off_leaves_a_custom_default_agent_for_build() {
    let f = in_mode(
        "mine",
        vec![
            request("session/set_config_option", "mode"),
            answer(
                "mode",
                json!({"configOptions": options("m", "default", &["default"], "build")}),
            ),
        ],
    );
    let mut h = f.start(false);
    prompt(&h, "x");
    until(&mut h, is_turn).await;
    assert_eq!(f.configured(), [("mode".to_string(), "build".to_string())]);
    shutdown(&mut h).await;
}

#[tokio::test]
async fn auto_on_keeps_the_users_agent() {
    let f = in_mode("mine", Vec::new());
    let mut h = f.start(true);
    prompt(&h, "x");
    until(&mut h, is_turn).await;
    assert!(f.configured().is_empty());
    shutdown(&mut h).await;
}

#[tokio::test]
async fn a_refused_change_to_build_ends_the_session_rather_than_run_ungated() {
    let f = in_mode(
        "mine",
        vec![
            request("session/set_config_option", "mode"),
            refuse("mode", -32602, "no"),
        ],
    );
    let mut h = f.start(false);
    prompt(&h, "x");
    let evs = until(&mut h, is_exit).await;
    assert_eq!(errors(&evs), ["OpenCode rejected mode build: no"]);
    assert_eq!(turns(&evs), [(true, false)]);
    assert!(f.texts("session/prompt").is_empty());
}

#[tokio::test]
async fn auto_off_fails_when_build_mode_is_not_offered() {
    let mut s = initialize("2.4.2");
    s.extend([
        request("session/new", "new"),
        answer(
            "new",
            json!({
                "sessionId": SID,
                "configOptions": [
                    {"id": "model", "currentValue": "m", "options": [{"value": "m"}]},
                    {"id": "effort", "currentValue": "default", "options": [{"value": "default"}]},
                    {"id": "mode", "currentValue": "custom", "options": [{"value": "custom"}, {"value": "plan"}]}
                ]
            }),
        ),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "x");
    let evs = until(&mut h, is_exit).await;
    assert!(
        errors(&evs)
            .iter()
            .any(|e| e.contains("did not offer `build` mode")),
        "{evs:?}"
    );
    assert_eq!(turns(&evs), [(true, false)]);
    assert!(f.texts("session/prompt").is_empty());
}

#[tokio::test]
async fn only_auto_off_adds_permission_rules_to_the_environment() {
    let inherited = std::env::var("OPENCODE_CONFIG_CONTENT").ok();
    for auto in [true, false] {
        let f = recorded("simple_turn");
        let mut h = f.spawn(MODEL, Effort::Default, auto, None);
        f.wait_for("initialize").await;
        shutdown(&mut h).await;
        let env: Option<String> =
            serde_json::from_value(f.read("env.json").remove(0)).expect("env");
        if auto {
            assert_eq!(env, inherited);
        } else {
            let config: Value = serde_json::from_str(&env.expect("set")).expect("json");
            for rules in [
                &config["permissions"],
                &config["agents"]["build"]["permissions"],
            ] {
                assert!(rules.as_array().is_some_and(|r| r.iter().any(|r| {
                    r == &json!({"action": "shell", "resource": "*", "effect": "ask"})
                })));
            }
        }
    }
}

// ---- permissions ----------------------------------------------------------------------------

#[tokio::test]
async fn allowing_a_permission_picks_once_with_the_numeric_id() {
    let f = recorded("tool_allow");
    let mut h = f.spawn(MODEL, Effort::Default, false, None);
    prompt(&h, "run it");
    let evs = until(&mut h, is_permission).await;
    let Some(ProviderEvent::PermissionRequest {
        request_id,
        tool_name,
        ..
    }) = evs.last()
    else {
        panic!("no permission request");
    };
    assert_eq!((request_id.as_str(), tool_name.as_str()), ("0", "Bash"));
    assert!(
        is_tool_use(&evs[evs.len() - 2]),
        "the tool use precedes its request"
    );
    respond(&h, "0", true);
    let rest = until(&mut h, is_turn).await;
    assert!(rest.iter().any(|e| matches!(
        e,
        ProviderEvent::ToolResult {
            is_error: false,
            ..
        }
    )));
    shutdown(&mut h).await;
    assert_eq!(
        f.reply(json!(0))["result"]["outcome"],
        json!({"outcome": "selected", "optionId": "once"})
    );
}

#[tokio::test]
async fn denying_a_permission_rejects_and_the_turn_ends_interrupted() {
    let f = recorded("tool_deny");
    let mut h = f.spawn(MODEL, Effort::Default, false, None);
    prompt(&h, "run it");
    until(&mut h, is_permission).await;
    respond(&h, "0", false);
    let rest = until(&mut h, is_turn).await;
    assert!(rest
        .iter()
        .any(|e| matches!(e, ProviderEvent::ToolResult { is_error: true, .. })));
    assert_eq!(turns(&rest), [(false, true)]);
    shutdown(&mut h).await;
    assert_eq!(f.reply(json!(0))["result"]["outcome"]["optionId"], "reject");
}

#[tokio::test]
async fn auto_mode_answers_permissions_without_an_event() {
    let f = recorded("tool_allow");
    let mut h = f.spawn(MODEL, Effort::Default, true, None);
    prompt(&h, "run it");
    let evs = until(&mut h, is_turn).await;
    assert!(!evs.iter().any(is_permission));
    shutdown(&mut h).await;
    assert_eq!(f.reply(json!(0))["result"]["outcome"]["optionId"], "once");
}

#[tokio::test]
async fn turning_auto_on_never_auto_allows_in_an_auto_off_process() {
    // A process spawned with Auto OFF turned user `deny` rules into `ask`: toggling Auto ON must
    // keep asking until the actor's idle respawn starts a clean process.
    let mut s = ready();
    s.extend([
        request("session/prompt", "p"),
        asks("r1", "c1"),
        line(),
        asks("r2", "c2"),
        line(),
        ends("p", "end_turn", 1),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "run it");
    until(&mut h, is_permission).await;
    send(&h, SessionCommand::SetAutoMode(true));
    assert!(during(&mut h, Duration::from_millis(200)).await.is_empty());
    respond(&h, "r1", true);
    let evs = until(&mut h, is_permission).await;
    assert!(evs.iter().any(is_permission));
    respond(&h, "r2", true);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, false)]);
    shutdown(&mut h).await;
    for id in ["r1", "r2"] {
        assert_eq!(f.reply(json!(id))["result"]["outcome"]["optionId"], "once");
    }
}

#[tokio::test]
async fn turning_auto_back_on_in_an_auto_on_process_answers_pending_and_later_requests() {
    // In a process spawned with Auto ON (clean env, user `deny` untouched), toggling OFF then ON
    // safely auto-allows pending and subsequent `ask` requests.
    let mut s = ready();
    s.extend([
        request("session/prompt", "p"),
        asks("r1", "c1"),
        line(),
        asks("r2", "c2"),
        line(),
        ends("p", "end_turn", 1),
    ]);
    let f = scripted(s);
    let mut h = f.start(true);
    send(&h, SessionCommand::SetAutoMode(false));
    prompt(&h, "run it");
    until(&mut h, is_permission).await;
    send(&h, SessionCommand::SetAutoMode(true));
    let evs = until(&mut h, is_turn).await;
    assert!(!evs.iter().any(is_permission));
    shutdown(&mut h).await;
    for id in ["r1", "r2"] {
        assert_eq!(f.reply(json!(id))["result"]["outcome"]["optionId"], "once");
    }
}

#[tokio::test]
async fn an_unknown_request_is_answered_with_its_id() {
    let mut s = ready();
    s.extend([
        request("session/prompt", "p"),
        out(json!({"jsonrpc": "2.0", "id": 99, "method": "fs/read_text_file", "params": {}})),
        line(),
        ends("p", "end_turn", 1),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "hello");
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, false)]);
    shutdown(&mut h).await;
    assert_eq!(f.reply(json!(99))["error"]["code"], -32601);
}

// ---- stop -----------------------------------------------------------------------------------

#[tokio::test]
async fn interrupt_before_the_session_is_ready_completes_the_turn() {
    let mut s = initialize("2.4.2");
    s.insert(1, pause(0.7));
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "hello");
    send(&h, SessionCommand::Interrupt);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    tokio::time::sleep(Duration::from_millis(900)).await;
    let rest = shutdown(&mut h).await;
    assert!(errors(&rest).is_empty());
    assert_eq!(
        f.methods(),
        ["initialize"],
        "the dropped prompt never created a session"
    );
}

#[tokio::test]
async fn interrupt_keeps_the_process_for_the_next_prompt() {
    let f = recorded("interrupt");
    let mut h = f.spawn(MODEL, Effort::Default, true, None);
    prompt(&h, "story");
    until(&mut h, is_delta).await;
    send(&h, SessionCommand::Interrupt);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    assert_eq!(
        texts(&evs).len(),
        1,
        "text streamed before the stop is kept"
    );
    prompt(&h, "again");
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, false)]);
    assert_eq!(texts(&evs), ["still alive"]);
    let rest = shutdown(&mut h).await;
    assert!(matches!(
        rest.last(),
        Some(ProviderEvent::Exited { code: Some(0) })
    ));
}

#[tokio::test]
async fn interrupt_with_a_pending_permission_answers_cancelled_then_cancels() {
    let f = recorded("interrupt_pending_permission");
    let mut h = f.spawn(MODEL, Effort::Default, false, None);
    prompt(&h, "run it");
    until(&mut h, is_permission).await;
    send(&h, SessionCommand::Interrupt);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    shutdown(&mut h).await;
    let sent = f.sent();
    let n = sent.len();
    assert_eq!(sent[n - 2]["id"], 0);
    assert_eq!(sent[n - 2]["result"]["outcome"]["outcome"], "cancelled");
    assert_eq!(sent[n - 1]["method"], "session/cancel");
    assert!(sent[n - 1].get("id").is_none(), "cancel is a notification");
}

#[tokio::test]
async fn a_permission_request_after_stop_is_cancelled_and_never_shown_even_in_auto() {
    for auto in [false, true] {
        let mut s = ready();
        s.extend([
            request("session/prompt", "p"),
            line(),
            asks("late", "c1"),
            line(),
            ends("p", "cancelled", 1),
            // And one that trails the answer.
            asks("later", "c2"),
            line(),
        ]);
        let f = scripted(s);
        let mut h = f.start(auto);
        prompt(&h, "run it");
        f.wait_for("session/prompt").await;
        send(&h, SessionCommand::Interrupt);
        let mut evs = until(&mut h, is_turn).await;
        assert_eq!(turns(&evs), [(false, true)]);
        evs.extend(during(&mut h, QUIET * 2).await);
        assert!(
            !evs.iter().any(|e| is_permission(e) || is_tool_use(e)),
            "{evs:?}"
        );
        assert_eq!(turns(&evs).len(), 1, "{evs:?}");
        shutdown(&mut h).await;
        for id in ["late", "later"] {
            assert_eq!(
                f.reply(json!(id))["result"]["outcome"]["outcome"],
                "cancelled"
            );
        }
    }
}

// ---- steering -------------------------------------------------------------------------------

#[tokio::test]
async fn an_injected_steer_ends_with_exactly_one_turn_complete() {
    let f = recorded("steer");
    let mut h = f.spawn(MODEL, Effort::Default, true, None);
    prompt(&h, "first");
    until(&mut h, is_tool_use).await;
    prompt(&h, "second");
    let mut evs = until(&mut h, is_turn).await;
    evs.extend(during(&mut h, QUIET * 2).await);
    assert_eq!(turns(&evs), [(false, false)]);
    assert_eq!(f.texts("session/prompt"), ["first"]);
    assert_eq!(f.texts("_session/steering"), ["second"]);
    shutdown(&mut h).await;
}

fn refused() -> Value {
    json!({"outcome": "promptRequired", "reason": "noRunningTurn"})
}

/// The steer is refused before, or only after, the running prompt answers.
#[tokio::test]
async fn a_refused_steer_follows_as_a_prompt_of_the_same_turn() {
    for steer_first in [true, false] {
        let mut answers = vec![answer("s", refused()), ends("a", "end_turn", 10)];
        if !steer_first {
            answers.reverse();
            answers.insert(1, pause(0.3));
        }
        let mut s = ready();
        s.extend([
            request("session/prompt", "a"),
            says("one"),
            request("_session/steering", "s"),
        ]);
        s.extend(answers);
        s.extend([
            request("session/prompt", "b"),
            says("two"),
            ends("b", "end_turn", 5),
        ]);
        let f = scripted(s);
        let mut h = f.start(false);
        prompt(&h, "first");
        until(&mut h, is_delta).await;
        prompt(&h, "second");
        let mut evs = until(&mut h, is_turn).await;
        evs.extend(during(&mut h, QUIET * 2).await);
        assert_eq!(turns(&evs), [(false, false)], "steer_first={steer_first}");
        assert_eq!(texts(&evs), ["one", "two"]);
        assert_eq!(f.texts("session/prompt"), ["first", "second"]);
        shutdown(&mut h).await;
    }
}

#[tokio::test]
async fn a_failed_steer_also_follows_as_a_prompt() {
    let mut s = ready();
    s.extend([
        request("session/prompt", "a"),
        says("one"),
        request("_session/steering", "s"),
        refuse("s", -32601, "Method not found"),
        ends("a", "end_turn", 1),
        request("session/prompt", "b"),
        ends("b", "end_turn", 1),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "first");
    until(&mut h, is_delta).await;
    prompt(&h, "second");
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, false)]);
    assert_eq!(f.texts("session/prompt"), ["first", "second"]);
    shutdown(&mut h).await;
}

#[tokio::test]
async fn stop_forgets_a_pending_steer_and_a_later_prompt_waits_for_the_cancel() {
    let mut s = ready();
    s.extend([
        request("session/prompt", "a"),
        says("one"),
        request("_session/steering", "s"),
        line(), // session/cancel
        pause(0.5),
        answer("s", refused()),
        ends("a", "cancelled", 1),
        request("session/prompt", "c"),
        says("three"),
        ends("c", "end_turn", 1),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "first");
    until(&mut h, is_delta).await;
    prompt(&h, "steered");
    f.wait_for("_session/steering").await;
    send(&h, SessionCommand::Interrupt);
    prompt(&h, "after stop");
    let mut evs = until(&mut h, is_turn).await;
    evs.extend(during(&mut h, QUIET * 2).await);
    // The queued prompt continues the turn: no `TurnComplete` between the two.
    assert_eq!(turns(&evs), [(false, false)]);
    assert_eq!(texts(&evs), ["one", "three"]);
    assert_eq!(f.texts("session/prompt"), ["first", "after stop"]);
    assert_eq!(f.texts("_session/steering"), ["steered"]);
    shutdown(&mut h).await;
}

#[tokio::test]
async fn stop_drops_a_prompt_that_was_waiting_for_the_running_one() {
    let mut s = ready();
    s.extend([
        request("session/prompt", "a"),
        says("one"),
        request("_session/steering", "s"),
        answer("s", refused()),
        pause(0.4),
        line(), // session/cancel
        ends("a", "cancelled", 1),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "first");
    until(&mut h, is_delta).await;
    prompt(&h, "waiting");
    f.wait_for("_session/steering").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    send(&h, SessionCommand::Interrupt);
    let mut evs = until(&mut h, is_turn).await;
    evs.extend(during(&mut h, QUIET * 2).await);
    assert_eq!(turns(&evs), [(false, true)]);
    assert_eq!(f.texts("session/prompt"), ["first"]);
    shutdown(&mut h).await;
}

#[tokio::test]
async fn refused_steers_run_ahead_of_prompts_queued_after_them() {
    let mut s = ready();
    s.extend([
        request("session/prompt", "a"),
        says("one"),
        request("_session/steering", "s"),
        ends("a", "end_turn", 1),
        pause(0.3),
        answer("s", refused()),
        request("session/prompt", "b"),
        says("two"),
        ends("b", "end_turn", 1),
        request("session/prompt", "c"),
        says("three"),
        ends("c", "end_turn", 1),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "first");
    until(&mut h, is_delta).await;
    prompt(&h, "steer-A");
    f.wait_for("_session/steering").await;
    // Arrives after the prompt already answered (held waiting for steer-A): must run after A.
    tokio::time::sleep(Duration::from_millis(80)).await;
    prompt(&h, "queued-B");
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, false)]);
    assert_eq!(texts(&evs), ["one", "two", "three"]);
    assert_eq!(f.texts("session/prompt"), ["first", "steer-A", "queued-B"]);
    shutdown(&mut h).await;
}

#[tokio::test]
async fn stop_while_only_a_steer_answer_is_missing_ends_the_turn() {
    let mut s = ready();
    s.extend([
        request("session/prompt", "a"),
        says("one"),
        request("_session/steering", "s"),
        // The prompt answers; the steer never does.
        ends("a", "end_turn", 1),
        line(), // session/cancel
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "first");
    until(&mut h, is_delta).await;
    prompt(&h, "steered");
    assert!(turns(&during(&mut h, QUIET * 2).await).is_empty());
    send(&h, SessionCommand::Interrupt);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    shutdown(&mut h).await;
}

// ---- turns with no prompt behind them -------------------------------------------------------

#[tokio::test]
async fn a_subagents_late_answer_is_its_own_turn_ended_by_silence() {
    let f = recorded("subagent");
    let mut h = f.spawn(MODEL, Effort::Default, false, None);
    prompt(&h, "delegate");
    until(&mut h, is_permission).await;
    respond(&h, "0", true);
    let first = until(&mut h, is_turn).await;
    let second = until(&mut h, is_turn).await;
    assert_eq!(turns(&first), [(false, false)]);
    assert_eq!(turns(&second), [(false, false)]);
    assert!(texts(&second)[0].contains("childmark"));
    shutdown(&mut h).await;
}

#[tokio::test]
async fn a_background_request_after_the_turn_is_shown_and_waited_for() {
    let f = recorded("background_subagent");
    let mut h = f.spawn(MODEL, Effort::Default, false, None);
    prompt(&h, "delegate");
    let first = until(&mut h, is_turn).await;
    assert_eq!(texts(&first), ["started"]);
    until(&mut h, is_permission).await;
    assert!(
        during(&mut h, QUIET * 2).await.is_empty(),
        "no end while the user decides"
    );
    respond(&h, "0", true);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, false)]);
    assert!(texts(&evs)[0].contains("bgmark"));
    shutdown(&mut h).await;
}

#[tokio::test]
async fn a_working_tool_holds_the_quiet_period() {
    let mut s = ready();
    s.extend([
        request("session/prompt", "p"),
        ends("p", "end_turn", 1),
        pause(0.2),
        says("checking"),
        tool("c1", "in_progress"),
        pause(1.2),
        tool("c1", "completed"),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "x");
    until(&mut h, is_turn).await;
    let mut evs = until(&mut h, is_tool_use).await;
    evs.extend(during(&mut h, QUIET * 2).await);
    assert!(turns(&evs).is_empty(), "{evs:?}");
    let rest = until(&mut h, is_turn).await;
    assert!(rest
        .iter()
        .any(|e| matches!(e, ProviderEvent::ToolResult { .. })));
    shutdown(&mut h).await;
}

/// An update trailing the turn it belonged to shows nothing, and must not hold the next turn open.
#[tokio::test]
async fn a_trailing_tool_update_does_not_hold_a_later_turn_open() {
    let mut s = ready();
    s.extend([
        request("session/prompt", "p"),
        tool("c1", "in_progress"),
        ends("p", "end_turn", 1),
        tool("c1", "in_progress"),
        pause(0.2),
        says("report"),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "x");
    until(&mut h, is_turn).await;
    let evs = until(&mut h, is_turn).await;
    assert_eq!(texts(&evs), ["report"]);
    shutdown(&mut h).await;
}

#[tokio::test]
async fn stop_ends_a_turn_with_no_prompt() {
    // Stop on a synthetic turn kills the old child (and any background subagent in its group)
    // and recycles into a fresh `acp` child that resumes the same session on the next prompt.
    let mut s = ready();
    s.extend([
        request("session/prompt", "p"),
        meta(json!({"spawn_bg": true})),
        ends("p", "end_turn", 1),
        pause(0.2),
        says("still going"),
        asks("r", "ses_child:c"),
        meta(json!({"hang": true})),
        meta(json!({"restart": true})),
    ]);
    s.extend(initialize("2.4.2"));
    s.extend([
        request("session/resume", "res"),
        answer(
            "res",
            json!({"configOptions": options("m", "default", &["default"], "build")}),
        ),
        request("session/prompt", "q"),
        says("resumed"),
        ends("q", "end_turn", 1),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "x");
    until(&mut h, is_turn).await;
    let bg = background_pid(&f).await;
    let pre = until(&mut h, is_permission).await;
    assert_eq!(texts(&pre), ["still going"]);
    send(&h, SessionCommand::Interrupt);
    let mut evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    assert_reaped_soon(bg).await;
    evs.extend(during(&mut h, QUIET * 2).await);
    assert!(!evs.iter().any(is_permission), "{evs:?}");

    prompt(&h, "after");
    let next = until(&mut h, is_turn).await;
    assert_eq!(turns(&next), [(false, false)]);
    assert_eq!(texts(&next), ["resumed"]);
    shutdown(&mut h).await;
    assert_eq!(f.read("runs.log").len(), 2, "recycled into a second child");
    assert!(f.methods().contains(&"session/resume".to_string()));
}

#[tokio::test]
async fn stop_during_prompt_after_subagent_recycles_child_and_kills_bg() {
    let mut s = ready();
    s.extend([
        request("session/prompt", "p"),
        meta(json!({"spawn_bg": true})),
        update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "sub1",
            "name": "subagent",
            "status": "in_progress",
            "rawInput": {"prompt": "bg"}
        })),
        meta(json!({"hang": true})),
        meta(json!({"restart": true})),
    ]);
    s.extend(initialize("2.4.2"));
    s.extend([
        request("session/resume", "res"),
        answer(
            "res",
            json!({"configOptions": options("m", "default", &["default"], "build")}),
        ),
        request("session/prompt", "q"),
        says("clean"),
        ends("q", "end_turn", 1),
    ]);
    let f = scripted(s);
    let mut h = f.start(true);
    prompt(&h, "start bg");
    until(&mut h, is_tool_use).await;
    let bg = background_pid(&f).await;
    send(&h, SessionCommand::Interrupt);
    let evs = until(&mut h, is_turn).await;
    assert_eq!(turns(&evs), [(false, true)]);
    assert_reaped_soon(bg).await;

    prompt(&h, "next");
    let next = until(&mut h, is_turn).await;
    assert_eq!(turns(&next), [(false, false)]);
    assert_eq!(texts(&next), ["clean"]);
    shutdown(&mut h).await;
    assert_eq!(f.read("runs.log").len(), 2);
}

#[tokio::test]
async fn a_prompt_takes_over_a_turn_with_no_prompt() {
    let mut s = ready();
    s.extend([
        request("session/prompt", "p"),
        ends("p", "end_turn", 1),
        pause(0.2),
        says("report"),
        request("session/prompt", "q"),
        says("answer"),
        ends("q", "end_turn", 1),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "x");
    until(&mut h, is_turn).await;
    until(&mut h, is_delta).await;
    prompt(&h, "next");
    let mut evs = until(&mut h, is_turn).await;
    evs.extend(during(&mut h, QUIET * 2).await);
    assert_eq!(texts(&evs), ["report", "answer"]);
    assert_eq!(turns(&evs), [(false, false)]);
    assert!(f.texts("_session/steering").is_empty());
    shutdown(&mut h).await;
}

/// A subagent's request still open when the parent's prompt ends: ending the turn would take the
/// request off the screen and leave the subagent waiting for ever.
#[tokio::test]
async fn a_request_left_open_by_a_clean_end_keeps_the_turn_open() {
    let mut s = ready();
    s.extend([
        request("session/prompt", "p"),
        asks("r", "ses_child:c"),
        ends("p", "end_turn", 7),
        line(),
        says("done"),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "x");
    until(&mut h, is_permission).await;
    assert!(turns(&during(&mut h, QUIET * 2).await).is_empty());
    respond(&h, "r", true);
    let evs = until(&mut h, is_turn).await;
    assert!(matches!(
        evs.last(),
        Some(ProviderEvent::TurnComplete { usage, .. }) if usage.input_tokens == 7
    ));
    shutdown(&mut h).await;
    assert_eq!(f.reply(json!("r"))["result"]["outcome"]["optionId"], "once");
}

// ---- the process ----------------------------------------------------------------------------

#[tokio::test]
async fn a_child_that_dies_mid_turn_fails_the_turn() {
    for code in [0, 3] {
        let mut s = ready();
        s.extend([
            request("session/prompt", "p"),
            says("partial"),
            meta(json!({"stderr": "boom"})),
            meta(json!({"exit": code})),
        ]);
        let f = scripted(s);
        let mut h = f.start(false);
        prompt(&h, "x");
        let evs = until(&mut h, is_exit).await;
        assert_eq!(texts(&evs), ["partial"]);
        assert_eq!(turns(&evs), [(true, false)], "code {code}");
        let errs = errors(&evs);
        assert_eq!(errs.len(), 1);
        assert!(
            errs[0].starts_with("opencode exited unexpectedly"),
            "{errs:?}"
        );
        assert!(errs[0].ends_with("boom"), "{errs:?}");
    }
}

#[tokio::test]
async fn a_child_that_dies_in_a_turn_with_no_prompt_fails_it() {
    let mut s = ready();
    s.extend([
        request("session/prompt", "p"),
        ends("p", "end_turn", 1),
        pause(0.1),
        says("report"),
        meta(json!({"exit": 3})),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "x");
    let evs = until(&mut h, is_exit).await;
    assert_eq!(turns(&evs), [(false, false), (true, false)]);
}

#[tokio::test]
async fn a_child_that_dies_idle_reports_its_stderr_then_exits() {
    let mut s = initialize("2.4.2");
    s.extend([meta(json!({"stderr": "boom"})), meta(json!({"exit": 3}))]);
    let f = scripted(s);
    let mut h = f.start(false);
    let evs = until(&mut h, is_exit).await;
    assert_eq!(
        errors(&evs),
        ["opencode exited unexpectedly (exit code 3): boom"]
    );
    assert!(turns(&evs).is_empty());
    assert!(matches!(
        evs.last(),
        Some(ProviderEvent::Exited { code: Some(3) })
    ));
}

#[tokio::test]
async fn a_shutdown_mid_turn_is_not_an_error() {
    let mut s = ready();
    s.extend([request("session/prompt", "p"), says("partial")]);
    let f = scripted(s);
    let mut h = f.start(false);
    prompt(&h, "x");
    until(&mut h, is_delta).await;
    let evs = shutdown(&mut h).await;
    assert!(errors(&evs).is_empty());
    assert_eq!(texts(&evs), ["partial"]);
    assert!(turns(&evs).is_empty());
}

#[tokio::test]
async fn a_child_that_ignores_stdin_close_is_terminated() {
    let mut s = initialize("2.4.2");
    s.push(meta(json!({"hang": true})));
    let f = scripted(s);
    let mut h = f.start(false);
    f.wait_for("initialize").await;
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
    let f = recorded("simple_turn");
    let h = f.spawn(MODEL, Effort::Default, true, None);
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

/// Whether `pid` is gone (a zombie awaiting its reaper counts as gone).
fn process_gone(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let stat = String::from_utf8_lossy(&out.stdout);
    stat.trim().is_empty() || stat.trim().starts_with('Z')
}

/// The pid the fake wrote to `bg.pid` for the `sleep` it started in its group.
async fn background_pid(f: &Fake) -> u32 {
    let path = f.dir.path().join("bg.pid");
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

/// Fails the test (and cleans up) if `pid` outlives the session.
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
async fn a_clean_shutdown_kills_what_the_child_left_in_its_group() {
    let mut s = initialize("2.4.2");
    s.push(meta(json!({"spawn_bg": true})));
    let f = scripted(s);
    let mut h = f.start(false);
    let pid = background_pid(&f).await;
    assert!(!process_gone(pid));
    shutdown(&mut h).await;
    assert_reaped_soon(pid).await;
}

#[tokio::test]
async fn an_unexpected_exit_kills_what_the_child_left_in_its_group() {
    let mut s = initialize("2.4.2");
    s.extend([
        meta(json!({"spawn_bg": true})),
        meta(json!({"signal": "SIGKILL"})),
    ]);
    let f = scripted(s);
    let mut h = f.start(false);
    let pid = background_pid(&f).await;
    let evs = until(&mut h, is_exit).await;
    assert!(errors(&evs)[0].contains("SIGKILL"), "{evs:?}");
    assert_reaped_soon(pid).await;
}

// ---- preflight and delete -------------------------------------------------------------------

/// A one-shot command that prints and exits.
fn command(entries: Vec<Value>) -> Fake {
    scripted(entries.into_iter().map(meta).collect())
}

fn run_of(f: &Fake) -> (Value, Value) {
    let run = f.read("runs.log").remove(0);
    (run["argv"].clone(), run["cwd"].clone())
}

#[test]
fn preflight_fills_the_catalog_from_a_standalone_run_outside_any_project() {
    let f = command(vec![
        json!({"stdout": "opencode-go/x-free\nopencode-go/y\nopencode/z"}),
    ]);
    assert_eq!(f.provider.preflight(), Ok(()));
    let ids: Vec<_> = f.provider.models().into_iter().map(|m| m.id).collect();
    assert_eq!(ids, ["opencode-go/x-free", "opencode-go/y", "opencode/z"]);
    assert_eq!(f.provider.default_model(), "opencode-go/y");
    assert_eq!(run_of(&f), (json!(["models", "--standalone"]), json!("/")));
}

#[test]
fn preflight_failures_are_classified() {
    let run = |entries: Vec<Value>| command(entries).provider.preflight();
    assert_eq!(
        run(vec![
            json!({"stderr": "Error: network is unreachable"}),
            json!({"exit": 1})
        ]),
        Err(ProviderError::Protocol(
            "opencode models failed: Error: network is unreachable".into()
        ))
    );
    assert_eq!(
        run(vec![
            json!({"stderr": "run `opencode auth login` first"}),
            json!({"exit": 1})
        ]),
        Err(ProviderError::NotLoggedIn)
    );
    assert_eq!(
        run(vec![json!({"stdout": ""})]),
        Err(ProviderError::Protocol(
            "opencode models listed no models".into()
        ))
    );
}

#[test]
fn delete_runs_a_standalone_delete_and_tolerates_a_missing_session() {
    let ok = command(vec![json!({"stdout": "Session ses_a deleted"})]);
    assert_eq!(ok.provider.delete_session("ses_a"), Ok(()));
    assert_eq!(
        run_of(&ok),
        (
            json!(["session", "delete", "--standalone", "ses_a"]),
            json!("/")
        )
    );
    let missing = command(vec![
        json!({"stderr": "Session not found: ses_a"}),
        json!({"exit": 1}),
    ]);
    assert_eq!(missing.provider.delete_session("ses_a"), Ok(()));
    let broken = command(vec![
        json!({"stderr": "database is locked"}),
        json!({"exit": 1}),
    ]);
    assert_eq!(
        broken.provider.delete_session("ses_a"),
        Err(ProviderError::Protocol(
            "opencode session delete failed: database is locked".into()
        ))
    );
}
