//! Read side: one query-only connection behind a mutex, async via `spawn_blocking`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension, Row};
use time::OffsetDateTime;
use xade_core::{
    Message, ProviderId, QuotaWindow, Session, SessionId, SessionOrigin, UsageTotals, Workspace,
    WorkspaceId,
};

use crate::schema::{self, to_u64};
use crate::{search, Page, SessionCard, StoreError};

/// A stored session that has a `provider_session_id`, as the importer sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportRef {
    pub id: SessionId,
    pub origin: SessionOrigin,
    pub updated_at: OffsetDateTime,
}

#[derive(Clone)]
pub struct StoreReader {
    conn: Arc<Mutex<Connection>>,
}

impl StoreReader {
    /// The database must already exist (open the writer first).
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Ok(Self {
            conn: Arc::new(Mutex::new(schema::open_reader(path)?)),
        })
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap_or_else(|e| e.into_inner());
            f(&guard)
        })
        .await
        .map_err(|e| StoreError::Task(e.to_string()))?
    }

    /// Most recently opened first.
    pub async fn list_workspaces(&self) -> Result<Vec<Workspace>, StoreError> {
        self.with_conn(list_workspaces).await
    }

    /// Most recently updated first; `page` is zero-based; `origin` of `None` lists all.
    pub async fn list_sessions(
        &self,
        scope: Option<WorkspaceId>,
        origin: Option<SessionOrigin>,
        page: u32,
        page_size: u32,
    ) -> Result<Page<SessionCard>, StoreError> {
        self.with_conn(move |c| list_sessions(c, scope, origin, page, page_size))
            .await
    }

    /// Last saved quota windows (at most one per kind), possibly stale: check `resets_at`.
    pub async fn load_quota(&self) -> Result<Vec<QuotaWindow>, StoreError> {
        self.with_conn(load_quota).await
    }

    /// `provider_session_id` -> stored session, for deciding what an import must skip.
    pub async fn import_index(
        &self,
        provider: ProviderId,
    ) -> Result<HashMap<String, ImportRef>, StoreError> {
        self.with_conn(move |c| import_index(c, provider)).await
    }

    pub async fn load_session(&self, id: SessionId) -> Result<(Session, Vec<Message>), StoreError> {
        self.with_conn(move |c| load_session(c, id)).await
    }

    /// Highest `seq` in the session, or 0 when it has no messages.
    pub async fn max_seq(&self, session_id: SessionId) -> Result<i64, StoreError> {
        self.with_conn(move |c| {
            Ok(c.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM messages WHERE session_id = ?1",
                [session_id.0.to_string()],
                |r| r.get(0),
            )?)
        })
        .await
    }

    pub async fn search(
        &self,
        scope: Option<WorkspaceId>,
        origin: Option<SessionOrigin>,
        query: String,
        page: u32,
        page_size: u32,
    ) -> Result<Page<SessionCard>, StoreError> {
        self.with_conn(move |c| search::search(c, scope, origin, &query, page, page_size))
            .await
    }
}

pub(crate) const CARD_COLS: &str =
    "id, workspace_id, title, preview_snippet, model_id, updated_at, origin, provider_session_id";

pub(crate) fn card_from_row(r: &Row) -> rusqlite::Result<SessionCard> {
    Ok(SessionCard {
        id: schema::session_id(0, &r.get::<_, String>(0)?)?,
        workspace_id: schema::workspace_id(1, &r.get::<_, String>(1)?)?,
        title: r.get(2)?,
        preview_snippet: r.get(3)?,
        model_id: r.get(4)?,
        updated_at: schema::parse_ts(5, &r.get::<_, String>(5)?)?,
        origin: schema::parse_origin(6, &r.get::<_, String>(6)?)?,
        excerpt: None,
        provider_session_id: r.get(7)?,
    })
}

fn list_workspaces(conn: &Connection) -> Result<Vec<Workspace>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, root_path, display_name, last_opened_at, is_pinned
         FROM workspaces ORDER BY last_opened_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Workspace {
            id: schema::workspace_id(0, &r.get::<_, String>(0)?)?,
            root_path: PathBuf::from(r.get::<_, String>(1)?),
            display_name: r.get(2)?,
            last_opened_at: schema::parse_ts(3, &r.get::<_, String>(3)?)?,
            is_pinned: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn list_sessions(
    conn: &Connection,
    scope: Option<WorkspaceId>,
    origin: Option<SessionOrigin>,
    page: u32,
    page_size: u32,
) -> Result<Page<SessionCard>, StoreError> {
    let size = page_size.max(1);
    let scope = scope.map(|w| w.0.to_string());
    let origin = origin.map(schema::origin_str);
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sessions
         WHERE (?1 IS NULL OR workspace_id = ?1) AND (?2 IS NULL OR origin = ?2)",
        params![scope, origin],
        |r| r.get(0),
    )?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {CARD_COLS} FROM sessions
         WHERE (?1 IS NULL OR workspace_id = ?1) AND (?2 IS NULL OR origin = ?2)
         ORDER BY updated_at DESC, id LIMIT ?3 OFFSET ?4"
    ))?;
    let offset = i64::from(page) * i64::from(size);
    let items = stmt
        .query_map(
            params![scope, origin, i64::from(size), offset],
            card_from_row,
        )?
        .collect::<Result<_, _>>()?;
    Ok(Page {
        items,
        total: to_u64(total),
        page,
        page_size: size,
    })
}

