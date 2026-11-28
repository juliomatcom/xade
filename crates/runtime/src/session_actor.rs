//! One task per live session: the only place provider events become writes and `AppEvent`s.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::timeout;
use uuid::Uuid;
use xade_core::{
    AgentProvider, Delivery, Effort, Message, ProviderEvent, Role, Session, SessionCommand,
    SessionHandle, SessionId, SessionStatus, SpawnConfig,
};
use xade_store::{WriteOp, WriterHandle};

use crate::multi::{format_broadcast_prompt, MemberSignal, PendingBroadcast, PendingUserMsg};
use crate::{AppEvent, AppEventKind};

pub(crate) enum ActorCmd {
    /// Text, delivery, and context the provider gets in front of the text.
    Send(String, Option<Delivery>, Option<String>),
    /// Team user prompt tagged with manager dispatch sequence and interrupt epoch so races with
    /// `NeedsApproval` or stale `Idle` signals never drop or reorder prompts.
    SendTeamUser {
        text: String,
        delivery: Option<Delivery>,
        context: Option<String>,
        dispatch_seq: u64,
        epoch: u64,
    },
    /// One or more coalesced team broadcasts. Never steers: deferred back if the actor is not Idle.
    SendBroadcast {
        batch: Vec<PendingBroadcast>,
        status_summary: String,
        dispatch_seq: u64,
        epoch: u64,
    },
    Interrupt,
    Respond {
        request_id: String,
        allow: bool,
        enable_auto: bool,
    },
    SetAutoMode(bool),
    SetModel(String),
    SetEffort(Effort),
    Rename(String),
    Close,
}

pub(crate) struct Actor {
    session: Session,
    cwd: PathBuf,
    provider: Arc<dyn AgentProvider>,
    writer: WriterHandle,
    out: UnboundedSender<AppEvent>,
    handle: SessionHandle,
    status: SessionStatus,
    next_seq: i64,
    pending_model: Option<String>,
    pending_effort: Option<Effort>,
    /// Auto changed on a provider that needs a new process for it.
    pending_auto: bool,
    last_error: Option<String>,
    /// Transcript of a forked history, prepended to the first prompt only.
    handoff: Option<String>,
    /// The hand-off sent and not yet seen processed (no assistant output since). Put back when
    /// the turn ends first (stopped during the handshake, errored), so it is not lost.
    handoff_sent: Option<String>,
    /// A new session has no row until its first message, so one nobody wrote in leaves nothing.
    /// Until then `UpdateSession` writes match no row and `session` carries every change.
    unstored: bool,
    /// The row was just inserted and `SessionStored` is still to be sent.
    announce_stored: bool,
    /// When this session belongs to a multi-session, signals status and assistant text back to
    /// `SessionManager` for broadcast routing and queue draining.
    team: Option<(UnboundedSender<MemberSignal>, u64)>,
    /// Latest team dispatch sequence received by this actor (echoed on `Status` signals).
    dispatch_seq: u64,
}

impl Actor {
    pub(crate) fn new(
        session: Session,
        cwd: PathBuf,
        provider: Arc<dyn AgentProvider>,
        writer: WriterHandle,
        out: UnboundedSender<AppEvent>,
        handle: SessionHandle,
        next_seq: i64,
    ) -> Self {
        Self {
            session,
            cwd,
            provider,
            writer,
            out,
            handle,
            status: SessionStatus::Idle,
            next_seq,
            pending_model: None,
            pending_effort: None,
            pending_auto: false,
            last_error: None,
            handoff: None,
            handoff_sent: None,
            unstored: false,
            announce_stored: false,
            team: None,
            dispatch_seq: 0,
        }
    }

    /// A session that is not in the store yet.
    pub(crate) fn unstored(mut self) -> Self {
        self.unstored = true;
        self
    }

    /// Text the provider gets in front of the first prompt (a fork's history or team preamble).
    pub(crate) fn with_handoff(mut self, handoff: Option<String>) -> Self {
        self.handoff = handoff;
        self
    }

