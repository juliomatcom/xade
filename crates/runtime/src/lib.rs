//! Session manager and per-session actors between the UI and the providers.

mod fork;
pub mod git;
mod handoff;
mod manager;
mod multi;
mod session_actor;
mod transcript;

use std::path::PathBuf;

use tokio::sync::oneshot;
use xade_core::{
    Delivery, Effort, Message, MultiMemberSpec, MultiSession, MultiSessionId, ProviderEvent,
    ProviderId, QuotaWindow, Session, SessionId, SessionStatus, TeamMessage, WorkspaceId,
};
use xade_store::WriterHandle;

pub use git::GitState;
pub use manager::SessionManager;
pub use transcript::transcript;

pub enum ManagerCmd {
    Create {
        workspace_id: WorkspaceId,
        cwd: PathBuf,
        provider: ProviderId,
        model_id: String,
        effort: Effort,
        auto_mode: bool,
    },
    /// Loads the row and spawns with `resume`; a no-op when the session is already live.
    Open {
        session_id: SessionId,
    },
    Send {
        session_id: SessionId,
        text: String,
        /// `Some` when sent while a turn was running.
        delivery: Option<Delivery>,
        /// Prepended to the prompt the provider sees; never stored or shown as the user's text.
        context: Option<String>,
    },
    Interrupt {
        session_id: SessionId,
    },
    /// `enable_auto` is "Approve & Enable Auto".
    RespondPermission {
        session_id: SessionId,
        request_id: String,
        allow: bool,
        enable_auto: bool,
    },
    SetAutoMode {
        session_id: SessionId,
        on: bool,
    },
    /// Applied at the next idle point with a resume respawn.
    SetModel {
        session_id: SessionId,
        model_id: String,
    },
    SetEffort {
        session_id: SessionId,
        effort: Effort,
    },
    /// Sets the title (live or dormant); an empty one is ignored. Imports overwrite the titles
    /// of imported sessions, so the UI offers this for Xade-origin sessions only.
    Rename {
        session_id: SessionId,
        title: String,
    },
    Close {
        session_id: SessionId,
    },
    /// Xade-origin sessions only: closes it if live, deletes the provider's transcript, then
    /// the row. Replies with `SessionDeleted`, or `Error` (nothing deleted) on any failure.
    DeleteSession {
        session_id: SessionId,
    },
    /// Copies the session's history into a new Xade session for `provider`, which continues
    /// there with a one-time hand-off: a state snapshot the source provider writes from
    /// `handoff_prompt`, else the history transcript, followed by `resume_prompt`. Replies `ForkPreparing`, then
    /// `SessionForked`, or `ForkFailed` (nothing written) on any failure.
    Fork {
        session_id: SessionId,
        provider: ProviderId,
        /// What the source provider is asked about the transcript to produce the snapshot.
        handoff_prompt: String,
        /// Tells the new provider to continue the conversation, not discuss the hand-off; stored
        /// beside the hand-off files and added after the `@` line of the first send. Empty: none.
        resume_prompt: String,
    },
    /// Reads every provider's own history and imports what is new or newer (idempotent).
    /// Replies with `AppEventKind::ImportDone`.
    Import,
    /// Creates a multi-session team; members start dormant and spawn lazily on first message.
    CreateMulti {
        workspace_id: WorkspaceId,
        cwd: PathBuf,
        title: String,
        members: Vec<MultiMemberSpec>,
    },
    /// Opens a stored multi-session; members open dormant until addressed or broadcast to.
    OpenMulti {
        multi_id: MultiSessionId,
    },
    /// Sends a user message to a multi-session (`#handle: msg` targets one member; otherwise all).
    /// Only user messages can steer (`steer: true`); broadcasts always queue.
    SendMulti {
        multi_id: MultiSessionId,
        text: String,
        steer: bool,
        context: Option<String>,
    },
    /// Interrupts all running members of a multi-session and clears all pending queues.
    InterruptMultiAll {
        multi_id: MultiSessionId,
    },
    /// Resets the consecutive broadcast hop cap and resumes queued broadcast delivery.
    ResumeMultiBroadcasts {
        multi_id: MultiSessionId,
    },
    /// Renames a multi-session.
    RenameMulti {
        multi_id: MultiSessionId,
        title: String,
    },
    /// Closes all member sessions of a multi-session.
    CloseMulti {
        multi_id: MultiSessionId,
    },
    /// Closes live members, deletes each member's provider transcript, then cascades DB rows.
    DeleteMulti {
        multi_id: MultiSessionId,
    },
    /// Replies once every child has exited.
    ShutdownAll(oneshot::Sender<()>),
}

