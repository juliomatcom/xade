//! Routes `ManagerCmd`s to session actors; owns the map of live sessions.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;
use xade_core::{
    assign_handles, extract_broadcasts, panic_message, parse_user_target, AgentProvider, Delivery,
    Effort, ImportedSession, Message, MultiMemberSpec, MultiSession, MultiSessionId, ProviderId,
    Role, Session, SessionId, SessionOrigin, SessionStatus, SpawnConfig, TeamMessage,
    TeamMessageKind, UsageTotals, WorkspaceId,
};
use xade_store::{throwaway, StoreReader, WriteOp, WriterHandle};

use crate::fork::{ForkJob, Throwaways};
use crate::handoff;
use crate::multi::{
    team_preamble, MemberSignal, PendingBroadcast, PendingUserMsg, TeamMember, TeamState,
    MAX_BROADCAST_HOPS,
};
use crate::session_actor::{Actor, ActorCmd};
use crate::transcript::transcript;
use crate::{flush, AppEvent, AppEventKind, ManagerCmd};

pub struct SessionManager {
    providers: Vec<Arc<dyn AgentProvider>>,
    writer: WriterHandle,
    reader: StoreReader,
    events: UnboundedSender<AppEvent>,
    live: HashMap<SessionId, UnboundedSender<ActorCmd>>,
    /// Opened sessions whose provider process starts on their first `Send`.
    dormant: HashMap<SessionId, Dormant>,
    /// Open multi-session teams.
    teams: HashMap<MultiSessionId, TeamState>,
    /// Fast lookup from member `SessionId` to its owning `MultiSessionId`.
    member_to_team: HashMap<SessionId, MultiSessionId>,
    /// Member actors currently shutting down after `CloseMulti`; awaited if reopened quickly.
    closing_members: HashMap<SessionId, UnboundedSender<ActorCmd>>,
    /// Monotonic generation stamped onto every opened/created `TeamState` so stale signals from
    /// closing actors can never poison a newly reopened instance.
    next_team_gen: u64,
    /// Signals sent from multi-session member actors back into this manager loop.
    team_tx: UnboundedSender<MemberSignal>,
    tasks: Vec<JoinHandle<()>>,
    /// Parsed histories coming back from the blocking import tasks.
    import_tx: UnboundedSender<Vec<(ProviderId, Vec<ImportedSession>)>>,
    /// Where fork hand-off files live; `None` when no safe folder exists (hand-offs go inline).
    forks_dir: Option<PathBuf>,
    /// Snapshot turns' provider conversations that may still exist; imports skip them.
    throwaways: Throwaways,
    /// Set to true on shutdown so a running fork snapshot stops and cleans up.
    fork_cancel: watch::Sender<bool>,
}

/// An opened session with no provider process yet: resuming one is how the CLI touches the
/// transcript, so it waits for the user to send something.
struct Dormant {
    session: Session,
    cwd: std::path::PathBuf,
    provider: Arc<dyn AgentProvider>,
    next_seq: i64,
    /// Set while the provider has not stored a session id yet: a fork's history or a team member's
    /// initial instructions.
    handoff: Option<String>,
    /// Set for members of a multi-session so waking attaches coordinator signals + generation.
    team: Option<(UnboundedSender<MemberSignal>, u64)>,
}

impl SessionManager {
    /// Spawns the manager task; call inside a tokio runtime.
    pub fn start(
        providers: Vec<Arc<dyn AgentProvider>>,
        writer: WriterHandle,
        reader: StoreReader,
    ) -> (UnboundedSender<ManagerCmd>, UnboundedReceiver<AppEvent>) {
        Self::start_with(providers, writer, reader, handoff::default_dir())
    }

    /// Like `start`, with fork hand-off files in `forks_dir` instead of the temp folder.
    pub fn start_in(
        providers: Vec<Arc<dyn AgentProvider>>,
        writer: WriterHandle,
        reader: StoreReader,
        forks_dir: PathBuf,
    ) -> (UnboundedSender<ManagerCmd>, UnboundedReceiver<AppEvent>) {
        Self::start_with(providers, writer, reader, Some(forks_dir))
    }

    fn start_with(
        providers: Vec<Arc<dyn AgentProvider>>,
        writer: WriterHandle,
        reader: StoreReader,
        forks_dir: Option<PathBuf>,
    ) -> (UnboundedSender<ManagerCmd>, UnboundedReceiver<AppEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let (import_tx, import_rx) = mpsc::unbounded_channel();
        let (team_tx, team_rx) = mpsc::unbounded_channel();
        let supervisor_events = events.clone();
        // Snapshot conversations an earlier run left: hidden from imports until deleted.
        let leftovers = forks_dir.as_deref().map(handoff::read_markers);
        let throwaways = Throwaways::default();
        if let Some(found) = &leftovers {
            let mut set = throwaways.lock().unwrap_or_else(PoisonError::into_inner);
            set.extend(found.iter().map(|l| l.psid.clone()));
        }
        tokio::spawn(sweep(
            providers.clone(),
            leftovers.unwrap_or_default(),
            throwaways.clone(),
        ));
        let manager = SessionManager {
            providers,
            writer,
            reader,
            events,
            live: HashMap::new(),
            dormant: HashMap::new(),
            teams: HashMap::new(),
            member_to_team: HashMap::new(),
            closing_members: HashMap::new(),
            next_team_gen: 1,
            team_tx,
            tasks: Vec::new(),
            import_tx,
            forks_dir,
            throwaways,
            fork_cancel: watch::channel(false).0,
        };
        let task = tokio::spawn(manager.run(cmd_rx, import_rx, team_rx));
        tokio::spawn(supervise(
            task,
            "The session manager",
            None,
            supervisor_events,
            None,
        ));
        (cmd_tx, event_rx)
    }

