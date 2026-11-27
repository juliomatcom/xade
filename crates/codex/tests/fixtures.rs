//! Replays each recorded Codex app-server fixture through `Translator`, asserting the
//! event sequence and protocol facts verified against codex-cli 0.172.0 (`gpt-6-luna`).

#![allow(clippy::expect_used)]

use std::collections::HashMap;

use serde_json::{json, Value};
use xade_codex::import::{parse_thread, should_import_thread};
use xade_codex::models::{parse_account, parse_models, AccountStatus};
use xade_codex::rpc::{self, Incoming};
use xade_codex::Translator;
use xade_core::{Effort, ProviderEvent, Role, UsageTotals};

const ALL: [&str; 16] = [
    "account",
    "auto",
    "bad_model",
    "bad_resume",
    "edit",
    "edit_update",
    "import",
    "interrupt",
    "interrupt_pending_permission",
    "resume",
    "simple_turn",
    "steer",
    "thinking",
    "tool_allow",
    "tool_deny",
    "two_turns",
];

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
    thread_id: String,
    methods: HashMap<u64, String>,
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
            if let (Some(id), Some(method)) = (frame["id"].as_u64(), frame["method"].as_str()) {
                self.methods.insert(id, method.to_string());
            }
            return;
        }
        match rpc::parse_frame(frame) {
            Some(Incoming::Response { id, result }) => {
                let Some(id) = id.as_u64() else { return };
                let method = self.methods.get(&id).cloned().unwrap_or_default();
                if matches!(method.as_str(), "thread/start" | "thread/resume") {
                    if let Ok(r) = result {
                        if let Some(tid) = r.pointer("/thread/id").and_then(Value::as_str) {
                            self.thread_id = tid.to_string();
                        }
                    }
                }
            }
            Some(Incoming::Notification { method, params }) => {
                if !self.thread_id.is_empty() && rpc::for_thread(&params, &self.thread_id) {
                    self.events.extend(self.t.on_notification(&method, &params));
                }
            }
            Some(Incoming::Request { id, method, params })
                if matches!(
                    method.as_str(),
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
                ) && !self.thread_id.is_empty()
                    && rpc::for_thread(&params, &self.thread_id) =>
            {
                self.events.extend(
                    self.t
                        .on_approval_request(rpc::id_key(&id), &method, &params),
                );
            }
            _ => {}
        }
    }
}

fn replay(name: &str) -> Replay {
    let mut r = Replay::default();
    for (dir, frame) in lines(name) {
        r.feed(&dir, &frame);
    }
    r
}

fn run(name: &str) -> Vec<ProviderEvent> {
    replay(name).events
}

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
fn every_fixture_parses_and_ends_with_no_pending_text() {
    for name in ALL {
        let mut r = replay(name);
        assert!(r.t.flush_text().is_empty(), "{name}: text left unflushed");
        for (dir, frame) in lines(name) {
            if dir == "out" {
                assert!(rpc::parse_frame(&frame).is_some(), "{name}: {frame}");
            }
        }
    }
}

#[test]
fn simple_turn() {
    let ev = run("simple_turn");
    assert_eq!(shape(&ev), ["delta", "text:pong", "turn:ok"]);
    let u = usages(&ev)[0];
    assert_eq!(
        (u.input_tokens, u.output_tokens, u.cache_read_tokens),
        (13399, 5, 0)
    );
    assert_eq!(
        (u.context_tokens_used, u.context_window_max),
        (13404, 258400)
    );
    assert_eq!(u.cost_usd, 0.0);
}

#[test]
fn two_turns_report_per_turn_usage_deltas() {
    let ev = run("two_turns");
    assert_eq!(
        shape(&ev),
        [
            "delta",
            "text:ok",
            "turn:ok",
            "delta",
            "text:zebra",
            "turn:ok"
        ]
    );
    let u = usages(&ev);
    assert_eq!(u.len(), 2);
    assert_eq!(
        (
            u[0].input_tokens,
            u[0].cache_read_tokens,
            u[0].output_tokens
        ),
        (13438, 0, 5)
    );
    assert_eq!(
        (
            u[1].input_tokens,
            u[1].cache_read_tokens,
            u[1].output_tokens
        ),
        (26, 13435, 17)
    );
    assert_eq!(u[1].context_tokens_used, 13478);
}

