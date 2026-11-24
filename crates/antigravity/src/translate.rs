//! Pure ACP -> `ProviderEvent` translation. No I/O: the process task feeds it `session/update`
//! params, permission requests and `session/prompt` responses, and sends what comes back.

use std::collections::HashSet;

use serde_json::{json, Value};
use xade_core::{ProviderEvent, UsageTotals};

#[derive(Debug, Default)]
pub struct Translator {
    /// Streamed assistant text not yet finalized as an `AssistantText`.
    text_buf: String,
    thought_chars: usize,
    seen_tools: HashSet<String>,
    finished_tools: HashSet<String>,
    context_used: u64,
    context_size: u64,
    /// Sum over every `session/prompt` response of the running turn (a steer adds a second one).
    turn_usage: UsageTotals,
}

impl Translator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handles the `params.update` object of one `session/update` notification.
    pub fn on_update(&mut self, update: &Value) -> Vec<ProviderEvent> {
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut out = Vec::new();
        match kind {
            "agent_message_chunk" => {
                if let Some(text) = chunk_text(update).filter(|t| !t.is_empty()) {
                    self.text_buf.push_str(text);
                    out.push(ProviderEvent::TextDelta(text.to_string()));
                }
            }
            "agent_thought_chunk" => {
                self.flush(&mut out);
                if let Some(text) = chunk_text(update) {
                    self.thought_chars += text.chars().count();
                    out.push(ProviderEvent::ThinkingProgress {
                        estimated_tokens: (self.thought_chars / 4) as u64,
                    });
                }
            }
            "tool_call" => {
                self.flush(&mut out);
                self.tool(update, &mut out);
            }
            "tool_call_update" => self.tool(update, &mut out),
            "usage_update" => {
                self.context_used = update.get("used").and_then(Value::as_u64).unwrap_or(0);
                self.context_size = update.get("size").and_then(Value::as_u64).unwrap_or(0);
            }
            other => tracing::debug!(update = other, "skipped ACP update"),
        }
        out
    }

    /// Handles the params of a `session/request_permission` request that must reach the user
    /// (Auto OFF). `key` is [`crate::rpc::id_key`] of the request id.
    pub fn on_permission_request(&mut self, key: String, params: &Value) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        self.flush(&mut out);
        let call = params.get("toolCall").unwrap_or(&Value::Null);
        self.ensure_tool_use(call, &mut out);
        out.push(ProviderEvent::PermissionRequest {
            request_id: key,
            tool_use_id: str_field(call, "toolCallId"),
            tool_name: str_field(call, "name"),
            input_json: tool_input(call),
        });
        out
    }

    /// Handles a successful `session/prompt` response. `last` is true when no other prompt is
    /// still outstanding; `interrupted` is the caller's interrupt flag.
    pub fn on_prompt_result(
        &mut self,
        result: &Value,
        last: bool,
        interrupted: bool,
    ) -> Vec<ProviderEvent> {
        let usage = result.get("usage").unwrap_or(&Value::Null);
        self.turn_usage.input_tokens += u64_field(usage, "inputTokens");
        self.turn_usage.output_tokens += u64_field(usage, "outputTokens");
        self.turn_usage.cache_read_tokens += u64_field(usage, "cachedReadTokens");
        if !last {
            return vec![];
        }
        let mut out = Vec::new();
        self.flush(&mut out);
        let stop = result
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("end_turn");
        let interrupted = interrupted || stop == "cancelled";
        let is_error = !interrupted && stop != "end_turn";
        if is_error {
            out.push(ProviderEvent::Error(format!("Antigravity stopped: {stop}")));
        }
        out.push(self.turn_complete(is_error, interrupted));
        out
    }

    /// Handles a JSON-RPC error answering `session/prompt`.
    pub fn on_prompt_error(&mut self, msg: &str, last: bool) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        self.flush(&mut out);
        out.push(ProviderEvent::Error(msg.to_string()));
        if last {
            out.push(self.turn_complete(true, false));
        }
        out
    }

    /// Finalizes buffered text as an `AssistantText`.
    pub fn flush_text(&mut self) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        self.flush(&mut out);
        out
    }

    fn flush(&mut self, out: &mut Vec<ProviderEvent>) {
        if !self.text_buf.is_empty() {
            out.push(ProviderEvent::AssistantText {
                text: std::mem::take(&mut self.text_buf),
            });
        }
    }

    fn turn_complete(&mut self, is_error: bool, interrupted: bool) -> ProviderEvent {
        let mut usage = std::mem::take(&mut self.turn_usage);
        usage.context_tokens_used = self.context_used;
        usage.context_window_max = self.context_size;
        self.thought_chars = 0;
        ProviderEvent::TurnComplete {
            usage,
            is_error,
            interrupted,
        }
    }

    /// `tool_call` and `tool_call_update` share one shape: announce the tool once, and finish it once
    /// when its status is terminal (anything but `pending` / `in_progress`).
    fn tool(&mut self, update: &Value, out: &mut Vec<ProviderEvent>) {
        let id = str_field(update, "toolCallId");
        if id.is_empty() {
            return;
        }
        self.ensure_tool_use(update, out);
        let Some(status) = update.get("status").and_then(Value::as_str) else {
            return;
        };
        if matches!(status, "pending" | "in_progress") || self.finished_tools.contains(&id) {
            return;
        }
        self.finished_tools.insert(id.clone());
        // The text so far was shown before this result.
        self.flush(out);
        out.push(ProviderEvent::ToolResult {
            tool_use_id: id,
            output: tool_output(update),
            is_error: status != "completed",
            structured_json: update
                .get("content")
                .map(|c| json!({ "content": c }).to_string()),
        });
    }

    /// Emits the `ToolUse` the first time a tool call id is seen, however it first appears.
    fn ensure_tool_use(&mut self, call: &Value, out: &mut Vec<ProviderEvent>) {
        let id = str_field(call, "toolCallId");
        if id.is_empty() || !self.seen_tools.insert(id.clone()) {
            return;
        }
        self.flush(out);
        out.push(ProviderEvent::ToolUse {
            tool_use_id: id,
            name: str_field(call, "name"),
            input_json: tool_input(call),
        });
    }
}

