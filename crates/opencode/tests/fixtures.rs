//! Replays each recorded fixture through the Translator, the way the process task will drive it:
//! `in` frames track the running prompt and the interrupt flag, `out` frames are fed in order, and
//! whatever follows the last response is closed as a turn OpenCode started by itself.
//! Each test asserts a recorded ACP fact.

#![allow(clippy::expect_used)]

use std::collections::{BTreeSet, HashMap};

use serde_json::{json, Value};
use xade_core::{ProviderEvent, UsageTotals};
use xade_opencode::rpc::{self, Incoming};
use xade_opencode::Translator;

const ALL: [&str; 20] = [
    "background_subagent",
    "bad_model",
    "bad_resume",
    "edit",
    "edit_auto",
    "edit_deny",
    "interrupt",
    "interrupt_pending_permission",
    "patch",
    "resume",
    "set_config",
    "simple_turn",
    "steer",
    "steer_idle",
    "subagent",
    "thinking",
    "tool_allow",
    "tool_deny",
    "two_turns",
    "unknown_method",
];

/// Every `{dir, frame}` line of a fixture.
fn lines(name: &str) -> Vec<(String, Value)> {
    let path = format!("{}/fixtures/{name}.ndjson", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .expect("fixture")
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).expect("line");
            (
                v["dir"].as_str().expect("dir").to_string(),
                v["frame"].clone(),
            )
        })
        .collect()
}

#[derive(Default)]
struct Replay {
    t: Translator,
    session_id: String,
    /// The id the last `session/resume` asked for.
    resumed: String,
    prompt: Option<u64>,
    methods: HashMap<u64, String>,
    interrupt: bool,
    /// Events arrived with no prompt running.
    unprompted: bool,
    events: Vec<ProviderEvent>,
}

impl Replay {
    fn feed(&mut self, dir: &str, frame: &Value) {
        if dir == "meta" {
            if frame.get("restart").is_some() {
                *self = Replay {
                    events: std::mem::take(&mut self.events),
                    ..Replay::default()
                };
            }
            return;
        }
        if dir == "in" {
            self.on_in(frame);
            return;
        }
        let ev = match rpc::parse_frame(frame) {
            Some(Incoming::Notification { method, params }) if method == "session/update" => {
                rpc::update_for(&params, &self.session_id)
                    .map_or_else(Vec::new, |u| self.t.on_update(u))
            }
            Some(Incoming::Request { id, method, params })
                if method == "session/request_permission" =>
            {
                assert_eq!(params["sessionId"], self.session_id.as_str());
                self.t.on_permission_request(rpc::id_key(&id), &params)
            }
            Some(Incoming::Response { id, result }) => return self.on_response(&id, result),
            _ => return,
        };
        self.unprompted |= self.prompt.is_none() && !ev.is_empty();
        self.events.extend(ev);
    }

    fn on_in(&mut self, frame: &Value) {
        let id = frame["id"].as_u64();
        match frame["method"].as_str() {
            Some("session/prompt") => {
                assert_eq!(self.prompt, None, "one prompt at a time");
                self.end_unprompted();
                self.interrupt = false;
                self.prompt = id;
            }
            Some("session/cancel") => self.interrupt = self.prompt.is_some(),
            _ => {}
        }
        if let (Some(id), Some(method)) = (id, frame["method"].as_str()) {
            self.methods.insert(id, method.into());
        }
    }

    fn on_response(&mut self, id: &Value, result: Result<Value, rpc::RpcError>) {
        let Some(id) = id.as_u64() else { return };
        let method = self.methods.get(&id).cloned().unwrap_or_default();
        match (method.as_str(), result) {
            ("session/prompt", result) => {
                assert_eq!(self.prompt.take(), Some(id));
                let ev = match result {
                    Ok(r) => self.t.on_prompt_result(&r, true, self.interrupt),
                    Err(e) => self.t.on_prompt_error(&e.message, true),
                };
                self.events.extend(ev);
            }
            ("session/new", Ok(r)) => {
                self.session_id = r["sessionId"].as_str().expect("sessionId").to_string();
            }
            ("session/resume", Ok(_)) => {
                self.session_id = self.resumed.clone();
            }
            _ => {}
        }
    }

