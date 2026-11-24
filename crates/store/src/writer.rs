//! The only write path: one std thread owns the write connection.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use time::OffsetDateTime;
use xade_core::{
    panic_message, Effort, Message, QuotaWindow, Role, Session, SessionId, SessionOrigin,
    UsageTotals, Workspace, WorkspaceId,
};

use crate::schema::{self, fmt_ts, to_i64};
use crate::throwaway;
use crate::StoreError;

const PREVIEW_CHARS: usize = 800;

pub enum WriteOp {
    /// Insert, or update display name, last-opened time and `is_open` by `root_path`.
    UpsertWorkspace(Workspace),
    /// Sets `last_opened_at` to now and lists the workspace in the sidebar.
    TouchWorkspace(WorkspaceId),
    /// Removes the workspace from the sidebar; its sessions stay.
    CloseWorkspace(WorkspaceId),
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
    /// Keeps the last known window per kind.
    UpsertQuota(QuotaWindow),
    /// Idempotent import of a session from a provider's own history, in one transaction:
    /// upserts the workspace by `workspace_root` (`session.workspace_id` is ignored), then
    /// upserts the session by `(provider_id, provider_session_id)` as `ClaudeCode` origin and
    /// replaces its messages. A session with `Xade` origin is never touched. On update the
    /// session id, effort, auto mode and cost are kept, and a zero context max keeps the old one.
    ImportSession {
        session: Session,
        messages: Vec<Message>,
        workspace_root: PathBuf,
    },
    /// Deletes `ClaudeCode`-origin sessions whose workspace root is throwaway (see
    /// `throwaway::is_throwaway_dir`; messages and search rows go with them), then workspaces
    /// that are throwaway and hold no session. Replies with the counts.
    PruneThrowaway(mpsc::Sender<PruneCounts>),
    /// Deletes the session row; its messages and search rows go with it.
    DeleteSession(SessionId),
    /// Replies once every earlier op has been applied.
    Flush(mpsc::Sender<()>),
    /// Panics the writer thread, to exercise its supervision. Debug builds only.
    #[cfg(debug_assertions)]
    Poison,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneCounts {
    pub sessions: u32,
    pub workspaces: u32,
}

/// Whether the writer thread is still running, shared by every handle.
#[derive(Default)]
struct Health {
    dead: AtomicBool,
    reported: AtomicBool,
    reason: Mutex<String>,
}

impl Health {
    fn mark_dead(&self, reason: String) {
        let mut r = self.reason.lock().unwrap_or_else(|e| e.into_inner());
        if r.is_empty() {
            *r = reason;
        }
        self.dead.store(true, Ordering::SeqCst);
    }
}

/// Cheap to clone; the writer thread exits when the last handle is dropped.
#[derive(Clone)]
pub struct WriterHandle {
    tx: mpsc::Sender<WriteOp>,
    health: Arc<Health>,
}

impl WriterHandle {
    /// A failed send means the writer thread is gone; see [`WriterHandle::take_failure`].
    pub fn send(&self, op: WriteOp) -> Result<(), StoreError> {
        self.tx.send(op).map_err(|_| {
            self.health
                .mark_dead("the store writer channel is closed".into());
            StoreError::WriterClosed
        })
    }

