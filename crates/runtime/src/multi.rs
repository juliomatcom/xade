//! Multi-session team state, preambles, and broadcast queueing/routing helpers.

use std::collections::VecDeque;

use xade_core::{MultiSession, MultiSessionId, ProviderId, SessionId, SessionStatus};

/// Consecutive agent-to-agent broadcast hops allowed before auto-delivery pauses for user review.
pub(crate) const MAX_BROADCAST_HOPS: u32 = 12;

/// Signal sent from a multi-session member's `SessionActor` back to `SessionManager`.
pub(crate) enum MemberSignal {
    Status {
        multi_id: MultiSessionId,
        generation: u64,
        session_id: SessionId,
        dispatch_seq: u64,
        status: SessionStatus,
    },
    AssistantText {
        multi_id: MultiSessionId,
        generation: u64,
        session_id: SessionId,
        text: String,
    },
    /// A broadcast arrived at the actor after the actor left `Idle`: put it back at the front of
    /// the member's queue unless superseded by an interrupt (`epoch`).
    BroadcastDeferred {
        multi_id: MultiSessionId,
        generation: u64,
        session_id: SessionId,
        epoch: u64,
        batch: Vec<PendingBroadcast>,
    },
    /// A user prompt arrived while the actor was in `NeedsApproval`: re-queue at the front of
    /// `queued_user` unless superseded by an interrupt (`epoch`).
    UserDeferred {
        multi_id: MultiSessionId,
        generation: u64,
        session_id: SessionId,
        epoch: u64,
        msg: PendingUserMsg,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct PendingBroadcast {
    pub sender_handle: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingUserMsg {
    pub text: String,
    pub context: Option<String>,
}

pub(crate) struct TeamMember {
    pub session_id: SessionId,
    pub handle: String,
    pub provider_id: ProviderId,
    pub status: SessionStatus,
    /// Monotonic counter bumped on every dispatch to this member so stale `Idle` signals from
    /// an earlier turn cannot prematurely drain queues after a newer send/steer.
    pub dispatch_seq: u64,
    /// Monotonic counter bumped on `Interrupt` / `InterruptMultiAll` so late `BroadcastDeferred`
    /// / `UserDeferred` signals from before the interrupt are discarded.
    pub interrupt_epoch: u64,
    /// Suppresses broadcast emission while an interrupted turn unwinds until it reaches `Idle`.
    pub suppress_broadcasts: bool,
    pub queued_user: VecDeque<PendingUserMsg>,
    pub queued_broadcasts: VecDeque<PendingBroadcast>,
}

pub(crate) struct TeamState {
    pub multi: MultiSession,
    /// Unique per open instance of this multi-session so late signals from a closing actor cannot
    /// poison a newly reopened instance.
    pub generation: u64,
    pub members: Vec<TeamMember>,
    pub next_team_seq: i64,
    pub consecutive_hops: u32,
    pub broadcasts_paused: bool,
}

impl TeamState {
    pub(crate) fn handles(&self) -> Vec<String> {
        self.members.iter().map(|m| m.handle.clone()).collect()
    }

    pub(crate) fn member(&self, session_id: SessionId) -> Option<&TeamMember> {
        self.members.iter().find(|m| m.session_id == session_id)
    }

    pub(crate) fn member_mut(&mut self, session_id: SessionId) -> Option<&mut TeamMember> {
        self.members.iter_mut().find(|m| m.session_id == session_id)
    }

    pub(crate) fn status_summary(&self) -> String {
        self.members
            .iter()
            .map(|m| format!("#{} {}", m.handle, status_word(&m.status)))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn status_word(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Idle => "idle",
        SessionStatus::Starting | SessionStatus::Streaming => "working",
        SessionStatus::NeedsApproval => "awaiting-approval",
        SessionStatus::Error(_) => "error",
        SessionStatus::Closed => "closed",
    }
}

/// First-prompt hidden preamble telling a team member its handle, roster, shared workspace, and
/// the strict user-vs-broadcast channel separation.
pub(crate) fn team_preamble(my_handle: &str, roster: &[(String, ProviderId, String)]) -> String {
    let members = roster
        .iter()
        .map(|(handle, provider, model)| {
            let p = match provider {
                ProviderId::Claude => "Claude",
                ProviderId::Antigravity => "Antigravity",
                ProviderId::Codex => "Codex",
            };
            format!("- #{handle} ({p}, {model})")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "[Team Session Instructions]\n\
         You are #{my_handle}, part of a multi-model team working together in the same workspace directory.\n\
         Team roster:\n\
         {members}\n\
         \n\
         Communication rules:\n\
         1. Normal text is shown ONLY to the user. Teammates never see normal text.\n\
         2. To send a message to all teammates, wrap it in <broadcast>...</broadcast>.\n\
         3. Only use <broadcast> when teammates need to act on or know something (e.g. a plan hand-off, completed deliverable, API contract, or blocking question).\n\
         4. NEVER use <broadcast> for acknowledgments (\"Got it\", \"Starting\"), status chatter, or messages meant only for the user.\n\
         5. All teammates share the same working directory: coordinate file ownership and git operations (branch, commit, push) so teammates do not overwrite each other's work.\n\
         [End Team Session Instructions]"
    )
}

/// Wraps one or more coalesced broadcasts into a provider prompt with live team status and a
/// compact syntax reminder (prevents instruction drift after compaction).
pub(crate) fn format_broadcast_prompt(
    batch: &[PendingBroadcast],
    status_summary: &str,
) -> (String, String, String) {
    let mut unique_senders: Vec<&str> = Vec::new();
    for b in batch {
        if !unique_senders.contains(&b.sender_handle.as_str()) {
            unique_senders.push(b.sender_handle.as_str());
        }
    }
    let sender_handle = unique_senders.join(", ");
    let display_text = if batch.len() == 1 {
        batch[0].content.clone()
    } else {
        batch
            .iter()
            .map(|b| format!("From #{}:\n{}", b.sender_handle, b.content))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let body = batch
        .iter()
        .map(|b| format!("From #{}:\n{}", b.sender_handle, b.content))
        .collect::<Vec<_>>()
        .join("\n\n");
    let prompt_text = format!(
        "[Team broadcast | status: {status_summary} | normal text is for the user only; wrap messages for teammates in <broadcast>...</broadcast>]\n\n{body}"
    );
    (sender_handle, display_text, prompt_text)
}
