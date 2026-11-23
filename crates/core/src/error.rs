use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum ProviderError {
    #[error("provider CLI is not installed")]
    NotInstalled,
    #[error("provider CLI is not logged in")]
    NotLoggedIn,
    #[error("failed to spawn provider: {0}")]
    Spawn(String),
    #[error("provider protocol error: {0}")]
    Protocol(String),
}