    /// Attaches the multi-session coordinator channel and open generation.
    pub(crate) fn with_team(mut self, team: Option<(UnboundedSender<MemberSignal>, u64)>) -> Self {
        self.team = team;
        self
    }

    pub(crate) fn id(&self) -> SessionId {
        self.session.id
    }

    pub(crate) fn team_meta(&self) -> Option<(xade_core::MultiSessionId, u64)> {
        let mid = self.session.multi_session_id?;
        let (_, generation) = self.team.as_ref()?;
        Some((mid, *generation))
    }

    pub(crate) async fn run(mut self, mut cmds: UnboundedReceiver<ActorCmd>) {
        loop {
            let keep_going = tokio::select! {
                cmd = cmds.recv() => match cmd {
                    Some(c) => self.on_cmd(c).await,
                    None => { self.shutdown_child().await; false }
                },
                ev = self.handle.events.recv() => match ev {
                    Some(ev) => self.on_event(ev).await,
                    None => {
                        self.set_status(SessionStatus::Error("provider stopped".into()));
                        false
                    }
                },
            };
            if std::mem::take(&mut self.announce_stored) {
                // The UI reads the store as soon as it hears of the row.
                crate::flush(&self.writer).await;
                self.emit(AppEventKind::SessionStored);
            }
            if !keep_going {
                break;
            }
        }
    }

    fn emit(&self, kind: AppEventKind) {
        let _ = self.out.send(AppEvent {
            session_id: Some(self.session.id),
            kind,
        });
    }

    fn set_status(&mut self, status: SessionStatus) {
        self.status = status.clone();
        self.emit(AppEventKind::Status(status.clone()));
        if let (Some((tx, generation)), Some(multi_id)) =
            (&self.team, self.session.multi_session_id)
        {
            let _ = tx.send(MemberSignal::Status {
                multi_id,
                generation: *generation,
                session_id: self.session.id,
                dispatch_seq: self.dispatch_seq,
                status,
            });
        }
    }

    fn write(&self, op: WriteOp) {
        if let Err(e) = self.writer.send(op) {
            tracing::error!(error = %e, "store write dropped");
        }
    }

    fn update(&self) -> WriteOp {
        WriteOp::UpdateSession {
            id: self.session.id,
            title: None,
            provider_session_id: None,
            model_id: None,
            effort: None,
            auto_mode: None,
            preview_snippet: None,
            usage: None,
        }
    }

    fn insert_message(&mut self, role: Role, text: String) -> Message {
        let m = Message {
            id: Uuid::new_v4(),
            session_id: self.session.id,
            seq: self.next_seq,
            role,
            content_text: text,
            thinking_text: None,
            tool_name: None,
            tool_use_id: None,
            tool_payload_json: None,
            is_error: false,
            created_at: OffsetDateTime::now_utc(),
            provider_id: None,
            sender_handle: None,
        };
        self.next_seq += 1;
        m
    }

    /// Stores a message, after the session's row when this is its first.
    fn store_message(&mut self, m: Message) {
        if std::mem::take(&mut self.unstored) {
            self.write(WriteOp::InsertSession(self.session.clone()));
            self.announce_stored = true;
        }
        self.write(WriteOp::InsertMessage(m));
    }

    fn command(&self, cmd: SessionCommand) {
        if self.handle.commands.send(cmd).is_err() {
            self.emit(AppEventKind::Error("provider is not running".into()));
        }
    }

