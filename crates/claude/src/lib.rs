//! Claude Code CLI adapter: `ClaudeProvider` implements `aw_core::AgentProvider`.

pub mod frames;
pub mod process;
pub mod provider;
pub mod translate;

pub use provider::ClaudeProvider;
pub use translate::Translator;
