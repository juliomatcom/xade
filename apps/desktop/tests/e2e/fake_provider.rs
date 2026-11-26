//! The model, mocked: a provider that answers every prompt with `Echo: <prompt>` and keeps a
//! canned history. Everything above `AgentProvider` (session manager, store, UI) is the real
//! thing.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use time::OffsetDateTime;
use tokio::sync::mpsc;
use xade_core::{
    AgentProvider, Effort, ImportedMessage, ImportedSession, ModelOption, ProviderError,
    ProviderEvent, ProviderId, Role, SessionCommand, SessionHandle, SpawnConfig, UsageTotals,
};

pub struct FakeProvider {
    history: Vec<ImportedSession>,
    next_id: AtomicU64,
}

impl FakeProvider {
    /// `history`: the sessions this provider "already has", as (title, messages), all in `cwd`.
    pub fn new(cwd: &Path, history: &[(&str, &[(Role, &str)])]) -> Self {
        let now = OffsetDateTime::now_utc();
        let history = history
            .iter()
            .enumerate()
            .map(|(i, (title, messages))| ImportedSession {
                provider_session_id: format!("past-session-{i}"),
                cwd: cwd.into(),
                title: (*title).into(),
                model_id: "fake-model".into(),
                created_at: now,
                updated_at: now,
                usage: UsageTotals::default(),
                messages: messages
                    .iter()
                    .map(|(role, text)| ImportedMessage {
                        role: *role,
                        content_text: (*text).into(),
                        tool_name: None,
                        tool_use_id: None,
                        tool_payload_json: None,
                        is_error: false,
                        created_at: now,
                    })
                    .collect(),
            })
            .collect();
        FakeProvider {
            history,
            next_id: AtomicU64::new(1),
        }
    }
}

impl AgentProvider for FakeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Claude
    }
    fn display_name(&self) -> &'static str {
        "Fake"
    }
    fn models(&self) -> Vec<ModelOption> {
        vec![ModelOption {
            id: "fake-model".into(),
            label: "Fake model".into(),
            description: "Scripted".into(),
            effort_levels: Vec::new(),
        }]
    }
    fn default_model(&self) -> String {
        "fake-model".into()
    }
    fn effort_levels(&self) -> Vec<Effort> {
        vec![Effort::Default]
    }
    fn preflight(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn import_sessions(&self) -> Vec<ImportedSession> {
        self.history.clone()
    }
    fn spawn(&self, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError> {
        let psid = cfg.resume.clone().unwrap_or_else(|| {
            let n = self.next_id.fetch_add(1, Ordering::Relaxed);
            format!("fake-session-{n}")
        });
        let model_id = cfg.model_id.clone();
        let (commands, mut cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, events) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    SessionCommand::SendPrompt(text) => {
                        let _ = ev_tx.send(ProviderEvent::Started {
                            provider_session_id: psid.clone(),
                            model_id: model_id.clone(),
                        });
                        let reply = if text.contains("[Team broadcast |") {
                            "Received team broadcast!".to_string()
                        } else if text.contains("TRIGGER_BROADCAST") {
                            "Private note to user.\n<broadcast>Plan ready from teammate</broadcast>"
                                .to_string()
                        } else {
                            format!("Echo: {text}")
                        };
                        let _ = ev_tx.send(ProviderEvent::AssistantText { text: reply });
                        let _ = ev_tx.send(ProviderEvent::TurnComplete {
                            usage: UsageTotals::default(),
                            is_error: false,
                            interrupted: false,
                        });
                    }
                    SessionCommand::Shutdown => {
                        let _ = ev_tx.send(ProviderEvent::Exited { code: Some(0) });
                        return;
                    }
                    _ => {}
                }
            }
        });
        Ok(SessionHandle { commands, events })
    }
}