    /// Returns false when the actor should end.
    async fn on_cmd(&mut self, cmd: ActorCmd) -> bool {
        match cmd {
            ActorCmd::Send(text, delivery, context) => {
                // A prompt sent mid-turn steers the running turn: the CLI folds it in at the
                // next tool boundary and ends both with one result.
                if !matches!(self.status, SessionStatus::Idle | SessionStatus::Streaming) {
                    self.emit(AppEventKind::Error("session is busy".into()));
                    return true;
                }
                self.dispatch_user_send(text, delivery, context);
            }
            ActorCmd::SendTeamUser {
                text,
                delivery,
                context,
                dispatch_seq,
                epoch,
            } => {
                self.dispatch_seq = dispatch_seq;
                if !matches!(self.status, SessionStatus::Idle | SessionStatus::Streaming) {
                    if self.status == SessionStatus::NeedsApproval {
                        if let (Some((tx, generation)), Some(multi_id)) =
                            (&self.team, self.session.multi_session_id)
                        {
                            let _ = tx.send(MemberSignal::UserDeferred {
                                multi_id,
                                generation: *generation,
                                session_id: self.session.id,
                                epoch,
                                msg: PendingUserMsg { text, context },
                            });
                        }
                    } else {
                        self.emit(AppEventKind::Error("session is busy".into()));
                    }
                    return true;
                }
                self.dispatch_user_send(text, delivery, context);
            }
            ActorCmd::SendBroadcast {
                batch,
                status_summary,
                dispatch_seq,
                epoch,
            } => {
                self.dispatch_seq = dispatch_seq;
                // Broadcasts strictly queue and never steer: if a turn started before this arrived,
                // hand the batch back to the manager to re-queue at the front.
                if self.status != SessionStatus::Idle {
                    if let (Some((tx, generation)), Some(multi_id)) =
                        (&self.team, self.session.multi_session_id)
                    {
                        let _ = tx.send(MemberSignal::BroadcastDeferred {
                            multi_id,
                            generation: *generation,
                            session_id: self.session.id,
                            epoch,
                            batch,
                        });
                    }
                    return true;
                }
                let (sender_handle, display_text, prompt_text) =
                    format_broadcast_prompt(&batch, &status_summary);
                let mut msg = self.insert_message(Role::User, display_text);
                msg.sender_handle = Some(sender_handle);
                self.emit(AppEventKind::MemberMessage(msg.clone()));
                self.store_message(msg);
                self.mark_sent_if_changed();
                self.set_status(SessionStatus::Streaming);
                let prompt = self.build_prompt(None, prompt_text);
                self.command(SessionCommand::SendPrompt(prompt));
            }
            ActorCmd::Interrupt => self.command(SessionCommand::Interrupt),
            ActorCmd::Respond {
                request_id,
                allow,
                enable_auto,
            } => {
                if allow && enable_auto {
                    self.set_auto(true);
                }
                self.command(SessionCommand::RespondPermission { request_id, allow });
                if self.status == SessionStatus::NeedsApproval {
                    self.set_status(SessionStatus::Streaming);
                }
                return self.apply_pending().await;
            }
            ActorCmd::SetAutoMode(on) => {
                self.set_auto(on);
                return self.apply_pending().await;
            }
            ActorCmd::SetModel(m) => {
                if self.session.multi_session_id.is_some() {
                    return true;
                }
                // Stored at once: the respawn may wait for the turn to end, and closing the
                // window before then must not lose the choice.
                let mut op = self.update();
                if let WriteOp::UpdateSession { model_id, .. } = &mut op {
                    *model_id = Some(m.clone());
                }
                self.write(op);
                self.pending_model = Some(m);
                return self.apply_pending().await;
            }
            ActorCmd::SetEffort(e) => {
                let mut op = self.update();
                if let WriteOp::UpdateSession { effort, .. } = &mut op {
                    *effort = Some(e);
                }
                self.write(op);
                self.pending_effort = Some(e);
                return self.apply_pending().await;
            }
            ActorCmd::Rename(title) => {
                // Also keeps the first send from deriving a title over the chosen one.
                self.session.title.clone_from(&title);
                let mut op = self.update();
                if let WriteOp::UpdateSession { title: t, .. } = &mut op {
                    *t = Some(title);
                }
                self.write(op);
            }
            ActorCmd::Close => {
                self.shutdown_child().await;
                self.set_status(SessionStatus::Closed);
                return false;
            }
        }
        true
    }