    async fn run(
        mut self,
        mut rx: UnboundedReceiver<ManagerCmd>,
        mut imports: UnboundedReceiver<Vec<(ProviderId, Vec<ImportedSession>)>>,
        mut team_signals: UnboundedReceiver<MemberSignal>,
    ) {
        loop {
            let cmd = tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(c) => c,
                    None => break,
                },
                // `self` holds senders for both channels, so neither yields `None`.
                Some(batches) = imports.recv() => {
                    self.apply_import(batches).await;
                    continue;
                }
                Some(sig) = team_signals.recv() => {
                    self.on_member_signal(sig);
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
                        sent_model_id: String::new(),
                        sent_effort: effort,
                        model_id,
                        effort,
                        auto_mode,
                        usage: UsageTotals::default(),
                        created_at: now,
                        updated_at: now,
                        multi_session_id: None,
                        agent_handle: None,
                    };
                    self.create(session, cwd).await;
                }
                ManagerCmd::Open { session_id } => self.open(session_id).await,
                ManagerCmd::Send {
                    session_id,
                    text,
                    delivery,
                    context,
                } => {
                    if self.member_to_team.contains_key(&session_id) {
                        self.emit(
                            Some(session_id),
                            AppEventKind::Error(
                                "Send messages to a multi-session via SendMulti.".into(),
                            ),
                        );
                        continue;
                    }
                    self.forward(session_id, ActorCmd::Send(text, delivery, context));
                }
                ManagerCmd::Interrupt { session_id } => {
                    let mut should_forward = true;
                    if let Some(&multi_id) = self.member_to_team.get(&session_id) {
                        if let Some(team) = self.teams.get_mut(&multi_id) {
                            if let Some(m) = team.member_mut(session_id) {
                                m.queued_user.clear();
                                m.queued_broadcasts.clear();
                                m.interrupt_epoch += 1;
                                let busy = matches!(
                                    m.status,
                                    SessionStatus::Streaming | SessionStatus::NeedsApproval
                                );
                                m.suppress_broadcasts = busy;
                                should_forward = busy
                                    && self.live.get(&session_id).is_some_and(|tx| !tx.is_closed());
                            }
                        }
                    }
                    if should_forward {
                        self.forward(session_id, ActorCmd::Interrupt);
                    }
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
                ManagerCmd::Rename { session_id, title } => {
                    if self.member_to_team.contains_key(&session_id) {
                        continue;
                    }
                    let title = title.trim().to_string();
                    if !title.is_empty() {
                        self.forward(session_id, ActorCmd::Rename(title));
                    }
                }
                ManagerCmd::Close { session_id } => {
                    if self.member_to_team.contains_key(&session_id) {
                        continue;
                    }
                    if self.dormant.remove(&session_id).is_some() {
                        self.emit(
                            Some(session_id),
                            AppEventKind::Status(SessionStatus::Closed),
                        );
                    }
                    if let Some(tx) = self.live.remove(&session_id) {
                        let _ = tx.send(ActorCmd::Close);
                    }
                }
                ManagerCmd::DeleteSession { session_id } => self.delete(session_id).await,
                ManagerCmd::Fork {
                    session_id,
                    provider,
                    handoff_prompt,
                    resume_prompt,
                } => self.fork(session_id, provider, handoff_prompt, resume_prompt),
                ManagerCmd::Import => self.start_import(),
                ManagerCmd::CreateMulti {
                    workspace_id,
                    cwd,
                    title,
                    members,
                } => self.create_multi(workspace_id, cwd, title, members).await,
                ManagerCmd::OpenMulti { multi_id } => self.open_multi(multi_id).await,
                ManagerCmd::SendMulti {
                    multi_id,
                    text,
                    steer,
                    context,
                } => self.send_multi(multi_id, text, steer, context),
                ManagerCmd::InterruptMultiAll { multi_id } => self.interrupt_multi_all(multi_id),
                ManagerCmd::ResumeMultiBroadcasts { multi_id } => {
                    self.resume_multi_broadcasts(multi_id)
                }
                ManagerCmd::RenameMulti { multi_id, title } => self.rename_multi(multi_id, title),
                ManagerCmd::CloseMulti { multi_id } => self.close_multi(multi_id),
                ManagerCmd::DeleteMulti { multi_id } => self.delete_multi(multi_id).await,
                ManagerCmd::ShutdownAll(done) => {
                    let _ = self.fork_cancel.send(true);
                    self.teams.clear();
                    self.member_to_team.clear();
                    self.dormant.clear();
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

    fn write(&self, op: WriteOp) {
        if let Err(e) = self.writer.send(op) {
            tracing::error!(error = %e, "store write dropped");
        }
    }

    fn forward(&mut self, id: SessionId, cmd: ActorCmd) {
        if self.dormant.contains_key(&id) {
            match cmd {
                ActorCmd::Send(..)
                | ActorCmd::SendTeamUser { .. }
                | ActorCmd::SendBroadcast { .. } => {
                    if !self.wake(id) {
                        return;
                    }
                }
                ActorCmd::SetAutoMode(_)
                | ActorCmd::SetModel(_)
                | ActorCmd::SetEffort(_)
                | ActorCmd::Rename(_) => {
                    return self.configure_dormant(id, cmd);
                }
                // Nothing runs, so there is nothing to interrupt or approve.
                ActorCmd::Interrupt | ActorCmd::Respond { .. } | ActorCmd::Close => return,
            }
        }
        let sent = self.live.get(&id).map(|tx| tx.send(cmd).is_ok());
        if sent != Some(true) {
            self.live.remove(&id);
            self.emit(
                Some(id),
                AppEventKind::Error("session is not running".into()),
            );
        }
    }

    /// Starts the provider process of a dormant session; false (after reporting) on failure.
    fn wake(&mut self, id: SessionId) -> bool {
        let Some(d) = self.dormant.remove(&id) else {
            return true;
        };
        let spawned = d.provider.spawn(SpawnConfig {
            cwd: d.cwd.clone(),
            model_id: d.session.model_id.clone(),
            effort: d.session.effort,
            auto_mode: d.session.auto_mode,
            resume: d.session.provider_session_id.clone(),
        });
        match spawned {
            Ok(handle) => {
                let actor = Actor::new(
                    d.session,
                    d.cwd,
                    d.provider,
                    self.writer.clone(),
                    self.events.clone(),
                    handle,
                    d.next_seq,
                )
                .with_handoff(d.handoff)
                .with_team(d.team);
                self.start_actor(actor);
                true
            }
            Err(e) => {
                let err = SessionStatus::Error(e.to_string());
                if let Some(&multi_id) = self.member_to_team.get(&id) {
                    if let Some(team) = self.teams.get_mut(&multi_id) {
                        if let Some(m) = team.member_mut(id) {
                            m.status = err.clone();
                        }
                    }
                }
                self.emit(Some(id), AppEventKind::Status(err));
                false
            }
        }
    }

    /// Stores a model, effort, auto-mode or title change for the first spawn.
    fn configure_dormant(&mut self, id: SessionId, cmd: ActorCmd) {
        let Some(d) = self.dormant.get_mut(&id) else {
            return;
        };
        let mut op = WriteOp::UpdateSession {
            id,
            title: None,
            provider_session_id: None,
            model_id: None,
            effort: None,
            auto_mode: None,
            preview_snippet: None,
            usage: None,
        };
        if let WriteOp::UpdateSession {
            title,
            model_id,
            effort,
            auto_mode,
            ..
        } = &mut op
        {
            match cmd {
                ActorCmd::SetAutoMode(on) => {
                    d.session.auto_mode = on;
                    *auto_mode = Some(on);
                }
                ActorCmd::SetModel(m) => {
                    if d.session.multi_session_id.is_some() {
                        return;
                    }
                    d.session.model_id = m.clone();
                    *model_id = Some(m);
                }
                ActorCmd::SetEffort(e) => {
                    d.session.effort = e;
                    *effort = Some(e);
                }
                ActorCmd::Rename(t) => {
                    d.session.title.clone_from(&t);
                    *title = Some(t);
                }
                _ => return,
            }
        }
        if let Err(e) = self.writer.send(op) {
            tracing::error!(error = %e, "store write dropped");
        }
    }

    fn provider(&self, id: ProviderId) -> Option<Arc<dyn AgentProvider>> {
        self.providers.iter().find(|p| p.id() == id).cloned()
    }

    fn start_actor(&mut self, actor: Actor) {
        let id = actor.id();
        let team_panic = actor
            .team_meta()
            .map(|(mid, generation)| (mid, generation, self.team_tx.clone()));
        let (tx, rx) = mpsc::unbounded_channel();
        self.live.insert(id, tx);
        self.tasks.retain(|t| !t.is_finished());
        let task = tokio::spawn(actor.run(rx));
        self.tasks.push(tokio::spawn(supervise(
            task,
            "A session",
            Some(id),
            self.events.clone(),
            team_panic,
        )));
    }

    /// Parses each provider's history off the manager task, in parallel; `apply_import` runs for
    /// each batch as it arrives, so a slow provider never holds back another's history.
    fn start_import(&self) {
        for p in self.providers.clone() {
            let tx = self.import_tx.clone();
            let events = self.events.clone();
            tokio::spawn(async move {
                let id = p.id();
                match tokio::task::spawn_blocking(move || p.import_sessions()).await {
                    Ok(sessions) => {
                        let _ = tx.send(vec![(id, sessions)]);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "import task failed");
                        let kind = AppEventKind::Error(format!("Importing history failed: {e}"));
                        let _ = events.send(AppEvent {
                            session_id: None,
                            kind,
                        });
                        // Still finish the run, so `ImportDone` always arrives.
                        let _ = tx.send(Vec::new());
                    }
                }
            });
        }
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
            for mut imp in sessions {
                imp.updated_at = store_precision(imp.updated_at);
                if throwaway::is_throwaway_dir(&imp.cwd) || self.is_snapshot_turn(&imp) {
                    skipped += 1;
                    continue;
                }
                let id = match index.get(&imp.provider_session_id) {
                    Some(r) if r.origin == SessionOrigin::Xade => continue,
                    Some(r) if self.dormant.contains_key(&r.id) => continue,
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

    /// A fork's snapshot turn: a conversation in flight (or left behind), or one whose first
    /// prompt mentions a file of the forks folder.
    fn is_snapshot_turn(&self, imp: &ImportedSession) -> bool {
        let known = self
            .throwaways
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&imp.provider_session_id);
        known
            || self.forks_dir.as_deref().is_some_and(|dir| {
                imp.messages
                    .iter()
                    .find(|m| m.role == Role::User)
                    .is_some_and(|m| handoff::mentions_dir(dir, &m.content_text))
            })
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
        )
        .unstored();
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
        if session.multi_session_id.is_some() {
            return fail(
                self,
                "Delete the multi-session instead of an individual team member.".into(),
            );
        }
        self.dormant.remove(&id);
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
        if let Some(dir) = self.forks_dir.clone() {
            if let Err(e) = tokio::task::spawn_blocking(move || handoff::remove_all(&dir, id)).await
            {
                tracing::warn!(error = %e, "fork file cleanup failed");
            }
        }
        self.emit(Some(id), AppEventKind::SessionDeleted { session_id: id });
    }

    /// Replies `ForkPreparing` at once, then `SessionForked` or `ForkFailed` (nothing written),
    /// from a task of its own: the snapshot turn can take minutes.
    fn fork(
        &mut self,
        source: SessionId,
        target: ProviderId,
        handoff_prompt: String,
        resume_prompt: String,
    ) {
        self.emit(Some(source), AppEventKind::ForkPreparing { from: source });
        let job = ForkJob {
            providers: self.providers.clone(),
            writer: self.writer.clone(),
            reader: self.reader.clone(),
            dir: self.forks_dir.clone(),
            events: self.events.clone(),
            throwaways: self.throwaways.clone(),
            cancel: self.fork_cancel.subscribe(),
        };
        let task = tokio::spawn(job.run_and_report(source, target, handoff_prompt, resume_prompt));
        self.tasks.retain(|t| !t.is_finished());
        self.tasks.push(tokio::spawn(supervise(
            task,
            "A fork",
            None,
            self.events.clone(),
            None,
        )));
    }

    async fn open(&mut self, id: SessionId) {
        if self.dormant.contains_key(&id) || self.live.get(&id).is_some_and(|tx| !tx.is_closed()) {
            return;
        }
        self.live.remove(&id);
        let (session, messages) = match self.reader.load_session(id).await {
            Ok(x) => x,
            Err(e) => return self.emit(Some(id), AppEventKind::Error(e.to_string())),
        };
        if session.multi_session_id.is_some() {
            return self.emit(
                Some(id),
                AppEventKind::Error(
                    "Open the multi-session instead of an individual member.".into(),
                ),
            );
        }
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
        let handoff = if session.provider_session_id.is_none() {
            let (dir, text) = (self.forks_dir.clone(), transcript(&messages));
            tokio::task::spawn_blocking(move || handoff::first_prompt(dir.as_deref(), id, text))
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "fork hand-off task failed");
                    None
                })
        } else {
            None
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
        match (cwd, self.provider(session.provider_id)) {
            (Some(cwd), Some(provider)) => {
                self.dormant.insert(
                    id,
                    Dormant {
                        session,
                        cwd,
                        provider,
                        next_seq,
                        handoff,
                        team: None,
                    },
                );
                self.emit(Some(id), AppEventKind::Status(SessionStatus::Idle));
            }
            (None, _) => self.emit(
                Some(id),
                AppEventKind::Status(SessionStatus::Error("workspace not found".into())),
            ),
            (_, None) => self.emit(
                Some(id),
                AppEventKind::Status(SessionStatus::Error("unknown provider".into())),
            ),
        }
    }

