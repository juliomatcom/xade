//! Feeds each recorded fixture's "out" frames through the Translator.

#![allow(clippy::expect_used)]

use serde_json::Value;
use std::sync::atomic::Ordering;
use xade_claude::frames::{parse_line, InFrame};
use xade_claude::Translator;
use xade_core::{ProviderEvent, QuotaKind};

fn load(name: &str) -> Vec<Value> {
    let path = format!("{}/fixtures/{name}.ndjson", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).expect("fixture");
    text.lines()
        .map(|l| serde_json::from_str::<Value>(l).expect("line"))
        .filter(|l| l["dir"] == "out")
        .map(|l| l["frame"].clone())
        .collect()
}

fn run_with(name: &str, t: &mut Translator) -> Vec<ProviderEvent> {
    load(name)
        .into_iter()
        .flat_map(|f| {
            let frame: InFrame = serde_json::from_value(f).expect("frame parses");
            t.on_frame(frame)
        })
        .collect()
}

fn run(name: &str) -> Vec<ProviderEvent> {
    run_with(name, &mut Translator::default())
}

fn turns(ev: &[ProviderEvent]) -> Vec<&ProviderEvent> {
    ev.iter()
        .filter(|e| matches!(e, ProviderEvent::TurnComplete { .. }))
        .collect()
}

#[test]
fn every_fixture_parses_without_error() {
    for name in [
        "edit_allow",
        "init_first",
        "interrupt",
        "resume",
        "simple_turn",
        "steer",
        "thinking_summaries",
        "thinking",
        "tool_allow",
        "tool_deny",
        "two_turns",
    ] {
        let path = format!("{}/fixtures/{name}.ndjson", env!("CARGO_MANIFEST_DIR"));
        for line in std::fs::read_to_string(path).expect("fixture").lines() {
            let v: Value = serde_json::from_str(line).expect("line");
            if v["dir"] == "out" {
                parse_line(&v["frame"].to_string()).expect(name);
            }
        }
        let _ = run(name);
    }
}

#[test]
fn simple_turn() {
    let ev = run("simple_turn");
    assert!(
        matches!(&ev[0], ProviderEvent::Started { model_id, .. } if model_id.contains("haiku"))
    );
    assert!(ev.iter().any(|e| matches!(e, ProviderEvent::TextDelta(_))));
    assert!(ev
        .iter()
        .any(|e| matches!(e, ProviderEvent::AssistantText { text } if text == "hello fixture")));
    let t = turns(&ev);
    assert_eq!(t.len(), 1);
    match t[0] {
        ProviderEvent::TurnComplete {
            usage,
            is_error,
            interrupted,
        } => {
            assert!(!is_error && !interrupted);
            assert!(usage.output_tokens > 0);
            assert!(usage.cost_usd > 0.0);
            assert!(usage.context_tokens_used > 0);
            assert_eq!(usage.context_window_max, 1_000_000);
        }
        _ => unreachable!(),
    }
    assert!(matches!(
        ev.last(),
        Some(ProviderEvent::TurnComplete { .. })
    ));
}

#[test]
fn two_turns() {
    let ev = run("two_turns");
    assert_eq!(turns(&ev).len(), 2);
    let starts = ev
        .iter()
        .filter(|e| matches!(e, ProviderEvent::Started { .. }))
        .count();
    assert_eq!(starts, 2);
}

/// A prompt written mid-turn is folded into that turn: two prompts in, one turn out.
#[test]
fn steer_mid_turn_prompt_ends_with_one_turn() {
    let path = format!("{}/fixtures/steer.ndjson", env!("CARGO_MANIFEST_DIR"));
    let prompts = std::fs::read_to_string(path)
        .expect("fixture")
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["dir"] == "in" && v["frame"]["type"] == "user")
        .count();
    assert_eq!(prompts, 2);
    let ev = run("steer");
    assert_eq!(turns(&ev).len(), 1);
    assert!(ev
        .iter()
        .any(|e| matches!(e, ProviderEvent::AssistantText { text } if text.contains("BANANA"))));
}