    fn dispatch_user_send(
        &mut self,
        text: String,
        delivery: Option<Delivery>,
        context: Option<String>,
    ) {
        if self.session.title.is_empty() && self.session.multi_session_id.is_none() {
            let title = Session::derive_title(&text);
            if !title.is_empty() {
                self.session.title = title.clone();
                let mut op = self.update();
                if let WriteOp::UpdateSession { title: t, .. } = &mut op {
                    *t = Some(title);
                }
                self.write(op);
            }
        }
        let mut msg = self.insert_message(Role::User, text.clone());
        msg.tool_name = delivery.map(|d| d.tag().to_string());
        if self.session.multi_session_id.is_some() {
            self.emit(AppEventKind::MemberMessage(msg.clone()));
        }
        self.store_message(msg);
        self.mark_sent_if_changed();
        if self.status != SessionStatus::Streaming {
            self.set_status(SessionStatus::Streaming);
        }
        let prompt = self.build_prompt(context, text);
        self.command(SessionCommand::SendPrompt(prompt));
    }

    fn mark_sent_if_changed(&mut self) {
        if self.session.sent_model_id != self.session.model_id
            || self.session.sent_effort != self.session.effort
        {
            self.session
                .sent_model_id
                .clone_from(&self.session.model_id);
            self.session.sent_effort = self.session.effort;
            self.write(WriteOp::MarkSent {
                id: self.session.id,
                model_id: self.session.model_id.clone(),
                effort: self.session.effort,
            });
        }
    }