    // ---- multi-session coordination ----------------------------------------------------------

    async fn create_multi(
        &mut self,
        workspace_id: WorkspaceId,
        cwd: PathBuf,
        title: String,
        specs: Vec<MultiMemberSpec>,
    ) {
        if specs.is_empty() {
            return self.emit(
                None,
                AppEventKind::Error("A multi-session needs at least one model.".into()),
            );
        }
        let mut resolved = Vec::with_capacity(specs.len());
        for spec in specs {
            let Some(provider) = self.provider(spec.provider_id) else {
                return self.emit(None, AppEventKind::Error("unknown provider".into()));
            };
            resolved.push((spec, provider));
        }
        let now = OffsetDateTime::now_utc();
        let multi_id = MultiSessionId::new();
        let multi = MultiSession {
            id: multi_id,
            workspace_id,
            title: title.trim().to_string(),
            preview_snippet: String::new(),
            created_at: now,
            updated_at: now,
        };
        let model_ids: Vec<&str> = resolved.iter().map(|(s, _)| s.model_id.as_str()).collect();
        let handles = assign_handles(&model_ids);
        let roster: Vec<(String, ProviderId, String)> = handles
            .iter()
            .zip(resolved.iter())
            .map(|(h, (spec, _))| (h.clone(), spec.provider_id, spec.model_id.clone()))
            .collect();

        let mut sessions = Vec::with_capacity(resolved.len());
        let mut team_members = Vec::with_capacity(resolved.len());
        for ((spec, _), handle) in resolved.iter().zip(handles.iter()) {
            let sid = SessionId::new();
            sessions.push(Session {
                id: sid,
                workspace_id,
                provider_id: spec.provider_id,
                provider_session_id: None,
                origin: SessionOrigin::Xade,
                title: handle.clone(),
                preview_snippet: String::new(),
                model_id: spec.model_id.clone(),
                effort: spec.effort,
                sent_model_id: String::new(),
                sent_effort: spec.effort,
                auto_mode: spec.auto_mode,
                usage: UsageTotals::default(),
                created_at: now,
                updated_at: now,
                multi_session_id: Some(multi_id),
                agent_handle: Some(handle.clone()),
            });
            team_members.push(TeamMember {
                session_id: sid,
                handle: handle.clone(),
                provider_id: spec.provider_id,
                status: SessionStatus::Idle,
                dispatch_seq: 0,
                interrupt_epoch: 0,
                suppress_broadcasts: false,
                queued_user: VecDeque::new(),
                queued_broadcasts: VecDeque::new(),
            });
        }

        self.write(WriteOp::TouchWorkspace(workspace_id));
        if let Err(e) = self.writer.send(WriteOp::InsertMultiSession {
            multi: multi.clone(),
            members: sessions.clone(),
        }) {
            return self.emit(
                None,
                AppEventKind::Error(format!("Could not create the multi-session: {e}")),
            );
        }
        flush(&self.writer).await;
        // Confirm SQLite committed the transaction (StoreWriter logs and drops failed SQL ops).
        if let Err(e) = self.reader.load_multi_session(multi_id).await {
            return self.emit(
                None,
                AppEventKind::Error(format!("Could not save the multi-session: {e}")),
            );
        }

        let generation = self.next_team_gen;
        self.next_team_gen += 1;

        for ((session, (_, provider)), handle) in sessions.iter().zip(resolved).zip(handles.iter())
        {
            self.member_to_team.insert(session.id, multi_id);
            self.dormant.insert(
                session.id,
                Dormant {
                    session: session.clone(),
                    cwd: cwd.clone(),
                    provider,
                    next_seq: 1,
                    handoff: Some(team_preamble(handle, &roster)),
                    team: Some((self.team_tx.clone(), generation)),
                },
            );
        }
        self.teams.insert(
            multi_id,
            TeamState {
                multi: multi.clone(),
                generation,
                members: team_members,
                next_team_seq: 1,
                consecutive_hops: 0,
                broadcasts_paused: false,
            },
        );
        let member_ids: Vec<SessionId> = sessions.iter().map(|s| s.id).collect();
        self.emit(
            None,
            AppEventKind::MultiCreated {
                multi,
                members: sessions,
            },
        );
        for sid in member_ids {
            self.emit(Some(sid), AppEventKind::Status(SessionStatus::Idle));
        }
    }

