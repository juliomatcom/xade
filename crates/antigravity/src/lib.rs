//! Antigravity CLI adapter over ACP (`agy acp`, JSON-RPC 2.0 on stdio):
//! `AntigravityProvider` implements `xade_core::AgentProvider`.

pub mod models;
pub mod process;
pub mod provider;
pub mod rpc;
pub mod translate;
pub mod usage;

pub use provider::AntigravityProvider;
pub use translate::Translator;
