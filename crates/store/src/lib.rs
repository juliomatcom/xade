//! SQLite persistence: one writer thread, async readers, FTS5 search.

mod reader;
mod schema;
mod search;
pub mod throwaway;
mod writer;

use thiserror::Error;
use time::OffsetDateTime;
use xade_core::{SessionId, SessionOrigin, WorkspaceId};

pub use reader::{ImportRef, StoreReader};
pub use schema::SCHEMA_VERSION;
pub use writer::{PruneCounts, StoreWriter, WriteOp, WriterHandle};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("database schema version {found} is newer than supported {supported}")]
    SchemaTooNew { found: u32, supported: u32 },
    #[error("session not found")]
    NotFound,
    #[error("store writer has stopped")]
    WriterClosed,
    #[error("blocking task failed: {0}")]
    Task(String),
}

/// One page of results. `page` is zero-based.
#[derive(Debug, Clone, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: u64,
    pub page: u32,
    pub page_size: u32,
}

/// Home-grid view of a session. `excerpt` is set by search when a message body matched.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionCard {
    pub id: SessionId,
    pub workspace_id: WorkspaceId,
    pub title: String,
    pub preview_snippet: String,
    pub model_id: String,
    pub updated_at: OffsetDateTime,
    pub origin: SessionOrigin,
    pub excerpt: Option<String>,
}

/// Slices `all` to the requested page; `page_size` of 0 is treated as 1.
pub(crate) fn paginate<T>(all: Vec<T>, page: u32, page_size: u32) -> Page<T> {
    let size = page_size.max(1);
    let total = all.len() as u64;
    let items = all
        .into_iter()
        .skip((page as usize).saturating_mul(size as usize))
        .take(size as usize)
        .collect();
    Page {
        items,
        total,
        page,
        page_size: size,
    }
}