    async fn open_multi(&mut self, multi_id: MultiSessionId) {
        if self.teams.contains_key(&multi_id) {
            return;
        }
        let fail = |this: &Self, message: String| {
            this.emit(None, AppEventKind::MultiOpenFailed { multi_id, message });
        };
        // Flush earlier writes and wait for any closing member actors from a recent CloseMulti
        // so `--resume` never overlaps with an exiting child and `provider_session_id`/`next_seq`
        // are up to date.
        flush(&self.writer).await;
        let pre = match self.reader.load_multi_session(multi_id).await {
            Ok(x) => x,
            Err(e) => return fail(self, e.to_string()),
        };
        let mut waited_closing = false;
        for (s, _) in &pre.members {
            if let Some(tx) = self.closing_members.remove(&s.id) {
                waited_closing = true;
                if timeout(Duration::from_secs(10), tx.closed()).await.is_err() {
                    self.closing_members.insert(s.id, tx);
                    return fail(
                        self,
                        "Could not open the multi-session: previous instance is still closing."
                            .into(),
                    );
                }
            }
        }
        let loaded = if waited_closing {
            flush(&self.writer).await;
            match self.reader.load_multi_session(multi_id).await {
                Ok(x) => x,
                Err(e) => return fail(self, e.to_string()),
            }
        } else {
            pre
        };
        let cwd = match self.reader.list_workspaces().await {
            Ok(ws) => ws
                .into_iter()
                .find(|w| w.id == loaded.multi.workspace_id)
                .map(|w| w.root_path),
            Err(e) => return fail(self, e.to_string()),
        };
        let Some(cwd) = cwd else {
            return fail(self, "workspace not found".into());
        };
        let next_team_seq = match self.reader.max_team_seq(multi_id).await {
            Ok(seq) => seq + 1,
            Err(e) => return fail(self, format!("Could not open the multi-session: {e}")),
        };
        let roster: Vec<(String, ProviderId, String)> = loaded
            .members
            .iter()
            .map(|(s, _)| {
                (
                    s.agent_handle.clone().unwrap_or_default(),
                    s.provider_id,
                    s.model_id.clone(),
                )
            })
            .collect();

        let generation = self.next_team_gen;
        self.next_team_gen += 1;

        let mut team_members = Vec::with_capacity(loaded.members.len());
        let mut status_events = Vec::with_capacity(loaded.members.len());
        for (session, msgs) in &loaded.members {
            let handle = session.agent_handle.clone().unwrap_or_default();
            let next_seq = msgs.last().map_or(1, |m| m.seq + 1);
            let handoff = if session.provider_session_id.is_none() {
                let preamble = team_preamble(&handle, &roster);
                let history = transcript(msgs);
                if history.is_empty() {
                    Some(preamble)
                } else {
                    Some(format!("{preamble}\n\n{history}"))
                }
            } else {
                None
            };
            self.member_to_team.insert(session.id, multi_id);
            let status = match self.provider(session.provider_id) {
                Some(provider) => {
                    self.dormant.insert(
                        session.id,
                        Dormant {
                            session: session.clone(),
                            cwd: cwd.clone(),
                            provider,
                            next_seq,
                            handoff,
                            team: Some((self.team_tx.clone(), generation)),
                        },
                    );
                    SessionStatus::Idle
                }
                None => SessionStatus::Error("unknown provider".into()),
            };
            status_events.push((session.id, status.clone()));
            team_members.push(TeamMember {
                session_id: session.id,
                handle,
                provider_id: session.provider_id,
                status,
                dispatch_seq: 0,
                interrupt_epoch: 0,
                suppress_broadcasts: false,
                queued_user: VecDeque::new(),
                queued_broadcasts: VecDeque::new(),
            });
        }
        self.write(WriteOp::TouchWorkspace(loaded.multi.workspace_id));
        self.teams.insert(
            multi_id,
            TeamState {
                multi: loaded.multi.clone(),
                generation,
                members: team_members,
                next_team_seq,
                consecutive_hops: 0,
                broadcasts_paused: false,
            },
        );
        self.emit(
            None,
            AppEventKind::MultiOpened {
                multi: loaded.multi,
                members: loaded.members,
                team_messages: loaded.team_messages,
            },
        );
        for (sid, status) in status_events {
            self.emit(Some(sid), AppEventKind::Status(status));
        }
    }

