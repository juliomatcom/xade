//! Read side: one query-only connection behind a mutex, async via `spawn_blocking`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension, Row};
use time::OffsetDateTime;
use xade_core::{
    Effort, Message, MultiMemberBadge, MultiSession, MultiSessionId, ProviderId, QuotaWindow,
    Session, SessionId, SessionOrigin, TeamMessage, TeamMessageKind, UsageTotals, Workspace,
    WorkspaceId,
};

use crate::schema::{self, to_u64};
use crate::{search, CardTarget, Page, SessionCard, StoreError};

/// A stored session that has a `provider_session_id`, as the importer sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportRef {
    pub id: SessionId,
    pub origin: SessionOrigin,
    pub updated_at: OffsetDateTime,
}

/// A multi-session with its ordered member sessions (and each member's history) plus the shared
/// Team feed.
#[derive(Debug, Clone)]
pub struct LoadedMultiSession {
    pub multi: MultiSession,
    pub members: Vec<(Session, Vec<Message>)>,
    pub team_messages: Vec<TeamMessage>,
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

    /// Model, effort and Auto mode of the most recently updated standalone session `provider`
    /// has in Xade; another provider's model id is never returned.
    pub async fn last_model_choice(
        &self,
        provider: ProviderId,
    ) -> Result<Option<(String, Effort, bool)>, StoreError> {
        self.with_conn(move |c| {
            Ok(c.query_row(
                "SELECT model_id, effort, auto_mode FROM sessions
                 WHERE origin = 'xade' AND multi_session_id IS NULL AND provider_id = ?1
                 ORDER BY updated_at DESC LIMIT 1",
                [schema::provider_str(provider)],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        schema::parse_effort(1, &r.get::<_, String>(1)?)?,
                        r.get::<_, bool>(2)?,
                    ))
                },
            )
            .optional()?)
        })
        .await
    }

    /// Provider of the most recently updated standalone session created in Xade.
    pub async fn last_provider(&self) -> Result<Option<ProviderId>, StoreError> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT provider_id FROM sessions
                 WHERE origin = 'xade' AND multi_session_id IS NULL
                 ORDER BY updated_at DESC LIMIT 1",
                [],
                |r| schema::parse_provider(0, &r.get::<_, String>(0)?),
            )
            .optional()?)
        })
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

    pub async fn load_multi_session(
        &self,
        id: MultiSessionId,
    ) -> Result<LoadedMultiSession, StoreError> {
        self.with_conn(move |c| load_multi_session(c, id)).await
    }

    /// Highest `seq` in the multi-session's Team feed, or 0 when empty.
    pub async fn max_team_seq(&self, id: MultiSessionId) -> Result<i64, StoreError> {
        self.with_conn(move |c| {
            Ok(c.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM team_messages WHERE multi_session_id = ?1",
                [id.0.to_string()],
                |r| r.get(0),
            )?)
        })
        .await
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

pub(crate) fn card_from_union_row(r: &Row) -> rusqlite::Result<SessionCard> {
    let kind: String = r.get(0)?;
    let target = if kind == "m" {
        CardTarget::Multi(schema::multi_session_id(1, &r.get::<_, String>(1)?)?)
    } else {
        CardTarget::Session(schema::session_id(1, &r.get::<_, String>(1)?)?)
    };
    Ok(SessionCard {
        target,
        workspace_id: schema::workspace_id(2, &r.get::<_, String>(2)?)?,
        title: r.get(3)?,
        preview_snippet: r.get(4)?,
        model_id: r.get(5)?,
        updated_at: schema::parse_ts(6, &r.get::<_, String>(6)?)?,
        origin: schema::parse_origin(7, &r.get::<_, String>(7)?)?,
        excerpt: None,
        provider_session_id: r.get(8)?,
        provider_id: schema::parse_provider(9, &r.get::<_, String>(9)?)?,
        members: Vec::new(),
    })
}