#[test]
fn thinking_emits_progress_and_final_answer() {
    let ev = run("thinking");
    assert_eq!(shape(&ev), ["think", "delta", "text:391", "turn:ok"]);
    assert!(ev.iter().any(|e| matches!(
        e,
        ProviderEvent::ThinkingProgress { estimated_tokens } if *estimated_tokens > 0
    )));
}

#[test]
fn tool_allow_maps_command_to_bash_and_approves() {
    let ev = run("tool_allow");
    assert_eq!(
        shape(&ev),
        [
            "delta",
            "text:I’ll create the requested file in the current workspace.",
            "use:Bash",
            "perm:Bash",
            "result:ok",
            "delta",
            "text:Created `made_by_shell.txt`.",
            "turn:ok",
        ]
    );
    let Some(ProviderEvent::ToolUse {
        tool_use_id,
        input_json,
        ..
    }) = ev
        .iter()
        .find(|e| matches!(e, ProviderEvent::ToolUse { .. }))
    else {
        panic!("no tool use");
    };
    let parsed: Value = serde_json::from_str(input_json).expect("json");
    assert_eq!(
        parsed,
        json!({"command": "touch made_by_shell.txt", "description": ""})
    );
    let Some(ProviderEvent::PermissionRequest {
        request_id,
        tool_use_id: perm_tid,
        tool_name,
        input_json: perm_in,
    }) = ev
        .iter()
        .find(|e| matches!(e, ProviderEvent::PermissionRequest { .. }))
    else {
        panic!("no permission request");
    };
    assert_eq!(request_id, "0");
    assert_eq!(perm_tid, tool_use_id);
    assert_eq!(tool_name, "Bash");
    assert_eq!(perm_in, input_json);

    let u = usages(&ev)[0];
    assert_eq!(
        (u.input_tokens, u.cache_read_tokens, u.output_tokens),
        (26944 - 13418, 13418, 86)
    );
}

#[test]
fn tool_deny_marks_tool_as_error_and_completes_turn() {
    let ev = run("tool_deny");
    assert_eq!(
        shape(&ev),
        [
            "delta",
            "text:Running the requested command in the workspace.",
            "use:Bash",
            "perm:Bash",
            "result:err",
            "delta",
            "text:The command was rejected by the execution approval system, so `made_by_shell.txt` was not created.",
            "turn:ok",
        ]
    );
}

#[test]
fn edit_add_maps_to_write_with_create_structured_json() {
    let ev = run("edit");
    assert_eq!(
        shape(&ev),
        [
            "delta",
            "text:I’ll add `hello.txt` with exactly that one line.",
            "use:Write",
            "perm:Write",
            "result:ok",
            "delta",
            "text:Created [hello.txt](/tmp/xade-codex-rec.8D2K/edit/hello.txt) with the single line `hi`.",
            "turn:ok",
        ]
    );
    let Some(ProviderEvent::ToolUse {
        name, input_json, ..
    }) = ev
        .iter()
        .find(|e| matches!(e, ProviderEvent::ToolUse { .. }))
    else {
        panic!("no tool use");
    };
    assert_eq!(name, "Write");
    let input: Value = serde_json::from_str(input_json).expect("json");
    assert_eq!(
        input["file_path"],
        "/tmp/xade-codex-rec.8D2K/edit/hello.txt"
    );
    assert_eq!(input["content"], "hi\n");

    let Some(ProviderEvent::PermissionRequest {
        tool_name,
        input_json: perm_in,
        ..
    }) = ev
        .iter()
        .find(|e| matches!(e, ProviderEvent::PermissionRequest { .. }))
    else {
        panic!("no perm");
    };
    assert_eq!(tool_name, "Write");
    assert_eq!(perm_in, input_json);

    let Some(ProviderEvent::ToolResult {
        output,
        is_error,
        structured_json,
        ..
    }) = ev
        .iter()
        .find(|e| matches!(e, ProviderEvent::ToolResult { .. }))
    else {
        panic!("no result");
    };
    assert!(!is_error);
    assert_eq!(output, "/tmp/xade-codex-rec.8D2K/edit/hello.txt: edited");
    let structured: Value =
        serde_json::from_str(structured_json.as_deref().expect("structured")).expect("json");
    assert_eq!(
        structured,
        json!({"type": "create", "structuredPatch": [], "originalFile": null})
    );
}

