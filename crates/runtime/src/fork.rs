//! Fork: copies a session's history into a new session of another provider and prepares its
//! hand-off files. Runs in its own task so the manager loop never waits on a provider.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::watch;
use tokio::time::timeout;
use uuid::Uuid;
use xade_core::{
    AgentProvider, Effort, Message, ProviderEvent, ProviderId, Role, Session, SessionCommand,
    SessionHandle, SessionId, SessionOrigin, SpawnConfig, UsageTotals,
};
use xade_store::{StoreReader, WriteOp, WriterHandle};

use crate::flush;
use crate::handoff::{mention, remove_all, snapshot_path, transcript_path, write_private};
use crate::transcript::transcript;

/// The whole snapshot extraction, start to finish.
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) struct ForkJob {
    pub providers: Vec<Arc<dyn AgentProvider>>,
    pub writer: WriterHandle,
    pub reader: StoreReader,
    pub dir: PathBuf,
    /// Turns true when the app shuts down.
    pub cancel: watch::Receiver<bool>,
}

impl ForkJob {
    fn provider(&self, id: ProviderId) -> Option<Arc<dyn AgentProvider>> {
        self.providers.iter().find(|p| p.id() == id).cloned()
    }

    /// Copies the stored history into a new session of `target`, in one transaction, after
    /// asking the source provider for a state snapshot (best effort). The new session has no
    /// provider id, so opening it derives the hand-off.
    pub(crate) async fn run(
        self,
        source: SessionId,
        target: ProviderId,
        handoff_prompt: String,
    ) -> Result<SessionId, String> {
        let provider = self
            .provider(target)
            .ok_or("that provider is not available")?;
        // A live or just-closed actor's latest messages may still be queued in the writer.
        flush(&self.writer).await;
        let (src, history) = self
            .reader
            .load_session(source)
            .await
            .map_err(|e| e.to_string())?;
        if history.is_empty() {
            return Err("it has no messages yet".into());
        }
        let (model_id, effort) = match self
            .reader
            .last_model_choice(target)
            .await
            .map_err(|e| e.to_string())?
        {
            Some((model, effort, _)) => (model, effort),
            None => (provider.default_model(), Effort::Default),
        };
        let effort = supported_effort(provider.as_ref(), &model_id, effort);
        let basis = match src.title.trim() {
            "" => history
                .iter()
                .find(|m| m.role == Role::User)
                .map_or(String::new(), |m| m.content_text.clone()),
            title => title.to_string(),
        };
        let title = match Session::derive_title(&format!("Fork: {basis}")) {
            t if t.trim() == "Fork:" => "Fork".to_string(),
            t => t,
        };
        let id = SessionId::new();
        self.prepare_handoff(&src, &history, id, handoff_prompt)
            .await;
        let now = OffsetDateTime::now_utc();
        let session = Session {
            id,
            workspace_id: src.workspace_id,
            provider_id: target,
            provider_session_id: None,
            origin: SessionOrigin::Xade,
            title,
            preview_snippet: String::new(),
            sent_model_id: String::new(),
            sent_effort: effort,
            model_id,
            effort,
            auto_mode: src.auto_mode,
            usage: UsageTotals::default(),
            created_at: now,
            updated_at: now,
        };
        let count = history.len() as i64;
        let messages = history
            .into_iter()
            .zip(1..)
            .map(|(m, seq)| Message {
                id: Uuid::new_v4(),
                session_id: id,
                seq,
                thinking_text: None,
                ..m
            })
            .collect();
        let saved = self.save(session, messages, count).await;
        if saved.is_err() {
            self.remove_files(id).await;
        }
        saved.map(|()| id)
    }

