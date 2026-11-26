//! Domain types and the provider contract. No I/O.

pub mod error;
pub mod events;
pub mod ids;
pub mod model;
pub mod multi;
pub mod provider;

pub use error::{panic_message, ProviderError};
pub use events::{ProviderEvent, SessionCommand};
pub use ids::{MultiSessionId, ProviderId, SessionId, WorkspaceId};
pub use model::{
    Delivery, Effort, ImportedMessage, ImportedSession, Message, ModelOption, QuotaGroup,
    QuotaKind, QuotaWindow, Role, RunState, RunningSession, Session, SessionOrigin, SessionStatus,
    SpawnConfig, UsageTotals, Workspace,
};
pub use multi::{
    assign_handles, extract_broadcasts, parse_user_target, short_model_slug,
    split_broadcast_segments, MultiMemberBadge, MultiMemberSpec, MultiSession, TeamMessage,
    TeamMessageKind, TextSegment,
};
pub use provider::{AgentProvider, SessionHandle};