    fn end_unprompted(&mut self) {
        if std::mem::take(&mut self.unprompted) {
            self.events.extend(self.t.finish_turn(false));
        }
    }
}

fn replay(name: &str) -> Replay {
    let mut r = Replay::default();
    for (dir, frame) in lines(name) {
        // `session/resume` answers without the id: it is the one that was asked for.
        if dir == "in" && frame["method"] == "session/resume" {
            r.resumed = frame["params"]["sessionId"]
                .as_str()
                .expect("sessionId")
                .to_string();
        }
        r.feed(&dir, &frame);
    }
    r.end_unprompted();
    r
}

fn run(name: &str) -> Vec<ProviderEvent> {
    replay(name).events
}

/// A compact view: thinking is left out and consecutive deltas collapse into one tag.
fn shape(ev: &[ProviderEvent]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in ev {
        let tag = match e {
            ProviderEvent::ThinkingProgress { .. } => continue,
            ProviderEvent::TextDelta(_) => "delta".to_string(),
            ProviderEvent::AssistantText { .. } => "text".to_string(),
            ProviderEvent::ToolUse { name, .. } => format!("use:{name}"),
            ProviderEvent::ToolResult { is_error, .. } => {
                format!("result:{}", if *is_error { "err" } else { "ok" })
            }
            ProviderEvent::PermissionRequest { tool_name, .. } => format!("perm:{tool_name}"),
            ProviderEvent::TurnComplete {
                is_error,
                interrupted,
                ..
            } => match (is_error, interrupted) {
                (false, false) => "turn:ok".into(),
                (false, true) => "turn:interrupted".into(),
                (true, _) => "turn:error".into(),
            },
            ProviderEvent::Error(m) => format!("error:{m}"),
            other => format!("{other:?}"),
        };
        if tag == "delta" && out.last() == Some(&tag) {
            continue;
        }
        out.push(tag);
    }
    out
}

