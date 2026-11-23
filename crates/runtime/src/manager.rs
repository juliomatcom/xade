//! Routes `ManagerCmd`s to session actors; owns the map of live sessions.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;
use xade_core::{
    panic_message, AgentProvider, Effort, ImportedSession, Message, ProviderId, Session, SessionId,
    SessionOrigin, SessionStatus, SpawnConfig, UsageTotals, WorkspaceId,
};
use xade_store::{throwaway, StoreReader, WriteOp, WriterHandle};

use crate::session_actor::{Actor, ActorCmd};
use crate::{flush, AppEvent, AppEventKind, ManagerCmd};

pub struct SessionManager {
    providers: Vec<Arc<dyn AgentProvider>>,
    writer: WriterHandle,
    reader: StoreReader,
    events: UnboundedSender<AppEvent>,
    live: HashMap<SessionId, UnboundedSender<ActorCmd>>,
    tasks: Vec<JoinHandle<()>>,
    /// Parsed histories coming back from the blocking import tasks.
    import_tx: UnboundedSender<Vec<(ProviderId, Vec<ImportedSession>)>>,
}

impl SessionManager {
    /// Spawns the manager task; call inside a tokio runtime.
    pub fn start(
        providers: Vec<Arc<dyn AgentProvider>>,
        writer: WriterHandle,
        reader: StoreReader,
    ) -> (UnboundedSender<ManagerCmd>, UnboundedReceiver<AppEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let (import_tx, import_rx) = mpsc::unbounded_channel();
        let supervisor_events = events.clone();
        let manager = SessionManager {
            providers,
            writer,
            reader,
            events,
            live: HashMap::new(),
            tasks: Vec::new(),
            import_tx,
        };
        let task = tokio::spawn(manager.run(cmd_rx, import_rx));
        tokio::spawn(supervise(
            task,
            "The session manager",
            None,
            supervisor_events,
        ));
        (cmd_tx, event_rx)
    }

