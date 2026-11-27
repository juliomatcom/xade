//! Pure `codex app-server` -> `ProviderEvent` translation. No I/O: the process task feeds it
//! notifications and approval requests, and emits what comes back.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use xade_core::{ProviderEvent, UsageTotals};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TokenBreakdown {
    input: u64,
    cached_input: u64,
    output: u64,
}

impl TokenBreakdown {
    fn saturating_sub(self, other: Self) -> Self {
        Self {
            input: self.input.saturating_sub(other.input),
            cached_input: self.cached_input.saturating_sub(other.cached_input),
            output: self.output.saturating_sub(other.output),
        }
    }
}

#[derive(Debug, Clone)]
struct ToolMeta {
    name: String,
    input_json: String,
}

#[derive(Debug, Default)]
pub struct Translator {
    /// Streamed assistant text not yet finalized as an `AssistantText`.
    text_buf: String,
    /// Text of the current `agentMessage` already emitted by an early flush, so its
    /// `item/completed` emits only the rest.
    msg_flushed: String,
    thought_chars: usize,
    /// Metadata recorded on `item/started` so approvals and completion reuse the same tool shape.
    tools: HashMap<String, ToolMeta>,
    finished_tools: HashSet<String>,
    /// Cumulative `tokenUsage.total` from the latest `thread/tokenUsage/updated`.
    latest_total: Option<TokenBreakdown>,
    /// Baseline `tokenUsage.total` at the start of the active turn.
    turn_start_total: Option<TokenBreakdown>,
    /// `tokenUsage.last.totalTokens` from the latest `thread/tokenUsage/updated`.
    context_used: u64,
    /// `tokenUsage.modelContextWindow` from the latest `thread/tokenUsage/updated`.
    context_window: u64,
    /// True once an `error` notification (`willRetry: false`) has been emitted in the active turn.
    turn_had_error: bool,
}