    /// The reason the writer thread died, returned once; `None` while it runs or after it
    /// was reported.
    pub fn take_failure(&self) -> Option<String> {
        if !self.health.dead.load(Ordering::SeqCst)
            || self.health.reported.swap(true, Ordering::SeqCst)
        {
            return None;
        }
        Some(
            self.health
                .reason
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        )
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
        let health = Arc::new(Health::default());
        let thread_health = health.clone();
        // The JoinHandle is dropped on purpose: `Health` reports a panic or early exit.
        thread::Builder::new()
            .name("store-writer".into())
            .spawn(move || {
                let ran = catch_unwind(AssertUnwindSafe(|| {
                    while let Ok(op) = rx.recv() {
                        if let WriteOp::Flush(done) = op {
                            let _ = done.send(());
                        } else if let Err(e) = apply(&mut conn, op) {
                            tracing::error!(error = %e, "store write failed");
                        }
                    }
                }));
                if let Err(payload) = ran {
                    let msg = panic_message(&*payload);
                    tracing::error!(%msg, "store writer panicked");
                    thread_health.mark_dead(format!("the store writer panicked: {msg}"));
                } else {
                    thread_health.mark_dead("the store writer stopped".into());
                }
            })?;
        Ok(WriterHandle { tx, health })
    }
}

fn apply(conn: &mut Connection, op: WriteOp) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = fmt_ts(OffsetDateTime::now_utc());
    match op {
        WriteOp::UpsertWorkspace(w) => {
            tx.execute(
                "INSERT INTO workspaces (id, root_path, display_name, last_opened_at, is_open)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(root_path) DO UPDATE SET
                    display_name = excluded.display_name,
                    last_opened_at = excluded.last_opened_at,
                    is_open = excluded.is_open",
                params![
                    w.id.0.to_string(),
                    w.root_path.to_string_lossy(),
                    w.display_name,
                    fmt_ts(w.last_opened_at),
                    w.is_open
                ],
            )?;
        }
        WriteOp::TouchWorkspace(id) => {
            tx.execute(
                "UPDATE workspaces SET last_opened_at = ?1, is_open = 1 WHERE id = ?2",
                params![now, id.0.to_string()],
            )?;
        }
        WriteOp::CloseWorkspace(id) => {
            tx.execute(
                "UPDATE workspaces SET is_open = 0 WHERE id = ?1",
                params![id.0.to_string()],
            )?;
        }
        WriteOp::InsertSession(s) => {
            tx.execute(
                "INSERT INTO sessions (id, workspace_id, provider_id, provider_session_id, title,
                    preview_snippet, model_id, effort, auto_mode, context_tokens_used,
                    context_window_max, input_tokens, output_tokens, cache_read_tokens, cost_usd,
                    created_at, updated_at, origin)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                    ?18)",
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
                    schema::origin_str(s.origin),
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
        WriteOp::UpsertQuota(q) => {
            tx.execute(
                "INSERT INTO quota (kind, utilization_pct, resets_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(kind) DO UPDATE SET
                    utilization_pct = excluded.utilization_pct,
                    resets_at = excluded.resets_at,
                    updated_at = excluded.updated_at",
                params![
                    schema::quota_kind_str(q.kind),
                    q.utilization_pct,
                    q.resets_at.map(fmt_ts),
                    now
                ],
            )?;
        }
        WriteOp::ImportSession {
            session,
            messages,
            workspace_root,
        } => import_session(&tx, session, messages, &workspace_root)?,
        WriteOp::PruneThrowaway(reply) => {
            let counts = prune_throwaway(&tx)?;
            tx.commit()?;
            let _ = reply.send(counts);
            return Ok(());
        }
        WriteOp::DeleteSession(id) => {
            tx.execute(
                "DELETE FROM sessions WHERE id = ?1",
                params![id.0.to_string()],
            )?;
        }
        WriteOp::Flush(_) => {}
        #[cfg(debug_assertions)]
        WriteOp::Poison => panic!("forced store writer failure"),
    }
    tx.commit()
}