fn chunk_text(update: &Value) -> Option<&str> {
    update.get("content")?.get("text")?.as_str()
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn u64_field(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// `rawInput`, else `args`, else `{}`.
fn tool_input(call: &Value) -> String {
    call.get("rawInput")
        .or_else(|| call.get("args"))
        .filter(|v| !v.is_null())
        .map_or_else(|| "{}".to_string(), Value::to_string)
}

/// The text of a finished tool call. Never the raw JSON of `rawOutput`.
/// Order: `rawOutput.output`, `rawOutput` when it is itself a string (a denied command), the text
/// blocks of `content`, then `"<path>: edited"` for each diff block, else empty.
fn tool_output(update: &Value) -> String {
    let raw = update.get("rawOutput");
    if let Some(s) = raw.and_then(|r| r.get("output")).and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(s) = raw.and_then(Value::as_str) {
        return s.to_string();
    }
    let blocks = update
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let texts: Vec<&str> = blocks
        .iter()
        .filter_map(|b| b.get("content")?.get("text")?.as_str())
        .collect();
    if !texts.is_empty() {
        return texts.join("\n");
    }
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("diff"))
        .filter_map(|b| b.get("path").and_then(Value::as_str))
        .map(|p| format!("{p}: edited"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &str) -> Value {
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": t}})
    }

    fn call(id: &str, status: &str) -> Value {
        json!({"sessionUpdate": "tool_call", "toolCallId": id, "name": "run_command",
               "rawInput": {"CommandLine": "ls"}, "status": status})
    }

    fn update(id: &str, status: &str) -> Value {
        json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "name": "run_command",
               "status": status, "rawOutput": {"output": "ok"}})
    }

    #[test]
    fn text_is_flushed_before_a_tool_call_and_a_thought() {
        let mut t = Translator::new();
        assert_eq!(
            t.on_update(&text("a")),
            vec![ProviderEvent::TextDelta("a".into())]
        );
        t.on_update(&text("b"));
        let ev = t.on_update(&call("c1", "in_progress"));
        assert_eq!(
            ev,
            vec![
                ProviderEvent::AssistantText { text: "ab".into() },
                ProviderEvent::ToolUse {
                    tool_use_id: "c1".into(),
                    name: "run_command".into(),
                    input_json: r#"{"CommandLine":"ls"}"#.into(),
                },
            ]
        );
        t.on_update(&text("x"));
        let ev = t.on_update(&json!({"sessionUpdate": "agent_thought_chunk",
                                       "content": {"type": "text", "text": "12345678"}}));
        assert_eq!(
            ev,
            vec![
                ProviderEvent::AssistantText { text: "x".into() },
                ProviderEvent::ThinkingProgress {
                    estimated_tokens: 2
                },
            ]
        );
    }

    #[test]
    fn thought_estimate_counts_chars_not_bytes_and_resets_each_turn() {
        let mut t = Translator::new();
        let think = |s: &str| json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": s}});
        // 8 chars, 16 bytes: 2 tokens, not 4.
        let ev = t.on_update(&think("éééééééé"));
        assert_eq!(
            ev,
            vec![ProviderEvent::ThinkingProgress {
                estimated_tokens: 2
            }]
        );
        t.on_prompt_result(&json!({"stopReason": "end_turn"}), true, false);
        let ev = t.on_update(&think("abcd"));
        assert_eq!(
            ev,
            vec![ProviderEvent::ThinkingProgress {
                estimated_tokens: 1
            }]
        );
    }

    #[test]
    fn an_update_for_an_unseen_tool_synthesizes_its_tool_use() {
        let mut t = Translator::new();
        let ev = t.on_update(&update("c9", "completed"));
        assert_eq!(ev.len(), 2);
        assert!(
            matches!(&ev[0], ProviderEvent::ToolUse { tool_use_id, .. } if tool_use_id == "c9")
        );
        assert!(
            matches!(&ev[1], ProviderEvent::ToolResult { is_error: false, output, .. } if output == "ok")
        );
        // A pending update for an unseen id announces the tool too, but finishes nothing.
        let ev = t.on_update(&update("c10", "pending"));
        assert!(matches!(ev.as_slice(), [ProviderEvent::ToolUse { .. }]));
    }

    #[test]
    fn a_tool_is_announced_once_and_finished_once() {
        let mut t = Translator::new();
        assert_eq!(t.on_update(&call("c1", "in_progress")).len(), 1);
        assert!(t.on_update(&call("c1", "in_progress")).is_empty());
        assert!(t.on_update(&update("c1", "pending")).is_empty());
        assert!(t.on_update(&update("c1", "in_progress")).is_empty());
        assert_eq!(t.on_update(&update("c1", "completed")).len(), 1);
        assert!(t.on_update(&update("c1", "completed")).is_empty());
        assert!(t.on_update(&update("c1", "failed")).is_empty());
    }

    #[test]
    fn any_other_terminal_status_is_an_error_result() {
        for status in ["failed", "cancelled", "something_new"] {
            let mut t = Translator::new();
            let ev = t.on_update(&update("c1", status));
            assert!(
                matches!(
                    ev.last(),
                    Some(ProviderEvent::ToolResult { is_error: true, .. })
                ),
                "{status}"
            );
        }
    }

    #[test]
    fn tool_output_prefers_text_then_diff_and_never_dumps_raw_json() {
        let mut t = Translator::new();
        let out = |t: &mut Translator, id: &str, raw: Value, content: Option<Value>| {
            let mut u = json!({"sessionUpdate": "tool_call_update", "toolCallId": id,
                               "status": "completed", "rawOutput": raw});
            if let Some(c) = content {
                u["content"] = c;
            }
            match t.on_update(&u).pop() {
                Some(ProviderEvent::ToolResult {
                    output,
                    structured_json,
                    ..
                }) => (output, structured_json),
                other => panic!("{other:?}"),
            }
        };
        let text_blocks = json!([
            {"type": "content", "content": {"type": "text", "text": "a"}},
            {"type": "content", "content": {"type": "text", "text": "b"}}
        ]);
        let (o, s) = out(&mut t, "1", json!({}), Some(text_blocks.clone()));
        assert_eq!(o, "a\nb");
        assert_eq!(s, Some(json!({"content": text_blocks}).to_string()));
        let diff = json!([{"type": "diff", "path": "/x/a.txt", "oldText": "o", "newText": "n"},
                          {"type": "diff", "path": "/x/b.txt", "oldText": "o", "newText": "n"}]);
        assert_eq!(
            out(&mut t, "2", json!({}), Some(diff)).0,
            "/x/a.txt: edited\n/x/b.txt: edited"
        );
        assert_eq!(
            out(&mut t, "3", json!("denied"), None),
            ("denied".into(), None)
        );
        assert_eq!(
            out(&mut t, "4", json!({"output": "", "exit_code": 1}), None).0,
            ""
        );
        assert_eq!(out(&mut t, "5", json!({"weird": 1}), None).0, "");
    }

    #[test]
    fn tool_input_falls_back_to_args_then_empty_object() {
        assert_eq!(tool_input(&json!({"args": {"a": 1}})), r#"{"a":1}"#);
        assert_eq!(
            tool_input(&json!({"rawInput": {"b": 2}, "args": {"a": 1}})),
            r#"{"b":2}"#
        );
        assert_eq!(tool_input(&json!({"toolCallId": "x"})), "{}");
    }

    #[test]
    fn unknown_updates_are_skipped() {
        let mut t = Translator::new();
        for kind in [
            "session_info_update",
            "config_option_update",
            "user_message_chunk",
            "available_commands_update",
            "brand_new",
        ] {
            assert!(
                t.on_update(&json!({"sessionUpdate": kind})).is_empty(),
                "{kind}"
            );
        }
        assert!(t.on_update(&json!({})).is_empty());
    }

    #[test]
    fn a_permission_request_flushes_text_and_announces_an_unseen_tool() {
        let mut t = Translator::new();
        t.on_update(&text("about to run"));
        let params = json!({"sessionId": "s", "toolCall": {
            "toolCallId": "c1", "name": "run_command", "rawInput": {"CommandLine": "ls"}},
            "options": []});
        let ev = t.on_permission_request("1".into(), &params);
        assert_eq!(ev.len(), 3);
        assert!(matches!(&ev[0], ProviderEvent::AssistantText { text } if text == "about to run"));
        assert!(matches!(&ev[1], ProviderEvent::ToolUse { .. }));
        assert_eq!(
            ev[2],
            ProviderEvent::PermissionRequest {
                request_id: "1".into(),
                tool_use_id: "c1".into(),
                tool_name: "run_command".into(),
                input_json: r#"{"CommandLine":"ls"}"#.into(),
            }
        );
        // Seen already: only the request.
        let ev = t.on_permission_request("2".into(), &params);
        assert!(matches!(
            ev.as_slice(),
            [ProviderEvent::PermissionRequest { .. }]
        ));
    }

    fn result(stop: &str, input: u64, output: u64, cached: u64) -> Value {
        json!({"stopReason": stop, "usage": {"inputTokens": input, "outputTokens": output,
                                              "cachedReadTokens": cached}})
    }

    #[test]
    fn turn_end_flushes_text_and_reports_usage_and_context() {
        let mut t = Translator::new();
        t.on_update(&json!({"sessionUpdate": "usage_update", "used": 500, "size": 1000}));
        t.on_update(&text("hi"));
        let ev = t.on_prompt_result(&result("end_turn", 10, 2, 7), true, false);
        assert_eq!(
            ev,
            vec![
                ProviderEvent::AssistantText { text: "hi".into() },
                ProviderEvent::TurnComplete {
                    usage: UsageTotals {
                        context_tokens_used: 500,
                        context_window_max: 1000,
                        input_tokens: 10,
                        output_tokens: 2,
                        cache_read_tokens: 7,
                        cost_usd: 0.0,
                    },
                    is_error: false,
                    interrupted: false,
                },
            ]
        );
    }

    #[test]
    fn a_superseded_prompt_adds_usage_and_ends_nothing() {
        let mut t = Translator::new();
        assert!(t
            .on_prompt_result(&result("end_turn", 10, 1, 0), false, false)
            .is_empty());
        let ev = t.on_prompt_result(&result("end_turn", 5, 2, 3), true, false);
        match ev.as_slice() {
            [ProviderEvent::TurnComplete {
                usage,
                is_error: false,
                interrupted: false,
            }] => {
                assert_eq!(
                    (
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cache_read_tokens
                    ),
                    (15, 3, 3)
                );
            }
            other => panic!("{other:?}"),
        }
        // The next turn starts from zero.
        let ev = t.on_prompt_result(&result("end_turn", 1, 1, 1), true, false);
        assert!(
            matches!(&ev[0], ProviderEvent::TurnComplete { usage, .. } if usage.input_tokens == 1)
        );
    }

    #[test]
    fn cancel_is_interrupted_not_an_error() {
        let mut t = Translator::new();
        let ev = t.on_prompt_result(&result("cancelled", 1, 1, 0), true, false);
        assert!(matches!(
            ev.as_slice(),
            [ProviderEvent::TurnComplete {
                is_error: false,
                interrupted: true,
                ..
            }]
        ));
        // The caller's flag alone also marks it interrupted, whatever the stop reason.
        let ev = t.on_prompt_result(&result("refusal", 1, 1, 0), true, true);
        assert!(matches!(
            ev.as_slice(),
            [ProviderEvent::TurnComplete {
                is_error: false,
                interrupted: true,
                ..
            }]
        ));
    }

    #[test]
    fn any_other_stop_reason_is_an_error_turn() {
        let mut t = Translator::new();
        let ev = t.on_prompt_result(&result("max_tokens", 1, 1, 0), true, false);
        assert_eq!(
            ev[0],
            ProviderEvent::Error("Antigravity stopped: max_tokens".into())
        );
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
    fn a_prompt_error_ends_the_turn_only_when_it_is_the_last() {
        let mut t = Translator::new();
        t.on_update(&text("partial"));
        let ev = t.on_prompt_error("boom", false);
        assert_eq!(
            ev,
            vec![
                ProviderEvent::AssistantText {
                    text: "partial".into()
                },
                ProviderEvent::Error("boom".into()),
            ]
        );
        let ev = t.on_prompt_error("boom", true);
        assert_eq!(ev.len(), 2);
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
    fn flush_text_is_empty_without_text() {
        let mut t = Translator::new();
        assert!(t.flush_text().is_empty());
        t.on_update(&text("a"));
        assert_eq!(
            t.flush_text(),
            vec![ProviderEvent::AssistantText { text: "a".into() }]
        );
        assert!(t.flush_text().is_empty());
    }
}
