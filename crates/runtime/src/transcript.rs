//! The hand-off text for a forked session: its copied history as one block the new provider
//! reads in front of the first prompt. Pure; never stored.

use xade_core::{Message, Role};

/// Whole-transcript cap, in chars.
const MAX_CHARS: usize = 150_000;
/// Room kept for the "omitted" marker and the closing tag.
const RESERVE: usize = 200;
const TEXT_MAX: usize = 20_000;
const TOOL_INPUT_MAX: usize = 1_000;
const TOOL_OUTPUT_MAX: usize = 2_000;
const TRUNCATED: &str = "…[truncated]";

const HEAD: &str = "<previous_conversation>\nThis conversation started in another agent session in this same directory. Its history follows; continue from where it ends.\n\n";
const TAIL: &str = "</previous_conversation>";

/// `messages` as a `<previous_conversation>` block; empty when nothing in it is worth sending.
/// `System` rows, diffs (`tool_payload_json` of results) and delivery tags are left out. Over
/// the cap the oldest items go first; the first user message (the task) always stays.
pub fn transcript(messages: &[Message]) -> String {
    let items: Vec<(bool, String)> = messages
        .iter()
        .filter_map(|m| item(m).map(|text| (m.role == Role::User, text)))
        .collect();
    if items.is_empty() {
        return String::new();
    }
    let len = |s: &str| s.chars().count() + 1;
    let total: usize = items.iter().map(|(_, t)| len(t)).sum();
    let anchor = items.iter().position(|(user, _)| *user);

    let (mut lines, mut omitted_at) = (Vec::new(), None);
    if total + HEAD.chars().count() + TAIL.len() <= MAX_CHARS {
        lines.extend(items.iter().map(|(_, t)| t.as_str()));
    } else {
        let mut budget = MAX_CHARS
            .saturating_sub(HEAD.chars().count() + RESERVE)
            .saturating_sub(anchor.map_or(0, |a| len(&items[a].1)));
        let mut start = items.len();
        for i in (0..items.len()).rev() {
            if Some(i) == anchor {
                start = i;
                break;
            }
            let need = len(&items[i].1);
            if need > budget {
                break;
            }
            budget -= need;
            start = i;
        }
        let separate_anchor = anchor.filter(|a| *a < start);
        if let Some(a) = separate_anchor {
            lines.push(items[a].1.as_str());
        }
        let omitted = start - usize::from(separate_anchor.is_some());
        if omitted > 0 {
            omitted_at = Some((lines.len(), omitted));
        }
        lines.extend(items[start..].iter().map(|(_, t)| t.as_str()));
    }

    let mut out = String::from(HEAD);
    for (i, line) in lines.iter().enumerate() {
        if let Some((_, n)) = omitted_at.filter(|(at, _)| *at == i) {
            out.push_str(&format!("[… {n} earlier messages omitted]\n"));
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(TAIL);
    out
}

/// One transcript line (possibly multi-line text) for a message, or `None` when skipped.
fn item(m: &Message) -> Option<String> {
    match m.role {
        Role::System => None,
        Role::User | Role::Assistant if m.content_text.trim().is_empty() => None,
        Role::User => Some(format!("[user] {}", cut(&m.content_text, TEXT_MAX))),
        Role::Assistant => Some(format!("[assistant] {}", cut(&m.content_text, TEXT_MAX))),
        Role::ToolCall => {
            let name = m.tool_name.as_deref().unwrap_or("tool");
            let input = m.tool_payload_json.as_deref().unwrap_or("");
            Some(format!("[tool_call {name}] {}", cut(input, TOOL_INPUT_MAX)))
        }
        Role::ToolResult => {
            let tag = if m.is_error {
                "tool_result error"
            } else {
                "tool_result"
            };
            Some(format!("[{tag}] {}", cut(&m.content_text, TOOL_OUTPUT_MAX)))
        }
    }
}

/// At most `max` chars (never splitting one), with a marker when cut.
fn cut(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        None => s.to_string(),
        Some((at, _)) => format!("{}{TRUNCATED}", &s[..at]),
    }
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
        }
    }

    fn call(name: &str, input: &str) -> Message {
        let mut m = msg(Role::ToolCall, "");
        m.tool_name = Some(name.into());
        m.tool_payload_json = Some(input.into());
        m
    }

    #[test]
    fn formats_every_role_between_the_tags() {
        let mut result = msg(Role::ToolResult, "boom");
        result.is_error = true;
        result.tool_payload_json = Some(r#"{"diff":"hidden"}"#.into());
        let t = transcript(&[
            msg(Role::User, "fix it\nplease"),
            call("Read", r#"{"file_path":"a.rs"}"#),
            msg(Role::ToolResult, "contents"),
            result,
            msg(Role::Assistant, "done"),
        ]);
        assert_eq!(
            t,
            format!(
                "{HEAD}[user] fix it\nplease\n[tool_call Read] {{\"file_path\":\"a.rs\"}}\n\
                 [tool_result] contents\n[tool_result error] boom\n[assistant] done\n{TAIL}"
            )
        );
        assert!(!t.contains("hidden"));
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
    }

    #[test]
    fn a_user_message_alone_is_enough() {
        let t = transcript(&[msg(Role::User, "just me")]);
        assert!(t.contains("[user] just me\n"));
    }

    #[test]
    fn items_are_cut_with_a_marker_by_kind() {
        let t = transcript(&[
            call("Bash", &"i".repeat(1500)),
            msg(Role::ToolResult, &"o".repeat(2500)),
            msg(Role::Assistant, &"a".repeat(TEXT_MAX + 50)),
        ]);
        assert!(t.contains(&format!("{}{TRUNCATED}\n", "i".repeat(1000))));
        assert!(!t.contains(&"i".repeat(1001)));
        assert!(t.contains(&format!("{}{TRUNCATED}\n", "o".repeat(2000))));
        assert!(!t.contains(&"o".repeat(2001)));
        assert!(t.contains(&format!("{}{TRUNCATED}\n", "a".repeat(TEXT_MAX))));
        // Exactly at the limit is untouched.
        assert_eq!(cut(&"x".repeat(1000), 1000), "x".repeat(1000));
    }

    #[test]
    fn cutting_never_splits_a_char() {
        let s = "é😀".repeat(600);
        let c = cut(&s, 1001);
        assert!(c.ends_with(TRUNCATED));
        let body = c.strip_suffix(TRUNCATED).unwrap_or(&c);
        assert_eq!(body.chars().count(), 1001);
        assert!(s.starts_with(body));
        let t = transcript(&[call("T", &"😀".repeat(5000))]);
        assert!(t.contains(&format!("{}{TRUNCATED}", "😀".repeat(1000))));
    }

    #[test]
    fn over_the_cap_the_oldest_go_first_and_the_task_stays() {
        let mut ms = vec![
            msg(Role::Assistant, "preamble"),
            msg(Role::User, "THE TASK"),
        ];
        for i in 0..40 {
            ms.push(msg(
                Role::Assistant,
                &format!("{i:02}{}", "z".repeat(TEXT_MAX)),
            ));
        }
        let t = transcript(&ms);
        let n = t.chars().count();
        assert!(n <= MAX_CHARS, "{n} chars");
        assert!(t.starts_with(HEAD));
        assert!(t.ends_with(TAIL));
        // Task first, then the marker, then the newest items in order.
        let task = t.find("[user] THE TASK").expect("task kept");
        let marker = t.find("earlier messages omitted]").expect("marker");
        let newest = t.find("[assistant] 39").expect("newest kept");
        assert!(task < marker && marker < newest);
        assert!(!t.contains("[assistant] 00") && !t.contains("preamble"));
        let kept = t.matches("[assistant] ").count();
        assert!(t.contains(&format!("[… {} earlier messages omitted]", 41 - kept)));
        let pos: Vec<_> = (30..40)
            .filter_map(|i| t.find(&format!("[assistant] {i:02}")))
            .collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn under_the_cap_nothing_is_dropped() {
        let ms: Vec<_> = (0..50).map(|i| msg(Role::User, &format!("m{i}"))).collect();
        let t = transcript(&ms);
        assert!(!t.contains("omitted"));
        assert_eq!(t.matches("[user] ").count(), 50);
    }
}
