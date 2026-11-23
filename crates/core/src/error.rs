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

/// Readable text of a caught panic payload (`catch_unwind`, `JoinError::into_panic`).
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}
