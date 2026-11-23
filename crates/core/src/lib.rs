//! Domain types and the provider contract. No I/O.

pub mod error;
pub mod events;
pub mod ids;
pub mod model;
pub mod provider;

pub use error::{panic_message, ProviderError};
pub use events::{ProviderEvent, SessionCommand};
pub use ids::{ProviderId, SessionId, WorkspaceId};
pub use model::{
    Delivery, Effort, ImportedMessage, ImportedSession, Message, ModelOption, QuotaKind,
    QuotaWindow, Role, RunState, RunningSession, Session, SessionOrigin, SessionStatus,
    SpawnConfig, UsageTotals, Workspace,
};
pub use provider::{AgentProvider, SessionHandle};
