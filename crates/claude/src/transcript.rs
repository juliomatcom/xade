//! Reads Claude Code transcripts (`~/.claude/projects/<dir>/<sessionId>.jsonl`) into `ImportedSession`s.
//! One JSON entry per line; unknown entries and malformed lines are skipped.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use xade_core::{ImportedMessage, ImportedSession, Role, Session, UsageTotals};

use crate::translate::result_text;

/// User text starting with one of these is CLI scaffolding, not something the user typed.
const SKIPPED_TEXT_PREFIXES: [&str; 4] = [
    "<command-",
    "<local-command",
    "<system-reminder>",
    "[Request interrupted",
];
const SYNTHETIC_MODEL: &str = "<synthetic>";

#[derive(Debug, Default)]
pub struct Parsed {
    /// `None` when the file has no `cwd`, no timestamps or no messages.
    pub session: Option<ImportedSession>,
    pub bad_lines: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub files: usize,
    pub parsed: usize,
    /// Readable files that held no importable session.
    pub skipped: usize,
    pub read_errors: usize,
    pub bad_lines: usize,
    pub messages: usize,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub sessions: Vec<ImportedSession>,
    pub stats: ScanStats,
}

/// Parses every `<dir>/<sessionId>.jsonl` directly under the project directories of `root`.
pub fn scan_projects(root: &Path) -> Scan {
    let mut scan = Scan::default();
    let dirs = match std::fs::read_dir(root) {
        Ok(d) => d,
        // No history yet is normal.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return scan,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the Claude projects directory");
            scan.stats.read_errors += 1;
            return scan;
        }
    };
    for dir in dirs.flatten().filter(|d| d.path().is_dir()) {
        let files = match std::fs::read_dir(dir.path()) {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(error = %e, "project directory unreadable");
                scan.stats.read_errors += 1;
                continue;
            }
        };
        for path in files.flatten().map(|f| f.path()) {
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") || !path.is_file() {
                continue;
            }
            scan.stats.files += 1;
            match parse_file(&path) {
                Ok(parsed) => {
                    scan.stats.bad_lines += parsed.bad_lines;
                    match parsed.session {
                        Some(s) => {
                            scan.stats.parsed += 1;
                            scan.stats.messages += s.messages.len();
                            scan.sessions.push(s);
                        }
                        None => scan.stats.skipped += 1,
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "transcript unreadable");
                    scan.stats.read_errors += 1;
                }
            }
        }
    }
    scan
}

fn parse_file(path: &Path) -> std::io::Result<Parsed> {
    let id: PathBuf = path.file_stem().map(Into::into).unwrap_or_default();
    Ok(parse_transcript(
        &id.to_string_lossy(),
        BufReader::new(File::open(path)?),
    ))
}

#[derive(Default)]
struct Builder {
    cwd: Option<String>,
    first_ts: Option<OffsetDateTime>,
    last_ts: Option<OffsetDateTime>,
    ai_title: Option<String>,
    first_user_text: Option<String>,
    model: Option<String>,
    messages: Vec<ImportedMessage>,
    /// Usage per assistant `message.id`: one API message spans several entries with the same usage.
    usage_by_message: HashMap<String, [u64; 3]>,
    context_used: u64,
}

/// `session_id` is the file stem, which is what `--resume` takes.
pub fn parse_transcript(session_id: &str, reader: impl BufRead) -> Parsed {
    let mut b = Builder::default();
    let mut bad_lines = 0;
    for line in reader.lines() {
        let Ok(line) = line else {
            bad_lines += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(entry) => b.on_entry(&entry),
            Err(_) => bad_lines += 1,
        }
    }
    Parsed {
        session: b.finish(session_id),
        bad_lines,
    }
}

