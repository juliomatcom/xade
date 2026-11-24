//! Replays each recorded fixture through the Translator, the way the process task will drive it:
//! `in` frames track outstanding prompts and the interrupt flag, `out` frames are fed in order.
//! Each test asserts a recorded ACP fact (see wiki-llm/antigravity-protocol.md).

#![allow(clippy::expect_used)]

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;
use xade_antigravity::rpc::{self, Incoming};
use xade_antigravity::Translator;
use xade_core::{ProviderEvent, UsageTotals};

const ALL: [&str; 15] = [
    "bad_model",
    "bad_prompt_session",
    "bad_resume",
    "edit",
    "interrupt",
    "interrupt_pending_permission",
    "resume",
    "set_config",
    "simple_turn",
    "steer",
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
    outstanding: BTreeSet<u64>,
    methods: HashMap<u64, String>,
    interrupt: bool,
    events: Vec<ProviderEvent>,
    /// Ids answered by `session/prompt` responses, in order.
    prompt_results: Vec<u64>,
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
        match rpc::parse_frame(frame) {
            Some(Incoming::Notification { method, params }) if method == "session/update" => {
                if let Some(update) = rpc::update_for(&params, &self.session_id) {
                    self.events.extend(self.t.on_update(update));
                }
            }
            Some(Incoming::Request { id, method, params })
                if method == "session/request_permission" =>
            {
                self.events
                    .extend(self.t.on_permission_request(rpc::id_key(&id), &params));
            }
            Some(Incoming::Response { id, result }) => self.on_response(&id, result),
            _ => {}
        }
    }

    fn on_in(&mut self, frame: &Value) {
        match frame["method"].as_str() {
            Some("session/prompt") => {
                let id = frame["id"].as_u64().expect("numeric id");
                if self.outstanding.is_empty() {
                    self.interrupt = false;
                }
                self.outstanding.insert(id);
                self.methods.insert(id, "session/prompt".into());
            }
            Some("session/cancel") if !self.outstanding.is_empty() => self.interrupt = true,
            Some(m) => {
                if let Some(id) = frame["id"].as_u64() {
                    self.methods.insert(id, m.into());
                }
            }
            None => {}
        }
    }

    fn on_response(&mut self, id: &Value, result: Result<Value, rpc::RpcError>) {
        let Some(id) = id.as_u64() else { return };
        let method = self.methods.get(&id).cloned().unwrap_or_default();
        if method == "session/prompt" {
            self.outstanding.remove(&id);
            self.prompt_results.push(id);
            let last = self.outstanding.is_empty();
            let ev = match result {
                Ok(r) => self.t.on_prompt_result(&r, last, self.interrupt),
                Err(e) => self.t.on_prompt_error(&e.message, last),
            };
            self.events.extend(ev);
        } else if matches!(method.as_str(), "session/new" | "session/resume") {
            if let Ok(r) = result {
                self.session_id = r["sessionId"].as_str().expect("sessionId").to_string();
            }
        }
    }
}

fn replay(name: &str) -> Replay {
    let mut r = Replay::default();
    // Our own requests go out before their answers come in, so register methods first.
    for (dir, frame) in lines(name) {
        r.feed(&dir, &frame);
    }
    r
}

fn run(name: &str) -> Vec<ProviderEvent> {
    replay(name).events
}

