use serde::{Deserialize, Serialize};

use crate::model::{QuotaWindow, UsageTotals};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionCommand {
    SendPrompt(String),
    Interrupt,
    RespondPermission {
        request_id: String,
        allow: bool,
    },
    /// Live toggle; no respawn.
    SetAutoMode(bool),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProviderEvent {
    Started {
        provider_session_id: String,
        model_id: String,
    },
    TextDelta(String),
    /// No thinking text is exposed, only a token estimate.
    ThinkingProgress {
        estimated_tokens: u64,
    },
    /// Finalized text block.
    AssistantText {
        text: String,
    },
    ToolUse {
        tool_use_id: String,
        name: String,
        input_json: String,
    },
    ToolResult {
        tool_use_id: String,
        output: String,
        is_error: bool,
        /// Raw `tool_use_result` (originalFile, structuredPatch, ...).
        structured_json: Option<String>,
    },
    PermissionRequest {
        request_id: String,
        tool_use_id: String,
        tool_name: String,
        input_json: String,
    },
    TurnComplete {
        /// Deltas for this turn plus the current context.
        usage: UsageTotals,
        is_error: bool,
        interrupted: bool,
    },
    Quota(QuotaWindow),
    Error(String),
    Exited {
        code: Option<i32>,
    },
}
