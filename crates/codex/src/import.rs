//! Pure parsing and filtering for Codex `thread/list` + `thread/read` history import.

use std::path::PathBuf;

use serde_json::Value;
use time::OffsetDateTime;
use xade_core::{ImportedMessage, ImportedSession, Role, Session, UsageTotals};

use crate::translate::{
    command_input_json, command_result, file_change_result, file_change_tool, str_field,
};

/// Whether a `thread/list` entry should be imported into Xade.
/// Skips ephemeral threads, sub-agent threads (`parentThreadId != null`), threads originated by
/// Xade itself (`originator` starting with `"xade"`), and malformed entries without `id`/`cwd`.
pub fn should_import_thread(thread: &Value) -> bool {
    let id = thread.get("id").and_then(Value::as_str).unwrap_or_default();
    let cwd = thread
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if id.is_empty() || cwd.is_empty() {
        return false;
    }
    if thread
        .get("ephemeral")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return false;
    }
    if thread
        .get("parentThreadId")
        .and_then(Value::as_str)
        .is_some_and(|p| !p.is_empty())
    {
        return false;
    }
    let originator = thread
        .get("originator")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if originator.starts_with("xade") {
        return false;
    }
    true
}

/// Converts a `thread/list` summary and its `thread/read` (`includeTurns: true`) result into an
/// [`ImportedSession`]. Returns `None` if essential fields or messages are missing.
pub fn parse_thread(summary: &Value, detail: &Value) -> Option<ImportedSession> {
    let thread = detail.get("thread").unwrap_or(detail);
    let id = non_empty_str(thread, "id").or_else(|| non_empty_str(summary, "id"))?;
    let cwd = non_empty_str(thread, "cwd").or_else(|| non_empty_str(summary, "cwd"))?;
    let created_at = from_unix_secs(thread.get("createdAt").or_else(|| summary.get("createdAt")))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let updated_at = from_unix_secs(thread.get("updatedAt").or_else(|| summary.get("updatedAt")))
        .unwrap_or(created_at);
    let model_id = non_empty_str(thread, "model")
        .or_else(|| non_empty_str(summary, "model"))
        .unwrap_or_default()
        .to_string();

    let mut messages = Vec::new();
    if let Some(turns) = thread.get("turns").and_then(Value::as_array) {
        for turn in turns {
            let turn_start = from_unix_secs(turn.get("startedAt")).unwrap_or(created_at);
            let turn_end = from_unix_secs(turn.get("completedAt")).unwrap_or(turn_start);
            let Some(items) = turn.get("items").and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                push_item(item, turn_start, turn_end, &mut messages);
            }
        }
    }
    if messages.is_empty() {
        return None;
    }

    let title = non_empty_trimmed(thread, "name")
        .or_else(|| non_empty_trimmed(summary, "name"))
        .map(String::from)
        .or_else(|| {
            non_empty_trimmed(thread, "preview")
                .or_else(|| non_empty_trimmed(summary, "preview"))
                .map(Session::derive_title)
        })
        .or_else(|| {
            messages
                .iter()
                .find(|m| m.role == Role::User && !m.content_text.trim().is_empty())
                .map(|m| Session::derive_title(&m.content_text))
        })
        .unwrap_or_default();

    Some(ImportedSession {
        provider_session_id: id.to_string(),
        cwd: PathBuf::from(cwd),
        title,
        model_id,
        created_at,
        updated_at,
        usage: UsageTotals::default(),
        messages,
    })
}

