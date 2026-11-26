//! Multi-session domain types and deterministic member handle generation.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::ids::{MultiSessionId, ProviderId, WorkspaceId};
use crate::model::Effort;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiSession {
    pub id: MultiSessionId,
    pub workspace_id: WorkspaceId,
    pub title: String,
    pub preview_snippet: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// One member requested when creating a multi-session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiMemberSpec {
    pub provider_id: ProviderId,
    pub model_id: String,
    pub effort: Effort,
    pub auto_mode: bool,
}

/// Compact member descriptor shown on Home cards and headers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiMemberBadge {
    pub provider_id: ProviderId,
    pub handle: String,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeamMessageKind {
    /// Sent by the user; `target` is `None` for all members or `Some(handle)` for one.
    User { target: Option<String> },
    /// Broadcast emitted by a team member.
    Broadcast {
        sender_handle: String,
        sender_provider: ProviderId,
    },
}

/// One entry in a multi-session's shared chronological Team feed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamMessage {
    pub id: Uuid,
    pub multi_session_id: MultiSessionId,
    pub seq: i64,
    pub kind: TeamMessageKind,
    pub content_text: String,
    pub created_at: OffsetDateTime,
}

const VENDOR_PREFIXES: &[&str] = &["claude", "gemini"];
const EFFORT_SUFFIXES: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Short lowercase slug derived from a model id (e.g. `claude-sonnet-4-6` -> `sonnet`,
/// `gemini-4-neon` -> `neon`, `gemini-3.8-flash` -> `flash`).
pub fn short_model_slug(model_id: &str) -> String {
    let lower = model_id.trim().to_ascii_lowercase();
    let mut parts: Vec<String> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    // If a known vendor token appears (even after dotted region/provider prefixes like
    // `us.anthropic.claude-sonnet-4-6`), drop everything up through that token.
    if let Some(pos) = parts
        .iter()
        .position(|p| VENDOR_PREFIXES.contains(&p.as_str()))
    {
        parts.drain(..=pos);
    }
    while parts
        .last()
        .is_some_and(|p| EFFORT_SUFFIXES.contains(&p.as_str()))
    {
        parts.pop();
    }
    let meaningful: Vec<String> = parts.into_iter().filter(|p| !is_version_token(p)).collect();
    if meaningful.is_empty() {
        let fallback: String = lower
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let trimmed = fallback.trim_matches('-');
        if trimmed.is_empty() {
            return "agent".to_string();
        }
        return collapse_dashes(trimmed);
    }
    // Keep at most the last 2 meaningful tokens so verbose ids like
    // `gemini-pro-orchestrator-neon` stay compact (`orchestrator-neon`).
    let start = meaningful.len().saturating_sub(2);
    meaningful[start..].join("-")
}

/// Assigns deterministic, unique handles `<slug>-<n>` (1-based per slug) in roster order.
pub fn assign_handles<S: AsRef<str>>(model_ids: &[S]) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    model_ids
        .iter()
        .map(|m| {
            let slug = short_model_slug(m.as_ref());
            let n = counts.entry(slug.clone()).or_insert(0);
            *n += 1;
            format!("{slug}-{n}")
        })
        .collect()
}

fn is_version_token(tok: &str) -> bool {
    !tok.is_empty() && tok.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn collapse_dashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c == '-' {
            if !prev_dash {
                out.push(c);
            }
            prev_dash = true;
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    out
}

/// Segment of an assistant message split into user-only text and `<broadcast>` blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextSegment {
    Plain(String),
    Broadcast(String),
}

