//! Migrations, connection setup and the column encodings shared by reader and writer.

use std::path::Path;
use std::time::Duration;

use rusqlite::types::Type;
use rusqlite::{Connection, OpenFlags};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;
use xade_core::{Effort, ProviderId, QuotaKind, Role, SessionId, SessionOrigin, WorkspaceId};

use crate::StoreError;

/// Migrations in order; `PRAGMA user_version` is the count already applied.
const MIGRATIONS: &[&str] = &[V1, V2, V3, V4];

pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// `is_pinned` was never used; it becomes `is_open` and only the most recently opened
/// workspace stays in the sidebar.
const V3: &str = "
ALTER TABLE workspaces RENAME COLUMN is_pinned TO is_open;
UPDATE workspaces SET is_open = 1
 WHERE id = (SELECT id FROM workspaces ORDER BY last_opened_at DESC LIMIT 1);
";

/// Model and effort of the last sent message, so the context-cost warning survives a restart.
const V4: &str = "
ALTER TABLE sessions ADD COLUMN sent_model_id TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN sent_effort TEXT NOT NULL DEFAULT 'default';
UPDATE sessions SET sent_model_id = model_id, sent_effort = effort;
";

const V1: &str = "
CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    root_path TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    last_opened_at TEXT NOT NULL,
    is_pinned INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL,
    provider_session_id TEXT,
    title TEXT NOT NULL,
    preview_snippet TEXT NOT NULL DEFAULT '',
    model_id TEXT NOT NULL,
    effort TEXT NOT NULL,
    auto_mode INTEGER NOT NULL DEFAULT 0,
    context_tokens_used INTEGER NOT NULL DEFAULT 0,
    context_window_max INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd REAL NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX sessions_workspace_updated ON sessions(workspace_id, updated_at DESC);
CREATE INDEX sessions_updated ON sessions(updated_at DESC);

CREATE TABLE messages (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    role TEXT NOT NULL,
    content_text TEXT NOT NULL,
    thinking_text TEXT,
    tool_name TEXT,
    tool_use_id TEXT,
    tool_payload_json TEXT,
    is_error INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    UNIQUE(session_id, seq)
);

CREATE VIRTUAL TABLE search_index USING fts5(
    text, session_id UNINDEXED, source UNINDEXED, tokenize='trigram'
);

CREATE TRIGGER messages_search_ai AFTER INSERT ON messages
WHEN new.role IN ('user', 'assistant') AND new.content_text <> ''
BEGIN
    INSERT INTO search_index(text, session_id, source)
    VALUES (new.content_text, new.session_id, 'msg');
END;

CREATE TRIGGER sessions_search_ai AFTER INSERT ON sessions
BEGIN
    INSERT INTO search_index(text, session_id, source)
    VALUES (new.title, new.id, 'title');
END;

CREATE TRIGGER sessions_search_au AFTER UPDATE OF title ON sessions
WHEN old.title <> new.title
BEGIN
    DELETE FROM search_index WHERE session_id = old.id AND source = 'title';
    INSERT INTO search_index(text, session_id, source)
    VALUES (new.title, new.id, 'title');
END;

CREATE TRIGGER sessions_search_ad AFTER DELETE ON sessions
BEGIN
    DELETE FROM search_index WHERE session_id = old.id;
END;
";

const V2: &str = "
ALTER TABLE sessions ADD COLUMN origin TEXT NOT NULL DEFAULT 'xade';
CREATE UNIQUE INDEX sessions_provider_session ON sessions(provider_id, provider_session_id)
    WHERE provider_session_id IS NOT NULL;
CREATE TABLE quota (
    kind TEXT PRIMARY KEY,
    utilization_pct REAL NOT NULL,
    resets_at TEXT,
    updated_at TEXT NOT NULL
);
";

fn set_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(Duration::from_millis(5000))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Opens (creating if needed) the write connection and brings the schema up to date.
pub(crate) fn open_writer(path: &Path) -> Result<Connection, StoreError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut conn = Connection::open(path)?;
    set_pragmas(&conn)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// Opens a query-only connection to a database the writer has already created.
pub(crate) fn open_reader(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(conn)
}

pub(crate) fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let current: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i as u32 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

// Column encodings.

/// Fixed-width UTC RFC3339 so TEXT ordering matches time ordering.
pub(crate) fn fmt_ts(t: OffsetDateTime) -> String {
    let t = t.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.microsecond()
    )
}

fn conversion_error(
    idx: usize,
    e: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e))
}

