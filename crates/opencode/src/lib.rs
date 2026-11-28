//! OpenCode CLI adapter over ACP (`opencode acp`, JSON-RPC 2.0 on stdio).

pub mod models;
pub mod patch;
pub mod rpc;
pub mod translate;

pub use translate::Translator;