#[derive(Debug, Clone)]
pub struct AppEvent {
    /// `None` for global events (quota, import, git).
    pub session_id: Option<SessionId>,
    pub kind: AppEventKind,
}

#[derive(Debug, Clone)]
pub enum AppEventKind {
    Status(SessionStatus),
    /// Every provider event except `Quota`; deltas are never persisted.
    Provider(ProviderEvent),
    SessionCreated(Session),
    /// A new session's row is now readable: its first message was stored.
    SessionStored,
    /// Reply to `Open`: the stored row and its full history.
    SessionOpened {
        session: Session,
        messages: Vec<Message>,
    },
    Quota(QuotaWindow),
    /// Reply to `Import`: sessions created and sessions replaced by a newer transcript.
    ImportDone {
        imported: u32,
        updated: u32,
    },
    /// Produced by the UI, not the manager.
    Git {
        dir: PathBuf,
        state: GitState,
    },
    SessionDeleted {
        session_id: SessionId,
    },
    /// A `Fork` started (the session id is the source); it always ends with `SessionForked` or
    /// `ForkFailed` for that source.
    ForkPreparing {
        from: SessionId,
    },
    /// A `Fork` of `from` ended with nothing written; `message` is ready to show.
    ForkFailed {
        from: SessionId,
        message: String,
    },
    /// A fork's snapshot turn runs a throwaway conversation on a provider (`active`) until it
    /// is deleted (not active). Global event; its id must not count as a change in the sessions
    /// running elsewhere.
    Throwaway {
        provider_session_id: String,
        active: bool,
    },
    /// Reply to `Fork`: the stored copy, not yet opened.
    SessionForked {
        from: SessionId,
        session_id: SessionId,
    },
    /// Reply to `CreateMulti`: stored parent row and ordered member sessions (all dormant/idle).
    MultiCreated {
        multi: MultiSession,
        members: Vec<Session>,
    },
    /// Reply to `OpenMulti`: stored parent, ordered member histories, and shared Team feed.
    MultiOpened {
        multi: MultiSession,
        members: Vec<(Session, Vec<Message>)>,
        team_messages: Vec<TeamMessage>,
    },
    /// `OpenMulti` failed for `multi_id` (e.g. deleted before restore); lets UI drop ghost windows.
    MultiOpenFailed {
        multi_id: MultiSessionId,
        message: String,
    },
    /// A new user or broadcast entry appended to a multi-session's Team feed.
    MultiTeamMessage {
        multi_id: MultiSessionId,
        message: TeamMessage,
    },
    MultiRenamed {
        multi_id: MultiSessionId,
        title: String,
    },
    /// Auto-delivery of broadcasts paused (or resumed) due to the consecutive hop cap.
    MultiBroadcastPaused {
        multi_id: MultiSessionId,
        paused: bool,
        hops: u32,
    },
    MultiClosed {
        multi_id: MultiSessionId,
    },
    MultiDeleted {
        multi_id: MultiSessionId,
    },
    /// A user prompt or delivered broadcast written by the runtime into a multi-session member.
    MemberMessage(Message),
    Error(String),
}

/// Waits until every write sent so far is applied.
pub async fn flush(writer: &WriterHandle) {
    let w = writer.clone();
    match tokio::task::spawn_blocking(move || w.flush()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(error = %e, "store flush failed"),
        Err(e) => tracing::error!(error = %e, "store flush task failed"),
    }
}