    fn send_multi(
        &mut self,
        multi_id: MultiSessionId,
        text: String,
        steer: bool,
        context: Option<String>,
    ) {
        let (body, target, targets, renamed, tm, unpaused) = {
            let Some(team) = self.teams.get_mut(&multi_id) else {
                return self.emit(
                    None,
                    AppEventKind::Error("multi-session is not open".into()),
                );
            };
            let handles = team.handles();
            let (target, body) = parse_user_target(&text, &handles);
            if body.is_empty() {
                return;
            }
            let body = body.to_string();

            let renamed = if team.multi.title.is_empty() {
                let derived = Session::derive_title(&body);
                if !derived.is_empty() {
                    team.multi.title = derived.clone();
                    Some(derived)
                } else {
                    None
                }
            } else {
                None
            };

            let tm = TeamMessage {
                id: Uuid::new_v4(),
                multi_session_id: multi_id,
                seq: team.next_team_seq,
                kind: TeamMessageKind::User {
                    target: target.clone(),
                },
                content_text: body.clone(),
                created_at: OffsetDateTime::now_utc(),
            };
            team.next_team_seq += 1;

            // Fresh user input resets the agent-to-agent hop counter.
            team.consecutive_hops = 0;
            let unpaused = std::mem::replace(&mut team.broadcasts_paused, false);

            let targets: Vec<SessionId> = team
                .members
                .iter()
                .filter(|m| target.as_deref().is_none_or(|t| t == m.handle))
                .map(|m| m.session_id)
                .collect();
            (body, target, targets, renamed, tm, unpaused)
        };

        if let Some(title) = renamed {
            self.write(WriteOp::UpdateMultiSessionTitle {
                id: multi_id,
                title: title.clone(),
            });
            self.emit(None, AppEventKind::MultiRenamed { multi_id, title });
        }
        self.write(WriteOp::InsertTeamMessage(tm.clone()));
        self.emit(
            None,
            AppEventKind::MultiTeamMessage {
                multi_id,
                message: tm,
            },
        );
        if unpaused {
            self.emit(
                None,
                AppEventKind::MultiBroadcastPaused {
                    multi_id,
                    paused: false,
                    hops: 0,
                },
            );
        }

        let mut delivered_any = false;
        for sid in &targets {
            let action = {
                let Some(team) = self.teams.get_mut(&multi_id) else {
                    break;
                };
                let Some(member) = team.member_mut(*sid) else {
                    continue;
                };
                match &member.status {
                    SessionStatus::Idle => {
                        // Optimistically mark Streaming and bump dispatch_seq so broadcasts queue
                        // and any stale Idle from an earlier turn is ignored.
                        member.status = SessionStatus::Streaming;
                        member.dispatch_seq += 1;
                        member.suppress_broadcasts = false;
                        delivered_any = true;
                        Some((None, member.dispatch_seq, member.interrupt_epoch))
                    }
                    SessionStatus::Streaming if steer => {
                        member.dispatch_seq += 1;
                        member.suppress_broadcasts = false;
                        delivered_any = true;
                        Some((
                            Some(Delivery::Steered),
                            member.dispatch_seq,
                            member.interrupt_epoch,
                        ))
                    }
                    SessionStatus::Starting
                    | SessionStatus::Streaming
                    | SessionStatus::NeedsApproval => {
                        member.queued_user.push_back(PendingUserMsg {
                            text: body.clone(),
                            context: context.clone(),
                        });
                        delivered_any = true;
                        None
                    }
                    SessionStatus::Error(_) | SessionStatus::Closed => None,
                }
            };
            if let Some((delivery, dispatch_seq, epoch)) = action {
                self.forward(
                    *sid,
                    ActorCmd::SendTeamUser {
                        text: body.clone(),
                        delivery,
                        context: context.clone(),
                        dispatch_seq,
                        epoch,
                    },
                );
            }
        }
        if !delivered_any {
            let err_sid = (targets.len() == 1).then(|| targets[0]);
            self.emit(
                err_sid,
                AppEventKind::Error(if target.is_some() {
                    "member session is not running".into()
                } else {
                    "no active team members available".into()
                }),
            );
        }

        // If this user turn unpaused broadcasts, drain any non-target Idle members that still
        // hold queued broadcasts so nothing is left stranded.
        if unpaused {
            let stranded: Vec<SessionId> = self
                .teams
                .get(&multi_id)
                .map(|t| {
                    t.members
                        .iter()
                        .filter(|m| {
                            m.status == SessionStatus::Idle && !m.queued_broadcasts.is_empty()
                        })
                        .map(|m| m.session_id)
                        .collect()
                })
                .unwrap_or_default();
            self.drain_wave(multi_id, &stranded);
        }
    }

