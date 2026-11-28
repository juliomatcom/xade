//! OpenCode CLI adapter over ACP (`opencode acp`, JSON-RPC 2.0 on stdio): `OpenCodeProvider`.

pub mod models;
pub mod patch;
pub mod process;
pub mod provider;
pub mod rpc;
pub mod translate;

pub use provider::OpenCodeProvider;
pub use translate::Translator;
