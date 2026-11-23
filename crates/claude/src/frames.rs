//! Serde types for the CLI frames we use. Unknown frames, blocks and fields are tolerated.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// A line the CLI wrote to stdout.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InFrame {
    System(SystemFrame),
    StreamEvent {
        event: StreamEvent,
    },
    Assistant {
        message: AssistantMessage,
    },
    User(UserFrame),
    Result(ResultFrame),
    ControlRequest {
        request_id: String,
        request: ControlRequest,
    },
    RateLimitEvent {
        rate_limit_info: RateLimitInfo,
    },
    ControlResponse {},
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemFrame {
    pub subtype: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub estimated_tokens: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamEvent {
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub delta: Option<Delta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Delta {
    /// Absent on `message_delta` (it carries `stop_reason` instead).
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssistantMessage {
    #[serde(default)]
    pub content: Vec<AssistantBlock>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    Thinking {},
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserFrame {
    pub message: UserMessage,
    #[serde(default)]
    pub tool_use_result: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserMessage {
    /// A string or an array of blocks; only `tool_result` blocks matter.
    #[serde(default)]
    pub content: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResultFrame {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default, rename = "modelUsage")]
    pub model_usage: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ControlRequest {
    CanUseTool {
        tool_name: String,
        #[serde(default)]
        input: Value,
        #[serde(default)]
        tool_use_id: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitInfo {
    #[serde(default, rename = "unifiedWindows")]
    pub unified_windows: Option<UnifiedWindows>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnifiedWindows {
    #[serde(default)]
    pub five_hour: Option<RateWindow>,
    #[serde(default)]
    pub seven_day: Option<RateWindow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateWindow {
    #[serde(default)]
    pub utilization: f64,
    #[serde(default, rename = "resetsAt")]
    pub resets_at: Option<f64>,
}

/// Parses one stdout line. Unknown frame types become `InFrame::Unknown`.
pub fn parse_line(line: &str) -> Result<InFrame, serde_json::Error> {
    serde_json::from_str(line)
}

/// A frame we write to stdin.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct OutFrame(pub Value);

impl OutFrame {
    pub fn user_message(text: &str) -> Self {
        Self(json!({
            "type": "user",
            "session_id": "",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        }))
    }

    pub fn interrupt(request_id: &str) -> Self {
        Self(json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {"subtype": "interrupt"},
        }))
    }

    pub fn allow(request_id: &str, updated_input: Value) -> Self {
        Self(json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": {"behavior": "allow", "updatedInput": updated_input},
            },
        }))
    }

    pub fn deny(request_id: &str, message: &str) -> Self {
        Self(json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": {"behavior": "deny", "message": message},
            },
        }))
    }
}
