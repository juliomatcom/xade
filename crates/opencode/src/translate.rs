//! Pure ACP -> `ProviderEvent` translation. No I/O: the process task feeds it `session/update`
//! params, permission requests and `session/prompt` responses, and sends what comes back.
//!
//! Tools are renamed and reshaped to Claude's (`Bash`, `Read`, `Write`, `Edit`) so tool cards, the
//! diff modal and the changes summary work as they do for the other providers.

use std::collections::HashMap;

use serde_json::{json, Value};
use xade_core::{ProviderEvent, UsageTotals};

use crate::patch;

/// Marks an update or permission request that comes from a subagent's session.
const CHILD_META: &str = "opencode/child-session";

#[derive(Debug, Default)]
pub struct Translator {
    /// Streamed assistant text not yet finalized as an `AssistantText`.
    text_buf: String,
    thought_chars: usize,
    /// Every tool call seen, by its ACP id.
    tools: HashMap<String, Tool>,
    context_used: u64,
    context_size: u64,
    /// Sum over every `session/prompt` response of the running turn.
    turn_usage: UsageTotals,
}

/// What is known of one tool call. OpenCode spreads it over several frames: the first has no
/// arguments yet, later ones omit the name, and the final one omits the arguments.
#[derive(Debug, Default)]
struct Tool {
    /// The id Xade knows it by: the ACP one, suffixed when a model reuses it for a later call.
    id: String,
    reuses: u32,
    name: String,
    title: String,
    /// `locations[0].path`.
    location: String,
    raw_input: Value,
    /// `oldText` of the first diff block seen; `Some(None)` is a file that did not exist.
    original: Option<Option<String>>,
    announced: bool,
    finished: bool,
}

/// A tool call in Claude's shape.
struct Mapped {
    name: String,
    input_json: String,
}

