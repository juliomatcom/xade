//! Session manager and per-session actors between the UI and the providers.

pub mod git;
mod manager;
mod session_actor;

use std::path::PathBuf;

use tokio::sync::oneshot;
use xade_core::{
    Delivery, Effort, Message, ProviderEvent, ProviderId, QuotaWindow, Session, SessionId,
    SessionStatus, WorkspaceId,
};
use xade_store::WriterHandle;

pub use git::GitState;
pub use manager::SessionManager;

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
    Close {
        session_id: SessionId,
    },
    /// Xade-origin sessions only: closes it if live, deletes the provider's transcript, then
    /// the row. Replies with `SessionDeleted`, or `Error` (nothing deleted) on any failure.
    DeleteSession {
        session_id: SessionId,
    },
    /// Reads every provider's own history and imports what is new or newer (idempotent).
    /// Replies with `AppEventKind::ImportDone`.
    Import,
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
