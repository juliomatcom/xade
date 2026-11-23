use tokio::sync::mpsc;

use crate::error::ProviderError;
use crate::events::{ProviderEvent, SessionCommand};
use crate::ids::ProviderId;
use crate::model::{Effort, ModelOption, SpawnConfig};

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
    /// Must be called inside a tokio runtime: starts a task and returns its channels.
    fn spawn(&self, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError>;
}