    fn on_member_signal(&mut self, sig: MemberSignal) {
        match sig {
            MemberSignal::Status {
                multi_id,
                generation,
                session_id,
                dispatch_seq,
                status,
            } => {
                let should_drain = {
                    let Some(team) = self.teams.get_mut(&multi_id) else {
                        return;
                    };
                    if team.generation != generation {
                        return;
                    }
                    let Some(member) = team.member_mut(session_id) else {
                        return;
                    };
                    // Ignore a stale `Idle` from an earlier turn when a newer prompt/steer is
                    // already in flight to this actor.
                    if status == SessionStatus::Idle && dispatch_seq < member.dispatch_seq {
                        return;
                    }
                    member.status = status.clone();
                    if status == SessionStatus::Idle {
                        member.suppress_broadcasts = false;
                        true
                    } else {
                        if matches!(status, SessionStatus::Error(_) | SessionStatus::Closed) {
                            self.live.remove(&session_id);
                        }
                        false
                    }
                };
                if should_drain {
                    self.drain_member(multi_id, session_id);
                }
            }
            MemberSignal::AssistantText {
                multi_id,
                generation,
                session_id,
                text,
            } => {
                let broadcasts = extract_broadcasts(&text);
                if broadcasts.is_empty() {
                    return;
                }
                let Some((sender_handle, sender_provider)) = self
                    .teams
                    .get(&multi_id)
                    .filter(|t| t.generation == generation)
                    .and_then(|t| t.member(session_id))
                    .filter(|m| !m.suppress_broadcasts)
                    .map(|m| (m.handle.clone(), m.provider_id))
                else {
                    return;
                };

                let mut to_emit = Vec::with_capacity(broadcasts.len());
                let mut recipients = Vec::new();
                let mut hit_cap = None;
                if let Some(team) = self.teams.get_mut(&multi_id) {
                    for content in &broadcasts {
                        let tm = TeamMessage {
                            id: Uuid::new_v4(),
                            multi_session_id: multi_id,
                            seq: team.next_team_seq,
                            kind: TeamMessageKind::Broadcast {
                                sender_handle: sender_handle.clone(),
                                sender_provider,
                            },
                            content_text: content.clone(),
                            created_at: OffsetDateTime::now_utc(),
                        };
                        team.next_team_seq += 1;
                        to_emit.push(tm);
                    }
                    let mut queued_any = false;
                    for m in &mut team.members {
                        if m.session_id == session_id
                            || matches!(m.status, SessionStatus::Error(_) | SessionStatus::Closed)
                        {
                            continue;
                        }
                        queued_any = true;
                        for content in &broadcasts {
                            m.queued_broadcasts.push_back(PendingBroadcast {
                                sender_handle: sender_handle.clone(),
                                content: content.clone(),
                            });
                        }
                        if m.status == SessionStatus::Idle {
                            recipients.push(m.session_id);
                        }
                    }
                    if queued_any && !team.broadcasts_paused {
                        if team.consecutive_hops >= MAX_BROADCAST_HOPS {
                            team.broadcasts_paused = true;
                            hit_cap = Some(team.consecutive_hops);
                        } else {
                            team.consecutive_hops += 1;
                        }
                    }
                }
                for tm in to_emit {
                    self.write(WriteOp::InsertTeamMessage(tm.clone()));
                    self.emit(
                        None,
                        AppEventKind::MultiTeamMessage {
                            multi_id,
                            message: tm,
                        },
                    );
                }
                if let Some(hops) = hit_cap {
                    self.emit(
                        None,
                        AppEventKind::MultiBroadcastPaused {
                            multi_id,
                            paused: true,
                            hops,
                        },
                    );
                    return;
                }
                if self
                    .teams
                    .get(&multi_id)
                    .is_some_and(|t| !t.broadcasts_paused)
                {
                    self.drain_wave(multi_id, &recipients);
                }
            }
            MemberSignal::BroadcastDeferred {
                multi_id,
                generation,
                session_id,
                epoch,
                batch,
            } => {
                if let Some(team) = self.teams.get_mut(&multi_id) {
                    if team.generation != generation {
                        return;
                    }
                    if let Some(member) = team.member_mut(session_id) {
                        if epoch < member.interrupt_epoch {
                            return;
                        }
                        for item in batch.into_iter().rev() {
                            member.queued_broadcasts.push_front(item);
                        }
                    }
                }
            }
            MemberSignal::UserDeferred {
                multi_id,
                generation,
                session_id,
                epoch,
                msg,
            } => {
                if let Some(team) = self.teams.get_mut(&multi_id) {
                    if team.generation != generation {
                        return;
                    }
                    if let Some(member) = team.member_mut(session_id) {
                        if epoch < member.interrupt_epoch {
                            return;
                        }
                        member.queued_user.push_front(msg);
                    }
                }
            }
        }
    }