#[test]
fn edit_update_maps_to_edit_with_parsed_structured_patch() {
    let ev = run("edit_update");
    assert_eq!(
        shape(&ev),
        [
            "delta",
            "text:I’ll locate the line in `notes.txt` and make the one-line edit.",
            "use:Bash",
            "perm:Bash",
            "result:ok",
            "use:Edit",
            "perm:Edit",
            "result:ok",
            "delta",
            "text:Replaced `beta` with `BETA` in [notes.txt](/tmp/xade-codex-rec.Y7KF/edit_update/notes.txt:2).",
            "turn:ok",
        ]
    );
    let results: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::ToolResult {
                output,
                structured_json,
                is_error,
                ..
            } => Some((output.as_str(), structured_json.as_deref(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0], ("2:beta\n", None, false));
    assert_eq!(
        results[1].0,
        "/tmp/xade-codex-rec.Y7KF/edit_update/notes.txt: edited"
    );
    assert!(!results[1].2);
    let structured: Value = serde_json::from_str(results[1].1.expect("structured")).expect("json");
    assert_eq!(
        structured["structuredPatch"],
        json!([{
            "oldStart": 1,
            "newStart": 1,
            "lines": [" alpha", "-beta", "+BETA", " gamma"]
        }])
    );
}

#[test]
fn auto_runs_command_with_no_approval_request() {
    let ev = run("auto");
    assert_eq!(
        shape(&ev),
        [
            "delta",
            "text:I’ll run the requested command now.",
            "use:Bash",
            "result:ok",
            "delta",
            "text:done",
            "turn:ok",
        ]
    );
    assert!(!ev
        .iter()
        .any(|e| matches!(e, ProviderEvent::PermissionRequest { .. })));
}

#[test]
fn interrupt_flushes_streamed_text_and_marks_turn_interrupted() {
    let ev = run("interrupt");
    assert_eq!(shape(&ev), ["delta", "text:# The", "turn:interrupted"]);
}

#[test]
fn interrupt_pending_permission_ends_as_interrupted() {
    let ev = run("interrupt_pending_permission");
    assert_eq!(
        shape(&ev),
        [
            "delta",
            "text:I’ll create `pending.txt` in the current workspace now.",
            "use:Bash",
            "perm:Bash",
            "turn:interrupted",
        ]
    );
}

#[test]
fn steer_flushes_aborted_message_and_completes_single_turn() {
    let ev = run("steer");
    assert_eq!(
        shape(&ev),
        [
            "delta",
            "text:1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n",
            "delta",
            "text:steered",
            "turn:ok",
        ]
    );
    assert_eq!(usages(&ev).len(), 1);
}

#[test]
fn bad_model_emits_error_event() {
    let ev = run("bad_model");
    assert_eq!(
        shape(&ev),
        ["error:The model `no-such-model` does not exist or you do not have access to it."]
    );
}

#[test]
fn resume_keeps_context_and_computes_second_turn_delta() {
    let ev = run("resume");
    assert_eq!(
        shape(&ev),
        [
            "delta",
            "text:ok",
            "turn:ok",
            "delta",
            "text:walrus",
            "turn:ok"
        ]
    );
    let u = usages(&ev);
    assert_eq!(u.len(), 2);
    assert_eq!(
        (
            u[0].input_tokens,
            u[0].cache_read_tokens,
            u[0].output_tokens
        ),
        (13422, 0, 5)
    );
    assert_eq!(
        (
            u[1].input_tokens,
            u[1].cache_read_tokens,
            u[1].output_tokens
        ),
        (26, 13419, 6)
    );
    assert_eq!(u[1].context_tokens_used, 13451);
}

#[test]
fn bad_resume_returns_no_rollout_error() {
    let f = result_of("bad_resume", "thread/resume");
    assert_eq!(f["error"]["code"], -32600);
    assert!(f["error"]["message"]
        .as_str()
        .expect("message")
        .contains("no rollout found"));
}

#[test]
fn account_fixture_parses_models_and_api_key_account() {
    let acct = result_of("account", "account/read");
    assert_eq!(
        parse_account(&acct["result"]),
        AccountStatus::LoggedIn(Some("API key".into()))
    );
    let rl = result_of("account", "account/rateLimits/read");
    assert_eq!(rl["error"]["code"], -32600);

    let list = result_of("account", "model/list");
    let (models, default_id) = parse_models(&list["result"]);
    assert_eq!(default_id.as_deref(), Some("gpt-6.2-sol"));
    assert!(models.iter().any(|m| m.id == "gpt-6-luna"));
    let sol = models.iter().find(|m| m.id == "gpt-6.2-sol").expect("sol");
    assert_eq!(
        sol.effort_levels,
        [
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max
        ]
    );
    let old = models.iter().find(|m| m.id == "gpt-5.5").expect("gpt-5.5");
    assert_eq!(
        old.effort_levels,
        [Effort::Low, Effort::Medium, Effort::High, Effort::XHigh]
    );
}

#[test]
fn import_fixture_maps_thread_list_and_thread_read() {
    let list = result_of("import", "thread/list");
    let summary = &list["result"]["data"][0];
    assert_eq!(summary["originator"], "xade");
    assert!(!should_import_thread(summary));

    let mut tui_summary = summary.clone();
    tui_summary["originator"] = json!("codex-tui");
    assert!(should_import_thread(&tui_summary));

    let read = result_of("import", "thread/read");
    let imp = parse_thread(summary, &read["result"]).expect("parsed imported session");
    assert_eq!(imp.provider_session_id, summary["id"].as_str().expect("id"));
    assert_eq!(imp.model_id, "gpt-6-luna");
    assert_eq!(
        imp.title,
        "Run the shell command `touch imported.txt` now. Do not ask…"
    );
    assert_eq!(imp.created_at.unix_timestamp(), 1_795_786_627);
    assert_eq!(imp.updated_at.unix_timestamp(), 1_795_786_631);
    assert_eq!(imp.usage, UsageTotals::default());

    let roles: Vec<Role> = imp.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            Role::User,
            Role::Assistant,
            Role::ToolCall,
            Role::ToolResult,
            Role::Assistant,
        ]
    );
    assert_eq!(
        imp.messages[0].content_text,
        "Run the shell command `touch imported.txt` now. Do not ask me anything."
    );
    assert_eq!(imp.messages[2].tool_name.as_deref(), Some("Bash"));
    let input: Value = serde_json::from_str(
        imp.messages[2]
            .tool_payload_json
            .as_deref()
            .expect("bash input"),
    )
    .expect("json");
    assert_eq!(
        input,
        json!({"command": "touch imported.txt", "description": ""})
    );
    assert_eq!(imp.messages[2].tool_use_id, imp.messages[3].tool_use_id);
    assert!(!imp.messages[3].is_error);
    assert_eq!(imp.messages[4].content_text, "Created `imported.txt`.");
    assert!(imp.messages[3].created_at > imp.messages[2].created_at);
}

#[test]
fn no_fixture_leaks_the_user_or_home_directory() {
    for name in ALL {
        let path = format!("{}/fixtures/{name}.ndjson", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(path).expect("fixture");
        assert!(!text.contains("/home/"), "{name}");
        assert!(!text.contains("/Users/"), "{name}");
    }
}
