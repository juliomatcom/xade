//! Antigravity CLI adapter over ACP (`agy acp`, JSON-RPC 2.0 on stdio).
//! This phase holds the wire types and the pure translator; the process and provider come next.

pub mod rpc;
pub mod translate;

pub use translate::Translator;