fn texts(ev: &[ProviderEvent]) -> Vec<&str> {
    ev.iter()
        .filter_map(|e| match e {
            ProviderEvent::AssistantText { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn usages(ev: &[ProviderEvent]) -> Vec<&UsageTotals> {
    ev.iter()
        .filter_map(|e| match e {
            ProviderEvent::TurnComplete { usage, .. } => Some(usage),
            _ => None,
        })
        .collect()
}

fn parsed(json: &str) -> Value {
    serde_json::from_str(json).expect("json")
}

/// `(tool_use_id, name, input)` of every `ToolUse`.
fn uses(ev: &[ProviderEvent]) -> Vec<(&str, &str, Value)> {
    ev.iter()
        .filter_map(|e| match e {
            ProviderEvent::ToolUse {
                tool_use_id,
                name,
                input_json,
            } => Some((tool_use_id.as_str(), name.as_str(), parsed(input_json))),
            _ => None,
        })
        .collect()
}

/// `(tool_use_id, output, structured)` of every `ToolResult`.
fn results(ev: &[ProviderEvent]) -> Vec<(&str, &str, Option<Value>)> {
    ev.iter()
        .filter_map(|e| match e {
            ProviderEvent::ToolResult {
                tool_use_id,
                output,
                structured_json,
                ..
            } => Some((
                tool_use_id.as_str(),
                output.as_str(),
                structured_json.as_deref().map(parsed),
            )),
            _ => None,
        })
        .collect()
}

/// Each permission request must name an announced call and show the same name and input.
fn requests_match_their_calls(ev: &[ProviderEvent]) -> usize {
    let calls = uses(ev);
    let mut n = 0;
    for e in ev {
        if let ProviderEvent::PermissionRequest {
            tool_use_id,
            tool_name,
            input_json,
            ..
        } = e
        {
            let call = calls
                .iter()
                .find(|c| c.0 == tool_use_id)
                .expect("announced");
            assert_eq!((call.1, &call.2), (tool_name.as_str(), &parsed(input_json)));
            n += 1;
        }
    }
    n
}

/// The `result` (or `error`) frame answering the first request that used `method`.
fn answer(name: &str, method: &str) -> Value {
    let all = lines(name);
    let id = all
        .iter()
        .find(|(d, f)| d == "in" && f["method"] == method)
        .map(|(_, f)| f["id"].clone())
        .expect("request");
    all.into_iter()
        .find(|(d, f)| d == "out" && f["id"] == id && f.get("method").is_none())
        .map(|(_, f)| f)
        .expect("response")
}

/// `configOptions[id]` of a response.
fn option<'a>(response: &'a Value, id: &str) -> &'a Value {
    response["result"]["configOptions"]
        .as_array()
        .expect("configOptions")
        .iter()
        .find(|o| o["id"] == id)
        .expect("option")
}

fn values(option: &Value) -> Vec<&str> {
    option["options"]
        .as_array()
        .expect("options")
        .iter()
        .map(|o| o["value"].as_str().expect("value"))
        .collect()
}

#[test]
fn every_fixture_replays_with_nothing_left_over() {
    let mut files: Vec<String> =
        std::fs::read_dir(format!("{}/fixtures", env!("CARGO_MANIFEST_DIR")))
            .expect("fixtures")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
    files.sort();
    assert_eq!(files, ALL.map(|n| format!("{n}.ndjson")));
    for name in ALL {
        let mut r = replay(name);
        assert!(r.t.flush_text().is_empty(), "{name}: text left unflushed");
        assert_eq!(r.prompt, None, "{name}: unanswered prompt");
        for (dir, frame) in lines(name) {
            if dir == "out" {
                assert!(rpc::parse_frame(&frame).is_some(), "{name}: {frame}");
            }
        }
        // Every call is announced once, in Claude's shape, and finishes at most once.
        let ids: Vec<&str> = uses(&r.events).iter().map(|u| u.0).collect();
        assert_eq!(
            ids.len(),
            ids.iter().collect::<BTreeSet<_>>().len(),
            "{name}"
        );
        let done: Vec<&str> = results(&r.events).iter().map(|u| u.0).collect();
        assert!(done.iter().all(|d| ids.contains(d)), "{name}");
        assert_eq!(
            done.len(),
            done.iter().collect::<BTreeSet<_>>().len(),
            "{name}"
        );
        requests_match_their_calls(&r.events);
    }
}

#[test]
fn handshake_facts() {
    let init = answer("simple_turn", "initialize");
    let r = &init["result"];
    assert_eq!(r["protocolVersion"], 1);
    assert_eq!(r["agentInfo"]["name"], "OpenCode");
    assert!(r["agentInfo"]["version"]
        .as_str()
        .expect("version")
        .starts_with("2."));
    assert_eq!(r["_meta"]["steering"]["supported"], true);
    for cap in ["resume", "delete"] {
        assert!(r["agentCapabilities"]["sessionCapabilities"][cap].is_object());
    }
    assert_eq!(r["authMethods"][0]["id"], "opencode-login");

    let new = answer("simple_turn", "session/new");
    assert!(new["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .starts_with("ses_"));
    assert!(new["result"].get("models").is_none());
    let ids: Vec<&Value> = new["result"]["configOptions"]
        .as_array()
        .expect("configOptions")
        .iter()
        .map(|o| &o["id"])
        .collect();
    assert_eq!(ids, ["model", "effort", "mode"]);
    assert_eq!(option(&new, "effort")["category"], "thought_level");
    assert_eq!(option(&new, "effort")["currentValue"], "default");
    assert_eq!(option(&new, "mode")["currentValue"], "build");
}

#[test]
fn set_config_option_answers_with_the_models_effort_levels() {
    let set_model = answer("set_config", "session/set_config_option");
    assert_eq!(
        option(&set_model, "model")["currentValue"],
        "opencode-go/glm-5.3-flash"
    );
    assert_eq!(
        values(option(&set_model, "effort")),
        ["low", "high", "max", "default"]
    );
    let current: Vec<Value> = lines("set_config")
        .iter()
        .filter(|(d, f)| d == "out" && f["result"]["sessionId"].is_null())
        .filter(|(_, f)| f["result"]["configOptions"].is_array())
        .map(|(_, f)| option(f, "effort")["currentValue"].clone())
        .collect();
    assert_eq!(current, ["default", "high", "default"]);
}

#[test]
fn error_answers() {
    let model = answer("bad_model", "session/set_config_option");
    assert_eq!(model["error"]["code"], -32602);
    assert!(model["error"]["message"]
        .as_str()
        .expect("message")
        .contains("model not found"));
    // The session still takes the next one.
    assert!(lines("bad_model")
        .iter()
        .any(|(d, f)| d == "out" && f["id"] == 4 && f["result"]["configOptions"].is_array()));

    let resume = answer("bad_resume", "session/resume");
    assert_eq!(resume["error"]["code"], -32602);
    assert!(resume["error"]["message"]
        .as_str()
        .expect("message")
        .contains("session not found"));

    let unknown = answer("unknown_method", "xade/no_such_method");
    assert_eq!(unknown["error"]["code"], -32601);
}

#[test]
fn simple_turn() {
    let ev = run("simple_turn");
    assert_eq!(shape(&ev), ["delta", "text", "turn:ok"]);
    assert_eq!(texts(&ev), ["hello fixture"]);
    let u = usages(&ev)[0];
    assert_eq!(
        (u.input_tokens, u.output_tokens, u.cache_read_tokens),
        (1131, 4, 5760)
    );
    assert_eq!(
        (u.context_tokens_used, u.context_window_max),
        (6910, 1_000_000)
    );
    assert_eq!(u.cost_usd, 0.0);
}

/// `usage` in a prompt response is per turn, not cumulative.
#[test]
fn two_turns_report_per_turn_usage() {
    let ev = run("two_turns");
    assert_eq!(
        shape(&ev),
        ["delta", "text", "turn:ok", "delta", "text", "turn:ok"]
    );
    assert_eq!(texts(&ev), ["OK", "42"]);
    let responses: Vec<u64> = lines("two_turns")
        .iter()
        .filter(|(d, _)| d == "out")
        .filter_map(|(_, f)| f["result"]["usage"]["outputTokens"].as_u64())
        .collect();
    let u = usages(&ev);
    assert_eq!(responses, [u[0].output_tokens, u[1].output_tokens]);
}

#[test]
fn thinking_is_progress_only() {
    let ev = run("thinking");
    assert!(ev.iter().any(
        |e| matches!(e, ProviderEvent::ThinkingProgress { estimated_tokens } if *estimated_tokens > 0)
    ));
    assert_eq!(shape(&ev).last().map(String::as_str), Some("turn:ok"));
    // The finalized text is exactly what streamed.
    let streamed: String = ev
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts(&ev).concat(), streamed);
}

#[test]
fn an_allowed_command() {
    let ev = run("tool_allow");
    assert_eq!(
        shape(&ev),
        [
            "use:Bash",
            "perm:Bash",
            "result:ok",
            "delta",
            "text",
            "turn:ok"
        ]
    );
    assert_eq!(
        uses(&ev)[0].2,
        json!({"command": "echo hello-fixture", "description": ""})
    );
    assert_eq!(results(&ev)[0].1, "hello-fixture\n");
    assert_eq!(requests_match_their_calls(&ev), 1);
}

/// A rejected call fails, and the whole prompt ends `cancelled`.
#[test]
fn a_denied_command_ends_the_turn_as_interrupted() {
    let ev = run("tool_deny");
    assert_eq!(
        shape(&ev),
        ["use:Bash", "perm:Bash", "result:err", "turn:interrupted"]
    );
    assert_eq!(results(&ev)[0].1, "The user declined this tool call");
    assert_eq!(
        answer("tool_deny", "session/prompt")["result"]["stopReason"],
        "cancelled"
    );
}

#[test]
fn writes_and_edits_with_auto_off() {
    let ev = run("edit");
    assert_eq!(
        shape(&ev),
        [
            "use:Write",
            "perm:Write",
            "result:ok",
            "use:Read",
            "result:ok",
            "use:Write",
            "perm:Write",
            "result:ok",
            "use:Edit",
            "perm:Edit",
            "result:ok",
            "delta",
            "text",
            "turn:ok"
        ]
    );
    let u = uses(&ev);
    assert_eq!(
        u[0].2,
        json!({"file_path": "notes.txt", "content": "alpha"})
    );
    assert_eq!(u[1].2, json!({"file_path": "notes.txt"}));
    assert_eq!(
        u[3].2,
        json!({"file_path": "notes.txt", "old_string": "beta", "new_string": "gamma"})
    );
    let r = results(&ev);
    // The old text comes from the permission request: the completed frame has none.
    assert_eq!(
        r[0].2,
        Some(json!({"type": "create", "structuredPatch": [], "originalFile": null}))
    );
    assert_eq!(
        r[2].2,
        Some(json!({"type": "update", "structuredPatch": [], "originalFile": "alpha"}))
    );
    assert_eq!((&r[1].2, &r[3].2), (&None, &None));
    assert_eq!(r[3].1, "Edited notes.txt (1 replacement)");
}

#[test]
fn writes_with_auto_on_have_no_old_text() {
    for (dir, frame) in lines("edit_auto") {
        let update = &frame["params"]["update"];
        if dir == "out" && update["status"] == "completed" {
            let diff = update["content"]
                .as_array()
                .expect("content")
                .iter()
                .any(|b| b["type"] == "diff");
            // Only the `edit` result carries a diff block; neither `write` does.
            let text = update["content"][0]["content"]["text"]
                .as_str()
                .expect("text");
            assert_eq!(diff, text.starts_with("Edited"), "{text}");
        }
    }
    let ev = run("edit_auto");
    assert_eq!(
        shape(&ev),
        [
            "use:Write",
            "result:ok",
            "use:Write",
            "result:ok",
            "use:Edit",
            "result:ok",
            "delta",
            "text",
            "turn:ok"
        ]
    );
    let r = results(&ev);
    let kind = |i: usize| r[i].2.as_ref().map(|v| v["type"].clone());
    assert_eq!(
        (kind(0), kind(1)),
        (Some(json!("create")), Some(json!("update")))
    );
}

#[test]
fn a_denied_write_carries_no_diff() {
    let ev = run("edit_deny");
    assert_eq!(
        shape(&ev),
        ["use:Write", "perm:Write", "result:err", "turn:interrupted"]
    );
    assert_eq!(results(&ev)[0].2, None);
}

#[test]
fn patches_are_reviewable_writes_and_edits() {
    let ev = run("patch");
    assert_eq!(
        shape(&ev),
        [
            "use:Write",
            "perm:Write",
            "result:ok",
            "use:Edit",
            "perm:Edit",
            "result:ok",
            "use:Edit",
            "perm:Edit",
            "result:ok",
            "use:Edit",
            "perm:Edit",
            "result:ok",
            "delta",
            "text",
            "turn:ok"
        ]
    );
    let u = uses(&ev);
    assert_eq!(u[0].2, json!({"file_path": "solo.txt", "content": "one\n"}));
    let lines = |input: &Value| -> Vec<Value> {
        input["structuredPatch"]
            .as_array()
            .expect("hunks")
            .iter()
            .map(|h| h["lines"].clone())
            .collect()
    };
    assert_eq!(u[1].2["file_path"], "a.txt");
    assert_eq!(
        lines(&u[1].2),
        [
            json!([" *** Add File: a.txt", "+hello"]),
            json!([" *** Add File: b.txt", "+bye"])
        ]
    );
    assert_eq!(
        lines(&u[2].2),
        [
            json!([" *** Update File: a.txt", "-hello", "+world"]),
            json!(["-b.txt (deleted)"])
        ]
    );
    assert_eq!(
        lines(&u[3].2),
        [json!([" *** Move: a.txt -> c.txt", "-world", "+earth"])]
    );
    let r = results(&ev);
    assert_eq!(
        r[0].2,
        Some(json!({"type": "create", "structuredPatch": [], "originalFile": null}))
    );
    assert!(r[1..].iter().all(|r| r.2.is_none()));
}

/// A cancelled prompt answers without `usage`, and the process takes the next one.
#[test]
fn interrupt_mid_stream() {
    let ev = run("interrupt");
    assert_eq!(
        shape(&ev),
        [
            "delta",
            "text",
            "turn:interrupted",
            "delta",
            "text",
            "turn:ok"
        ]
    );
    assert_eq!(texts(&ev)[1], "still alive");
    assert!(answer("interrupt", "session/prompt")["result"]["usage"].is_null());
    assert_eq!(usages(&ev)[0].output_tokens, 0);
}

#[test]
fn interrupt_with_a_pending_permission() {
    let ev = run("interrupt_pending_permission");
    assert_eq!(
        shape(&ev),
        [
            "use:Bash",
            "perm:Bash",
            "result:err",
            "turn:interrupted",
            "delta",
            "text",
            "turn:ok"
        ]
    );
}

/// `_session/steering` joins the running prompt: one turn, one response.
#[test]
fn steering_joins_the_running_turn() {
    assert_eq!(
        answer("steer", "_session/steering")["result"],
        json!({"outcome": "injected"})
    );
    let ev = run("steer");
    assert_eq!(
        shape(&ev),
        ["use:Bash", "result:ok", "delta", "text", "turn:ok"]
    );
    assert!(texts(&ev)[0].contains("BANANA"));
}

#[test]
fn steering_an_idle_session_asks_for_a_prompt_and_starts_nothing() {
    assert_eq!(
        answer("steer_idle", "_session/steering")["result"],
        json!({"outcome": "promptRequired", "reason": "noRunningTurn"})
    );
    assert_eq!(shape(&run("steer_idle")), ["delta", "text", "turn:ok"]);
}

#[test]
fn resume_keeps_the_context_and_settings_without_replaying_history() {
    let resumed = answer("resume", "session/resume");
    assert_eq!(
        option(&resumed, "model")["currentValue"],
        "opencode-go/glm-5.3-flash"
    );
    assert_eq!(option(&resumed, "effort")["currentValue"], "low");
    let all = lines("resume");
    let from = all
        .iter()
        .position(|(_, f)| f["method"] == "session/resume")
        .expect("resume");
    let until = from
        + all[from..]
            .iter()
            .position(|(_, f)| f["method"] == "session/prompt")
            .expect("prompt");
    assert!(
        all[from..until]
            .iter()
            .all(|(_, f)| f["method"] != "session/update"),
        "history replayed"
    );
    let ev = run("resume");
    assert_eq!(texts(&ev), ["OK", "kumquat7"]);
}

/// A subagent's tools arrive under our session with namespaced ids; its follow-up answer comes
/// after the prompt response, with no prompt running and nothing marking its end.
#[test]
fn a_subagent_answers_after_the_prompt_ended() {
    let ev = run("subagent");
    assert_eq!(
        shape(&ev),
        [
            "use:subagent",
            "result:ok",
            "use:Bash",
            "perm:Bash",
            "result:ok",
            "delta",
            "text",
            "turn:ok",
            "delta",
            "text",
            "turn:ok"
        ]
    );
    let child = uses(&ev)[1].0;
    assert!(child.starts_with("ses_") && child.contains(':'), "{child}");
    assert!(texts(&ev)[1].contains("childmark"));
}

/// Once the parent's prompt ended, the subagent's tool updates are not forwarded: only its
/// permission request and the parent's follow-up arrive.
#[test]
fn a_background_subagent_asks_for_permission_after_the_turn() {
    let ev = run("background_subagent");
    assert_eq!(
        shape(&ev),
        [
            "use:subagent",
            "result:ok",
            "delta",
            "text",
            "turn:ok",
            "use:Bash",
            "perm:Bash",
            "delta",
            "text",
            "turn:ok"
        ]
    );
    assert_eq!(texts(&ev)[0], "started");
    assert_eq!(requests_match_their_calls(&ev), 1);
    let all = lines("background_subagent");
    let end = all
        .iter()
        .position(|(_, f)| f["result"]["stopReason"] == "end_turn")
        .expect("response");
    assert!(all[end..]
        .iter()
        .all(|(_, f)| !f["params"]["update"]["sessionUpdate"]
            .as_str()
            .is_some_and(|k| k.starts_with("tool_call"))));
}