    /// Drains a wave of `Idle` recipients, marking them `Streaming` before snapshotting the team
    /// status header so teammates in the same wave see accurate statuses.
    fn drain_wave(&mut self, multi_id: MultiSessionId, session_ids: &[SessionId]) {
        enum Next {
            User {
                sid: SessionId,
                msg: PendingUserMsg,
                dispatch_seq: u64,
                epoch: u64,
            },
            Broadcast {
                sid: SessionId,
                batch: Vec<PendingBroadcast>,
                dispatch_seq: u64,
                epoch: u64,
            },
        }
        let (planned, status_summary) = {
            let Some(team) = self.teams.get_mut(&multi_id) else {
                return;
            };
            let paused = team.broadcasts_paused;
            let mut planned = Vec::new();
            for &sid in session_ids {
                let Some(member) = team.member_mut(sid) else {
                    continue;
                };
                if member.status != SessionStatus::Idle {
                    continue;
                }
                if let Some(msg) = member.queued_user.pop_front() {
                    member.status = SessionStatus::Streaming;
                    member.dispatch_seq += 1;
                    member.suppress_broadcasts = false;
                    planned.push(Next::User {
                        sid,
                        msg,
                        dispatch_seq: member.dispatch_seq,
                        epoch: member.interrupt_epoch,
                    });
                } else if !paused && !member.queued_broadcasts.is_empty() {
                    let batch: Vec<PendingBroadcast> = member.queued_broadcasts.drain(..).collect();
                    member.status = SessionStatus::Streaming;
                    member.dispatch_seq += 1;
                    member.suppress_broadcasts = false;
                    planned.push(Next::Broadcast {
                        sid,
                        batch,
                        dispatch_seq: member.dispatch_seq,
                        epoch: member.interrupt_epoch,
                    });
                }
            }
            (planned, team.status_summary())
        };
        for item in planned {
            match item {
                Next::User {
                    sid,
                    msg,
                    dispatch_seq,
                    epoch,
                } => {
                    self.forward(
                        sid,
                        ActorCmd::SendTeamUser {
                            text: msg.text,
                            delivery: Some(Delivery::Queued),
                            context: msg.context,
                            dispatch_seq,
                            epoch,
                        },
                    );
                }
                Next::Broadcast {
                    sid,
                    batch,
                    dispatch_seq,
                    epoch,
                } => {
                    self.forward(
                        sid,
                        ActorCmd::SendBroadcast {
                            batch,
                            status_summary: status_summary.clone(),
                            dispatch_seq,
                            epoch,
                        },
                    );
                }
            }
        }
    }

    fn drain_member(&mut self, multi_id: MultiSessionId, session_id: SessionId) {
        self.drain_wave(multi_id, &[session_id]);
    }

    fn interrupt_multi_all(&mut self, multi_id: MultiSessionId) {
        let Some(team) = self.teams.get_mut(&multi_id) else {
            return;
        };
        let mut to_interrupt = Vec::new();
        for m in &mut team.members {
            m.queued_user.clear();
            m.queued_broadcasts.clear();
            m.interrupt_epoch += 1;
            let busy = matches!(
                m.status,
                SessionStatus::Streaming | SessionStatus::NeedsApproval
            );
            m.suppress_broadcasts = busy;
            if busy {
                to_interrupt.push(m.session_id);
            }
        }
        team.consecutive_hops = 0;
        let was_paused = std::mem::replace(&mut team.broadcasts_paused, false);
        if was_paused {
            self.emit(
                None,
                AppEventKind::MultiBroadcastPaused {
                    multi_id,
                    paused: false,
                    hops: 0,
                },
            );
        }
        for sid in to_interrupt {
            if self.live.get(&sid).is_some_and(|tx| !tx.is_closed()) {
                self.forward(sid, ActorCmd::Interrupt);
            } else {
                self.live.remove(&sid);
            }
        }
    }

    fn resume_multi_broadcasts(&mut self, multi_id: MultiSessionId) {
        let (was_paused, idle_members): (bool, Vec<SessionId>) = {
            let Some(team) = self.teams.get_mut(&multi_id) else {
                return;
            };
            team.consecutive_hops = 0;
            let was_paused = std::mem::replace(&mut team.broadcasts_paused, false);
            let idle = team
                .members
                .iter()
                .filter(|m| m.status == SessionStatus::Idle && !m.queued_broadcasts.is_empty())
                .map(|m| m.session_id)
                .collect();
            (was_paused, idle)
        };
        if was_paused {
            self.emit(
                None,
                AppEventKind::MultiBroadcastPaused {
                    multi_id,
                    paused: false,
                    hops: 0,
                },
            );
        }
        self.drain_wave(multi_id, &idle_members);
    }

    fn rename_multi(&mut self, multi_id: MultiSessionId, title: String) {
        let title = title.trim().to_string();
        if title.is_empty() {
            return;
        }
        if let Some(team) = self.teams.get_mut(&multi_id) {
            team.multi.title.clone_from(&title);
        }
        self.write(WriteOp::UpdateMultiSessionTitle {
            id: multi_id,
            title: title.clone(),
        });
        self.emit(None, AppEventKind::MultiRenamed { multi_id, title });
    }

