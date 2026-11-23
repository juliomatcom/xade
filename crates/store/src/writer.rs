//! The only write path: one std thread owns the write connection.

use std::path::Path;
use std::sync::mpsc;
use std::thread;

use rusqlite::{params, Connection, TransactionBehavior};
use time::OffsetDateTime;
use xade_core::{Effort, Message, Role, Session, SessionId, UsageTotals, Workspace, WorkspaceId};

use crate::schema::{self, fmt_ts, to_i64};
use crate::StoreError;

const PREVIEW_CHARS: usize = 200;

pub enum WriteOp {
    /// Insert, or update display name, last-opened time and pin by `root_path`.
    UpsertWorkspace(Workspace),
    /// Sets `last_opened_at` to now.
    TouchWorkspace(WorkspaceId),
    /// Stored as given, including `updated_at`.
    InsertSession(Session),
    /// `None` fields are left unchanged; `updated_at` is always set to now.
    UpdateSession {
        id: SessionId,
        title: Option<String>,
        provider_session_id: Option<String>,
        model_id: Option<String>,
        effort: Option<Effort>,
        auto_mode: Option<bool>,
        preview_snippet: Option<String>,
        usage: Option<UsageTotals>,
    },
    InsertMessage(Message),
    /// Adds the turn's token and cost deltas; replaces the context figures.
    ApplyTurnUsage {
        session_id: SessionId,
        turn: UsageTotals,
    },
    /// Replies once every earlier op has been applied.
    Flush(mpsc::Sender<()>),
}

/// Cheap to clone; the writer thread exits when the last handle is dropped.
#[derive(Clone)]
pub struct WriterHandle {
    tx: mpsc::Sender<WriteOp>,
}

impl WriterHandle {
    pub fn send(&self, op: WriteOp) -> Result<(), StoreError> {
        self.tx.send(op).map_err(|_| StoreError::WriterClosed)
    }

    /// Blocks until all previously sent ops are applied.
    pub fn flush(&self) -> Result<(), StoreError> {
        let (done_tx, done_rx) = mpsc::channel();
        self.send(WriteOp::Flush(done_tx))?;
        done_rx.recv().map_err(|_| StoreError::WriterClosed)
    }
}

pub struct StoreWriter;

impl StoreWriter {
    /// Opens the database (creating and migrating it) and starts the writer thread.
    pub fn spawn(path: &Path) -> Result<WriterHandle, StoreError> {
        let mut conn = schema::open_writer(path)?;
        let (tx, rx) = mpsc::channel::<WriteOp>();
        thread::Builder::new()
            .name("store-writer".into())
            .spawn(move || {
                while let Ok(op) = rx.recv() {
                    if let WriteOp::Flush(done) = op {
                        let _ = done.send(());
                    } else if let Err(e) = apply(&mut conn, op) {
                        tracing::error!(error = %e, "store write failed");
                    }
                }
            })?;
        Ok(WriterHandle { tx })
    }
}