    async fn run(
        mut self,
        mut rx: UnboundedReceiver<ManagerCmd>,
        mut imports: UnboundedReceiver<Vec<(ProviderId, Vec<ImportedSession>)>>,
    ) {
        loop {
            let cmd = tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(c) => c,
                    None => break,
                },
                // `self` holds a sender, so this never yields `None`.
                Some(batches) = imports.recv() => {
                    self.apply_import(batches).await;
                    continue;
                }
            };
            match cmd {
                ManagerCmd::Create {
                    workspace_id,
                    cwd,
                    provider,
                    model_id,
                    effort,
                    auto_mode,
                } => {
                    let now = OffsetDateTime::now_utc();
                    let session = Session {
                        id: SessionId::new(),
                        workspace_id,
                        provider_id: provider,
                        provider_session_id: None,
                        origin: SessionOrigin::Xade,
                        title: String::new(),
                        preview_snippet: String::new(),
                        model_id,
                        effort,
                        auto_mode,
                        usage: UsageTotals::default(),
                        created_at: now,
                        updated_at: now,
                    };
                    self.create(session, cwd).await;
                }
                ManagerCmd::Open { session_id } => self.open(session_id).await,
                ManagerCmd::Send { session_id, text } => {
                    self.forward(session_id, ActorCmd::Send(text));
                }
                ManagerCmd::Interrupt { session_id } => {
                    self.forward(session_id, ActorCmd::Interrupt);
                }
                ManagerCmd::RespondPermission {
                    session_id,
                    request_id,
                    allow,
                    enable_auto,
                } => self.forward(
                    session_id,
                    ActorCmd::Respond {
                        request_id,
                        allow,
                        enable_auto,
                    },
                ),
                ManagerCmd::SetAutoMode { session_id, on } => {
                    self.forward(session_id, ActorCmd::SetAutoMode(on));
                }
                ManagerCmd::SetModel {
                    session_id,
                    model_id,
                } => self.forward(session_id, ActorCmd::SetModel(model_id)),
                ManagerCmd::SetEffort { session_id, effort } => {
                    self.forward(session_id, ActorCmd::SetEffort(effort));
                }
                ManagerCmd::Close { session_id } => {
                    if let Some(tx) = self.live.remove(&session_id) {
                        let _ = tx.send(ActorCmd::Close);
                    }
                }
                ManagerCmd::DeleteSession { session_id } => self.delete(session_id).await,
                ManagerCmd::Import => self.start_import(),
                ManagerCmd::ShutdownAll(done) => {
                    for (_, tx) in self.live.drain() {
                        let _ = tx.send(ActorCmd::Close);
                    }
                    for t in self.tasks.drain(..) {
                        if timeout(Duration::from_secs(10), t).await.is_err() {
                            tracing::warn!("a session did not finish before shutdown");
                        }
                    }
                    let _ = done.send(());
                }
            }
        }
    }

    fn emit(&self, session_id: Option<SessionId>, kind: AppEventKind) {
        let _ = self.events.send(AppEvent { session_id, kind });
    }

    fn forward(&mut self, id: SessionId, cmd: ActorCmd) {
        let sent = self.live.get(&id).map(|tx| tx.send(cmd).is_ok());
        if sent != Some(true) {
            self.emit(
                Some(id),
                AppEventKind::Error("session is not running".into()),
            );
        }
    }

    fn provider(&self, id: ProviderId) -> Option<Arc<dyn AgentProvider>> {
        self.providers.iter().find(|p| p.id() == id).cloned()
    }

    fn start_actor(&mut self, actor: Actor) {
        let id = actor.id();
        let (tx, rx) = mpsc::unbounded_channel();
        self.live.insert(id, tx);
        self.tasks.retain(|t| !t.is_finished());
        let task = tokio::spawn(actor.run(rx));
        self.tasks.push(tokio::spawn(supervise(
            task,
            "A session",
            Some(id),
            self.events.clone(),
        )));
    }

    /// Parses histories off the manager task; `apply_import` runs when they arrive.
    fn start_import(&self) {
        let providers = self.providers.clone();
        let tx = self.import_tx.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let mut batches = Vec::new();
            for p in providers {
                let id = p.id();
                match tokio::task::spawn_blocking(move || p.import_sessions()).await {
                    Ok(sessions) => batches.push((id, sessions)),
                    Err(e) => {
                        tracing::error!(error = %e, "import task failed");
                        let kind = AppEventKind::Error(format!("Importing history failed: {e}"));
                        let _ = events.send(AppEvent {
                            session_id: None,
                            kind,
                        });
                    }
                }
            }
            let _ = tx.send(batches);
        });
    }

    /// Skips sessions Xade owns or runs, transcripts not newer than the stored row, and
    /// sessions from throwaway directories; first prunes what earlier runs left of those.
    async fn apply_import(&mut self, batches: Vec<(ProviderId, Vec<ImportedSession>)>) {
        let (mut imported, mut updated, mut skipped) = (0u32, 0u32, 0u32);
        let (prune_tx, prune_rx) = std::sync::mpsc::channel();
        let _ = self.writer.send(WriteOp::PruneThrowaway(prune_tx));
        flush(&self.writer).await;
        let pruned = prune_rx.try_recv().unwrap_or_default();
        for (provider_id, sessions) in batches {
            let Some(provider) = self.provider(provider_id) else {
                continue;
            };
            let index = match self.reader.import_index(provider_id).await {
                Ok(i) => i,
                Err(e) => {
                    self.emit(None, AppEventKind::Error(e.to_string()));
                    continue;
                }
            };
            let default_model = provider.default_model();
            for imp in sessions {
                if throwaway::is_throwaway_dir(&imp.cwd) {
                    skipped += 1;
                    continue;
                }
                let id = match index.get(&imp.provider_session_id) {
                    Some(r) if r.origin == SessionOrigin::Xade => continue,
                    Some(r) if self.live.get(&r.id).is_some_and(|tx| !tx.is_closed()) => continue,
                    Some(r) if r.updated_at >= imp.updated_at => continue,
                    Some(r) => {
                        updated += 1;
                        r.id
                    }
                    None => {
                        imported += 1;
                        SessionId::new()
                    }
                };
                let (session, messages, root) = imported_rows(id, provider_id, &default_model, imp);
                let _ = self.writer.send(WriteOp::ImportSession {
                    session,
                    messages,
                    workspace_root: root,
                });
            }
        }
        flush(&self.writer).await;
        tracing::info!(
            imported,
            updated,
            skipped_throwaway = skipped,
            pruned_sessions = pruned.sessions,
            pruned_workspaces = pruned.workspaces,
            "import applied"
        );
        self.emit(None, AppEventKind::ImportDone { imported, updated });
    }

    async fn create(&mut self, session: Session, cwd: std::path::PathBuf) {
        let Some(provider) = self.provider(session.provider_id) else {
            return self.emit(None, AppEventKind::Error("unknown provider".into()));
        };
        let cfg = SpawnConfig {
            cwd: cwd.clone(),
            model_id: session.model_id.clone(),
            effort: session.effort,
            auto_mode: session.auto_mode,
            resume: None,
        };
        let handle = match provider.spawn(cfg) {
            Ok(h) => h,
            Err(e) => return self.emit(None, AppEventKind::Error(e.to_string())),
        };
        let _ = self.writer.send(WriteOp::InsertSession(session.clone()));
        let _ = self
            .writer
            .send(WriteOp::TouchWorkspace(session.workspace_id));
        flush(&self.writer).await;
        let id = session.id;
        self.emit(Some(id), AppEventKind::SessionCreated(session.clone()));
        let actor = Actor::new(
            session,
            cwd,
            provider,
            self.writer.clone(),
            self.events.clone(),
            handle,
            1,
        );
        self.start_actor(actor);
        self.emit(Some(id), AppEventKind::Status(SessionStatus::Idle));
    }

    /// Xade-origin sessions only. The provider copy goes first: if that fails nothing is
    /// deleted, and if the row went first an import would bring the session back.
    async fn delete(&mut self, id: SessionId) {
        let fail = |this: &Self, msg: String| this.emit(Some(id), AppEventKind::Error(msg));
        let mut session = match self.reader.load_session(id).await {
            Ok((s, _)) => s,
            Err(e) => return fail(self, format!("Could not delete the session: {e}")),
        };
        if session.origin != SessionOrigin::Xade {
            return fail(
                self,
                "Only sessions created in Xade can be deleted here.".into(),
            );
        }
        if let Some(tx) = self.live.remove(&id) {
            let _ = tx.send(ActorCmd::Close);
            // The actor drops its receiver when it ends, after its child exited.
            if timeout(Duration::from_secs(10), tx.closed()).await.is_err() {
                self.live.insert(id, tx);
                return fail(
                    self,
                    "Could not delete the session: it did not stop in time.".into(),
                );
            }
            // The actor may have stored the provider id while it ran.
            flush(&self.writer).await;
            session = match self.reader.load_session(id).await {
                Ok((s, _)) => s,
                Err(e) => return fail(self, format!("Could not delete the session: {e}")),
            };
        }
        if let (Some(psid), Some(provider)) = (
            session.provider_session_id.clone(),
            self.provider(session.provider_id),
        ) {
            let done = tokio::task::spawn_blocking(move || provider.delete_session(&psid)).await;
            match done {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return fail(self, format!("Could not delete the transcript: {e}")),
                Err(e) => return fail(self, format!("Could not delete the transcript: {e}")),
            }
        }
        if let Err(e) = self.writer.send(WriteOp::DeleteSession(id)) {
            return fail(self, format!("Could not delete the session: {e}"));
        }
        flush(&self.writer).await;
        self.emit(Some(id), AppEventKind::SessionDeleted { session_id: id });
    }

    async fn open(&mut self, id: SessionId) {
        if self.live.get(&id).is_some_and(|tx| !tx.is_closed()) {
            return;
        }
        self.live.remove(&id);
        let (session, messages) = match self.reader.load_session(id).await {
            Ok(x) => x,
            Err(e) => return self.emit(Some(id), AppEventKind::Error(e.to_string())),
        };
        let cwd = match self.reader.list_workspaces().await {
            Ok(ws) => ws
                .into_iter()
                .find(|w| w.id == session.workspace_id)
                .map(|w| w.root_path),
            Err(e) => return self.emit(Some(id), AppEventKind::Error(e.to_string())),
        };
        // A wrong guess here would reuse sequence numbers of stored messages.
        let next_seq = match self.reader.max_seq(id).await {
            Ok(seq) => seq + 1,
            Err(e) => {
                return self.emit(
                    Some(id),
                    AppEventKind::Error(format!("Could not open the session: {e}")),
                )
            }
        };
        let _ = self
            .writer
            .send(WriteOp::TouchWorkspace(session.workspace_id));
        self.emit(
            Some(id),
            AppEventKind::SessionOpened {
                session: session.clone(),
                messages,
            },
        );
        let spawned = match (cwd, self.provider(session.provider_id)) {
            (Some(cwd), Some(provider)) => provider
                .spawn(SpawnConfig {
                    cwd: cwd.clone(),
                    model_id: session.model_id.clone(),
                    effort: session.effort,
                    auto_mode: session.auto_mode,
                    resume: session.provider_session_id.clone(),
                })
                .map(|h| (cwd, provider, h))
                .map_err(|e| e.to_string()),
            (None, _) => Err("workspace not found".to_string()),
            (_, None) => Err("unknown provider".to_string()),
        };
        match spawned {
            Ok((cwd, provider, handle)) => {
                let actor = Actor::new(
                    session,
                    cwd,
                    provider,
                    self.writer.clone(),
                    self.events.clone(),
                    handle,
                    next_seq,
                );
                self.start_actor(actor);
                self.emit(Some(id), AppEventKind::Status(SessionStatus::Idle));
            }
            Err(msg) => self.emit(Some(id), AppEventKind::Status(SessionStatus::Error(msg))),
        }
    }
}

