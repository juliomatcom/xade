//! Claude Code CLI adapter: `ClaudeProvider` implements `xade_core::AgentProvider`.

pub mod frames;
pub mod models;
pub mod process;
pub mod provider;
pub mod transcript;
pub mod translate;

pub use provider::ClaudeProvider;
pub use translate::Translator;