/// Splits assistant text into plain (user-only) and `<broadcast>...</broadcast>` segments,
/// ignoring any tags inside fenced markdown code blocks (```` ``` ```` or `~~~`, both outside and
/// inside broadcasts) and inside inline backtick code spans.
pub fn split_broadcast_segments(text: &str) -> Vec<TextSegment> {
    let mut segments = Vec::new();
    let mut plain = String::new();
    let mut broadcast = String::new();
    let mut in_broadcast = false;
    let mut fence: Option<(char, usize)> = None;

    for raw_line in text.split_inclusive('\n') {
        let trimmed_start = raw_line.trim_start();
        if let Some((fch, flen)) = fence {
            if in_broadcast {
                broadcast.push_str(raw_line);
            } else {
                plain.push_str(raw_line);
            }
            if is_closing_fence(trimmed_start, fch, flen) {
                fence = None;
            }
            continue;
        }
        if let Some((fch, flen)) = opening_fence(trimmed_start) {
            if in_broadcast {
                broadcast.push_str(raw_line);
            } else {
                plain.push_str(raw_line);
            }
            fence = Some((fch, flen));
            continue;
        }

        // Scan the line byte-by-byte while skipping inline backtick code spans.
        let bytes = raw_line.as_bytes();
        let mut i = 0;
        let mut inline_ticks: Option<usize> = None;
        while i < bytes.len() {
            if bytes[i] == b'`' {
                let run_len = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                let chunk = &raw_line[i..i + run_len];
                if in_broadcast {
                    broadcast.push_str(chunk);
                } else {
                    plain.push_str(chunk);
                }
                match inline_ticks {
                    None => inline_ticks = Some(run_len),
                    Some(n) if n == run_len => inline_ticks = None,
                    Some(_) => {}
                }
                i += run_len;
                continue;
            }
            if inline_ticks.is_none() {
                if in_broadcast {
                    if starts_with_ci(&raw_line[i..], "</broadcast>") {
                        let content = broadcast.trim().to_string();
                        broadcast.clear();
                        if !content.is_empty() {
                            segments.push(TextSegment::Broadcast(content));
                        }
                        in_broadcast = false;
                        i += "</broadcast>".len();
                        continue;
                    }
                    if starts_with_ci(&raw_line[i..], "<broadcast>") {
                        // Malformed unclosed outer `<broadcast>`: flush it as plain text and
                        // start a fresh broadcast from this opening tag.
                        plain.push_str("<broadcast>");
                        plain.push_str(&broadcast);
                        broadcast.clear();
                        let p = plain.trim().to_string();
                        plain.clear();
                        if !p.is_empty() {
                            segments.push(TextSegment::Plain(p));
                        }
                        i += "<broadcast>".len();
                        continue;
                    }
                } else if starts_with_ci(&raw_line[i..], "<broadcast>") {
                    let p = plain.trim().to_string();
                    plain.clear();
                    if !p.is_empty() {
                        segments.push(TextSegment::Plain(p));
                    }
                    in_broadcast = true;
                    i += "<broadcast>".len();
                    continue;
                }
            }
            let ch = raw_line[i..].chars().next().unwrap_or('\0');
            let clen = ch.len_utf8();
            if in_broadcast {
                broadcast.push(ch);
            } else {
                plain.push(ch);
            }
            i += clen;
        }
    }

    if in_broadcast {
        // Unclosed `<broadcast>` tag: treat as plain text so nothing is silently lost.
        plain.push_str("<broadcast>");
        plain.push_str(&broadcast);
    }
    let p = plain.trim().to_string();
    if !p.is_empty() {
        segments.push(TextSegment::Plain(p));
    }
    segments
}

/// Extracts non-empty `<broadcast>...</broadcast>` payloads, ignoring fenced markdown code blocks
/// and inline backtick code spans.
pub fn extract_broadcasts(text: &str) -> Vec<String> {
    split_broadcast_segments(text)
        .into_iter()
        .filter_map(|seg| match seg {
            TextSegment::Broadcast(body) => Some(body),
            TextSegment::Plain(_) => None,
        })
        .collect()
}

/// Parses an optional leading `#<handle>: <message>` prefix against the active team `handles`.
/// Requires no whitespace between `#` and the handle (so Markdown `# heading:` is not hijacked).
/// Returns `(Some(canonical_handle), trimmed_body)` when the handle matches (even if `body` is
/// empty, so callers ignore an empty targeted command instead of broadcasting it to everyone);
/// otherwise `(None, trimmed_text)` (send to all).
pub fn parse_user_target<'a>(text: &'a str, handles: &[String]) -> (Option<String>, &'a str) {
    let trimmed = text.trim();
    if let Some(after_hash) = trimmed.strip_prefix('#') {
        if let Some((candidate, rest)) = after_hash.split_once(':') {
            // Disallow whitespace after `#` so `# opus-1: notes` (Markdown heading) stays untargeted.
            if !candidate.starts_with(char::is_whitespace) {
                let candidate = candidate.trim_end();
                if let Some(matched) = handles.iter().find(|h| h.eq_ignore_ascii_case(candidate)) {
                    return (Some(matched.clone()), rest.trim());
                }
            }
        }
    }
    (None, trimmed)
}

fn opening_fence(trimmed_start: &str) -> Option<(char, usize)> {
    let first = trimmed_start.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = trimmed_start.chars().take_while(|&c| c == first).count();
    if count < 3 {
        return None;
    }
    // CommonMark: backtick info strings cannot contain backticks.
    if first == '`' && trimmed_start[count..].contains('`') {
        return None;
    }
    Some((first, count))
}

fn is_closing_fence(trimmed_start: &str, fch: char, flen: usize) -> bool {
    let count = trimmed_start.chars().take_while(|&c| c == fch).count();
    count >= flen && trimmed_start[count..].trim().is_empty()
}