    async fn save(
        &self,
        session: Session,
        messages: Vec<Message>,
        count: i64,
    ) -> Result<(), String> {
        let (id, workspace_id) = (session.id, session.workspace_id);
        self.writer
            .send(WriteOp::InsertSessionWithMessages { session, messages })
            .map_err(|e| e.to_string())?;
        let _ = self.writer.send(WriteOp::TouchWorkspace(workspace_id));
        flush(&self.writer).await;
        // The writer only logs a failed transaction; the stored count says whether it landed.
        match self.reader.max_seq(id).await {
            Ok(n) if n == count => Ok(()),
            Ok(_) => Err("the copy was not saved".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn remove_files(&self, id: SessionId) {
        let dir = self.dir.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || remove_all(&dir, id)).await {
            tracing::warn!(error = %e, "fork file cleanup failed");
        }
    }

    /// Writes the transcript file, then has the source provider summarize it into the
    /// snapshot file. Any failure leaves out the snapshot: the first send then uses the
    /// transcript.
    async fn prepare_handoff(
        &self,
        src: &Session,
        history: &[Message],
        id: SessionId,
        handoff_prompt: String,
    ) {
        let text = transcript(history);
        if text.is_empty() {
            return;
        }
        let path = transcript_path(&self.dir, id);
        let to_write = (path.clone(), text);
        match tokio::task::spawn_blocking(move || write_private(&to_write.0, &to_write.1)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return tracing::warn!(error = %e, "could not write the fork transcript"),
            Err(e) => return tracing::warn!(error = %e, "fork transcript task failed"),
        }
        let Some(provider) = self.provider(src.provider_id) else {
            return;
        };
        let cwd = match self.reader.list_workspaces().await {
            Ok(ws) => ws
                .into_iter()
                .find(|w| w.id == src.workspace_id)
                .map(|w| w.root_path),
            Err(e) => return tracing::warn!(error = %e, "fork snapshot: workspace lookup failed"),
        };
        let Some(cwd) = cwd else {
            return tracing::warn!("fork snapshot: workspace not found");
        };
        let cfg = SpawnConfig {
            cwd,
            model_id: src.model_id.clone(),
            effort: src.effort,
            auto_mode: false,
            resume: None,
        };
        let prompt = format!("{}\n\n{handoff_prompt}", mention(&path));
        let Some(snapshot) = extract(provider, cfg, prompt, self.cancel.clone()).await else {
            return;
        };
        let file = snapshot_path(&self.dir, id);
        match tokio::task::spawn_blocking(move || write_private(&file, &snapshot)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "could not write the fork snapshot"),
            Err(e) => tracing::warn!(error = %e, "fork snapshot task failed"),
        }
    }
}

/// Runs one throwaway turn on `provider` (no actor, no stored rows): asks `prompt`, denies
/// every permission request and returns the assistant's text. The conversation it created is
/// deleted at the end, whatever the outcome. `None` on any failure or an empty answer.
async fn extract(
    provider: Arc<dyn AgentProvider>,
    cfg: SpawnConfig,
    prompt: String,
    mut cancel: watch::Receiver<bool>,
) -> Option<String> {
    let mut handle = match provider.spawn(cfg) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "fork snapshot: could not start the provider");
            return None;
        }
    };
    let (mut psid, mut exited) = (None, false);
    let answer = if handle
        .commands
        .send(SessionCommand::SendPrompt(prompt))
        .is_ok()
    {
        let run = collect(&mut handle, &mut psid, &mut exited, &mut cancel);
        match timeout(EXTRACT_TIMEOUT, run).await {
            Ok(a) => a,
            Err(_) => {
                tracing::warn!("fork snapshot: timed out");
                None
            }
        }
    } else {
        None
    };
    let _ = handle.commands.send(SessionCommand::Shutdown);
    if !exited {
        let events = &mut handle.events;
        let _ = timeout(Duration::from_secs(8), async {
            while let Some(ev) = events.recv().await {
                if matches!(ev, ProviderEvent::Exited { .. }) {
                    break;
                }
            }
        })
        .await;
    }
    if let Some(psid) = psid {
        match tokio::task::spawn_blocking(move || provider.delete_session(&psid)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "fork snapshot: could not delete its chat"),
            Err(e) => tracing::warn!(error = %e, "fork snapshot: delete task failed"),
        }
    }
    if answer.is_none() {
        tracing::warn!("fork snapshot: none, the first send will use the transcript");
    }
    answer
}

/// The assistant text of the turn, or `None` when it failed, was cancelled or said nothing.
async fn collect(
    handle: &mut SessionHandle,
    psid: &mut Option<String>,
    exited: &mut bool,
    cancel: &mut watch::Receiver<bool>,
) -> Option<String> {
    let mut blocks: Vec<String> = Vec::new();
    loop {
        if *cancel.borrow() {
            return None;
        }
        let ev = tokio::select! {
            ev = handle.events.recv() => ev,
            _ = cancel.changed() => return None,
        };
        match ev? {
            ProviderEvent::Started {
                provider_session_id,
                ..
            } => *psid = Some(provider_session_id),
            ProviderEvent::PermissionRequest { request_id, .. } => {
                let _ = handle.commands.send(SessionCommand::RespondPermission {
                    request_id,
                    allow: false,
                });
            }
            ProviderEvent::AssistantText { text } => blocks.push(text),
            ProviderEvent::TurnComplete {
                is_error,
                interrupted,
                ..
            } => {
                if is_error || interrupted {
                    return None;
                }
                let text = blocks.join("\n\n");
                return (!text.trim().is_empty()).then_some(text);
            }
            ProviderEvent::Exited { .. } => {
                *exited = true;
                return None;
            }
            _ => {}
        }
    }
}

/// `effort` when the model lists it (or the provider does, for an unknown model), else `Default`.
fn supported_effort(provider: &dyn AgentProvider, model_id: &str, effort: Effort) -> Effort {
    let levels = match provider.models().into_iter().find(|m| m.id == model_id) {
        Some(m) => m.effort_levels,
        None => provider.effort_levels(),
    };
    if effort == Effort::Default || levels.contains(&effort) {
        effort
    } else {
        Effort::Default
    }
}