fn prune_throwaway(tx: &Transaction) -> rusqlite::Result<PruneCounts> {
    let mut counts = PruneCounts::default();
    let sessions: Vec<(String, String)> = tx
        .prepare(
            "SELECT s.id, w.root_path FROM sessions s JOIN workspaces w ON w.id = s.workspace_id
             WHERE s.origin = ?1",
        )?
        .query_map([schema::origin_str(SessionOrigin::ClaudeCode)], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (id, root) in sessions {
        if throwaway::is_throwaway_dir(Path::new(&root)) {
            // The sessions delete trigger and the messages cascade clear the search rows.
            counts.sessions += tx.execute("DELETE FROM sessions WHERE id = ?1", [id])? as u32;
        }
    }
    let empty: Vec<(String, String)> = tx
        .prepare(
            "SELECT id, root_path FROM workspaces
             WHERE NOT EXISTS (SELECT 1 FROM sessions WHERE workspace_id = workspaces.id)",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, root) in empty {
        if throwaway::is_throwaway_dir(Path::new(&root)) {
            counts.workspaces += tx.execute("DELETE FROM workspaces WHERE id = ?1", [id])? as u32;
        }
    }
    Ok(counts)
}

fn import_session(
    tx: &Transaction,
    s: Session,
    messages: Vec<Message>,
    root: &Path,
) -> rusqlite::Result<()> {
    let Some(provider_session_id) = s.provider_session_id.as_deref() else {
        return Ok(());
    };
    let provider = schema::provider_str(s.provider_id);
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT id, origin FROM sessions WHERE provider_id = ?1 AND provider_session_id = ?2",
            params![provider, provider_session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if matches!(&existing, Some((_, o)) if o == schema::origin_str(SessionOrigin::Xade)) {
        return Ok(());
    }

    let root_path = root.to_string_lossy();
    let display_name = root.file_name().map_or_else(
        || root_path.to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    tx.execute(
        "INSERT INTO workspaces (id, root_path, display_name, last_opened_at, is_open)
         VALUES (?1, ?2, ?3, ?4, 0) ON CONFLICT(root_path) DO NOTHING",
        params![
            WorkspaceId::new().0.to_string(),
            root_path,
            display_name,
            fmt_ts(s.updated_at)
        ],
    )?;
    let workspace_id: String = tx.query_row(
        "SELECT id FROM workspaces WHERE root_path = ?1",
        [&root_path],
        |r| r.get(0),
    )?;

    let u = &s.usage;
    let preview = messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::User | Role::Assistant) && !m.content_text.is_empty())
        .map(|m| tail_chars(&m.content_text, PREVIEW_CHARS))
        .unwrap_or_default();
    let session_id = match existing {
        Some((id, _)) => {
            tx.execute(
                "UPDATE sessions SET
                    workspace_id = ?2, title = ?3, preview_snippet = ?4, model_id = ?5,
                    context_tokens_used = ?6,
                    context_window_max = CASE WHEN ?7 > 0 THEN ?7 ELSE context_window_max END,
                    input_tokens = ?8, output_tokens = ?9, cache_read_tokens = ?10,
                    created_at = ?11, updated_at = ?12
                 WHERE id = ?1",
                params![
                    id,
                    workspace_id,
                    s.title,
                    preview,
                    s.model_id,
                    to_i64(u.context_tokens_used),
                    to_i64(u.context_window_max),
                    to_i64(u.input_tokens),
                    to_i64(u.output_tokens),
                    to_i64(u.cache_read_tokens),
                    fmt_ts(s.created_at),
                    fmt_ts(s.updated_at),
                ],
            )?;
            // No delete trigger on messages: drop their search rows by hand.
            tx.execute(
                "DELETE FROM search_index WHERE session_id = ?1 AND source = 'msg'",
                [&id],
            )?;
            tx.execute("DELETE FROM messages WHERE session_id = ?1", [&id])?;
            id
        }
        None => {
            let id = s.id.0.to_string();
            tx.execute(
                "INSERT INTO sessions (id, workspace_id, provider_id, provider_session_id, title,
                    preview_snippet, model_id, effort, auto_mode, context_tokens_used,
                    context_window_max, input_tokens, output_tokens, cache_read_tokens, cost_usd,
                    created_at, updated_at, origin)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?10, ?11, ?12, ?13, 0, ?14, ?15, ?16)",
                params![
                    id,
                    workspace_id,
                    provider,
                    provider_session_id,
                    s.title,
                    preview,
                    s.model_id,
                    schema::effort_str(Effort::Default),
                    to_i64(u.context_tokens_used),
                    to_i64(u.context_window_max),
                    to_i64(u.input_tokens),
                    to_i64(u.output_tokens),
                    to_i64(u.cache_read_tokens),
                    fmt_ts(s.created_at),
                    fmt_ts(s.updated_at),
                    schema::origin_str(SessionOrigin::ClaudeCode),
                ],
            )?;
            id
        }
    };
    let mut insert = tx.prepare(
        "INSERT INTO messages (id, session_id, seq, role, content_text, thinking_text,
            tool_name, tool_use_id, tool_payload_json, is_error, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    for m in messages {
        insert.execute(params![
            m.id.to_string(),
            session_id,
            m.seq,
            schema::role_str(m.role),
            m.content_text,
            m.thinking_text,
            m.tool_name,
            m.tool_use_id,
            m.tool_payload_json,
            m.is_error,
            fmt_ts(m.created_at),
        ])?;
    }
    Ok(())
}

fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}
