//! Codex CLI adapter over `codex app-server` (JSON-RPC 2.0 on stdio).

pub mod models;
pub mod rpc;
pub mod translate;

pub use translate::Translator;