/// Case-insensitive ASCII prefix match.
fn starts_with_ci(haystack: &str, needle_lower: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle_lower.as_bytes();
    h.len() >= n.len()
        && h[..n.len()]
            .iter()
            .zip(n.iter())
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_slugs_for_real_models() {
        assert_eq!(short_model_slug("claude-opus-4-6"), "opus");
        assert_eq!(short_model_slug("claude-sonnet-4-6"), "sonnet");
        assert_eq!(short_model_slug("claude-haiku-4-5-20251001"), "haiku");
        assert_eq!(short_model_slug("opus"), "opus");
        assert_eq!(short_model_slug("gemini-4-neon"), "neon");
        assert_eq!(short_model_slug("gemini-4-neon-high"), "neon");
        assert_eq!(short_model_slug("gemini-3.8-flash"), "flash");
        assert_eq!(
            short_model_slug("gemini-pro-orchestrator-neon"),
            "orchestrator-neon"
        );
        assert_eq!(short_model_slug("us.anthropic.claude-sonnet-4-6"), "sonnet");
        assert_eq!(short_model_slug(""), "agent");
    }

    #[test]
    fn assign_handles_numbers_per_slug() {
        let handles = assign_handles(&[
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-sonnet-4-6",
            "gemini-4-neon",
        ]);
        assert_eq!(handles, ["opus-1", "sonnet-1", "sonnet-2", "neon-1"]);
    }

    #[test]
    fn broadcast_parser_extracts_and_ignores_code_fences() {
        let text = concat!(
            "Starting work for the user.\n",
            "```xml\n",
            "<broadcast>example inside code block</broadcast>\n",
            "```\n",
            "<broadcast>\n",
            "Plan ready:\n",
            "- #sonnet-1: API\n",
            "</broadcast>\n",
            "User note after.\n",
            "<BROADCAST>Second note</BROADCAST>\n",
            "<broadcast>   </broadcast>"
        );
        let broadcasts = extract_broadcasts(text);
        assert_eq!(broadcasts, ["Plan ready:\n- #sonnet-1: API", "Second note"]);
        let segs = split_broadcast_segments(text);
        assert!(
            matches!(&segs[0], TextSegment::Plain(p) if p.contains("example inside code block"))
        );
        assert!(matches!(&segs[1], TextSegment::Broadcast(b) if b.starts_with("Plan ready:")));
        assert!(matches!(&segs[2], TextSegment::Plain(p) if p == "User note after."));
        assert!(matches!(&segs[3], TextSegment::Broadcast(b) if b == "Second note"));
    }

    #[test]
    fn unclosed_broadcast_stays_plain_text() {
        let text = "Intro\n<broadcast>unfinished message";
        assert!(extract_broadcasts(text).is_empty());
        let segs = split_broadcast_segments(text);
        assert_eq!(segs.len(), 2);
        assert!(matches!(&segs[1], TextSegment::Plain(p) if p.contains("unfinished message")));
    }

    #[test]
    fn parse_user_target_matches_active_handles_only() {
        let handles = vec!["opus-1".into(), "sonnet-1".into(), "neon-1".into()];
        assert_eq!(
            parse_user_target("#opus-1: Plan Feature X", &handles),
            (Some("opus-1".into()), "Plan Feature X")
        );
        assert_eq!(
            parse_user_target("#OPUS-1: case insensitive", &handles),
            (Some("opus-1".into()), "case insensitive")
        );
        assert_eq!(
            parse_user_target("# Heading: not a handle", &handles),
            (None, "# Heading: not a handle")
        );
        assert_eq!(
            parse_user_target("Everyone work together", &handles),
            (None, "Everyone work together")
        );
        assert_eq!(
            parse_user_target("#opus-1:   ", &handles),
            (Some("opus-1".into()), "")
        );
        assert_eq!(
            parse_user_target("# opus-1: Markdown heading", &handles),
            (None, "# opus-1: Markdown heading")
        );
    }

    #[test]
    fn broadcast_parser_ignores_inline_backticks_and_handles_fences_inside_and_nested() {
        // Inline code spans must never trigger a broadcast.
        assert!(extract_broadcasts("Use `<broadcast>` and `</broadcast>` tags.").is_empty());
        assert!(extract_broadcasts("Inline ``<broadcast>nope</broadcast>`` text.").is_empty());
        assert!(extract_broadcasts("```<broadcast>single-line</broadcast>```").is_empty());

        // Code fence inside a broadcast containing `</broadcast>` must not close it prematurely.
        let fenced_inside = concat!(
            "<broadcast>\n",
            "Check this snippet:\n",
            "```xml\n",
            "</broadcast>\n",
            "```\n",
            "Still part of the broadcast.\n",
            "</broadcast>"
        );
        let got = extract_broadcasts(fenced_inside);
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("Still part of the broadcast."));

        // Malformed unclosed outer tag followed by a real tag keeps only the real broadcast.
        let nested = "oops <broadcast>unclosed <broadcast>real café</broadcast>";
        assert_eq!(extract_broadcasts(nested), ["real café"]);
    }
}