impl Translator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handles one server notification (`method`, `params`) for the active thread.
    pub fn on_notification(&mut self, method: &str, params: &Value) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        match method {
            "turn/started" => {
                self.turn_start_total = self.latest_total;
                self.thought_chars = 0;
                self.turn_had_error = false;
            }
            "item/started" => {
                if let Some(item) = params.get("item") {
                    self.on_item_started(item, &mut out);
                }
            }
            "item/agentMessage/delta" => {
                if let Some(delta) = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty())
                {
                    self.text_buf.push_str(delta);
                    out.push(ProviderEvent::TextDelta(delta.to_string()));
                }
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                self.flush(&mut out);
                if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                    self.thought_chars += delta.chars().count();
                    out.push(ProviderEvent::ThinkingProgress {
                        estimated_tokens: (self.thought_chars / 4) as u64,
                    });
                }
            }
            "item/completed" => {
                if let Some(item) = params.get("item") {
                    self.on_item_completed(item, &mut out);
                }
            }
            "thread/tokenUsage/updated" => self.on_token_usage(params),
            "error" => self.on_error(params, &mut out),
            "turn/completed" => self.on_turn_completed(params, &mut out),
            other => tracing::debug!(method = other, "skipped Codex notification"),
        }
        out
    }

    /// Handles an approval request (`item/commandExecution/requestApproval` or
    /// `item/fileChange/requestApproval`) that must reach the user (Auto OFF).
    /// `key` is [`crate::rpc::id_key`] of the request id.
    pub fn on_approval_request(
        &mut self,
        key: String,
        method: &str,
        params: &Value,
    ) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        self.flush(&mut out);
        let item_id = str_field(params, "itemId");
        let meta = if let Some(existing) = self.tools.get(&item_id) {
            existing.clone()
        } else {
            let fallback = match method {
                "item/commandExecution/requestApproval" => ToolMeta {
                    name: "Bash".to_string(),
                    input_json: command_input_json(params),
                },
                _ => ToolMeta {
                    name: "Edit".to_string(),
                    input_json: "{}".to_string(),
                },
            };
            if !item_id.is_empty() {
                self.tools.insert(item_id.clone(), fallback.clone());
                out.push(ProviderEvent::ToolUse {
                    tool_use_id: item_id.clone(),
                    name: fallback.name.clone(),
                    input_json: fallback.input_json.clone(),
                });
            }
            fallback
        };
        out.push(ProviderEvent::PermissionRequest {
            request_id: key,
            tool_use_id: item_id,
            tool_name: meta.name,
            input_json: meta.input_json,
        });
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
            let text = std::mem::take(&mut self.text_buf);
            self.msg_flushed.push_str(&text);
            out.push(ProviderEvent::AssistantText { text });
        }
    }

    fn on_item_started(&mut self, item: &Value, out: &mut Vec<ProviderEvent>) {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "userMessage" | "agentMessage" | "reasoning" => {
                // Flush any prior agentMessage that never received `item/completed` (e.g. mid-steer).
                self.flush(out);
                if kind == "agentMessage" {
                    self.msg_flushed.clear();
                }
            }
            "commandExecution" => {
                self.flush(out);
                self.ensure_command_tool(item, out);
            }
            "fileChange" => {
                self.flush(out);
                self.ensure_file_change_tool(item, out);
            }
            other => tracing::debug!(item_type = other, "skipped Codex item/started"),
        }
    }

    fn on_item_completed(&mut self, item: &Value, out: &mut Vec<ProviderEvent>) {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "agentMessage" => {
                let buf = std::mem::take(&mut self.text_buf);
                let flushed = std::mem::take(&mut self.msg_flushed);
                let authoritative = item.get("text").and_then(Value::as_str).unwrap_or_default();
                let text = if authoritative.is_empty() {
                    buf
                } else if let Some(rest) = authoritative.strip_prefix(flushed.as_str()) {
                    rest.to_string()
                } else if flushed.is_empty() {
                    authoritative.to_string()
                } else {
                    // Part was already shown and the final text diverges: keep what streamed.
                    buf
                };
                if !text.is_empty() {
                    out.push(ProviderEvent::AssistantText { text });
                }
            }
            "commandExecution" => {
                let id = str_field(item, "id");
                if id.is_empty() || !self.finished_tools.insert(id.clone()) {
                    return;
                }
                self.ensure_command_tool(item, out);
                self.flush(out);
                let (output, is_error) = command_result(item);
                out.push(ProviderEvent::ToolResult {
                    tool_use_id: id,
                    output,
                    is_error,
                    structured_json: None,
                });
            }
            "fileChange" => {
                let id = str_field(item, "id");
                if id.is_empty() || !self.finished_tools.insert(id.clone()) {
                    return;
                }
                self.ensure_file_change_tool(item, out);
                self.flush(out);
                let (output, is_error, structured_json) = file_change_result(item);
                out.push(ProviderEvent::ToolResult {
                    tool_use_id: id,
                    output,
                    is_error,
                    structured_json,
                });
            }
            "userMessage" | "reasoning" => {}
            other => tracing::debug!(item_type = other, "skipped Codex item/completed"),
        }
    }

    fn ensure_command_tool(&mut self, item: &Value, out: &mut Vec<ProviderEvent>) {
        let id = str_field(item, "id");
        if id.is_empty() || self.tools.contains_key(&id) {
            return;
        }
        let meta = ToolMeta {
            name: "Bash".to_string(),
            input_json: command_input_json(item),
        };
        self.tools.insert(id.clone(), meta.clone());
        out.push(ProviderEvent::ToolUse {
            tool_use_id: id,
            name: meta.name,
            input_json: meta.input_json,
        });
    }

    fn ensure_file_change_tool(&mut self, item: &Value, out: &mut Vec<ProviderEvent>) {
        let id = str_field(item, "id");
        if id.is_empty() || self.tools.contains_key(&id) {
            return;
        }
        let (name, input_json) = file_change_tool(item);
        let meta = ToolMeta {
            name: name.to_string(),
            input_json,
        };
        self.tools.insert(id.clone(), meta.clone());
        out.push(ProviderEvent::ToolUse {
            tool_use_id: id,
            name: meta.name,
            input_json: meta.input_json,
        });
    }

    fn on_token_usage(&mut self, params: &Value) {
        let Some(tu) = params.get("tokenUsage") else {
            return;
        };
        let total = parse_breakdown(tu.get("total"));
        let last = parse_breakdown(tu.get("last"));
        if self.turn_start_total.is_none() {
            self.turn_start_total = Some(total.saturating_sub(last));
        }
        self.latest_total = Some(total);
        if let Some(last_total) = tu
            .get("last")
            .and_then(|l| l.get("totalTokens"))
            .and_then(Value::as_u64)
        {
            self.context_used = last_total;
        }
        if let Some(window) = tu.get("modelContextWindow").and_then(Value::as_u64) {
            self.context_window = window;
        }
    }

    fn on_error(&mut self, params: &Value, out: &mut Vec<ProviderEvent>) {
        if params
            .get("willRetry")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return;
        }
        self.flush(out);
        self.turn_had_error = true;
        let msg = params
            .pointer("/error/message")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("Codex error");
        out.push(ProviderEvent::Error(msg.to_string()));
    }

    fn on_turn_completed(&mut self, params: &Value, out: &mut Vec<ProviderEvent>) {
        self.flush(out);
        let turn = params.get("turn").unwrap_or(&Value::Null);
        let status = turn
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("completed");
        let interrupted = status == "interrupted";
        let is_error = status == "failed";
        if is_error && !self.turn_had_error {
            let msg = turn
                .pointer("/error/message")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("Codex turn failed");
            out.push(ProviderEvent::Error(msg.to_string()));
        }
        let delta = match (self.latest_total, self.turn_start_total) {
            (Some(end), Some(start)) => end.saturating_sub(start),
            (Some(end), None) => end,
            (None, _) => TokenBreakdown::default(),
        };
        let usage = UsageTotals {
            context_tokens_used: self.context_used,
            context_window_max: self.context_window,
            input_tokens: delta.input.saturating_sub(delta.cached_input),
            output_tokens: delta.output,
            cache_read_tokens: delta.cached_input,
            cost_usd: 0.0,
        };
        self.thought_chars = 0;
        self.turn_had_error = false;
        self.turn_start_total = self.latest_total;
        out.push(ProviderEvent::TurnComplete {
            usage,
            is_error,
            interrupted,
        });
    }
}