fn flag(entry: &Value, key: &str) -> bool {
    entry.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn token(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(Value::as_u64).unwrap_or(0)
}

impl Builder {
    fn on_entry(&mut self, entry: &Value) {
        let ts = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok());
        if let Some(t) = ts {
            self.first_ts = Some(self.first_ts.map_or(t, |f| f.min(t)));
            self.last_ts = Some(self.last_ts.map_or(t, |l| l.max(t)));
        }
        if self.cwd.is_none() {
            self.cwd = entry
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(String::from);
        }
        if flag(entry, "isSidechain") || flag(entry, "isMeta") || flag(entry, "isCompactSummary") {
            return;
        }
        let at = ts.unwrap_or(OffsetDateTime::UNIX_EPOCH);
        match entry.get("type").and_then(Value::as_str) {
            Some("ai-title") => {
                if let Some(t) = entry
                    .get("aiTitle")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    self.ai_title = Some(t.to_string());
                }
            }
            Some("user") => self.on_user(entry, at),
            Some("assistant") => self.on_assistant(entry, at),
            _ => {}
        }
    }

    fn push(&mut self, role: Role, at: OffsetDateTime) -> &mut ImportedMessage {
        self.messages.push(ImportedMessage {
            role,
            content_text: String::new(),
            tool_name: None,
            tool_use_id: None,
            tool_payload_json: None,
            is_error: false,
            created_at: at,
        });
        let last = self.messages.len() - 1;
        &mut self.messages[last]
    }

    fn user_text(&mut self, text: &str, at: OffsetDateTime) {
        let trimmed = text.trim_start();
        if trimmed.trim().is_empty() || SKIPPED_TEXT_PREFIXES.iter().any(|p| trimmed.starts_with(p))
        {
            return;
        }
        if self.first_user_text.is_none() {
            self.first_user_text = Some(text.to_string());
        }
        self.push(Role::User, at).content_text = text.to_string();
    }

    fn on_user(&mut self, entry: &Value, at: OffsetDateTime) {
        let Some(content) = entry.pointer("/message/content") else {
            return;
        };
        let blocks = match content {
            Value::String(s) => return self.user_text(s, at),
            Value::Array(blocks) => blocks,
            _ => return,
        };
        let payload = entry
            .get("toolUseResult")
            .filter(|v| !v.is_null())
            .map(Value::to_string);
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        self.user_text(t, at);
                    }
                }
                Some("tool_result") => {
                    let m = self.push(Role::ToolResult, at);
                    m.content_text = result_text(block.get("content"));
                    m.tool_use_id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .map(String::from);
                    m.tool_payload_json = payload.clone();
                    m.is_error = block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                }
                _ => {}
            }
        }
    }

    fn on_assistant(&mut self, entry: &Value, at: OffsetDateTime) {
        let Some(message) = entry.get("message") else {
            return;
        };
        if let Some(m) = message
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty() && *m != SYNTHETIC_MODEL)
        {
            self.model = Some(m.to_string());
        }
        if let Some(u) = message.get("usage").filter(|u| u.is_object()) {
            let input = token(u, "input_tokens");
            let cache_read = token(u, "cache_read_input_tokens");
            self.context_used = input + cache_read + token(u, "cache_creation_input_tokens");
            let key = message.get("id").and_then(Value::as_str).map_or_else(
                || format!("entry-{}", self.usage_by_message.len()),
                String::from,
            );
            self.usage_by_message
                .insert(key, [input, token(u, "output_tokens"), cache_read]);
        }
        let blocks = match message.get("content") {
            Some(Value::String(s)) if !s.trim().is_empty() => {
                self.push(Role::Assistant, at).content_text = s.clone();
                return;
            }
            Some(Value::Array(blocks)) => blocks,
            _ => return,
        };
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    if !text.trim().is_empty() {
                        self.push(Role::Assistant, at).content_text = text.to_string();
                    }
                }
                Some("tool_use") => {
                    let m = self.push(Role::ToolCall, at);
                    m.tool_name = block.get("name").and_then(Value::as_str).map(String::from);
                    m.tool_use_id = block.get("id").and_then(Value::as_str).map(String::from);
                    m.tool_payload_json = block.get("input").map(Value::to_string);
                }
                _ => {}
            }
        }
    }

    fn finish(self, session_id: &str) -> Option<ImportedSession> {
        let (cwd, created_at, updated_at) = (self.cwd?, self.first_ts?, self.last_ts?);
        if self.messages.is_empty() {
            return None;
        }
        let title = self
            .ai_title
            .or_else(|| self.first_user_text.as_deref().map(Session::derive_title))
            .unwrap_or_default();
        let (mut input, mut output, mut cache_read) = (0, 0, 0);
        for [i, o, c] in self.usage_by_message.values() {
            input += i;
            output += o;
            cache_read += c;
        }
        Some(ImportedSession {
            provider_session_id: session_id.to_string(),
            cwd: cwd.into(),
            title,
            model_id: self.model.unwrap_or_default(),
            created_at,
            updated_at,
            usage: UsageTotals {
                context_tokens_used: self.context_used,
                context_window_max: 0,
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cache_read,
                cost_usd: 0.0,
            },
            messages: self.messages,
        })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn parse(id: &str, text: &str) -> Parsed {
        parse_transcript(id, text.as_bytes())
    }

    fn fixture() -> ImportedSession {
        let path = format!(
            "{}/fixtures/transcripts/tool_allow.jsonl",
            env!("CARGO_MANIFEST_DIR")
        );
        let file = File::open(path).expect("fixture");
        let parsed = parse_transcript("787c9e37-3779-4a27-a9ed-e0c95c008543", BufReader::new(file));
        assert_eq!(parsed.bad_lines, 0);
        parsed.session.expect("session")
    }

    #[test]
    fn real_transcript_maps_title_model_and_roles() {
        let s = fixture();
        assert_eq!(
            s.provider_session_id,
            "787c9e37-3779-4a27-a9ed-e0c95c008543"
        );
        assert_eq!(s.cwd, PathBuf::from("/tmp/xade-fixture"));
        assert_eq!(s.title, "Notes.txt file creation and edit");
        assert_eq!(s.model_id, "claude-haiku-5-5");
        assert!(s.created_at < s.updated_at);
        let roles: Vec<_> = s.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles[0], Role::User);
        assert_eq!(roles.iter().filter(|r| **r == Role::ToolCall).count(), 3);
        assert_eq!(roles.iter().filter(|r| **r == Role::ToolResult).count(), 3);
        assert_eq!(roles.last(), Some(&Role::Assistant));
        // Thinking blocks are skipped.
        assert!(s.messages.iter().all(|m| m.role != Role::System));
        assert!(s
            .messages
            .windows(2)
            .all(|w| w[0].created_at <= w[1].created_at));
    }

    #[test]
    fn real_transcript_tool_calls_pair_with_results_and_carry_patch() {
        let s = fixture();
        let calls: Vec<_> = s
            .messages
            .iter()
            .filter(|m| m.role == Role::ToolCall)
            .collect();
        let names: Vec<_> = calls
            .iter()
            .filter_map(|m| m.tool_name.as_deref())
            .collect();
        assert_eq!(names, ["Bash", "Write", "Edit"]);
        for call in &calls {
            assert!(call
                .tool_payload_json
                .as_deref()
                .is_some_and(|j| j.starts_with('{')));
            let result = s
                .messages
                .iter()
                .find(|m| m.role == Role::ToolResult && m.tool_use_id == call.tool_use_id)
                .expect("paired result");
            assert!(result.tool_payload_json.is_some());
            assert!(!result.is_error);
        }
        let edit_result = s
            .messages
            .iter()
            .rfind(|m| m.role == Role::ToolResult)
            .expect("edit result");
        let payload: Value =
            serde_json::from_str(edit_result.tool_payload_json.as_deref().expect("payload"))
                .expect("json");
        assert!(payload["structuredPatch"].is_array());
        assert_eq!(payload["newString"], "hello");
    }

    #[test]
    fn real_transcript_usage_dedupes_entries_of_one_api_message() {
        let s = fixture();
        // Two entries share each message id; output tokens are counted once per message.
        assert_eq!(s.usage.output_tokens, 258 + 222 + 153 + 5);
        assert_eq!(s.usage.input_tokens, 2 * 4);
        assert_eq!(s.usage.context_tokens_used, 2 + 46036 + 266);
        assert_eq!(s.usage.context_window_max, 0);
        assert_eq!(s.usage.cost_usd, 0.0);
    }

    fn line(v: Value) -> String {
        format!("{v}\n")
    }

    #[test]
    fn ai_title_last_wins_and_command_wrappers_are_skipped() {
        let mut t = String::new();
        let at = |n: u32| format!("2026-01-01T00:00:0{n}.000Z");
        t += &line(serde_json::json!({"type":"ai-title","aiTitle":"First","sessionId":"s"}));
        t += &line(
            serde_json::json!({"type":"user","cwd":"/w","timestamp":at(1),"message":{"role":"user","content":"<command-name>/clear</command-name>"}}),
        );
        t += &line(
            serde_json::json!({"type":"user","timestamp":at(2),"message":{"role":"user","content":[{"type":"text","text":"<local-command-stdout>x</local-command-stdout>"}]}}),
        );
        t += &line(
            serde_json::json!({"type":"user","timestamp":at(3),"message":{"role":"user","content":[{"type":"text","text":"<system-reminder>r</system-reminder>"}]}}),
        );
        t += &line(
            serde_json::json!({"type":"user","timestamp":at(4),"message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}),
        );
        t += &line(
            serde_json::json!({"type":"user","timestamp":at(5),"message":{"role":"user","content":"real prompt"}}),
        );
        t += &line(serde_json::json!({"type":"ai-title","aiTitle":"Second"}));
        let p = parse("s", &t);
        let s = p.session.expect("session");
        assert_eq!(s.title, "Second");
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].content_text, "real prompt");
        assert_eq!(s.messages[0].role, Role::User);
    }

    #[test]
    fn title_falls_back_to_first_user_text() {
        let t = line(
            serde_json::json!({"type":"user","cwd":"/w","timestamp":"2026-01-01T00:00:00Z",
            "message":{"role":"user","content":"  fix the bug \nplease"}}),
        );
        let s = parse("s", &t).session.expect("session");
        assert_eq!(s.title, "fix the bug");
    }

    #[test]
    fn sidechain_meta_and_unknown_entries_are_skipped_and_bad_lines_counted() {
        let ts = "2026-01-01T00:00:00Z";
        let mut t = String::new();
        t += &line(
            serde_json::json!({"type":"user","cwd":"/w","timestamp":ts,"message":{"role":"user","content":"hello"}}),
        );
        t += &line(
            serde_json::json!({"type":"user","isSidechain":true,"timestamp":ts,"message":{"role":"user","content":"sub"}}),
        );
        t += &line(
            serde_json::json!({"type":"user","isMeta":true,"timestamp":ts,"message":{"role":"user","content":"meta"}}),
        );
        t += &line(
            serde_json::json!({"type":"assistant","isSidechain":true,"timestamp":ts,"message":{"model":"side","content":[{"type":"text","text":"sub answer"}]}}),
        );
        t += &line(
            serde_json::json!({"type":"assistant","timestamp":ts,"message":{"model":"<synthetic>","content":[{"type":"text","text":"ok"}]}}),
        );
        t += &line(
            serde_json::json!({"type":"assistant","timestamp":ts,"message":{"model":"m-1","content":[{"type":"thinking","thinking":""},{"type":"text","text":"hi"}]}}),
        );
        t += &line(serde_json::json!({"type":"future-entry","x":[1,2]}));
        t += "{not json\n\n";
        let p = parse("s", &t);
        assert_eq!(p.bad_lines, 1);
        let s = p.session.expect("session");
        assert_eq!(s.model_id, "m-1");
        let texts: Vec<_> = s.messages.iter().map(|m| m.content_text.as_str()).collect();
        assert_eq!(texts, ["hello", "ok", "hi"]);
    }

    #[test]
    fn files_without_cwd_or_messages_are_not_sessions() {
        let no_cwd = line(
            serde_json::json!({"type":"user","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"x"}}),
        );
        assert!(parse("s", &no_cwd).session.is_none());
        let no_msgs = line(
            serde_json::json!({"type":"ai-title","cwd":"/w","timestamp":"2026-01-01T00:00:00Z","aiTitle":"t"}),
        );
        assert!(parse("s", &no_msgs).session.is_none());
        assert!(parse("s", "").session.is_none());
    }

    #[test]
    fn tool_result_array_content_and_error_flag() {
        let t = line(
            serde_json::json!({"type":"user","cwd":"/w","timestamp":"2026-01-01T00:00:00Z",
            "toolUseResult":{"k":1},
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,
                "content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}]}}),
        );
        let s = parse("s", &t).session.expect("session");
        let m = &s.messages[0];
        assert_eq!(m.role, Role::ToolResult);
        assert_eq!(m.content_text, "a\nb");
        assert!(m.is_error);
        assert_eq!(m.tool_payload_json.as_deref(), Some(r#"{"k":1}"#));
    }

    #[test]
    fn scan_projects_reads_only_top_level_jsonl_in_project_dirs() {
        let root = std::env::temp_dir().join(format!("xade-scan-{}", std::process::id()));
        let proj = root.join("-proj");
        std::fs::create_dir_all(proj.join("nested")).expect("dirs");
        let entry = line(
            serde_json::json!({"type":"user","cwd":"/w","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hi"}}),
        );
        std::fs::write(proj.join("abc.jsonl"), &entry).expect("write");
        std::fs::write(proj.join("nested/other.jsonl"), &entry).expect("write");
        std::fs::write(proj.join("notes.txt"), "x").expect("write");
        std::fs::write(proj.join("empty.jsonl"), "").expect("write");
        let scan = scan_projects(&root);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(scan.sessions.len(), 1);
        assert_eq!(scan.sessions[0].provider_session_id, "abc");
        assert_eq!(
            scan.stats,
            ScanStats {
                files: 2,
                parsed: 1,
                skipped: 1,
                read_errors: 0,
                bad_lines: 0,
                messages: 1
            }
        );
    }
}