fn push_item(
    item: &Value,
    turn_start: OffsetDateTime,
    turn_end: OffsetDateTime,
    out: &mut Vec<ImportedMessage>,
) {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    let item_start = from_unix_ms(item.get("startedAtMs")).unwrap_or(turn_start);
    let item_end = from_unix_ms(item.get("completedAtMs"))
        .or_else(|| from_unix_ms(item.get("startedAtMs")))
        .unwrap_or(turn_end);

    match kind {
        "userMessage" => {
            let text = user_message_text(item);
            if !text.trim().is_empty() {
                out.push(ImportedMessage {
                    role: Role::User,
                    content_text: text,
                    tool_name: None,
                    tool_use_id: None,
                    tool_payload_json: None,
                    is_error: false,
                    created_at: item_start,
                });
            }
        }
        "agentMessage" => {
            let text = item.get("text").and_then(Value::as_str).unwrap_or_default();
            if !text.trim().is_empty() {
                out.push(ImportedMessage {
                    role: Role::Assistant,
                    content_text: text.to_string(),
                    tool_name: None,
                    tool_use_id: None,
                    tool_payload_json: None,
                    is_error: false,
                    created_at: item_start,
                });
            }
        }
        "commandExecution" => {
            let id = str_field(item, "id");
            if id.is_empty() {
                return;
            }
            out.push(ImportedMessage {
                role: Role::ToolCall,
                content_text: String::new(),
                tool_name: Some("Bash".to_string()),
                tool_use_id: Some(id.clone()),
                tool_payload_json: Some(command_input_json(item)),
                is_error: false,
                created_at: item_start,
            });
            let (output, is_error) = command_result(item);
            out.push(ImportedMessage {
                role: Role::ToolResult,
                content_text: output,
                tool_name: None,
                tool_use_id: Some(id),
                tool_payload_json: None,
                is_error,
                created_at: item_end,
            });
        }
        "fileChange" => {
            let id = str_field(item, "id");
            if id.is_empty() {
                return;
            }
            let (name, input_json) = file_change_tool(item);
            out.push(ImportedMessage {
                role: Role::ToolCall,
                content_text: String::new(),
                tool_name: Some(name.to_string()),
                tool_use_id: Some(id.clone()),
                tool_payload_json: Some(input_json),
                is_error: false,
                created_at: item_start,
            });
            let (output, is_error, structured_json) = file_change_result(item);
            out.push(ImportedMessage {
                role: Role::ToolResult,
                content_text: output,
                tool_name: None,
                tool_use_id: Some(id),
                tool_payload_json: structured_json,
                is_error,
                created_at: item_end,
            });
        }
        // Reasoning and unknown item types are skipped.
        _ => {}
    }
}

