//! Codex CLI adapter over `codex app-server` (JSON-RPC 2.0 on stdio):
//! `CodexProvider` implements `xade_core::AgentProvider`.

pub mod models;
pub mod process;
pub mod provider;
pub mod rpc;
pub mod translate;
pub mod usage;

pub use provider::CodexProvider;
pub use translate::Translator;