pub(crate) fn hydrate_multi_members(
    conn: &Connection,
    cards: &mut [SessionCard],
) -> Result<(), StoreError> {
    let mut stmt = conn.prepare(
        "SELECT provider_id, agent_handle, model_id FROM sessions
         WHERE multi_session_id = ?1
         ORDER BY member_order, id",
    )?;
    for card in cards {
        let Some(mid) = card.multi_id() else {
            continue;
        };
        let members: Vec<MultiMemberBadge> = stmt
            .query_map([mid.0.to_string()], |r| {
                Ok(MultiMemberBadge {
                    provider_id: schema::parse_provider(0, &r.get::<_, String>(0)?)?,
                    handle: r.get::<_, String>(1)?,
                    model_id: r.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        if let Some(first) = members.first() {
            card.provider_id = first.provider_id;
        }
        card.members = members;
    }
    Ok(())
}

fn list_workspaces(conn: &Connection) -> Result<Vec<Workspace>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, root_path, display_name, last_opened_at, is_open
         FROM workspaces ORDER BY last_opened_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Workspace {
            id: schema::workspace_id(0, &r.get::<_, String>(0)?)?,
            root_path: PathBuf::from(r.get::<_, String>(1)?),
            display_name: r.get(2)?,
            last_opened_at: schema::parse_ts(3, &r.get::<_, String>(3)?)?,
            is_open: r.get(4)?,
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
    let tx = conn.unchecked_transaction()?;
    let size = page_size.max(1);
    let scope = scope.map(|w| w.0.to_string());
    let origin = origin.map(schema::origin_str);
    let total: i64 = tx.query_row(
        "SELECT
            (SELECT COUNT(*) FROM sessions
              WHERE multi_session_id IS NULL
                AND (?1 IS NULL OR workspace_id = ?1)
                AND (?2 IS NULL OR origin = ?2))
            +
            (SELECT COUNT(*) FROM multi_sessions
              WHERE (?1 IS NULL OR workspace_id = ?1)
                AND (?2 IS NULL OR 'xade' = ?2))",
        params![scope, origin],
        |r| r.get(0),
    )?;
    let mut stmt = tx.prepare(
        "SELECT 's' AS row_kind, id, workspace_id, title, preview_snippet, model_id,
                updated_at, origin, provider_session_id, provider_id
         FROM sessions
         WHERE multi_session_id IS NULL
           AND (?1 IS NULL OR workspace_id = ?1)
           AND (?2 IS NULL OR origin = ?2)
         UNION ALL
         SELECT 'm' AS row_kind, id, workspace_id, title, preview_snippet, '' AS model_id,
                updated_at, 'xade' AS origin, NULL AS provider_session_id, 'claude' AS provider_id
         FROM multi_sessions
         WHERE (?1 IS NULL OR workspace_id = ?1)
           AND (?2 IS NULL OR 'xade' = ?2)
         ORDER BY updated_at DESC, id
         LIMIT ?3 OFFSET ?4",
    )?;
    let offset = i64::from(page).saturating_mul(i64::from(size));
    let mut items: Vec<SessionCard> = stmt
        .query_map(
            params![scope, origin, i64::from(size), offset],
            card_from_union_row,
        )?
        .collect::<Result<_, _>>()?;
    hydrate_multi_members(&tx, &mut items)?;
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

const SESSION_COLS: &str = "id, workspace_id, provider_id, provider_session_id, title,
    preview_snippet, model_id, effort, auto_mode, context_tokens_used, context_window_max,
    input_tokens, output_tokens, cache_read_tokens, cost_usd, created_at, updated_at,
    origin, sent_model_id, sent_effort, multi_session_id, agent_handle";

fn session_from_row(r: &Row) -> rusqlite::Result<Session> {
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
        sent_model_id: r.get(18)?,
        sent_effort: schema::parse_effort(19, &r.get::<_, String>(19)?)?,
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
        multi_session_id: r
            .get::<_, Option<String>>(20)?
            .map(|id| schema::multi_session_id(20, &id))
            .transpose()?,
        agent_handle: r.get(21)?,
    })
}

fn load_messages(conn: &Connection, session_key: &str) -> Result<Vec<Message>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, seq, role, content_text, thinking_text, tool_name, tool_use_id,
            tool_payload_json, is_error, created_at, provider_id, sender_handle
         FROM messages WHERE session_id = ?1 ORDER BY seq",
    )?;
    let messages = stmt
        .query_map([session_key], |r| {
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
                provider_id: r
                    .get::<_, Option<String>>(11)?
                    .map(|p| schema::parse_provider(11, &p))
                    .transpose()?,
                sender_handle: r.get(12)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(messages)
}

fn load_session(conn: &Connection, id: SessionId) -> Result<(Session, Vec<Message>), StoreError> {
    let key = id.0.to_string();
    let session = conn
        .query_row(
            &format!("SELECT {SESSION_COLS} FROM sessions WHERE id = ?1"),
            [&key],
            session_from_row,
        )
        .optional()?
        .ok_or(StoreError::NotFound)?;
    let messages = load_messages(conn, &key)?;
    Ok((session, messages))
}

fn load_multi_session(
    conn: &Connection,
    id: MultiSessionId,
) -> Result<LoadedMultiSession, StoreError> {
    let tx = conn.unchecked_transaction()?;
    let key = id.0.to_string();
    let multi = tx
        .query_row(
            "SELECT id, workspace_id, title, preview_snippet, created_at, updated_at
             FROM multi_sessions WHERE id = ?1",
            [&key],
            |r| {
                Ok(MultiSession {
                    id: schema::multi_session_id(0, &r.get::<_, String>(0)?)?,
                    workspace_id: schema::workspace_id(1, &r.get::<_, String>(1)?)?,
                    title: r.get(2)?,
                    preview_snippet: r.get(3)?,
                    created_at: schema::parse_ts(4, &r.get::<_, String>(4)?)?,
                    updated_at: schema::parse_ts(5, &r.get::<_, String>(5)?)?,
                })
            },
        )
        .optional()?
        .ok_or(StoreError::NotFound)?;

    let mut member_stmt = tx.prepare(&format!(
        "SELECT {SESSION_COLS} FROM sessions
         WHERE multi_session_id = ?1
         ORDER BY member_order, id"
    ))?;
    let member_rows: Vec<Session> = member_stmt
        .query_map([&key], session_from_row)?
        .collect::<Result<_, _>>()?;
    let mut members = Vec::with_capacity(member_rows.len());
    for session in member_rows {
        let messages = load_messages(&tx, &session.id.0.to_string())?;
        members.push((session, messages));
    }

    let mut team_stmt = tx.prepare(
        "SELECT id, multi_session_id, seq, kind, target_handle, sender_handle, sender_provider,
                content_text, created_at
         FROM team_messages
         WHERE multi_session_id = ?1
         ORDER BY seq",
    )?;
    let team_messages = team_stmt
        .query_map([&key], |r| {
            let kind_str: String = r.get(3)?;
            let kind = match kind_str.as_str() {
                "user" => TeamMessageKind::User { target: r.get(4)? },
                "broadcast" => TeamMessageKind::Broadcast {
                    sender_handle: r.get::<_, String>(5)?,
                    sender_provider: schema::parse_provider(6, &r.get::<_, String>(6)?)?,
                },
                other => return Err(schema::unknown_value(3, other.to_string())),
            };
            Ok(TeamMessage {
                id: schema::parse_uuid(0, &r.get::<_, String>(0)?)?,
                multi_session_id: schema::multi_session_id(1, &r.get::<_, String>(1)?)?,
                seq: r.get(2)?,
                kind,
                content_text: r.get(7)?,
                created_at: schema::parse_ts(8, &r.get::<_, String>(8)?)?,
            })
        })?
        .collect::<Result<_, _>>()?;

    Ok(LoadedMultiSession {
        multi,
        members,
        team_messages,
    })
}