impl Tool {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            ..Self::default()
        }
    }

    /// Starts over for a new call under an id that already finished.
    fn reuse(&mut self, acp_id: &str) {
        let reuses = self.reuses + 1;
        *self = Self::new(&format!("{acp_id}#{reuses}"));
        self.reuses = reuses;
    }

    /// Takes whatever a `tool_call`, `tool_call_update` or permission `toolCall` carries.
    fn merge(&mut self, call: &Value) {
        if let Some(name) = non_empty(call, "name") {
            self.name = name.to_string();
        }
        if let Some(title) = non_empty(call, "title") {
            self.title = title.to_string();
        }
        if let Some(path) = call
            .pointer("/locations/0/path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
        {
            self.location = path.to_string();
        }
        if let Some(input) = call
            .get("rawInput")
            .filter(|v| v.as_object().is_some_and(|o| !o.is_empty()))
        {
            self.raw_input = input.clone();
        }
        if self.original.is_none() {
            self.original = diff_blocks(call)
                .next()
                .map(|d| d.get("oldText").and_then(Value::as_str).map(str::to_string));
        }
    }

    fn arg(&self, key: &str) -> Option<&str> {
        self.raw_input.get(key).and_then(Value::as_str)
    }

    fn path(&self) -> &str {
        self.arg("path")
            .filter(|p| !p.is_empty())
            .unwrap_or(&self.location)
    }

    fn mapped(&self) -> Mapped {
        let (name, input) = match self.name.as_str() {
            "shell" => (
                "Bash",
                json!({"command": self.arg("command").unwrap_or(&self.title), "description": ""}),
            ),
            "read" => ("Read", json!({"file_path": self.path()})),
            "write" => (
                "Write",
                json!({"file_path": self.path(), "content": self.arg("content").unwrap_or_default()}),
            ),
            "edit" => (
                "Edit",
                json!({
                    "file_path": self.path(),
                    "old_string": self.arg("oldString").unwrap_or_default(),
                    "new_string": self.arg("newString").unwrap_or_default(),
                }),
            ),
            "patch" => self.mapped_patch(),
            other => {
                let name = [other, &self.title]
                    .into_iter()
                    .find(|n| !n.is_empty())
                    .unwrap_or("tool");
                let input = match &self.raw_input {
                    Value::Null => json!({}),
                    v => v.clone(),
                };
                (name, input)
            }
        };
        Mapped {
            name: name.to_string(),
            input_json: input.to_string(),
        }
    }

    fn mapped_patch(&self) -> (&'static str, Value) {
        let text = self.arg("patchText").unwrap_or_default();
        let Some(files) = patch::parse(text) else {
            let mut input = json!({"file_path": self.location});
            if !text.is_empty() {
                input["structuredPatch"] = json!([patch::raw_hunk(text)]);
            }
            return ("Edit", input);
        };
        if let Some((path, content)) = patch::single_add(&files) {
            return ("Write", json!({"file_path": path, "content": content}));
        }
        let path = files.first().map_or("", |f| f.path.as_str());
        (
            "Edit",
            json!({"file_path": path, "structuredPatch": patch::structured_hunks(&files)}),
        )
    }

    /// What the diff modal needs besides the input once a call succeeded. Only a `Write` needs it:
    /// by then the file on disk is the new one. An `Edit` is diffed from its input.
    fn result_json(&self, output: &str) -> Option<String> {
        if self.mapped().name != "Write" {
            return None;
        }
        // Only a permission request previews a `write`'s old text. Without one (Auto ON) the
        // old content is unknown, and the whole new content is shown.
        let created = match &self.original {
            Some(old) => old.is_none(),
            None => output.starts_with("Created"),
        };
        Some(
            json!({
                "type": if created { "create" } else { "update" },
                "structuredPatch": [],
                "originalFile": self.original.clone().flatten(),
            })
            .to_string(),
        )
    }
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
        let child = is_child(update);
        let mut out = Vec::new();
        match kind {
            // A subagent's words, thoughts and context are not this conversation's.
            "agent_message_chunk" | "agent_thought_chunk" | "usage_update" if child => {}
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
            "tool_call" => self.tool(update, true, &mut out),
            "tool_call_update" => self.tool(update, false, &mut out),
            "usage_update" => {
                self.context_used = u64_field(update, "used");
                self.context_size = u64_field(update, "size");
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
        // The request is what gets approved, so it is what is shown.
        let (tool_use_id, mapped) = match non_empty(call, "toolCallId") {
            Some(acp_id) => {
                let tool = self.entry(acp_id);
                if tool.finished {
                    tool.reuse(acp_id);
                }
                tool.merge(call);
                let shown = (tool.id.clone(), tool.mapped());
                self.announce(acp_id, &mut out);
                shown
            }
            None => {
                let mut tool = Tool::default();
                tool.merge(call);
                (String::new(), tool.mapped())
            }
        };
        out.push(ProviderEvent::PermissionRequest {
            request_id: key,
            tool_use_id,
            tool_name: mapped.name,
            input_json: mapped.input_json,
        });
        out
    }

    /// Handles a successful `session/prompt` response. `last` is false when another prompt follows
    /// at once, so the turn goes on; `interrupted` is the caller's interrupt flag.
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
        let mut out = Vec::new();
        self.flush(&mut out);
        if !last {
            return out;
        }
        let stop = result
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("end_turn");
        let interrupted = interrupted || stop == "cancelled";
        let is_error = !interrupted && stop != "end_turn";
        if is_error {
            out.push(ProviderEvent::Error(format!("OpenCode stopped: {stop}")));
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

    /// Ends a turn that no `session/prompt` response will end: one OpenCode started by itself
    /// when a background subagent finished.
    pub fn finish_turn(&mut self, interrupted: bool) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        self.flush(&mut out);
        out.push(self.turn_complete(false, interrupted));
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

    fn entry(&mut self, acp_id: &str) -> &mut Tool {
        self.tools
            .entry(acp_id.to_string())
            .or_insert_with(|| Tool::new(acp_id))
    }

    /// `tool_call` and `tool_call_update` share one shape. The call is announced once it runs or
    /// has ended (`pending` arrives before its arguments), and finished once, by a status that is
    /// neither `pending` nor `in_progress`. An update without a status changes neither.
    fn tool(&mut self, update: &Value, is_call: bool, out: &mut Vec<ProviderEvent>) {
        let Some(acp_id) = non_empty(update, "toolCallId") else {
            return;
        };
        let status = update.get("status").and_then(Value::as_str);
        let tool = self.entry(acp_id);
        if tool.finished {
            if !(is_call && status == Some("pending")) {
                return;
            }
            tool.reuse(acp_id);
        }
        tool.merge(update);
        let Some(status) = status.filter(|s| *s != "pending") else {
            return;
        };
        self.announce(acp_id, out);
        if status == "in_progress" {
            return;
        }
        // The text so far was shown before this result.
        self.flush(out);
        let Some(tool) = self.tools.get_mut(acp_id) else {
            return;
        };
        let output = tool_output(update);
        let is_error = status != "completed";
        let structured_json = (!is_error).then(|| tool.result_json(&output)).flatten();
        tool.finished = true;
        // Only the id and the flag are needed from here on.
        tool.raw_input = Value::Null;
        tool.original = None;
        out.push(ProviderEvent::ToolResult {
            tool_use_id: tool.id.clone(),
            output,
            is_error,
            structured_json,
        });
    }

    /// Emits the call's `ToolUse`, the first time only.
    fn announce(&mut self, acp_id: &str, out: &mut Vec<ProviderEvent>) {
        let Some(tool) = self.tools.get_mut(acp_id) else {
            return;
        };
        if std::mem::replace(&mut tool.announced, true) {
            return;
        }
        let (tool_use_id, mapped) = (tool.id.clone(), tool.mapped());
        self.flush(out);
        out.push(ProviderEvent::ToolUse {
            tool_use_id,
            name: mapped.name,
            input_json: mapped.input_json,
        });
    }
}

fn is_child(update: &Value) -> bool {
    update
        .get("_meta")
        .and_then(|m| m.get(CHILD_META))
        .is_some_and(|c| !c.is_null())
}

fn chunk_text(update: &Value) -> Option<&str> {
    update.get("content")?.get("text")?.as_str()
}

fn non_empty<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn u64_field(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn content_blocks(call: &Value) -> &[Value] {
    call.get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn diff_blocks(call: &Value) -> impl Iterator<Item = &Value> {
    content_blocks(call)
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("diff"))
}

/// The text of a finished tool call. Never the raw JSON of `rawOutput`.
/// Order: the text blocks of `content`, `rawOutput.output`, `rawOutput` when it is itself a
/// string, then `"<path>: edited"` for each diff block, else empty.
fn tool_output(update: &Value) -> String {
    let texts: Vec<&str> = content_blocks(update)
        .iter()
        .filter_map(|b| b.get("content")?.get("text")?.as_str())
        .collect();
    if !texts.is_empty() {
        return texts.join("\n");
    }
    let raw = update.get("rawOutput");
    if let Some(s) = raw.and_then(|r| r.get("output")).and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(s) = raw.and_then(Value::as_str) {
        return s.to_string();
    }
    diff_blocks(update)
        .filter_map(|b| b.get("path").and_then(Value::as_str))
        .map(|p| format!("{p}: edited"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn text(t: &str) -> Value {
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": t}})
    }

    fn thought(t: &str) -> Value {
        json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": t}})
    }

    /// The opening frame, as recorded: `pending`, with partial or no arguments.
    fn pending(id: &str, name: &str, input: Value) -> Value {
        json!({"sessionUpdate": "tool_call", "toolCallId": id, "name": name, "title": name,
               "status": "pending", "locations": [], "rawInput": input})
    }

    fn running(id: &str, input: Value) -> Value {
        json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "in_progress",
               "rawInput": input})
    }

    fn done(id: &str, status: &str, content: Value) -> Value {
        json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status,
               "content": content})
    }

    fn said(t: &str) -> Value {
        json!([{"type": "content", "content": {"type": "text", "text": t}}])
    }

    fn permission(id: &str, name: &str, input: Value, content: Value) -> Value {
        json!({"sessionId": "s", "options": [], "toolCall": {
            "toolCallId": id, "name": name, "title": name, "status": "pending",
            "rawInput": input, "content": content}})
    }

    fn tool_use(ev: &[ProviderEvent]) -> (&str, &str, Value) {
        ev.iter()
            .find_map(|e| match e {
                ProviderEvent::ToolUse {
                    tool_use_id,
                    name,
                    input_json,
                } => Some((
                    tool_use_id.as_str(),
                    name.as_str(),
                    serde_json::from_str(input_json).expect("input json"),
                )),
                _ => None,
            })
            .expect("a ToolUse")
    }

    fn tool_result(ev: &[ProviderEvent]) -> (&str, bool, Option<Value>) {
        ev.iter()
            .find_map(|e| match e {
                ProviderEvent::ToolResult {
                    output,
                    is_error,
                    structured_json,
                    ..
                } => Some((
                    output.as_str(),
                    *is_error,
                    structured_json
                        .as_deref()
                        .map(|s| serde_json::from_str(s).expect("structured json")),
                )),
                _ => None,
            })
            .expect("a ToolResult")
    }

    /// Runs one tool call to completion and returns every event.
    fn call(t: &mut Translator, name: &str, input: Value, content: Value) -> Vec<ProviderEvent> {
        let mut ev = t.on_update(&pending("c", name, json!({})));
        ev.extend(t.on_update(&running("c", input)));
        ev.extend(t.on_update(&done("c", "completed", content)));
        ev
    }

    #[test]
    fn a_pending_call_is_not_announced_even_with_partial_input() {
        let mut t = Translator::new();
        assert!(t
            .on_update(&pending("c1", "shell", json!({"cwd": "/w"})))
            .is_empty());
        assert!(t
            .on_update(&json!({"sessionUpdate": "tool_call", "toolCallId": "c2", "name": "read"}))
            .is_empty());
        // The running frame has the arguments and no name: the cached one is used.
        let ev = t.on_update(&running("c1", json!({"command": "ls", "cwd": "/w"})));
        assert_eq!(ev.len(), 1);
        assert_eq!(
            tool_use(&ev),
            ("c1", "Bash", json!({"command": "ls", "description": ""}))
        );
    }

    #[test]
    fn an_update_without_a_status_is_not_terminal() {
        let mut t = Translator::new();
        t.on_update(&pending("c1", "shell", json!({})));
        let no_status = json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1",
                               "rawInput": {"command": "ls"}});
        assert!(t.on_update(&no_status).is_empty());
        t.on_update(&running("c1", json!({"command": "ls"})));
        assert!(t.on_update(&no_status).is_empty());
        assert_eq!(t.on_update(&done("c1", "completed", said("ok"))).len(), 1);
    }

    #[test]
    fn a_call_is_announced_once_and_finished_once() {
        let mut t = Translator::new();
        t.on_update(&pending("c1", "shell", json!({})));
        assert_eq!(
            t.on_update(&running("c1", json!({"command": "ls"}))).len(),
            1
        );
        assert!(t
            .on_update(&running("c1", json!({"command": "ls"})))
            .is_empty());
        assert_eq!(t.on_update(&done("c1", "completed", said("a"))).len(), 1);
        assert!(t.on_update(&done("c1", "completed", said("a"))).is_empty());
        assert!(t.on_update(&done("c1", "failed", said("a"))).is_empty());
        assert!(t
            .on_update(&running("c1", json!({"command": "ls"})))
            .is_empty());
    }

    #[test]
    fn an_id_reused_by_a_later_call_is_a_new_call_with_its_own_id() {
        let mut t = Translator::new();
        call(&mut t, "shell", json!({"command": "one"}), said("1"));
        let ev = call(&mut t, "shell", json!({"command": "two"}), said("2"));
        let (id, _, input) = tool_use(&ev);
        assert_eq!((id, input["command"].as_str()), ("c#1", Some("two")));
        assert!(matches!(
            ev.last(),
            Some(ProviderEvent::ToolResult { tool_use_id, .. }) if tool_use_id == "c#1"
        ));
        // A request for it, without an opening frame first, also shows its own arguments.
        let ev = t.on_permission_request(
            "0".into(),
            &permission("c", "shell", json!({"command": "three"}), json!([])),
        );
        let (id, _, input) = tool_use(&ev);
        assert_eq!((id, input["command"].as_str()), ("c#2", Some("three")));
    }

    #[test]
    fn a_call_first_seen_finished_is_announced_then_finished() {
        let mut t = Translator::new();
        let ev = t.on_update(
            &json!({"sessionUpdate": "tool_call_update", "toolCallId": "c9",
            "title": "glob **/*", "status": "completed", "content": said("x")}),
        );
        assert_eq!(ev.len(), 2);
        assert_eq!(tool_use(&ev), ("c9", "glob **/*", json!({})));
        assert!(matches!(&ev[1], ProviderEvent::ToolResult { .. }));
    }

    #[test]
    fn any_terminal_status_but_completed_is_an_error() {
        for status in ["failed", "cancelled", "something_new"] {
            let mut t = Translator::new();
            let ev = t.on_update(&done("c1", status, said("no")));
            assert_eq!(tool_result(&ev), ("no", true, None), "{status}");
        }
    }

    #[test]
    fn core_tools_take_claudes_names_and_input_keys() {
        let map = |name: &str, input: Value| {
            let mut t = Translator::new();
            let ev = call(&mut t, name, input, said("ok"));
            let (_, name, input) = tool_use(&ev);
            (name.to_string(), input)
        };
        assert_eq!(
            map("read", json!({"path": "a.rs"})),
            ("Read".into(), json!({"file_path": "a.rs"}))
        );
        assert_eq!(
            map("write", json!({"path": "a", "content": "x"})),
            ("Write".into(), json!({"file_path": "a", "content": "x"}))
        );
        assert_eq!(
            map(
                "edit",
                json!({"path": "a", "oldString": "x", "newString": "y"})
            ),
            (
                "Edit".into(),
                json!({"file_path": "a", "old_string": "x", "new_string": "y"})
            )
        );
        assert_eq!(
            map("webfetch", json!({"url": "https://e.test"})),
            ("webfetch".into(), json!({"url": "https://e.test"}))
        );
    }

    #[test]
    fn a_missing_path_or_command_falls_back_to_the_location_or_title() {
        let mut t = Translator::new();
        let ev = t.on_update(
            &json!({"sessionUpdate": "tool_call_update", "toolCallId": "1",
            "name": "read", "status": "in_progress", "locations": [{"path": "/w/a"}]}),
        );
        assert_eq!(tool_use(&ev).2, json!({"file_path": "/w/a"}));
        let ev = t.on_update(
            &json!({"sessionUpdate": "tool_call_update", "toolCallId": "2",
            "name": "shell", "title": "ls -la", "status": "in_progress"}),
        );
        assert_eq!(tool_use(&ev).2["command"], "ls -la");
    }

    #[test]
    fn a_write_result_carries_the_previewed_original() {
        let preview =
            |old: Value| json!([{"type": "diff", "path": "/w/a", "oldText": old, "newText": "n"}]);
        for (old, kind) in [(json!("before"), "update"), (Value::Null, "create")] {
            let mut t = Translator::new();
            t.on_update(&pending("c", "write", json!({})));
            t.on_update(&running("c", json!({"path": "a", "content": "n"})));
            t.on_permission_request(
                "0".into(),
                &permission(
                    "c",
                    "write",
                    json!({"path": "a", "content": "n"}),
                    preview(old.clone()),
                ),
            );
            // The completed frame has text only.
            let ev = t.on_update(&done("c", "completed", said("Wrote file successfully: a")));
            assert_eq!(
                tool_result(&ev).2,
                Some(json!({"type": kind, "structuredPatch": [], "originalFile": old}))
            );
        }
    }

    #[test]
    fn a_write_without_a_preview_is_told_apart_by_its_message() {
        for (message, kind) in [
            ("Created file successfully: a", "create"),
            ("Wrote file successfully: a", "update"),
        ] {
            let mut t = Translator::new();
            let ev = call(
                &mut t,
                "write",
                json!({"path": "a", "content": "n"}),
                said(message),
            );
            assert_eq!(
                tool_result(&ev).2,
                Some(json!({"type": kind, "structuredPatch": [], "originalFile": null}))
            );
        }
    }

    #[test]
    fn an_edit_and_other_tools_carry_no_result_json() {
        let diff = json!([{"type": "diff", "path": "/w/a", "oldText": "x", "newText": "y"}]);
        for name in ["edit", "shell", "read"] {
            let mut t = Translator::new();
            let ev = call(&mut t, name, json!({"path": "a"}), diff.clone());
            assert_eq!(tool_result(&ev).2, None, "{name}");
        }
    }

    #[test]
    fn a_failed_change_carries_no_result_json() {
        for (name, input) in [
            ("write", json!({"path": "a", "content": "n"})),
            ("edit", json!({"path": "a"})),
            (
                "patch",
                json!({"patchText": "*** Begin Patch\n*** Add File: a\n+x\n*** End Patch\n"}),
            ),
        ] {
            let mut t = Translator::new();
            t.on_update(&pending("c", name, json!({})));
            t.on_update(&running("c", input));
            let ev = t.on_update(&done("c", "failed", said("declined")));
            assert_eq!(tool_result(&ev), ("declined", true, None), "{name}");
        }
    }

    #[test]
    fn a_patch_adding_one_file_is_a_write() {
        let mut t = Translator::new();
        let ev = call(
            &mut t,
            "patch",
            json!({"patchText": "*** Begin Patch\n*** Add File: a\n+one\n*** End Patch\n"}),
            json!([{"type": "diff", "path": "/w/a", "oldText": null, "newText": "one\n"}]),
        );
        let (_, name, input) = tool_use(&ev);
        assert_eq!(
            (name, input),
            ("Write", json!({"file_path": "a", "content": "one\n"}))
        );
        assert_eq!(
            tool_result(&ev).2,
            Some(json!({"type": "create", "structuredPatch": [], "originalFile": null}))
        );
    }

    #[test]
    fn any_other_patch_is_an_edit_with_hunks() {
        let mut t = Translator::new();
        let text =
            "*** Begin Patch\n*** Update File: a\n@@\n-x\n+y\n*** Delete File: b\n*** End Patch\n";
        let ev = call(&mut t, "patch", json!({"patchText": text}), said("Success"));
        let (_, name, input) = tool_use(&ev);
        assert_eq!(name, "Edit");
        assert_eq!(input["file_path"], "a");
        assert_eq!(input["structuredPatch"].as_array().map(Vec::len), Some(2));
        // The diff modal keeps using the input's hunks.
        assert_eq!(tool_result(&ev).2, None);
    }

    #[test]
    fn an_unreadable_patch_is_shown_as_it_is() {
        let mut t = Translator::new();
        let ev = t.on_update(
            &json!({"sessionUpdate": "tool_call_update", "toolCallId": "c",
            "name": "patch", "status": "in_progress", "locations": [{"path": "/w/a"}],
            "rawInput": {"patchText": "rm everything"}}),
        );
        let (_, name, input) = tool_use(&ev);
        assert_eq!(name, "Edit");
        assert_eq!(input["file_path"], "/w/a");
        assert_eq!(
            input["structuredPatch"][0]["lines"],
            json!([" rm everything"])
        );
    }

    #[test]
    fn a_subagents_words_and_context_are_skipped_but_its_tools_are_not() {
        let child = |mut u: Value| {
            u["_meta"] = json!({"opencode/child-session": {"id": "ses_child", "depth": 1}});
            u
        };
        let mut t = Translator::new();
        t.on_update(&json!({"sessionUpdate": "usage_update", "used": 10, "size": 100}));
        t.on_update(&text("mine"));
        assert!(t.on_update(&child(text("theirs"))).is_empty());
        assert!(t.on_update(&child(thought("hmm"))).is_empty());
        assert!(t
            .on_update(&child(
                json!({"sessionUpdate": "usage_update", "used": 90, "size": 200})
            ))
            .is_empty());
        assert!(t
            .on_update(&child(pending("ses_child:c", "shell", json!({}))))
            .is_empty());
        let ev = t.on_update(&child(running("ses_child:c", json!({"command": "ls"}))));
        assert_eq!(tool_use(&ev).1, "Bash");
        let ev = t.finish_turn(false);
        assert!(matches!(
            ev.as_slice(),
            [ProviderEvent::TurnComplete { usage, .. }]
                if (usage.context_tokens_used, usage.context_window_max) == (10, 100)
        ));
    }

    #[test]
    fn a_permission_request_announces_a_pending_or_unseen_call_in_the_same_shape() {
        let mut t = Translator::new();
        t.on_update(&pending("c1", "shell", json!({"cwd": "/w"})));
        t.on_update(&text("about to run"));
        let request = permission("c1", "shell", json!({"command": "ls"}), json!([]));
        let ev = t.on_permission_request("0".into(), &request);
        assert_eq!(ev.len(), 3, "{ev:?}");
        assert!(matches!(&ev[0], ProviderEvent::AssistantText { text } if text == "about to run"));
        let (_, name, input) = tool_use(&ev);
        assert!(matches!(
            &ev[2],
            ProviderEvent::PermissionRequest { request_id, tool_use_id, tool_name, input_json }
                if request_id == "0" && tool_use_id == "c1" && tool_name == name
                    && serde_json::from_str::<Value>(input_json).ok() == Some(input.clone())
        ));
        // Seen already: only the request. An unseen one is announced.
        assert_eq!(t.on_permission_request("1".into(), &request).len(), 1);
        let unseen = permission("c2", "edit", json!({"path": "a"}), json!([]));
        let ev = t.on_permission_request("2".into(), &unseen);
        assert_eq!(tool_use(&ev).1, "Edit");
        assert!(
            matches!(&ev[1], ProviderEvent::PermissionRequest { tool_name, .. } if tool_name == "Edit")
        );
    }

    #[test]
    fn a_request_without_a_tool_call_id_still_reaches_the_user() {
        let mut t = Translator::new();
        let ev = t.on_permission_request(
            "0".into(),
            &json!({"toolCall": {"name": "shell", "rawInput": {"command": "ls"}}}),
        );
        assert!(matches!(
            ev.as_slice(),
            [ProviderEvent::PermissionRequest { tool_name, tool_use_id, .. }]
                if tool_name == "Bash" && tool_use_id.is_empty()
        ));
    }

    #[test]
    fn text_is_flushed_before_a_thought_a_tool_and_a_result() {
        let mut t = Translator::new();
        let flushed = |ev: &[ProviderEvent], want: &str| matches!(ev.first(), Some(ProviderEvent::AssistantText { text }) if text == want);
        assert_eq!(
            t.on_update(&text("a")),
            vec![ProviderEvent::TextDelta("a".into())]
        );
        assert!(flushed(&t.on_update(&thought("t")), "a"));
        t.on_update(&text("b"));
        // The opening frame shows nothing yet, so the text keeps growing.
        assert!(t.on_update(&pending("c", "shell", json!({}))).is_empty());
        t.on_update(&text("c"));
        assert!(flushed(
            &t.on_update(&running("c", json!({"command": "ls"}))),
            "bc"
        ));
        t.on_update(&text("d"));
        assert!(flushed(
            &t.on_update(&done("c", "completed", said("ok"))),
            "d"
        ));
    }

    #[test]
    fn thought_estimate_counts_chars_not_bytes_and_resets_each_turn() {
        let mut t = Translator::new();
        let estimate = |n: u64| {
            vec![ProviderEvent::ThinkingProgress {
                estimated_tokens: n,
            }]
        };
        assert_eq!(t.on_update(&thought("éééééééé")), estimate(2));
        t.on_prompt_result(&json!({"stopReason": "end_turn"}), true, false);
        assert_eq!(t.on_update(&thought("abcd")), estimate(1));
    }

    #[test]
    fn tool_output_prefers_text_then_raw_then_diff_and_never_dumps_json() {
        let two = json!([
            {"type": "content", "content": {"type": "text", "text": "a"}},
            {"type": "content", "content": {"type": "text", "text": "b"}}
        ]);
        assert_eq!(
            tool_output(&json!({"content": two, "rawOutput": "raw"})),
            "a\nb"
        );
        assert_eq!(tool_output(&json!({"rawOutput": {"output": "o"}})), "o");
        assert_eq!(tool_output(&json!({"rawOutput": "s"})), "s");
        let diffs = json!([{"type": "diff", "path": "/x/a"}, {"type": "diff", "path": "/x/b"}]);
        assert_eq!(
            tool_output(&json!({"content": diffs, "rawOutput": {"metadata": {}}})),
            "/x/a: edited\n/x/b: edited"
        );
        assert_eq!(tool_output(&json!({"rawOutput": {"weird": 1}})), "");
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
        assert!(t.on_update(&done("", "completed", said("x"))).is_empty());
    }

    fn result(stop: &str, input: u64, output: u64, cached: u64) -> Value {
        json!({"stopReason": stop, "usage": {"inputTokens": input, "outputTokens": output,
                                              "cachedReadTokens": cached}})
    }

    #[test]
    fn turn_end_flushes_text_and_reports_usage_and_context() {
        let mut t = Translator::new();
        t.on_update(
            &json!({"sessionUpdate": "usage_update", "used": 500, "size": 1000,
                            "cost": {"amount": 3, "currency": "USD"}}),
        );
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
    fn a_prompt_followed_by_another_flushes_and_adds_usage_but_ends_nothing() {
        let mut t = Translator::new();
        t.on_update(&text("first"));
        assert_eq!(
            t.on_prompt_result(&result("cancelled", 10, 1, 0), false, true),
            vec![ProviderEvent::AssistantText {
                text: "first".into()
            }]
        );
        let ev = t.on_prompt_result(&result("end_turn", 5, 2, 3), true, false);
        assert!(matches!(
            ev.as_slice(),
            [ProviderEvent::TurnComplete { usage, is_error: false, interrupted: false }]
                if (usage.input_tokens, usage.output_tokens, usage.cache_read_tokens) == (15, 3, 3)
        ));
        // The next turn starts from zero.
        let ev = t.on_prompt_result(&result("end_turn", 1, 1, 1), true, false);
        assert!(
            matches!(&ev[0], ProviderEvent::TurnComplete { usage, .. } if usage.input_tokens == 1)
        );
    }

    #[test]
    fn cancel_is_interrupted_not_an_error() {
        for (stop, flag) in [("cancelled", false), ("refusal", true)] {
            let mut t = Translator::new();
            let ev = t.on_prompt_result(&json!({"stopReason": stop}), true, flag);
            assert!(
                matches!(
                    ev.as_slice(),
                    [ProviderEvent::TurnComplete {
                        is_error: false,
                        interrupted: true,
                        ..
                    }]
                ),
                "{stop}"
            );
        }
    }

    #[test]
    fn any_other_stop_reason_is_an_error_turn() {
        let mut t = Translator::new();
        let ev = t.on_prompt_result(&result("max_tokens", 1, 1, 0), true, false);
        assert_eq!(
            ev[0],
            ProviderEvent::Error("OpenCode stopped: max_tokens".into())
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
        assert_eq!(
            t.on_prompt_error("boom", false),
            vec![
                ProviderEvent::AssistantText {
                    text: "partial".into()
                },
                ProviderEvent::Error("boom".into()),
            ]
        );
        assert!(matches!(
            t.on_prompt_error("boom", true).as_slice(),
            [
                ProviderEvent::Error(_),
                ProviderEvent::TurnComplete {
                    is_error: true,
                    interrupted: false,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn finishing_a_turn_without_a_response_flushes_text() {
        let mut t = Translator::new();
        t.on_update(&text("later"));
        assert!(matches!(
            t.finish_turn(true).as_slice(),
            [
                ProviderEvent::AssistantText { .. },
                ProviderEvent::TurnComplete {
                    is_error: false,
                    interrupted: true,
                    ..
                }
            ]
        ));
        assert!(t.flush_text().is_empty());
    }
}
