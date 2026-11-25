use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::ids::{ProviderId, SessionId, WorkspaceId};

const TITLE_MAX_CHARS: usize = 60;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Omit `--effort`.
    #[default]
    Default,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    /// Value for the CLI `--effort` flag; `None` means omit the flag.
    pub fn cli_value(self) -> Option<&'static str> {
        match self {
            Effort::Default => None,
            Effort::Low => Some("low"),
            Effort::Medium => Some("medium"),
            Effort::High => Some("high"),
            Effort::XHigh => Some("xhigh"),
            Effort::Max => Some("max"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub description: String,
    /// Levels the model accepts, excluding `Effort::Default` (always available).
    pub effort_levels: Vec<Effort>,
}

/// Where a session was created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionOrigin {
    #[default]
    Xade,
    ClaudeCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Closed,
    Starting,
    Idle,
    Streaming,
    NeedsApproval,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub root_path: PathBuf,
    pub display_name: String,
    pub last_opened_at: OffsetDateTime,
    /// Listed in the sidebar; import never sets it, adding or opening a session in it does.
    pub is_open: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub context_tokens_used: u64,
    pub context_window_max: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_usd: f64,
}

impl UsageTotals {
    /// Adds the turn's input, output, cache-read and cost; replaces the context figures.
    pub fn accumulate(&mut self, turn: &UsageTotals) {
        self.input_tokens += turn.input_tokens;
        self.output_tokens += turn.output_tokens;
        self.cache_read_tokens += turn.cache_read_tokens;
        self.cost_usd += turn.cost_usd;
        self.context_tokens_used = turn.context_tokens_used;
        self.context_window_max = turn.context_window_max;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub workspace_id: WorkspaceId,
    pub provider_id: ProviderId,
    pub provider_session_id: Option<String>,
    pub origin: SessionOrigin,
    pub title: String,
    pub preview_snippet: String,
    pub model_id: String,
    pub effort: Effort,
    /// Model and effort of the last message sent; the context-cost warning compares against them.
    /// Empty model: nothing sent yet.
    pub sent_model_id: String,
    pub sent_effort: Effort,
    pub auto_mode: bool,
    pub usage: UsageTotals,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl Session {
    /// First line of the prompt, trimmed, at most 60 chars (including the `…` when cut).
    pub fn derive_title(first_prompt: &str) -> String {
        let line = first_prompt.trim().lines().next().unwrap_or("").trim();
        if line.chars().count() <= TITLE_MAX_CHARS {
            return line.to_string();
        }
        let mut title: String = line.chars().take(TITLE_MAX_CHARS - 1).collect();
        title.truncate(title.trim_end().len());
        title.push('…');
        title
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    ToolCall,
    ToolResult,
    System,
}

/// How a prompt sent while a turn was running reached the CLI; kept in a user message's
/// `tool_name` so the badge survives a reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Queued,
    Steered,
}

impl Delivery {
    pub fn tag(self) -> &'static str {
        match self {
            Delivery::Queued => "queued",
            Delivery::Steered => "steered",
        }
    }

    /// The delivery recorded on a user message, if any.
    pub fn of(m: &Message) -> Option<Delivery> {
        if m.role != Role::User {
            return None;
        }
        match m.tool_name.as_deref() {
            Some("queued") => Some(Delivery::Queued),
            Some("steered") => Some(Delivery::Steered),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub session_id: SessionId,
    pub seq: i64,
    pub role: Role,
    pub content_text: String,
    pub thinking_text: Option<String>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_payload_json: Option<String>,
    pub is_error: bool,
    pub created_at: OffsetDateTime,
    /// The provider that wrote it when that is not the session's (history copied by a fork);
    /// `None` means the session's own.
    pub provider_id: Option<ProviderId>,
}

/// A message read from a provider's own history; the runtime assigns ids and `seq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportedMessage {
    pub role: Role,
    pub content_text: String,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_payload_json: Option<String>,
    pub is_error: bool,
    pub created_at: OffsetDateTime,
}

/// A session found in a provider's own history (e.g. Claude Code transcripts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportedSession {
    pub provider_session_id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub model_id: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub usage: UsageTotals,
    pub messages: Vec<ImportedMessage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunState {
    Busy,
    Idle,
}

/// A provider session currently running in a live process anywhere (terminal, IDE, SDK).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningSession {
    pub provider_session_id: String,
    pub state: RunState,
    /// Human label: "VS Code", "terminal", "SDK", or the raw entrypoint.
    pub location: String,
    pub pid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuotaKind {
    FiveHour,
    SevenDay,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub kind: QuotaKind,
    pub utilization_pct: f32,
    pub resets_at: Option<OffsetDateTime>,
}

/// Quota windows of one model group; a provider with a single pool uses an empty `name`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaGroup {
    pub name: String,
    pub windows: Vec<QuotaWindow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnConfig {
    pub cwd: PathBuf,
    pub model_id: String,
    pub effort: Effort,
    pub auto_mode: bool,
    /// `provider_session_id` to resume.
    pub resume: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_empty() {
        assert_eq!(Session::derive_title(""), "");
        assert_eq!(Session::derive_title("  \n\t "), "");
    }

    #[test]
    fn title_takes_first_line_trimmed() {
        assert_eq!(
            Session::derive_title("  fix the bug \nsecond line"),
            "fix the bug"
        );
        assert_eq!(Session::derive_title("\n\n  hello\nworld"), "hello");
    }

    #[test]
    fn title_short_and_exact_limit_untouched() {
        let exact = "a".repeat(60);
        assert_eq!(Session::derive_title(&exact), exact);
    }

    #[test]
    fn title_cut_at_60_chars_with_ellipsis() {
        let long = "a".repeat(100);
        let title = Session::derive_title(&long);
        assert_eq!(title.chars().count(), 60);
        assert_eq!(title, format!("{}…", "a".repeat(59)));
    }

    #[test]
    fn title_unicode_boundary() {
        let long = "é".repeat(80);
        let title = Session::derive_title(&long);
        assert_eq!(title.chars().count(), 60);
        assert!(title.ends_with('…'));
        let emoji = "😀".repeat(70);
        assert_eq!(Session::derive_title(&emoji).chars().count(), 60);
    }

    #[test]
    fn accumulate_adds_totals_and_replaces_context() {
        let mut total = UsageTotals {
            context_tokens_used: 10,
            context_window_max: 100,
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 3,
            cost_usd: 0.5,
        };
        let turn = UsageTotals {
            context_tokens_used: 40,
            context_window_max: 200,
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 30,
            cost_usd: 0.25,
        };
        total.accumulate(&turn);
        assert_eq!(total.input_tokens, 11);
        assert_eq!(total.output_tokens, 22);
        assert_eq!(total.cache_read_tokens, 33);
        assert_eq!(total.cost_usd, 0.75);
        assert_eq!(total.context_tokens_used, 40);
        assert_eq!(total.context_window_max, 200);
    }

    #[test]
    fn effort_cli_values() {
        assert_eq!(Effort::Default.cli_value(), None);
        assert_eq!(Effort::XHigh.cli_value(), Some("xhigh"));
        assert_eq!(Effort::Max.cli_value(), Some("max"));
        assert_eq!(serde_json::to_string(&Effort::XHigh).unwrap(), "\"xhigh\"");
    }
}