fn user_message_text(item: &Value) -> String {
    let Some(parts) = item.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    parts
        .iter()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn non_empty_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn non_empty_trimmed<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn from_unix_secs(v: Option<&Value>) -> Option<OffsetDateTime> {
    v.and_then(Value::as_i64)
        .and_then(|s| OffsetDateTime::from_unix_timestamp(s).ok())
}

fn from_unix_ms(v: Option<&Value>) -> Option<OffsetDateTime> {
    v.and_then(Value::as_i64)
        .and_then(|ms| OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn should_import_filters_xade_ephemeral_subagent_and_incomplete_threads() {
        let base = json!({
            "id": "t1",
            "cwd": "/work/app",
            "originator": "codex-tui",
            "ephemeral": false,
            "parentThreadId": null,
        });
        assert!(should_import_thread(&base));

        let mut xade = base.clone();
        xade["originator"] = json!("xade");
        assert!(!should_import_thread(&xade));

        let mut xade_sub = base.clone();
        xade_sub["originator"] = json!("xade-desktop");
        assert!(!should_import_thread(&xade_sub));

        let mut ephemeral = base.clone();
        ephemeral["ephemeral"] = json!(true);
        assert!(!should_import_thread(&ephemeral));

        let mut subagent = base.clone();
        subagent["parentThreadId"] = json!("parent-1");
        assert!(!should_import_thread(&subagent));

        let mut missing_cwd = base.clone();
        missing_cwd["cwd"] = json!("");
        assert!(!should_import_thread(&missing_cwd));
    }

    #[test]
    fn parse_thread_maps_file_changes_skips_reasoning_and_prefers_name_over_preview() {
        let summary = json!({
            "id": "t-edit",
            "cwd": "/work/repo",
            "model": "gpt-6.2-sol",
            "name": "Fix notes",
            "preview": "Replace beta with BETA",
            "createdAt": 1_700_000_000,
            "updatedAt": 1_700_000_050,
        });
        let detail = json!({
            "thread": {
                "id": "t-edit",
                "cwd": "/work/repo",
                "model": "gpt-6.2-sol",
                "name": "Fix notes",
                "preview": "Replace beta with BETA",
                "createdAt": 1_700_000_000,
                "updatedAt": 1_700_000_050,
                "turns": [{
                    "id": "turn-1",
                    "status": "completed",
                    "startedAt": 1_700_000_010,
                    "completedAt": 1_700_000_040,
                    "items": [
                        {
                            "type": "userMessage",
                            "id": "u1",
                            "content": [
                                {"type": "text", "text": "Line 1"},
                                {"type": "localImage", "path": "/tmp/x.png"},
                                {"type": "text", "text": "Line 2"}
                            ]
                        },
                        {"type": "reasoning", "id": "r1", "summary": ["thinking"]},
                        {
                            "type": "fileChange",
                            "id": "fc-add",
                            "status": "completed",
                            "changes": [{
                                "path": "/work/repo/new.txt",
                                "kind": {"type": "add"},
                                "diff": "hi\n"
                            }],
                            "startedAtMs": 1_700_000_015_000_i64,
                            "completedAtMs": 1_700_000_016_000_i64
                        },
                        {
                            "type": "fileChange",
                            "id": "fc-upd",
                            "status": "completed",
                            "changes": [{
                                "path": "/work/repo/notes.txt",
                                "kind": {"type": "update"},
                                "diff": "@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n"
                            }]
                        },
                        {
                            "type": "fileChange",
                            "id": "fc-dec",
                            "status": "declined",
                            "changes": [{
                                "path": "/work/repo/notes.txt",
                                "kind": {"type": "update"},
                                "diff": "@@ -1 +1 @@\n-a\n+b\n"
                            }]
                        },
                        {"type": "unknownFutureItem", "id": "x1"},
                        {"type": "agentMessage", "id": "a1", "text": "Done."}
                    ]
                }]
            }
        });

        let imp = parse_thread(&summary, &detail).expect("parsed");
        assert_eq!(imp.provider_session_id, "t-edit");
        assert_eq!(imp.cwd, PathBuf::from("/work/repo"));
        assert_eq!(imp.title, "Fix notes");
        assert_eq!(imp.model_id, "gpt-6.2-sol");
        assert_eq!(imp.created_at.unix_timestamp(), 1_700_000_000);
        assert_eq!(imp.updated_at.unix_timestamp(), 1_700_000_050);
        assert_eq!(imp.usage, UsageTotals::default());

        let roles: Vec<Role> = imp.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [
                Role::User,
                Role::ToolCall,
                Role::ToolResult,
                Role::ToolCall,
                Role::ToolResult,
                Role::ToolCall,
                Role::ToolResult,
                Role::Assistant,
            ]
        );
        // User joins text parts and falls back to turn startedAt when item startedAtMs is absent.
        assert_eq!(imp.messages[0].content_text, "Line 1\nLine 2");
        assert_eq!(imp.messages[0].created_at.unix_timestamp(), 1_700_000_010);

        // Single add -> Write + create structuredPatch.
        assert_eq!(imp.messages[1].tool_name.as_deref(), Some("Write"));
        assert_eq!(imp.messages[1].created_at.unix_timestamp(), 1_700_000_015);
        assert_eq!(imp.messages[2].created_at.unix_timestamp(), 1_700_000_016);
        assert!(!imp.messages[2].is_error);
        let add_payload: Value =
            serde_json::from_str(imp.messages[2].tool_payload_json.as_deref().expect("json"))
                .expect("valid json");
        assert_eq!(add_payload["type"], "create");

        // Update -> Edit + parsed structuredPatch hunks.
        assert_eq!(imp.messages[3].tool_name.as_deref(), Some("Edit"));
        let edit_payload: Value =
            serde_json::from_str(imp.messages[4].tool_payload_json.as_deref().expect("json"))
                .expect("valid json");
        assert_eq!(
            edit_payload["structuredPatch"][0]["lines"],
            json!([" alpha", "-beta", "+BETA", " gamma"])
        );

        // Declined file change -> is_error, no structured payload, status verb in output.
        assert!(imp.messages[6].is_error);
        assert_eq!(imp.messages[6].tool_payload_json, None);
        assert_eq!(
            imp.messages[6].content_text,
            "/work/repo/notes.txt: declined"
        );

        assert_eq!(imp.messages[7].content_text, "Done.");
    }

    #[test]
    fn parse_thread_derives_title_from_preview_when_name_is_null() {
        let summary = json!({
            "id": "t2",
            "cwd": "/work/repo",
            "name": null,
            "preview": "  first line of prompt\nsecond line",
            "createdAt": 100,
            "updatedAt": 120,
        });
        let detail = json!({
            "thread": {
                "id": "t2",
                "cwd": "/work/repo",
                "turns": [{
                    "startedAt": 100,
                    "items": [{"type": "userMessage", "content": [{"type": "text", "text": "hi"}]}]
                }]
            }
        });
        let imp = parse_thread(&summary, &detail).expect("parsed");
        assert_eq!(imp.title, "first line of prompt");
        assert_eq!(imp.model_id, "");
    }

    #[test]
    fn parse_thread_returns_none_when_no_messages() {
        let summary = json!({"id": "t3", "cwd": "/work/repo", "createdAt": 1, "updatedAt": 1});
        let detail = json!({"thread": {"id": "t3", "cwd": "/work/repo", "turns": []}});
        assert!(parse_thread(&summary, &detail).is_none());
    }
}