#[test]
fn tool_allow() {
    let ev = run("tool_allow");
    let reqs: Vec<&str> = ev
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::PermissionRequest { tool_name, .. } => Some(tool_name.as_str()),
            _ => None,
        })
        .collect();
    assert!(reqs.contains(&"Write"), "{reqs:?}");
    assert!(ev
        .iter()
        .any(|e| matches!(e, ProviderEvent::ToolUse { name, .. } if name == "Bash")));
    assert!(ev.iter().any(|e| matches!(
        e,
        ProviderEvent::ToolResult { structured_json: Some(j), .. } if j.contains("structuredPatch")
    )));
    // ToolUse precedes its PermissionRequest.
    let use_idx = ev
        .iter()
        .position(|e| matches!(e, ProviderEvent::ToolUse { name, .. } if name == "Write"))
        .expect("write tool use");
    let req_idx = ev
        .iter()
        .position(|e| matches!(e, ProviderEvent::PermissionRequest { tool_name, .. } if tool_name == "Write"))
        .expect("write request");
    assert!(use_idx < req_idx);
    assert!(ev
        .iter()
        .any(|e| matches!(e, ProviderEvent::AssistantText { text } if text == "DONE")));
}

#[test]
fn tool_deny() {
    let ev = run("tool_deny");
    assert!(ev
        .iter()
        .any(|e| matches!(e, ProviderEvent::ToolResult { is_error: true, .. })));
    assert!(matches!(
        turns(&ev)[0],
        ProviderEvent::TurnComplete {
            is_error: false,
            interrupted: false,
            ..
        }
    ));
}

#[test]
fn edit_allow() {
    let ev = run("edit_allow");
    assert!(ev.iter().any(
        |e| matches!(e, ProviderEvent::PermissionRequest { tool_name, input_json, .. }
            if tool_name == "Edit" && input_json.contains("old_string"))
    ));
    assert!(ev.iter().any(|e| matches!(
        e,
        ProviderEvent::ToolResult { structured_json: Some(j), .. } if j.contains("structuredPatch")
    )));
}

#[test]
fn interrupt() {
    let mut t = Translator::default();
    t.interrupt_flag().store(true, Ordering::SeqCst);
    let ev = run_with("interrupt", &mut t);
    let t = turns(&ev);
    assert_eq!(t.len(), 2);
    assert!(matches!(
        t[0],
        ProviderEvent::TurnComplete {
            is_error: false,
            interrupted: true,
            ..
        }
    ));
    assert!(matches!(
        t[1],
        ProviderEvent::TurnComplete {
            is_error: false,
            interrupted: false,
            ..
        }
    ));
    // The "[Request interrupted by user]" user text frame produces nothing.
    assert!(!ev
        .iter()
        .any(|e| matches!(e, ProviderEvent::ToolResult { .. })));
}

#[test]
fn interrupt_without_flag_is_an_error_turn() {
    let ev = run("interrupt");
    assert!(matches!(
        turns(&ev)[0],
        ProviderEvent::TurnComplete {
            is_error: true,
            interrupted: false,
            ..
        }
    ));
}

#[test]
fn rate_limit_quota() {
    let ev = run("simple_turn");
    let q: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::Quota(q) => Some(q),
            _ => None,
        })
        .collect();
    assert!(q.len() >= 2);
    assert_eq!(q[0].kind, QuotaKind::FiveHour);
    assert_eq!(q[1].kind, QuotaKind::SevenDay);
    assert!(q[0].utilization_pct > 0.0 && q[0].utilization_pct <= 100.0);
    assert!(q[0].resets_at.is_some());
}

#[test]
fn thinking_tokens() {
    let ev = run("tool_allow");
    assert!(ev
        .iter()
        .any(|e| matches!(e, ProviderEvent::ThinkingProgress { estimated_tokens } if *estimated_tokens > 0)));
    // Thinking blocks never become AssistantText.
    assert!(!ev
        .iter()
        .any(|e| matches!(e, ProviderEvent::AssistantText { text } if text.is_empty())));
}

#[test]
fn resume() {
    let ev = run("resume");
    assert_eq!(turns(&ev).len(), 1);
}
