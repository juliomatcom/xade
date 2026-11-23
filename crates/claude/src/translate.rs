//! Pure frame -> event translation. Holds only the state a mapping needs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;
use time::OffsetDateTime;
use xade_core::{ProviderEvent, QuotaKind, QuotaWindow, UsageTotals};

use crate::frames::{
    AssistantBlock, ControlRequest, InFrame, RateWindow, ResultFrame, StreamEvent, UserFrame,
};

#[derive(Debug, Default)]
pub struct Translator {
    /// Set by the command loop when it writes an interrupt; consumed by the next `result`.
    interrupted: Arc<AtomicBool>,
    context_tokens_used: u64,
    context_window_max: u64,
}

impl Translator {
    pub fn new(interrupted: Arc<AtomicBool>) -> Self {
        Self {
            interrupted,
            ..Self::default()
        }
    }

    /// The flag to set when an interrupt is sent.
    pub fn interrupt_flag(&self) -> Arc<AtomicBool> {
        self.interrupted.clone()
    }

    pub fn on_frame(&mut self, frame: InFrame) -> Vec<ProviderEvent> {
        match frame {
            InFrame::System(s) => match s.subtype.as_str() {
                "init" => match s.session_id {
                    Some(id) => vec![ProviderEvent::Started {
                        provider_session_id: id,
                        model_id: s.model.unwrap_or_default(),
                    }],
                    None => vec![],
                },
                "thinking_tokens" => s
                    .estimated_tokens
                    .map(|estimated_tokens| ProviderEvent::ThinkingProgress { estimated_tokens })
                    .into_iter()
                    .collect(),
                _ => vec![],
            },
            InFrame::StreamEvent { event } => text_delta(event).into_iter().collect(),
            InFrame::Assistant { message } => {
                if let Some(u) = &message.usage {
                    self.context_tokens_used =
                        u.input_tokens + u.cache_read_input_tokens + u.cache_creation_input_tokens;
                }
                message
                    .content
                    .into_iter()
                    .filter_map(|b| match b {
                        AssistantBlock::Text { text } => {
                            Some(ProviderEvent::AssistantText { text })
                        }
                        AssistantBlock::ToolUse { id, name, input } => {
                            Some(ProviderEvent::ToolUse {
                                tool_use_id: id,
                                name,
                                input_json: input.to_string(),
                            })
                        }
                        AssistantBlock::Thinking {} | AssistantBlock::Other => None,
                    })
                    .collect()
            }
            InFrame::User(u) => tool_results(u),
            InFrame::Result(r) => vec![self.turn_complete(r)],
            InFrame::ControlRequest {
                request_id,
                request,
            } => match request {
                ControlRequest::CanUseTool {
                    tool_name,
                    input,
                    tool_use_id,
                } => vec![ProviderEvent::PermissionRequest {
                    request_id,
                    tool_use_id,
                    tool_name,
                    input_json: input.to_string(),
                }],
                ControlRequest::Other => vec![],
            },
            InFrame::RateLimitEvent { rate_limit_info } => {
                let Some(w) = rate_limit_info.unified_windows else {
                    return vec![];
                };
                [
                    (QuotaKind::FiveHour, w.five_hour),
                    (QuotaKind::SevenDay, w.seven_day),
                ]
                .into_iter()
                .filter_map(|(kind, win)| win.map(|w| ProviderEvent::Quota(quota(kind, w))))
                .collect()
            }
            InFrame::ControlResponse {} | InFrame::Unknown => vec![],
        }
    }

    fn turn_complete(&mut self, r: ResultFrame) -> ProviderEvent {
        if let Some(w) = r
            .model_usage
            .as_ref()
            .and_then(|m| m.values().next())
            .and_then(|v| v.get("contextWindow"))
            .and_then(Value::as_u64)
        {
            self.context_window_max = w;
        }
        let usage = r.usage.unwrap_or_default();
        let interrupted = self.interrupted.swap(false, Ordering::SeqCst);
        ProviderEvent::TurnComplete {
            usage: UsageTotals {
                context_tokens_used: self.context_tokens_used,
                context_window_max: self.context_window_max,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_input_tokens,
                cost_usd: r.total_cost_usd.unwrap_or(0.0),
            },
            is_error: r.is_error && !interrupted,
            interrupted,
        }
    }
}

fn text_delta(e: StreamEvent) -> Option<ProviderEvent> {
    if e.kind != "content_block_delta" {
        return None;
    }
    let d = e.delta?;
    match (d.kind.as_str(), d.text) {
        ("text_delta", Some(t)) => Some(ProviderEvent::TextDelta(t)),
        _ => None,
    }
}

fn tool_results(u: UserFrame) -> Vec<ProviderEvent> {
    let Value::Array(blocks) = u.message.content else {
        return vec![];
    };
    let structured_json = u.tool_use_result.map(|v| v.to_string());
    blocks
        .into_iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .map(|b| ProviderEvent::ToolResult {
            tool_use_id: b
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            output: result_text(b.get("content")),
            is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            structured_json: structured_json.clone(),
        })
        .collect()
}

/// Tool result content is a string or an array of `{type:"text", text}` parts.
fn result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn quota(kind: QuotaKind, w: RateWindow) -> QuotaWindow {
    QuotaWindow {
        kind,
        utilization_pct: (w.utilization * 100.0) as f32,
        resets_at: w
            .resets_at
            .and_then(|s| OffsetDateTime::from_unix_timestamp(s as i64).ok()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::frames::parse_line;

    fn feed(t: &mut Translator, line: &str) -> Vec<ProviderEvent> {
        t.on_frame(parse_line(line).expect("parse"))
    }

    #[test]
    fn unknown_frames_and_fields_are_skipped() {
        let mut t = Translator::default();
        assert!(feed(&mut t, r#"{"type":"brand_new","x":1}"#).is_empty());
        assert!(feed(
            &mut t,
            r#"{"type":"system","subtype":"whatever","extra":[1]}"#
        )
        .is_empty());
        assert!(feed(
            &mut t,
            r#"{"type":"control_request","request_id":"r","request":{"subtype":"future"}}"#
        )
        .is_empty());
    }

    #[test]
    fn user_text_frames_are_ignored_and_array_content_is_joined() {
        let mut t = Translator::default();
        assert!(feed(
            &mut t,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#
        )
        .is_empty());
        let ev = feed(
            &mut t,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}]},"tool_use_result":{"k":1}}"#,
        );
        assert_eq!(
            ev,
            vec![ProviderEvent::ToolResult {
                tool_use_id: "t1".into(),
                output: "a\nb".into(),
                is_error: false,
                structured_json: Some(r#"{"k":1}"#.into()),
            }]
        );
    }
}