    fn build_prompt(&mut self, context: Option<String>, text: String) -> String {
        let handoff = self.handoff.take();
        if handoff.is_some() {
            self.handoff_sent.clone_from(&handoff);
        }
        match (handoff, context) {
            (None, None) => text,
            (handoff, context) => [handoff, context, Some(text)]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    fn set_auto(&mut self, on: bool) {
        if self.session.auto_mode != on && self.provider.respawn_on_auto_change() {
            self.pending_auto = true;
        }
        self.session.auto_mode = on;
        self.command(SessionCommand::SetAutoMode(on));
        let mut op = self.update();
        if let WriteOp::UpdateSession { auto_mode, .. } = &mut op {
            *auto_mode = Some(on);
        }
        self.write(op);
    }

    /// Returns false when the actor should end.
    async fn on_event(&mut self, ev: ProviderEvent) -> bool {
        // The CLI can start a turn on its own (a finished background task or subagent), with no
        // prompt from us: output while idle means the session is working.
        if self.status == SessionStatus::Idle
            && matches!(
                ev,
                ProviderEvent::TextDelta(_)
                    | ProviderEvent::ThinkingProgress { .. }
                    | ProviderEvent::AssistantText { .. }
                    | ProviderEvent::ToolUse { .. }
            )
        {
            self.set_status(SessionStatus::Streaming);
        }
        // The provider answering means it processed the prompt, hand-off included.
        if matches!(
            ev,
            ProviderEvent::TextDelta(_)
                | ProviderEvent::ThinkingProgress { .. }
                | ProviderEvent::AssistantText { .. }
                | ProviderEvent::ToolUse { .. }
        ) {
            self.handoff_sent = None;
        }
        match &ev {
            ProviderEvent::Quota(q) => {
                self.write(WriteOp::UpsertQuota(q.clone()));
                let _ = self.out.send(AppEvent {
                    session_id: None,
                    kind: AppEventKind::Quota(q.clone()),
                });
                return true;
            }
            ProviderEvent::Started {
                provider_session_id,
                ..
            } => {
                if self.session.provider_session_id.as_deref() != Some(provider_session_id) {
                    self.session.provider_session_id = Some(provider_session_id.clone());
                    let mut op = self.update();
                    if let WriteOp::UpdateSession {
                        provider_session_id: p,
                        ..
                    } = &mut op
                    {
                        *p = Some(provider_session_id.clone());
                    }
                    self.write(op);
                }
            }
            ProviderEvent::AssistantText { text } => {
                let m = self.insert_message(Role::Assistant, text.clone());
                self.store_message(m);
                if let (Some((tx, generation)), Some(multi_id)) =
                    (&self.team, self.session.multi_session_id)
                {
                    let _ = tx.send(MemberSignal::AssistantText {
                        multi_id,
                        generation: *generation,
                        session_id: self.session.id,
                        text: text.clone(),
                    });
                }
            }
            ProviderEvent::ToolUse {
                tool_use_id,
                name,
                input_json,
            } => {
                let mut m = self.insert_message(Role::ToolCall, String::new());
                m.tool_name = Some(name.clone());
                m.tool_use_id = Some(tool_use_id.clone());
                m.tool_payload_json = Some(input_json.clone());
                self.store_message(m);
            }
            ProviderEvent::ToolResult {
                tool_use_id,
                output,
                is_error,
                structured_json,
            } => {
                let mut m = self.insert_message(Role::ToolResult, output.clone());
                m.tool_use_id = Some(tool_use_id.clone());
                m.tool_payload_json = structured_json.clone();
                m.is_error = *is_error;
                self.store_message(m);
            }
            ProviderEvent::PermissionRequest { .. } => {
                self.emit(AppEventKind::Provider(ev));
                self.set_status(SessionStatus::NeedsApproval);
                return true;
            }
            ProviderEvent::TurnComplete { usage, .. } => {
                if let Some(h) = self.handoff_sent.take() {
                    // The turn ended without any output: the provider may never have seen it.
                    self.handoff = Some(h);
                }
                self.write(WriteOp::ApplyTurnUsage {
                    session_id: self.session.id,
                    turn: usage.clone(),
                });
                self.session.usage.accumulate(usage);
                self.emit(AppEventKind::Provider(ev));
                // Apply pending model/effort respawn BEFORE advertising `Idle`, so the manager
                // does not dispatch queued team work into a shutting-down child.
                self.status = SessionStatus::Idle;
                if !self.apply_pending().await {
                    return false;
                }
                self.set_status(SessionStatus::Idle);
                return true;
            }
            ProviderEvent::Error(msg) => {
                tracing::warn!(session = %self.session.id.0, %msg, "provider error");
                self.last_error = Some(msg.clone());
            }
            ProviderEvent::Exited { code } => {
                let msg = self
                    .last_error
                    .take()
                    .unwrap_or_else(|| format!("provider exited (code {code:?})"));
                self.emit(AppEventKind::Provider(ev));
                self.set_status(SessionStatus::Error(msg));
                return false;
            }
            ProviderEvent::TextDelta(_)
            | ProviderEvent::ThinkingProgress { .. }
            | ProviderEvent::Subagent { .. } => {}
        }
        self.emit(AppEventKind::Provider(ev));
        true
    }

    /// Respawns with `resume` when a model, effort or (where the provider needs it) Auto change
    /// is pending and the session is idle. Returns false when the new child could not be started.
    async fn apply_pending(&mut self) -> bool {
        let pending =
            self.pending_model.is_some() || self.pending_effort.is_some() || self.pending_auto;
        if self.status != SessionStatus::Idle || !pending {
            return true;
        }
        // The new child takes the Auto mode the session has now.
        self.pending_auto = false;
        let model_id = self
            .pending_model
            .take()
            .unwrap_or_else(|| self.session.model_id.clone());
        let effort = self.pending_effort.take().unwrap_or(self.session.effort);
        self.shutdown_child().await;
        self.session.model_id = model_id.clone();
        self.session.effort = effort;
        let mut op = self.update();
        if let WriteOp::UpdateSession {
            model_id: m,
            effort: e,
            ..
        } = &mut op
        {
            *m = Some(model_id.clone());
            *e = Some(effort);
        }
        self.write(op);
        let cfg = SpawnConfig {
            cwd: self.cwd.clone(),
            model_id,
            effort,
            auto_mode: self.session.auto_mode,
            resume: self.session.provider_session_id.clone(),
        };
        match self.provider.spawn(cfg) {
            Ok(h) => {
                self.handle = h;
                true
            }
            Err(e) => {
                self.set_status(SessionStatus::Error(e.to_string()));
                false
            }
        }
    }

    /// Asks the child to stop and waits (bounded) for its `Exited`.
    async fn shutdown_child(&mut self) {
        let _ = self.handle.commands.send(SessionCommand::Shutdown);
        let events = &mut self.handle.events;
        let _ = timeout(Duration::from_secs(8), async {
            while let Some(ev) = events.recv().await {
                if matches!(ev, ProviderEvent::Exited { .. }) {
                    break;
                }
            }
        })
        .await;
    }
}