fn load_quota(conn: &Connection) -> Result<Vec<QuotaWindow>, StoreError> {
    let mut stmt =
        conn.prepare("SELECT kind, utilization_pct, resets_at FROM quota ORDER BY kind")?;
    let rows = stmt.query_map([], |r| {
        Ok(QuotaWindow {
            kind: schema::parse_quota_kind(0, &r.get::<_, String>(0)?)?,
            utilization_pct: r.get(1)?,
            resets_at: r
                .get::<_, Option<String>>(2)?
                .map(|s| schema::parse_ts(2, &s))
                .transpose()?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn import_index(
    conn: &Connection,
    provider: ProviderId,
) -> Result<HashMap<String, ImportRef>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT provider_session_id, id, origin, updated_at FROM sessions
         WHERE provider_id = ?1 AND provider_session_id IS NOT NULL",
    )?;
    let rows = stmt.query_map([schema::provider_str(provider)], |r| {
        Ok((
            r.get::<_, String>(0)?,
            ImportRef {
                id: schema::session_id(1, &r.get::<_, String>(1)?)?,
                origin: schema::parse_origin(2, &r.get::<_, String>(2)?)?,
                updated_at: schema::parse_ts(3, &r.get::<_, String>(3)?)?,
            },
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn load_session(conn: &Connection, id: SessionId) -> Result<(Session, Vec<Message>), StoreError> {
    let key = id.0.to_string();
    let session = conn
        .query_row(
            "SELECT id, workspace_id, provider_id, provider_session_id, title, preview_snippet,
                model_id, effort, auto_mode, context_tokens_used, context_window_max,
                input_tokens, output_tokens, cache_read_tokens, cost_usd, created_at, updated_at,
                origin
             FROM sessions WHERE id = ?1",
            [&key],
            |r| {
                Ok(Session {
                    id: schema::session_id(0, &r.get::<_, String>(0)?)?,
                    workspace_id: schema::workspace_id(1, &r.get::<_, String>(1)?)?,
                    provider_id: schema::parse_provider(2, &r.get::<_, String>(2)?)?,
                    provider_session_id: r.get(3)?,
                    origin: schema::parse_origin(17, &r.get::<_, String>(17)?)?,
                    title: r.get(4)?,
                    preview_snippet: r.get(5)?,
                    model_id: r.get(6)?,
                    effort: schema::parse_effort(7, &r.get::<_, String>(7)?)?,
                    auto_mode: r.get(8)?,
                    usage: UsageTotals {
                        context_tokens_used: to_u64(r.get(9)?),
                        context_window_max: to_u64(r.get(10)?),
                        input_tokens: to_u64(r.get(11)?),
                        output_tokens: to_u64(r.get(12)?),
                        cache_read_tokens: to_u64(r.get(13)?),
                        cost_usd: r.get(14)?,
                    },
                    created_at: schema::parse_ts(15, &r.get::<_, String>(15)?)?,
                    updated_at: schema::parse_ts(16, &r.get::<_, String>(16)?)?,
                })
            },
        )
        .optional()?
        .ok_or(StoreError::NotFound)?;

    let mut stmt = conn.prepare(
        "SELECT id, session_id, seq, role, content_text, thinking_text, tool_name, tool_use_id,
            tool_payload_json, is_error, created_at
         FROM messages WHERE session_id = ?1 ORDER BY seq",
    )?;
    let messages = stmt
        .query_map([&key], |r| {
            Ok(Message {
                id: schema::parse_uuid(0, &r.get::<_, String>(0)?)?,
                session_id: schema::session_id(1, &r.get::<_, String>(1)?)?,
                seq: r.get(2)?,
                role: schema::parse_role(3, &r.get::<_, String>(3)?)?,
                content_text: r.get(4)?,
                thinking_text: r.get(5)?,
                tool_name: r.get(6)?,
                tool_use_id: r.get(7)?,
                tool_payload_json: r.get(8)?,
                is_error: r.get(9)?,
                created_at: schema::parse_ts(10, &r.get::<_, String>(10)?)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok((session, messages))
}
