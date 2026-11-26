//! The hand-off text for a forked session: its copied history as one block the new provider
//! reads from a file. Pure; never stored.

use xade_core::{Message, Role};

const HEAD: &str = "<previous_conversation>\nThis conversation started in another agent session in this same directory. Its history follows; continue from where it ends.\n\n";
const TAIL: &str = "</previous_conversation>";

/// Payload keys that name what a tool call acts on, most specific first.
const TARGET_KEYS: [&str; 8] = [
    "file_path",
    "path",
    "Path",
    "notebook_path",
    "CommandLine",
    "command",
    "pattern",
    "url",
];

/// `messages` as a `<previous_conversation>` block; empty when nothing in it is worth sending.
/// User and assistant text is complete; a tool call is one line with its target; tool results,
/// diffs, `System` rows and delivery tags are left out. Nothing is capped.
pub fn transcript(messages: &[Message]) -> String {
    let items: Vec<String> = messages.iter().filter_map(item).collect();
    if items.is_empty() {
        return String::new();
    }
    let mut out = String::from(HEAD);
    for line in &items {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(TAIL);
    out
}

/// One transcript line (possibly multi-line text) for a message, or `None` when skipped.
fn item(m: &Message) -> Option<String> {
    match m.role {
        Role::System | Role::ToolResult => None,
        Role::User | Role::Assistant if m.content_text.trim().is_empty() => None,
        Role::User => Some(format!("[user] {}", m.content_text)),
        Role::Assistant => Some(format!("[assistant] {}", m.content_text)),
        Role::ToolCall => {
            let name = m.tool_name.as_deref().unwrap_or("tool");
            let target = m.tool_payload_json.as_deref().and_then(target);
            Some(match target {
                Some(t) => format!("[tool_call {name}] {t}"),
                None => format!("[tool_call {name}]"),
            })
        }
    }
}

/// The first `TARGET_KEYS` string in a tool input, on one line.
fn target(input_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(input_json).ok()?;
    let obj = value.as_object()?;
    TARGET_KEYS
        .iter()
        .filter_map(|k| obj.get(*k)?.as_str())
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;
    use uuid::Uuid;
    use xade_core::SessionId;

    fn msg(role: Role, text: &str) -> Message {
        Message {
            id: Uuid::new_v4(),
            session_id: SessionId::new(),
            seq: 1,
            role,
            content_text: text.into(),
            thinking_text: None,
            tool_name: None,
            tool_use_id: None,
            tool_payload_json: None,
            is_error: false,
            created_at: OffsetDateTime::now_utc(),
            provider_id: None,
            sender_handle: None,
        }
    }

    fn call(name: &str, input: &str) -> Message {
        let mut m = msg(Role::ToolCall, "");
        m.tool_name = Some(name.into());
        m.tool_payload_json = Some(input.into());
        m
    }

    #[test]
    fn text_in_full_tool_calls_as_one_line_and_no_results() {
        let mut result = msg(Role::ToolResult, "boom");
        result.is_error = true;
        result.tool_payload_json = Some(r#"{"diff":"hidden"}"#.into());
        let t = transcript(&[
            msg(Role::User, "fix it\nplease"),
            call("Read", r#"{"file_path":"a.rs","limit":5}"#),
            msg(Role::ToolResult, "contents"),
            result,
            call(
                "Bash",
                r#"{"command":"cargo test\n  -p x","description":"d"}"#,
            ),
            msg(Role::Assistant, "done\nfor now"),
        ]);
        assert_eq!(
            t,
            format!(
                "{HEAD}[user] fix it\nplease\n[tool_call Read] a.rs\n\
                 [tool_call Bash] cargo test -p x\n[assistant] done\nfor now\n{TAIL}"
            )
        );
        assert!(!t.contains("hidden") && !t.contains("contents") && !t.contains("boom"));
    }

    #[test]
    fn the_target_is_the_first_known_key_else_nothing() {
        let line = |input: &str| transcript(&[call("T", input)]);
        let body = |s: String| s.lines().nth(3).unwrap_or_default().to_string();
        assert_eq!(
            body(line(r#"{"pattern":"fn x","path":"src"}"#)),
            "[tool_call T] src"
        );
        assert_eq!(body(line(r#"{"CommandLine":"ls"}"#)), "[tool_call T] ls");
        assert_eq!(
            body(line(r#"{"notebook_path":"n.ipynb"}"#)),
            "[tool_call T] n.ipynb"
        );
        assert_eq!(
            body(line(r#"{"url":"https://x.dev"}"#)),
            "[tool_call T] https://x.dev"
        );
        assert_eq!(body(line(r#"{"other":"x"}"#)), "[tool_call T]");
        assert_eq!(body(line(r#"{"path":5}"#)), "[tool_call T]");
        assert_eq!(body(line("not json")), "[tool_call T]");
        assert_eq!(
            body(transcript(&[msg(Role::ToolCall, "")])),
            "[tool_call tool]"
        );
    }

    #[test]
    fn skips_system_empty_and_delivery_tags() {
        let mut queued = msg(Role::User, "later");
        queued.tool_name = Some("queued".into());
        let mut steered = msg(Role::User, "now");
        steered.tool_name = Some("steered".into());
        let t = transcript(&[
            msg(Role::System, "secret system text"),
            msg(Role::Assistant, "   "),
            queued,
            steered,
        ]);
        assert!(t.contains("[user] later\n[user] now\n"));
        assert!(!t.contains("secret"));
        assert!(!t.contains("queued") && !t.contains("steered"));
        assert!(!t.contains("[assistant]"));
    }

    #[test]
    fn nothing_to_send_is_empty() {
        assert_eq!(transcript(&[]), "");
        assert_eq!(transcript(&[msg(Role::System, "x")]), "");
        assert_eq!(transcript(&[msg(Role::ToolResult, "x")]), "");
    }

    #[test]
    fn nothing_is_capped() {
        let big = "é😀".repeat(100_000);
        let mut ms = vec![msg(Role::User, "THE TASK")];
        ms.extend((0..50).map(|_| msg(Role::Assistant, &big)));
        ms.push(call(
            "Bash",
            &format!(r#"{{"command":"{}"}}"#, "c".repeat(5000)),
        ));
        let t = transcript(&ms);
        assert_eq!(t.matches(&big).count(), 50);
        assert!(t.contains(&"c".repeat(5000)));
        assert!(t.contains("[user] THE TASK\n"));
        assert!(t.starts_with(HEAD) && t.ends_with(TAIL));
    }
}