    fn close_multi(&mut self, multi_id: MultiSessionId) {
        let Some(team) = self.teams.remove(&multi_id) else {
            return;
        };
        for m in team.members {
            self.member_to_team.remove(&m.session_id);
            if self.dormant.remove(&m.session_id).is_some() {
                self.emit(
                    Some(m.session_id),
                    AppEventKind::Status(SessionStatus::Closed),
                );
            }
            if let Some(tx) = self.live.remove(&m.session_id) {
                if !tx.is_closed() {
                    let _ = tx.send(ActorCmd::Close);
                    self.closing_members.insert(m.session_id, tx);
                }
            }
        }
        self.emit(None, AppEventKind::MultiClosed { multi_id });
    }

    async fn delete_multi(&mut self, multi_id: MultiSessionId) {
        let fail = |this: &Self, msg: String| this.emit(None, AppEventKind::Error(msg));
        let loaded = match self.reader.load_multi_session(multi_id).await {
            Ok(x) => x,
            Err(e) => return fail(self, format!("Could not delete the multi-session: {e}")),
        };
        let member_ids: Vec<SessionId> = loaded.members.iter().map(|(s, _)| s.id).collect();
        // Close the open team cleanly first so UI & manager maps stay consistent even if a
        // subsequent transcript delete fails.
        if self.teams.contains_key(&multi_id) {
            self.close_multi(multi_id);
        }
        let mut waited_live = false;
        for &sid in &member_ids {
            if let Some(tx) = self.closing_members.remove(&sid) {
                waited_live = true;
                if timeout(Duration::from_secs(10), tx.closed()).await.is_err() {
                    self.closing_members.insert(sid, tx);
                    return fail(
                        self,
                        "Could not delete the multi-session: a member did not stop in time.".into(),
                    );
                }
            }
        }
        let members = if waited_live {
            flush(&self.writer).await;
            match self.reader.load_multi_session(multi_id).await {
                Ok(x) => x.members,
                Err(e) => return fail(self, format!("Could not delete the multi-session: {e}")),
            }
        } else {
            loaded.members
        };
        for (session, _) in &members {
            if let (Some(psid), Some(provider)) = (
                session.provider_session_id.clone(),
                self.provider(session.provider_id),
            ) {
                let done =
                    tokio::task::spawn_blocking(move || provider.delete_session(&psid)).await;
                match done {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        return fail(self, format!("Could not delete a member transcript: {e}"))
                    }
                    Err(e) => {
                        return fail(self, format!("Could not delete a member transcript: {e}"))
                    }
                }
            }
        }
        if let Err(e) = self.writer.send(WriteOp::DeleteMultiSession(multi_id)) {
            return fail(self, format!("Could not delete the multi-session: {e}"));
        }
        flush(&self.writer).await;
        self.emit(None, AppEventKind::MultiDeleted { multi_id });
    }
}

/// Deletes the provider conversations earlier runs' snapshot turns left, then their markers. A
/// marker stays when its provider is missing or the delete fails.
async fn sweep(
    providers: Vec<Arc<dyn AgentProvider>>,
    leftovers: Vec<handoff::Leftover>,
    throwaways: Throwaways,
) {
    for l in leftovers {
        let Some(provider) = l
            .provider
            .and_then(|id| providers.iter().find(|p| p.id() == id).cloned())
        else {
            tracing::warn!("a leftover snapshot chat has no provider; kept for later");
            continue;
        };
        let psid = l.psid.clone();
        match tokio::task::spawn_blocking(move || provider.delete_session(&psid)).await {
            Ok(Ok(())) => {
                if let Err(e) = std::fs::remove_file(&l.marker) {
                    tracing::warn!(error = %e, "could not remove a throwaway marker");
                }
                throwaways
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&l.psid);
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "could not delete a leftover snapshot chat"),
            Err(e) => tracing::warn!(error = %e, "leftover snapshot chat task failed"),
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
    team_panic: Option<(MultiSessionId, u64, UnboundedSender<MemberSignal>)>,
) {
    let Err(e) = task.await else { return };
    if !e.is_panic() {
        return;
    }
    let msg = panic_message(&*e.into_panic());
    tracing::error!(what, %msg, "task panicked");
    if let Some(sid) = session_id {
        let status = SessionStatus::Error(format!("session crashed: {msg}"));
        let _ = events.send(AppEvent {
            session_id: Some(sid),
            kind: AppEventKind::Status(status.clone()),
        });
        if let Some((multi_id, generation, tx)) = team_panic {
            let _ = tx.send(MemberSignal::Status {
                multi_id,
                generation,
                session_id: sid,
                dispatch_seq: u64::MAX,
                status,
            });
        }
    }
    let kind = AppEventKind::Error(format!("{what} crashed: {msg}"));
    let _ = events.send(AppEvent {
        session_id: None,
        kind,
    });
}

/// The store keeps microseconds. A finer clock would make a stored row look older than the import
/// it came from, so every import would update it again, on some platforms only.
fn store_precision(t: OffsetDateTime) -> OffsetDateTime {
    t.replace_nanosecond(t.nanosecond() / 1000 * 1000)
        .unwrap_or(t)
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
            provider_id: None,
            sender_handle: None,
        })
        .collect();
    let origin = match provider_id {
        ProviderId::Codex => SessionOrigin::Codex,
        // Neither imports; the origin is only ever read for an imported session.
        ProviderId::Claude | ProviderId::Antigravity | ProviderId::OpenCode => {
            SessionOrigin::ClaudeCode
        }
    };
    let session = Session {
        id,
        workspace_id: WorkspaceId::new(),
        provider_id,
        provider_session_id: Some(imp.provider_session_id),
        origin,
        title: imp.title,
        preview_snippet: String::new(),
        model_id: if imp.model_id.is_empty() {
            default_model.to_string()
        } else {
            imp.model_id
        },
        effort: Effort::Default,
        sent_model_id: String::new(),
        sent_effort: Effort::Default,
        auto_mode: false,
        usage: imp.usage,
        created_at: imp.created_at,
        updated_at: imp.updated_at,
        multi_session_id: None,
        agent_handle: None,
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
        supervise(task, "A session", Some(id), tx.clone(), None).await;
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
        supervise(tokio::spawn(async {}), "A session", Some(id), tx, None).await;
        assert!(rx.try_recv().is_err());
    }
}
