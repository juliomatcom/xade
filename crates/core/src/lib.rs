//! Domain types and the provider contract. No I/O.

pub mod error;
pub mod events;
pub mod ids;
pub mod model;
pub mod provider;

pub use error::ProviderError;
pub use events::{ProviderEvent, SessionCommand};
pub use ids::{ProviderId, SessionId, WorkspaceId};
pub use model::{
    Effort, Message, ModelOption, QuotaKind, QuotaWindow, Role, Session, SessionStatus,
    SpawnConfig, UsageTotals, Workspace,
};
pub use provider::{AgentProvider, SessionHandle};