/// Awaits a manager or session task and makes a panic visible: an error event, and for a
/// session its `Error` status. A cancelled task (runtime shutdown) is not an error.
async fn supervise(
    task: JoinHandle<()>,
    what: &'static str,
    session_id: Option<SessionId>,
    events: UnboundedSender<AppEvent>,
) {
    let Err(e) = task.await else { return };
    if !e.is_panic() {
        return;
    }
    let msg = panic_message(&*e.into_panic());
    tracing::error!(what, %msg, "task panicked");
    if session_id.is_some() {
        let status = SessionStatus::Error(format!("session crashed: {msg}"));
        let _ = events.send(AppEvent {
            session_id,
            kind: AppEventKind::Status(status),
        });
    }
    let kind = AppEventKind::Error(format!("{what} crashed: {msg}"));
    let _ = events.send(AppEvent {
        session_id: None,
        kind,
    });
}

/// Rows for `WriteOp::ImportSession`; the writer resolves the real workspace id from the path.
fn imported_rows(
    id: SessionId,
    provider_id: ProviderId,
    default_model: &str,
    imp: ImportedSession,
) -> (Session, Vec<Message>, std::path::PathBuf) {
    let messages = imp
        .messages
        .into_iter()
        .zip(1..)
        .map(|(m, seq)| Message {
            id: Uuid::new_v4(),
            session_id: id,
            seq,
            role: m.role,
            content_text: m.content_text,
            thinking_text: None,
            tool_name: m.tool_name,
            tool_use_id: m.tool_use_id,
            tool_payload_json: m.tool_payload_json,
            is_error: m.is_error,
            created_at: m.created_at,
        })
        .collect();
    let session = Session {
        id,
        workspace_id: WorkspaceId::new(),
        provider_id,
        provider_session_id: Some(imp.provider_session_id),
        origin: SessionOrigin::ClaudeCode,
        title: imp.title,
        preview_snippet: String::new(),
        model_id: if imp.model_id.is_empty() {
            default_model.to_string()
        } else {
            imp.model_id
        },
        effort: Effort::Default,
        auto_mode: false,
        usage: imp.usage,
        created_at: imp.created_at,
        updated_at: imp.updated_at,
    };
    (session, messages, imp.cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panicking_task_becomes_error_events() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = SessionId::new();
        let task = tokio::spawn(async { panic!("boom") });
        supervise(task, "A session", Some(id), tx.clone()).await;
        let first = rx.recv().await.expect("status event");
        assert_eq!(first.session_id, Some(id));
        assert!(
            matches!(first.kind, AppEventKind::Status(SessionStatus::Error(m)) if m.contains("boom"))
        );
        let second = rx.recv().await.expect("error event");
        assert!(
            matches!(second.kind, AppEventKind::Error(m) if m.contains("A session crashed: boom"))
        );
        // A task that ends normally says nothing.
        supervise(tokio::spawn(async {}), "A session", Some(id), tx).await;
        assert!(rx.try_recv().is_err());
    }
}