pub(crate) fn parse_ts(idx: usize, s: &str) -> rusqlite::Result<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).map_err(|e| conversion_error(idx, e))
}

pub(crate) fn parse_uuid(idx: usize, s: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| conversion_error(idx, e))
}

pub(crate) fn session_id(idx: usize, s: &str) -> rusqlite::Result<SessionId> {
    parse_uuid(idx, s).map(SessionId)
}

pub(crate) fn workspace_id(idx: usize, s: &str) -> rusqlite::Result<WorkspaceId> {
    parse_uuid(idx, s).map(WorkspaceId)
}

pub(crate) fn provider_str(p: ProviderId) -> &'static str {
    match p {
        ProviderId::Claude => "claude",
    }
}

pub(crate) fn parse_provider(idx: usize, s: &str) -> rusqlite::Result<ProviderId> {
    match s {
        "claude" => Ok(ProviderId::Claude),
        other => Err(conversion_error(idx, UnknownValue(other.to_string()))),
    }
}

pub(crate) fn effort_str(e: Effort) -> &'static str {
    match e {
        Effort::Default => "default",
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

pub(crate) fn parse_effort(idx: usize, s: &str) -> rusqlite::Result<Effort> {
    match s {
        "default" => Ok(Effort::Default),
        "low" => Ok(Effort::Low),
        "medium" => Ok(Effort::Medium),
        "high" => Ok(Effort::High),
        "xhigh" => Ok(Effort::XHigh),
        "max" => Ok(Effort::Max),
        other => Err(conversion_error(idx, UnknownValue(other.to_string()))),
    }
}

pub(crate) fn role_str(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::ToolCall => "tool_call",
        Role::ToolResult => "tool_result",
        Role::System => "system",
    }
}

pub(crate) fn parse_role(idx: usize, s: &str) -> rusqlite::Result<Role> {
    match s {
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool_call" => Ok(Role::ToolCall),
        "tool_result" => Ok(Role::ToolResult),
        "system" => Ok(Role::System),
        other => Err(conversion_error(idx, UnknownValue(other.to_string()))),
    }
}

pub(crate) fn origin_str(o: SessionOrigin) -> &'static str {
    match o {
        SessionOrigin::Xade => "xade",
        SessionOrigin::ClaudeCode => "claude_code",
    }
}

pub(crate) fn parse_origin(idx: usize, s: &str) -> rusqlite::Result<SessionOrigin> {
    match s {
        "xade" => Ok(SessionOrigin::Xade),
        "claude_code" => Ok(SessionOrigin::ClaudeCode),
        other => Err(conversion_error(idx, UnknownValue(other.to_string()))),
    }
}

pub(crate) fn quota_kind_str(k: QuotaKind) -> &'static str {
    match k {
        QuotaKind::FiveHour => "five_hour",
        QuotaKind::SevenDay => "seven_day",
    }
}

pub(crate) fn parse_quota_kind(idx: usize, s: &str) -> rusqlite::Result<QuotaKind> {
    match s {
        "five_hour" => Ok(QuotaKind::FiveHour),
        "seven_day" => Ok(QuotaKind::SevenDay),
        other => Err(conversion_error(idx, UnknownValue(other.to_string()))),
    }
}

pub(crate) fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

pub(crate) fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

#[derive(Debug, thiserror::Error)]
#[error("unknown stored value: {0}")]
struct UnknownValue(String);

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn v1_database_upgrades_with_existing_sessions_defaulting_to_xade() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(V1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(
            "INSERT INTO workspaces VALUES ('w', '/p', 'p', 't', 0);
             INSERT INTO sessions (id, workspace_id, provider_id, provider_session_id, title, model_id,
                effort, created_at, updated_at)
             VALUES ('s', 'w', 'claude', 'abc', 't', 'sonnet', 'default', 't', 't');",
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let origin: String = conn
            .query_row("SELECT origin FROM sessions WHERE id = 's'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(origin, "xade");
        conn.execute_batch("SELECT * FROM quota").unwrap();
        let v: u32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn v3_keeps_only_the_most_recent_workspace_open() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(V1).unwrap();
        conn.execute_batch(V2).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        conn.execute_batch(
            "INSERT INTO workspaces VALUES ('old', '/a', 'a', '2026-01-01T00:00:00Z', 0);
             INSERT INTO workspaces VALUES ('new', '/b', 'b', '2026-02-01T00:00:00Z', 0);",
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let open: Vec<String> = conn
            .prepare("SELECT id FROM workspaces WHERE is_open = 1")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(open, ["new"]);
    }
}
