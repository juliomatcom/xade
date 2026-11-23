//! Session manager and per-session actors between the UI and the providers.

pub mod git;
mod manager;
mod session_actor;

use std::path::PathBuf;

use tokio::sync::oneshot;
use xade_core::{
    Effort, Message, ProviderEvent, ProviderId, QuotaWindow, Session, SessionId, SessionStatus,
    WorkspaceId,
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
    /// Replies once every child has exited.
    ShutdownAll(oneshot::Sender<()>),
}

#[derive(Debug, Clone)]
pub struct AppEvent {
    /// `None` for global events (quota, git).
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
    /// Produced by the UI, not the manager.
    Git {
        dir: PathBuf,
        state: GitState,
    },
    Error(String),
}

/// Waits until every write sent so far is applied.
pub async fn flush(writer: &WriterHandle) {
    let w = writer.clone();
    if let Ok(Err(e)) = tokio::task::spawn_blocking(move || w.flush()).await {
        tracing::error!(error = %e, "store flush failed");
    }
}
