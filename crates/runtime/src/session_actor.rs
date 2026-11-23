//! One task per live session: the only place provider events become writes and `AppEvent`s.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::timeout;
use uuid::Uuid;
use xade_core::{
    AgentProvider, Effort, Message, ProviderEvent, Role, Session, SessionCommand, SessionHandle,
    SessionId, SessionStatus, SpawnConfig,
};
use xade_store::{WriteOp, WriterHandle};

use crate::{AppEvent, AppEventKind};

pub(crate) enum ActorCmd {
    Send(String),
    Interrupt,
    Respond {
        request_id: String,
        allow: bool,
        enable_auto: bool,
    },
    SetAutoMode(bool),
    SetModel(String),
    SetEffort(Effort),
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
    last_error: Option<String>,
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
            last_error: None,
        }
    }

    pub(crate) fn id(&self) -> SessionId {
        self.session.id
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
        self.emit(AppEventKind::Status(status));
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
        };
        self.next_seq += 1;
        m
    }

    fn command(&self, cmd: SessionCommand) {
        if self.handle.commands.send(cmd).is_err() {
            self.emit(AppEventKind::Error("provider is not running".into()));
        }
    }

    /// Returns false when the actor should end.
    async fn on_cmd(&mut self, cmd: ActorCmd) -> bool {
        match cmd {
            ActorCmd::Send(text) => {
                if self.status != SessionStatus::Idle {
                    self.emit(AppEventKind::Error("session is busy".into()));
                    return true;
                }
                if self.session.title.is_empty() {
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
                let msg = self.insert_message(Role::User, text.clone());
                self.write(WriteOp::InsertMessage(msg));
                self.set_status(SessionStatus::Streaming);
                self.command(SessionCommand::SendPrompt(text));
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
            }
            ActorCmd::SetAutoMode(on) => self.set_auto(on),
            ActorCmd::SetModel(m) => {
                self.pending_model = Some(m);
                return self.apply_pending().await;
            }
            ActorCmd::SetEffort(e) => {
                self.pending_effort = Some(e);
                return self.apply_pending().await;
            }
            ActorCmd::Close => {
                self.shutdown_child().await;
                self.set_status(SessionStatus::Closed);
                return false;
            }
        }
        true
    }

    fn set_auto(&mut self, on: bool) {
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
        match &ev {
            ProviderEvent::Quota(q) => {
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
                self.write(WriteOp::InsertMessage(m));
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
                self.write(WriteOp::InsertMessage(m));
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
                self.write(WriteOp::InsertMessage(m));
            }
            ProviderEvent::PermissionRequest { .. } => {
                self.emit(AppEventKind::Provider(ev));
                self.set_status(SessionStatus::NeedsApproval);
                return true;
            }
            ProviderEvent::TurnComplete { usage, .. } => {
                self.write(WriteOp::ApplyTurnUsage {
                    session_id: self.session.id,
                    turn: usage.clone(),
                });
                self.session.usage.accumulate(usage);
                self.emit(AppEventKind::Provider(ev));
                self.set_status(SessionStatus::Idle);
                return self.apply_pending().await;
            }
            ProviderEvent::Error(msg) => self.last_error = Some(msg.clone()),
            ProviderEvent::Exited { code } => {
                let msg = self
                    .last_error
                    .take()
                    .unwrap_or_else(|| format!("provider exited (code {code:?})"));
                self.emit(AppEventKind::Provider(ev));
                self.set_status(SessionStatus::Error(msg));
                return false;
            }
            ProviderEvent::TextDelta(_) | ProviderEvent::ThinkingProgress { .. } => {}
        }
        self.emit(AppEventKind::Provider(ev));
        true
    }

    /// Respawns with `resume` when a model/effort change is pending and the session is idle.
    /// Returns false when the new child could not be started.
    async fn apply_pending(&mut self) -> bool {
        if self.status != SessionStatus::Idle
            || (self.pending_model.is_none() && self.pending_effort.is_none())
        {
            return true;
        }
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