fn apply(conn: &mut Connection, op: WriteOp) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = fmt_ts(OffsetDateTime::now_utc());
    match op {
        WriteOp::UpsertWorkspace(w) => {
            tx.execute(
                "INSERT INTO workspaces (id, root_path, display_name, last_opened_at, is_pinned)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(root_path) DO UPDATE SET
                    display_name = excluded.display_name,
                    last_opened_at = excluded.last_opened_at,
                    is_pinned = excluded.is_pinned",
                params![
                    w.id.0.to_string(),
                    w.root_path.to_string_lossy(),
                    w.display_name,
                    fmt_ts(w.last_opened_at),
                    w.is_pinned
                ],
            )?;
        }
        WriteOp::TouchWorkspace(id) => {
            tx.execute(
                "UPDATE workspaces SET last_opened_at = ?1 WHERE id = ?2",
                params![now, id.0.to_string()],
            )?;
        }
        WriteOp::InsertSession(s) => {
            tx.execute(
                "INSERT INTO sessions (id, workspace_id, provider_id, provider_session_id, title,
                    preview_snippet, model_id, effort, auto_mode, context_tokens_used,
                    context_window_max, input_tokens, output_tokens, cache_read_tokens, cost_usd,
                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                params![
                    s.id.0.to_string(),
                    s.workspace_id.0.to_string(),
                    schema::provider_str(s.provider_id),
                    s.provider_session_id,
                    s.title,
                    s.preview_snippet,
                    s.model_id,
                    schema::effort_str(s.effort),
                    s.auto_mode,
                    to_i64(s.usage.context_tokens_used),
                    to_i64(s.usage.context_window_max),
                    to_i64(s.usage.input_tokens),
                    to_i64(s.usage.output_tokens),
                    to_i64(s.usage.cache_read_tokens),
                    s.usage.cost_usd,
                    fmt_ts(s.created_at),
                    fmt_ts(s.updated_at),
                ],
            )?;
        }
        WriteOp::UpdateSession {
            id,
            title,
            provider_session_id,
            model_id,
            effort,
            auto_mode,
            preview_snippet,
            usage,
        } => {
            let u = usage.as_ref();
            tx.execute(
                "UPDATE sessions SET
                    title = COALESCE(?2, title),
                    provider_session_id = COALESCE(?3, provider_session_id),
                    model_id = COALESCE(?4, model_id),
                    effort = COALESCE(?5, effort),
                    auto_mode = COALESCE(?6, auto_mode),
                    preview_snippet = COALESCE(?7, preview_snippet),
                    context_tokens_used = COALESCE(?8, context_tokens_used),
                    context_window_max = COALESCE(?9, context_window_max),
                    input_tokens = COALESCE(?10, input_tokens),
                    output_tokens = COALESCE(?11, output_tokens),
                    cache_read_tokens = COALESCE(?12, cache_read_tokens),
                    cost_usd = COALESCE(?13, cost_usd),
                    updated_at = ?14
                 WHERE id = ?1",
                params![
                    id.0.to_string(),
                    title,
                    provider_session_id,
                    model_id,
                    effort.map(schema::effort_str),
                    auto_mode,
                    preview_snippet,
                    u.map(|u| to_i64(u.context_tokens_used)),
                    u.map(|u| to_i64(u.context_window_max)),
                    u.map(|u| to_i64(u.input_tokens)),
                    u.map(|u| to_i64(u.output_tokens)),
                    u.map(|u| to_i64(u.cache_read_tokens)),
                    u.map(|u| u.cost_usd),
                    now,
                ],
            )?;
        }
        WriteOp::InsertMessage(m) => {
            tx.execute(
                "INSERT INTO messages (id, session_id, seq, role, content_text, thinking_text,
                    tool_name, tool_use_id, tool_payload_json, is_error, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    m.id.to_string(),
                    m.session_id.0.to_string(),
                    m.seq,
                    schema::role_str(m.role),
                    m.content_text,
                    m.thinking_text,
                    m.tool_name,
                    m.tool_use_id,
                    m.tool_payload_json,
                    m.is_error,
                    fmt_ts(m.created_at),
                ],
            )?;
            let preview = matches!(m.role, Role::User | Role::Assistant)
                .then(|| tail_chars(&m.content_text, PREVIEW_CHARS))
                .filter(|p| !p.is_empty());
            tx.execute(
                "UPDATE sessions SET preview_snippet = COALESCE(?2, preview_snippet), updated_at = ?3
                 WHERE id = ?1",
                params![m.session_id.0.to_string(), preview, now],
            )?;
        }
        WriteOp::ApplyTurnUsage { session_id, turn } => {
            tx.execute(
                "UPDATE sessions SET
                    input_tokens = input_tokens + ?2,
                    output_tokens = output_tokens + ?3,
                    cache_read_tokens = cache_read_tokens + ?4,
                    cost_usd = cost_usd + ?5,
                    context_tokens_used = ?6,
                    context_window_max = ?7,
                    updated_at = ?8
                 WHERE id = ?1",
                params![
                    session_id.0.to_string(),
                    to_i64(turn.input_tokens),
                    to_i64(turn.output_tokens),
                    to_i64(turn.cache_read_tokens),
                    turn.cost_usd,
                    to_i64(turn.context_tokens_used),
                    to_i64(turn.context_window_max),
                    now,
                ],
            )?;
        }
        WriteOp::Flush(_) => {}
    }
    tx.commit()
}

fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}