pub(crate) fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn u64_field(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn parse_breakdown(v: Option<&Value>) -> TokenBreakdown {
    let Some(v) = v else {
        return TokenBreakdown::default();
    };
    TokenBreakdown {
        input: u64_field(v, "inputTokens"),
        cached_input: u64_field(v, "cachedInputTokens"),
        output: u64_field(v, "outputTokens"),
    }
}

pub(crate) fn command_input_json(v: &Value) -> String {
    let unwrapped = v
        .get("commandActions")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|a| a.get("command"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let cmd = unwrapped
        .or_else(|| v.get("command").and_then(Value::as_str))
        .unwrap_or_default();
    json!({"command": cmd, "description": ""}).to_string()
}

pub(crate) fn command_result(item: &Value) -> (String, bool) {
    let status = item
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let exit_code = item.get("exitCode").and_then(Value::as_i64);
    let is_error = status != "completed" || !matches!(exit_code, None | Some(0));
    let output = item
        .get("aggregatedOutput")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    (output, is_error)
}

pub(crate) fn file_change_tool(item: &Value) -> (&'static str, String) {
    let changes = item
        .get("changes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if is_single_add(changes) {
        let path = str_field(&changes[0], "path");
        let diff = str_field(&changes[0], "diff");
        (
            "Write",
            json!({"file_path": path, "content": diff}).to_string(),
        )
    } else {
        let first_path = changes
            .first()
            .map(|c| str_field(c, "path"))
            .unwrap_or_default();
        let hunks = file_change_hunks(changes);
        (
            "Edit",
            json!({"file_path": first_path, "structuredPatch": hunks}).to_string(),
        )
    }
}

pub(crate) fn file_change_result(item: &Value) -> (String, bool, Option<String>) {
    let status = item
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let is_error = status != "completed";
    let changes = item
        .get("changes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let verb = if is_error { status } else { "edited" };
    let output = changes
        .iter()
        .filter_map(|c| c.get("path").and_then(Value::as_str))
        .filter(|p| !p.is_empty())
        .map(|p| format!("{p}: {verb}"))
        .collect::<Vec<_>>()
        .join("\n");
    let structured_json = (!is_error).then(|| file_change_structured_json(changes));
    (output, is_error, structured_json)
}

fn change_kind(change: &Value) -> Option<&str> {
    change
        .get("kind")
        .and_then(|k| k.get("type"))
        .and_then(Value::as_str)
}

fn is_single_add(changes: &[Value]) -> bool {
    changes.len() == 1 && change_kind(&changes[0]) == Some("add")
}

fn file_change_hunks(changes: &[Value]) -> Vec<Value> {
    let mut hunks = Vec::new();
    for change in changes {
        let diff = change
            .get("diff")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let parsed = parse_unified_hunks(diff);
        if !parsed.is_empty() {
            hunks.extend(parsed);
        } else if change_kind(change) == Some("delete") && !diff.is_empty() {
            let lines: Vec<String> = diff
                .lines()
                .map(|l| format!("-{}", l.trim_end_matches('\r')))
                .collect();
            hunks.push(json!({"oldStart": 1, "newStart": 1, "lines": lines}));
        }
    }
    hunks
}

fn file_change_structured_json(changes: &[Value]) -> String {
    if is_single_add(changes) {
        return json!({
            "type": "create",
            "structuredPatch": [],
            "originalFile": null,
        })
        .to_string();
    }
    json!({"structuredPatch": file_change_hunks(changes)}).to_string()
}

/// Parses unified diff `@@ -oldStart[,oldCount] +newStart[,newCount] @@` hunks into the
/// Claude-shaped `structuredPatch` array consumed by `diff.rs::rows_from_hunks`.
pub fn parse_unified_hunks(diff: &str) -> Vec<Value> {
    let mut hunks = Vec::new();
    let mut cur: Option<(u64, u64, Vec<String>)> = None;
    for raw in diff.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some((old_start, new_start)) = parse_hunk_header(line) {
            if let Some((os, ns, lines)) = cur.take() {
                hunks.push(json!({"oldStart": os, "newStart": ns, "lines": lines}));
            }
            cur = Some((old_start, new_start, Vec::new()));
            continue;
        }
        if let Some((_, _, ref mut lines)) = cur {
            if line.is_empty() {
                lines.push(" ".to_string());
            } else if line.starts_with([' ', '-', '+', '\\']) {
                lines.push(line.to_string());
            } else if line.starts_with("diff ") {
                if let Some((os, ns, finished)) = cur.take() {
                    hunks.push(json!({"oldStart": os, "newStart": ns, "lines": finished}));
                }
            }
        }
    }
    if let Some((os, ns, lines)) = cur {
        hunks.push(json!({"oldStart": os, "newStart": ns, "lines": lines}));
    }
    hunks
}

fn parse_hunk_header(line: &str) -> Option<(u64, u64)> {
    let rest = line.strip_prefix("@@ ")?;
    let body = rest.split(" @@").next()?;
    let mut parts = body.split_whitespace();
    let old_part = parts.next()?.strip_prefix('-')?;
    let new_part = parts.next()?.strip_prefix('+')?;
    let old_start = old_part.split(',').next()?.parse().ok()?;
    let new_start = new_part.split(',').next()?.parse().ok()?;
    Some((old_start, new_start))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_and_multi_hunk_unified_diffs() {
        let one = "@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n";
        assert_eq!(
            parse_unified_hunks(one),
            vec![json!({
                "oldStart": 1,
                "newStart": 1,
                "lines": [" alpha", "-beta", "+BETA", " gamma"]
            })]
        );

        let two = "@@ -2 +2,2 @@ fn a()\n keep\n+added\n@@ -10,2 +11,1 @@ fn b()\n-gone\n stay\n";
        assert_eq!(
            parse_unified_hunks(two),
            vec![
                json!({"oldStart": 2, "newStart": 2, "lines": [" keep", "+added"]}),
                json!({"oldStart": 10, "newStart": 11, "lines": ["-gone", " stay"]}),
            ]
        );
    }

    #[test]
    fn a_blank_line_inside_a_hunk_is_an_empty_context_line() {
        let diff = "@@ -1,3 +1,3 @@\n a\n\n-b\n+B\n";
        assert_eq!(
            parse_unified_hunks(diff),
            vec![json!({"oldStart": 1, "newStart": 1, "lines": [" a", " ", "-b", "+B"]})]
        );
    }

    #[test]
    fn text_flushed_early_is_not_repeated_on_completion() {
        let mut t = Translator::new();
        t.on_notification(
            "item/started",
            &json!({"item": {"type": "agentMessage", "id": "m1", "text": ""}}),
        );
        t.on_notification("item/agentMessage/delta", &json!({"delta": "Hello "}));
        let started = t.on_notification(
            "item/started",
            &json!({"item": {"type": "reasoning", "id": "r1"}}),
        );
        assert_eq!(
            started,
            vec![ProviderEvent::AssistantText {
                text: "Hello ".into()
            }]
        );
        t.on_notification("item/agentMessage/delta", &json!({"delta": "world"}));
        let done = t.on_notification(
            "item/completed",
            &json!({"item": {"type": "agentMessage", "id": "m1", "text": "Hello world"}}),
        );
        assert_eq!(
            done,
            vec![ProviderEvent::AssistantText {
                text: "world".into()
            }]
        );
    }

    #[test]
    fn a_declined_file_change_has_no_patch_and_says_so() {
        let mut t = Translator::new();
        let item = json!({"type": "fileChange", "id": "f1", "status": "declined",
            "changes": [{"path": "/w/a.txt", "kind": {"type": "update"},
                         "diff": "@@ -1 +1 @@\n-a\n+b\n"}]});
        let evs = t.on_notification("item/completed", &json!({ "item": item }));
        let result = evs
            .iter()
            .find_map(|e| match e {
                ProviderEvent::ToolResult {
                    output,
                    is_error,
                    structured_json,
                    ..
                } => Some((output.clone(), *is_error, structured_json.clone())),
                _ => None,
            })
            .expect("a tool result");
        assert_eq!(result, ("/w/a.txt: declined".to_string(), true, None));
    }

    #[test]
    fn item_completed_text_is_authoritative_over_deltas() {
        let mut t = Translator::new();
        assert_eq!(
            t.on_notification("item/agentMessage/delta", &json!({"delta": "hel"})),
            vec![ProviderEvent::TextDelta("hel".into())]
        );
        let ev = t.on_notification(
            "item/completed",
            &json!({"item": {"type": "agentMessage", "id": "m1", "text": "hello"}}),
        );
        assert_eq!(
            ev,
            vec![ProviderEvent::AssistantText {
                text: "hello".into()
            }]
        );
        assert!(t.flush_text().is_empty());
    }

    #[test]
    fn reasoning_deltas_flush_text_count_chars_and_reset_on_turn() {
        let mut t = Translator::new();
        t.on_notification("item/agentMessage/delta", &json!({"delta": "hi"}));
        let ev = t.on_notification(
            "item/reasoning/summaryTextDelta",
            &json!({"delta": "éééééééé"}),
        );
        assert_eq!(
            ev,
            vec![
                ProviderEvent::AssistantText { text: "hi".into() },
                ProviderEvent::ThinkingProgress {
                    estimated_tokens: 2
                },
            ]
        );
        let ev = t.on_notification("item/reasoning/textDelta", &json!({"delta": "1234"}));
        assert_eq!(
            ev,
            vec![ProviderEvent::ThinkingProgress {
                estimated_tokens: 3
            }]
        );
        t.on_notification("turn/completed", &json!({"turn": {"status": "completed"}}));
        let ev = t.on_notification("item/reasoning/summaryTextDelta", &json!({"delta": "abcd"}));
        assert_eq!(
            ev,
            vec![ProviderEvent::ThinkingProgress {
                estimated_tokens: 1
            }]
        );
    }

    #[test]
    fn error_with_will_retry_true_is_ignored() {
        let mut t = Translator::new();
        assert!(t
            .on_notification(
                "error",
                &json!({"error": {"message": "transient"}, "willRetry": true})
            )
            .is_empty());
        let ev = t.on_notification(
            "error",
            &json!({"error": {"message": "fatal"}, "willRetry": false}),
        );
        assert_eq!(ev, vec![ProviderEvent::Error("fatal".into())]);
    }

    #[test]
    fn failed_turn_without_prior_error_emits_fallback_error() {
        let mut t = Translator::new();
        let ev = t.on_notification("turn/completed", &json!({"turn": {"status": "failed"}}));
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0], ProviderEvent::Error("Codex turn failed".into()));
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
    fn failed_turn_after_error_notification_does_not_duplicate_error() {
        let mut t = Translator::new();
        let e1 = t.on_notification(
            "error",
            &json!({"error": {"message": "bad model"}, "willRetry": false}),
        );
        assert_eq!(e1, vec![ProviderEvent::Error("bad model".into())]);
        let e2 = t.on_notification("turn/completed", &json!({"turn": {"status": "failed"}}));
        assert_eq!(e2.len(), 1);
        assert!(matches!(
            &e2[0],
            ProviderEvent::TurnComplete {
                is_error: true,
                interrupted: false,
                ..
            }
        ));
    }

    #[test]
    fn command_nonzero_exit_code_is_error_even_if_status_completed() {
        let mut t = Translator::new();
        let ev = t.on_notification(
            "item/completed",
            &json!({
                "item": {
                    "type": "commandExecution",
                    "id": "e1",
                    "command": "false",
                    "status": "completed",
                    "exitCode": 1,
                    "aggregatedOutput": "boom"
                }
            }),
        );
        assert!(matches!(
            ev.last(),
            Some(ProviderEvent::ToolResult {
                is_error: true,
                output,
                ..
            }) if output == "boom"
        ));
    }

    #[test]
    fn resumed_turn_usage_uses_total_minus_last_when_no_prior_baseline() {
        let mut t = Translator::new();
        t.on_notification("turn/started", &json!({}));
        t.on_notification(
            "thread/tokenUsage/updated",
            &json!({
                "tokenUsage": {
                    "total": {"totalTokens": 26878, "inputTokens": 26867, "cachedInputTokens": 13419, "outputTokens": 11},
                    "last": {"totalTokens": 13451, "inputTokens": 13445, "cachedInputTokens": 13419, "outputTokens": 6},
                    "modelContextWindow": 258400
                }
            }),
        );
        let ev = t.on_notification("turn/completed", &json!({"turn": {"status": "completed"}}));
        match ev.as_slice() {
            [ProviderEvent::TurnComplete { usage, .. }] => {
                assert_eq!(usage.input_tokens, 26);
                assert_eq!(usage.cache_read_tokens, 13419);
                assert_eq!(usage.output_tokens, 6);
                assert_eq!(usage.context_tokens_used, 13451);
                assert_eq!(usage.context_window_max, 258400);
                assert_eq!(usage.cost_usd, 0.0);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_notifications_and_items_are_skipped() {
        let mut t = Translator::new();
        for m in [
            "hook/started",
            "hook/completed",
            "thread/status/changed",
            "warning",
            "turn/diff/updated",
            "serverRequest/resolved",
            "thread/goal/cleared",
            "brand/new",
        ] {
            assert!(t.on_notification(m, &json!({})).is_empty(), "{m}");
        }
        assert!(t
            .on_notification("item/started", &json!({"item": {"type": "futureTool"}}))
            .is_empty());
        assert!(t
            .on_notification("item/completed", &json!({"item": {"type": "futureTool"}}))
            .is_empty());
    }
}
