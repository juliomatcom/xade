use tokio::sync::mpsc;

use crate::error::ProviderError;
use crate::events::{ProviderEvent, SessionCommand};
use crate::ids::ProviderId;
use crate::model::{Effort, ImportedSession, ModelOption, QuotaGroup, RunningSession, SpawnConfig};

/// Not `Clone`: it owns the single receiver of the session's events.
#[derive(Debug)]
pub struct SessionHandle {
    pub commands: mpsc::UnboundedSender<SessionCommand>,
    pub events: mpsc::UnboundedReceiver<ProviderEvent>,
}

pub trait AgentProvider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn display_name(&self) -> &'static str;
    fn models(&self) -> Vec<ModelOption>;
    fn default_model(&self) -> String;
    fn effort_levels(&self) -> Vec<Effort>;
    /// Blocking is fine; called once at startup.
    fn preflight(&self) -> Result<(), ProviderError>;
    /// Account shown in the status bar, set by the last `preflight`; `None` when unknown or
    /// logged out. For on-screen display only: never log, persist or put it in a report.
    fn account_label(&self) -> Option<String> {
        None
    }
    /// Must be called inside a tokio runtime: starts a task and returns its channels.
    fn spawn(&self, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError>;
    /// Sessions found in the provider's own history. Blocking; callers use `spawn_blocking`.
    fn import_sessions(&self) -> Vec<ImportedSession> {
        Vec::new()
    }
    /// Sessions running right now in live processes. Blocking; callers use `spawn_blocking`.
    fn running_sessions(&self) -> Vec<RunningSession> {
        Vec::new()
    }
    /// The provider's quota right now, asked of it on demand. Providers that report quota in their
    /// event stream (Claude) keep the empty default. Blocking; callers use `spawn_blocking`.
    fn quota(&self) -> Result<Vec<QuotaGroup>, ProviderError> {
        Ok(Vec::new())
    }
    /// Removes the provider's own copy of a session (its transcript). Already gone is Ok.
    /// Blocking; callers use `spawn_blocking`.
    fn delete_session(&self, _provider_session_id: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}