/// A compact view: consecutive deltas / thinking updates collapse into one tag.
fn shape(ev: &[ProviderEvent]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in ev {
        let tag = match e {
            ProviderEvent::TextDelta(_) => "delta".to_string(),
            ProviderEvent::ThinkingProgress { .. } => "think".to_string(),
            ProviderEvent::AssistantText { text } => format!("text:{text}"),
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
        if (tag == "delta" || tag == "think") && out.last() == Some(&tag) {
            continue;
        }
        out.push(tag);
    }
    out
}

fn usages(ev: &[ProviderEvent]) -> Vec<&UsageTotals> {
    ev.iter()
        .filter_map(|e| match e {
            ProviderEvent::TurnComplete { usage, .. } => Some(usage),
            _ => None,
        })
        .collect()
}

fn results(ev: &[ProviderEvent]) -> Vec<(&str, &str, bool, Option<&str>)> {
    ev.iter()
        .filter_map(|e| match e {
            ProviderEvent::ToolResult {
                tool_use_id,
                output,
                is_error,
                structured_json,
            } => Some((
                tool_use_id.as_str(),
                output.as_str(),
                *is_error,
                structured_json.as_deref(),
            )),
            _ => None,
        })
        .collect()
}

/// The `result` of the first response whose request used `method`.
fn result_of(name: &str, method: &str) -> Value {
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

#[test]
fn every_fixture_replays_and_ends_with_no_pending_text() {
    for name in ALL {
        let mut r = replay(name);
        assert!(r.t.flush_text().is_empty(), "{name}: text left unflushed");
        assert!(r.outstanding.is_empty(), "{name}: unanswered prompt");
        for (dir, frame) in lines(name) {
            if dir == "out" {
                // Nothing we recorded may be a frame we cannot even classify.
                assert!(rpc::parse_frame(&frame).is_some(), "{name}: {frame}");
            }
        }
    }
}

#[test]
fn handshake_facts() {
    let init = result_of("simple_turn", "initialize");
    let r = &init["result"];
    assert_eq!(r["protocolVersion"], 1);
    assert_eq!(r["agentCapabilities"]["loadSession"], true);
    assert!(r["agentCapabilities"]["sessionCapabilities"]["resume"].is_object());
    assert_eq!(r["agentCapabilities"]["promptCapabilities"]["image"], true);
    assert_eq!(r["agentInfo"]["name"], "Antigravity ACP Server");
    assert!(!r["authMethods"].as_array().expect("authMethods").is_empty());

    let new = result_of("simple_turn", "session/new");
    let r = &new["result"];
    assert_eq!(r["sessionId"].as_str().expect("id").len(), 36);
    assert_eq!(r["modes"]["currentModeId"], "default");
    let ids: Vec<&str> = r["configOptions"]
        .as_array()
        .expect("configOptions")
        .iter()
        .map(|o| o["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, ["model", "thought_level", "mode"]);
    let model = &r["configOptions"][0];
    assert_eq!(model["type"], "select");
    let values: Vec<&str> = model["options"]
        .as_array()
        .expect("options")
        .iter()
        .map(|o| o["value"].as_str().expect("value"))
        .collect();
    assert!(values.contains(&"gemini-3.8-flash"));
    // The base ids the model picker will use; the effort suffix lives in `models.availableModels`.
    assert!(values.contains(&"gemini-4-neon"));
    assert!(r["models"]["availableModels"][0]["modelId"]
        .as_str()
        .expect("modelId")
        .starts_with("gemini-4-neon-"));
    let levels: Vec<&str> = r["configOptions"][1]["options"]
        .as_array()
        .expect("levels")
        .iter()
        .map(|o| o["value"].as_str().expect("value"))
        .collect();
    assert_eq!(levels, ["low", "medium", "high", "xhigh"]);
    assert_eq!(r["configOptions"][1]["currentValue"], "high");
}

#[test]
fn set_config_option_answers_with_the_new_options() {
    let all = lines("set_config");
    let answers: Vec<&Value> = all
        .iter()
        .filter(|(d, f)| {
            d == "out"
                && f["result"]["configOptions"].is_array()
                && f["result"]["sessionId"].is_null()
        })
        .map(|(_, f)| f)
        .collect();
    assert_eq!(answers.len(), 2);
    let current = |f: &Value, id: &str| {
        f["result"]["configOptions"]
            .as_array()
            .expect("options")
            .iter()
            .find(|o| o["id"] == id)
            .map(|o| o["currentValue"].clone())
            .expect("option")
    };
    assert_eq!(current(answers[0], "model"), "gemini-3.8-flash");
    assert_eq!(current(answers[1], "thought_level"), "low");
    assert_eq!(current(answers[1], "model"), "gemini-3.8-flash");
}

#[test]
fn bad_model_is_a_recoverable_error() {
    let all = lines("bad_model");
    let errors: Vec<&Value> = all
        .iter()
        .filter(|(d, f)| d == "out" && f.get("error").is_some())
        .map(|(_, f)| f)
        .collect();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["error"]["code"], -32602);
    assert!(errors[0]["error"]["message"]
        .as_str()
        .expect("message")
        .contains("Unknown model ID"));
    // The next set_config_option on the same session still works.
    assert!(all
        .iter()
        .any(|(d, f)| d == "out" && f["result"]["configOptions"].is_array()));
}

#[test]
fn bad_resume_fails_with_an_internal_error_not_an_auth_error() {
    let Incoming::Response { result, .. } =
        rpc::parse_frame(&result_of("bad_resume", "session/resume")).expect("response")
    else {
        panic!("not a response");
    };
    let err = result.expect_err("resume of an unknown id fails");
    assert_eq!(err.code, -32603);
    assert!(
        err.message.contains("Failed to read session"),
        "{}",
        err.message
    );
}

#[test]
fn unknown_method_is_method_not_found() {
    let f = result_of("unknown_method", "xade/no_such_method");
    assert_eq!(f["error"]["code"], -32601);
    assert!(f["error"]["message"]
        .as_str()
        .expect("message")
        .starts_with("Method not found"));
}

#[test]
fn prompt_to_an_unknown_session_errors_and_ends_the_turn() {
    let ev = run("bad_prompt_session");
    assert_eq!(ev.len(), 2, "{ev:?}");
    assert!(matches!(&ev[0], ProviderEvent::Error(m) if m.starts_with("Session not found")));
    assert!(matches!(
        &ev[1],
        ProviderEvent::TurnComplete {
            is_error: true,
            interrupted: false,
            ..
        }
    ));
}

#[test]
fn simple_turn() {
    let ev = run("simple_turn");
    assert_eq!(shape(&ev), ["delta", "text:hello fixture", "turn:ok"]);
    let u = usages(&ev)[0];
    assert_eq!(
        (u.input_tokens, u.output_tokens, u.cache_read_tokens),
        (10079, 28, 0)
    );
    // From the last `usage_update` before the response.
    assert_eq!(
        (u.context_tokens_used, u.context_window_max),
        (10107, 1_048_576)
    );
    assert_eq!(u.cost_usd, 0.0);
}

/// `usage` in a prompt response is per turn, not cumulative.
#[test]
fn two_turns_report_per_turn_usage() {
    let ev = run("two_turns");
    assert_eq!(
        shape(&ev),
        ["delta", "text:OK", "turn:ok", "delta", "text:42", "turn:ok"]
    );
    let u = usages(&ev);
    assert_eq!((u[0].input_tokens, u[0].cache_read_tokens), (10081, 0));
    assert_eq!((u[1].input_tokens, u[1].cache_read_tokens), (2193, 8037));
    assert_eq!(u[1].context_tokens_used, 10290);
}

#[test]
fn thinking_is_progress_only() {
    let ev = run("thinking");
    let s = shape(&ev);
    assert!(s.contains(&"think".to_string()), "{s:?}");
    assert_eq!(s.last().map(String::as_str), Some("turn:ok"));
    assert!(ev
        .iter()
        .any(|e| matches!(e, ProviderEvent::ThinkingProgress { estimated_tokens } if *estimated_tokens > 0)));
    // The finalized text is exactly what streamed, across many chunks.
    let streamed: String = ev
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    let finalized: Vec<&str> = ev
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::AssistantText { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(finalized, [streamed.as_str()]);
    assert!(streamed.len() > 200);
}

#[test]
fn tool_allow_asks_after_the_tool_use_and_finishes_once() {
    let ev = run("tool_allow");
    assert_eq!(
        shape(&ev),
        [
            "use:run_command",
            "perm:run_command",
            "result:ok",
            "delta",
            "text:DONE",
            "turn:ok"
        ]
    );
    let Some(ProviderEvent::PermissionRequest {
        request_id,
        tool_use_id,
        input_json,
        ..
    }) = ev
        .iter()
        .find(|e| matches!(e, ProviderEvent::PermissionRequest { .. }))
    else {
        panic!("no request");
    };
    // agy sends the id as the string "1".
    assert_eq!(request_id, "1");
    assert!(tool_use_id.starts_with("call_"));
    assert!(input_json.contains("echo hello-fixture"));
    let r = results(&ev);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, tool_use_id);
    assert!(r[0].1.contains("hello-fixture"));
    // The output text comes from `rawOutput.output`, and `content` rides along as structured JSON.
    assert!(r[0].3.expect("structured").contains(r#""content""#));
}

/// Deny: agy marks the tool `failed` (not an error turn) and the turn still ends `end_turn`.
#[test]
fn tool_deny_is_a_failed_tool_and_a_normal_turn() {
    let ev = run("tool_deny");
    assert_eq!(
        shape(&ev),
        [
            "use:run_command",
            "perm:run_command",
            "result:err",
            "turn:ok"
        ]
    );
    let r = results(&ev);
    // `rawOutput` is a plain string here, not `{output}`.
    assert!(r[0].1.contains("user denied permission"), "{}", r[0].1);
}

#[test]
fn edit_carries_a_diff() {
    let ev = run("edit");
    assert_eq!(
        shape(&ev),
        [
            "use:read_file",
            "result:ok",
            "use:edit_file",
            "result:ok",
            "delta",
            "text:DONE",
            "turn:ok"
        ]
    );
    let r = results(&ev);
    assert_eq!(r[0].1, "", "a read with no output text");
    assert!(r[1].1.ends_with("readme.txt: edited"), "{}", r[1].1);
    let structured = r[1].3.expect("structured");
    assert!(structured.contains(r#""type":"diff""#) && structured.contains("oldText"));
    // No permission request: workspace edits are not gated in default mode.
    assert!(!ev
        .iter()
        .any(|e| matches!(e, ProviderEvent::PermissionRequest { .. })));
}

/// Cancel mid-stream: the text so far is kept, the turn is interrupted (not an error), and the
/// next prompt in the same process starts a fresh turn.
#[test]
fn interrupt_keeps_text_and_the_process_usable() {
    let ev = run("interrupt");
    assert_eq!(
        shape(&ev),
        [
            "think",
            "delta",
            "text:Silas measured his life in turns of a winding key and gallons of kerosene. For thirty-two years, the light",
            "turn:interrupted",
            "think",
            "delta",
            "text:still alive",
            "turn:ok"
        ]
    );
    let u = usages(&ev);
    assert_eq!(u[0].output_tokens, 3682);
    // The thinking estimate restarts with the new turn.
    let thinks: Vec<u64> = ev
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::ThinkingProgress { estimated_tokens } => Some(*estimated_tokens),
            _ => None,
        })
        .collect();
    let (max, last) = (thinks.iter().max(), thinks.last());
    assert!(last < max, "{thinks:?}");
}

/// Answering `cancelled` then sending `session/cancel` ends the turn as `cancelled`; the pending
/// tool call fails.
#[test]
fn interrupt_with_a_pending_permission() {
    let ev = run("interrupt_pending_permission");
    assert_eq!(
        shape(&ev),
        [
            "use:run_command",
            "perm:run_command",
            "result:err",
            "turn:interrupted"
        ]
    );
    let frames = lines("interrupt_pending_permission");
    let reply = frames
        .iter()
        .find(|(d, f)| d == "in" && f["id"] == "1")
        .map(|(_, f)| f)
        .expect("reply");
    assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
}

/// A second prompt mid-turn resolves the first at once; only the last one ends the turn.
#[test]
fn steer_ends_one_turn_and_sums_usage() {
    let r = replay("steer");
    assert_eq!(r.prompt_results.len(), 2);
    assert_eq!(
        shape(&r.events),
        [
            "think",
            "use:run_command",
            "perm:run_command",
            "result:ok",
            "think",
            "delta",
            "text:Standing resolutely against the churning sea, the solitary lighthouse casts its luminous beam across the dark waters to guide sailors home safely.\n\nBANANA",
            "turn:ok"
        ]
    );
    let u = usages(&r.events);
    assert_eq!(u.len(), 1);
    assert_eq!(u[0].input_tokens, 10093 + 2455);
    assert_eq!(u[0].output_tokens, 244 + 229);
    assert_eq!(u[0].cache_read_tokens, 8045);
}

/// `session/resume` does not replay history and keeps the conversation; the model falls back to
/// the CLI default, so it must be set again.
#[test]
fn resume_has_no_replay_and_keeps_context() {
    let ev = run("resume");
    assert_eq!(
        shape(&ev),
        [
            "delta",
            "text:OK",
            "turn:ok",
            "delta",
            "text:kumquat7",
            "turn:ok"
        ]
    );
    let all = lines("resume");
    let resume_at = all
        .iter()
        .position(|(d, f)| d == "in" && f["method"] == "session/resume")
        .expect("resume request");
    let prompt_at = all
        .iter()
        .rposition(|(d, f)| d == "in" && f["method"] == "session/prompt")
        .expect("prompt");
    for (dir, f) in &all[resume_at..prompt_at] {
        let kind = f["params"]["update"]["sessionUpdate"]
            .as_str()
            .unwrap_or("");
        assert!(
            dir != "out" || !kind.ends_with("_chunk") && kind != "tool_call",
            "replayed history: {kind}"
        );
    }
    let resumed = result_of("resume", "session/resume");
    assert_eq!(
        resumed["result"]["models"]["currentModelId"],
        "gemini-4-neon-high"
    );
    let model = resumed["result"]["configOptions"]
        .as_array()
        .expect("options")
        .iter()
        .find(|o| o["id"] == "model")
        .expect("model option");
    assert_eq!(model["currentValue"], "gemini-4-neon");
}

#[test]
fn no_fixture_leaks_the_user_or_the_home_directory() {
    for name in ALL {
        let path = format!("{}/fixtures/{name}.ndjson", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(path).expect("fixture");
        assert!(!text.contains("/home/"), "{name}");
        assert!(!text.contains("/Users/"), "{name}");
        assert!(!text.contains('@'), "{name}");
        assert!(!text.contains("available_commands_update"), "{name}");
    }
}
