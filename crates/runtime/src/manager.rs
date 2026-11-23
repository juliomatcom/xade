//! Routes `ManagerCmd`s to session actors; owns the map of live sessions.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aw_core::{
    AgentProvider, ProviderId, Session, SessionId, SessionStatus, SpawnConfig, UsageTotals,
};
use aw_store::{StoreReader, WriteOp, WriterHandle};
use time::OffsetDateTime;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::session_actor::{Actor, ActorCmd};
use crate::{flush, AppEvent, AppEventKind, ManagerCmd};

pub struct SessionManager {
    providers: Vec<Arc<dyn AgentProvider>>,
    writer: WriterHandle,
    reader: StoreReader,
    events: UnboundedSender<AppEvent>,
    live: HashMap<SessionId, UnboundedSender<ActorCmd>>,
    tasks: Vec<JoinHandle<()>>,
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
        let manager = SessionManager {
            providers,
            writer,
            reader,
            events,
            live: HashMap::new(),
            tasks: Vec::new(),
        };
        tokio::spawn(manager.run(cmd_rx));
        (cmd_tx, event_rx)
    }

    async fn run(mut self, mut rx: UnboundedReceiver<ManagerCmd>) {
        while let Some(cmd) = rx.recv().await {
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
                ManagerCmd::ShutdownAll(done) => {
                    for (_, tx) in self.live.drain() {
                        let _ = tx.send(ActorCmd::Close);
                    }
                    for t in self.tasks.drain(..) {
                        let _ = timeout(Duration::from_secs(10), t).await;
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
        self.tasks.push(tokio::spawn(actor.run(rx)));
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
        let next_seq = self.reader.max_seq(id).await.unwrap_or(0) + 1;
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
