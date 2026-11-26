//! A puppet provider: it records the prompts it gets, and the story decides what it says back.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use xade_core::{
    AgentProvider, Effort, ImportedSession, ModelOption, ProviderError, ProviderEvent, ProviderId,
    QuotaGroup, QuotaKind, QuotaWindow, SessionCommand, SessionHandle, SpawnConfig,
};

pub const FABLE: &str = "claude-fable-5-2";
pub const OPUS: &str = "claude-opus-5-5";
pub const SONNET: &str = "claude-sonnet-5-5";
pub const NEON: &str = "gemini-4-neon";

/// The live sessions of both providers, by model id (each model is used by one session).
#[derive(Default)]
pub struct Stage {
    events: Mutex<HashMap<String, mpsc::UnboundedSender<ProviderEvent>>>,
    prompts: Mutex<HashMap<String, usize>>,
}

impl Stage {
    pub fn emit(&self, model: &str, event: ProviderEvent) {
        let events = self.events.lock().unwrap();
        let tx = events
            .get(model)
            .unwrap_or_else(|| panic!("{model} has no running session yet"));
        tx.send(event).unwrap();
    }

    /// How many prompts `model` has received.
    pub fn prompts(&self, model: &str) -> usize {
        self.prompts
            .lock()
            .unwrap()
            .get(model)
            .copied()
            .unwrap_or(0)
    }
}

pub struct DemoProvider {
    pub id: ProviderId,
    pub stage: Arc<Stage>,
    pub history: Vec<ImportedSession>,
}

fn model(id: &str, label: &str) -> ModelOption {
    ModelOption {
        id: id.into(),
        label: label.into(),
        description: String::new(),
        effort_levels: vec![Effort::Low, Effort::Medium, Effort::High],
    }
}

fn window(kind: QuotaKind, utilization_pct: f32, hours: i64) -> QuotaWindow {
    QuotaWindow {
        kind,
        utilization_pct,
        resets_at: Some(time::OffsetDateTime::now_utc() + time::Duration::hours(hours)),
    }
}

impl AgentProvider for DemoProvider {
    fn id(&self) -> ProviderId {
        self.id
    }
    fn display_name(&self) -> &'static str {
        match self.id {
            ProviderId::Claude => "Claude",
            ProviderId::Antigravity => "Antigravity",
        }
    }
    fn models(&self) -> Vec<ModelOption> {
        match self.id {
            ProviderId::Claude => vec![
                model(FABLE, "Fable 5.2"),
                model(OPUS, "Opus 5.5"),
                model(SONNET, "Sonnet 5.5"),
            ],
            ProviderId::Antigravity => vec![model(NEON, "Gemini 4 Neon")],
        }
    }
    fn default_model(&self) -> String {
        self.models()[0].id.clone()
    }
    fn effort_levels(&self) -> Vec<Effort> {
        vec![Effort::Default, Effort::Low, Effort::Medium, Effort::High]
    }
    fn preflight(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn import_sessions(&self) -> Vec<ImportedSession> {
        self.history.clone()
    }
    fn quota(&self) -> Result<Vec<QuotaGroup>, ProviderError> {
        Ok(match self.id {
            // Claude reports quota in its event stream.
            ProviderId::Claude => Vec::new(),
            ProviderId::Antigravity => vec![QuotaGroup {
                name: "Gemini".into(),
                windows: vec![
                    window(QuotaKind::FiveHour, 31.0, 3),
                    window(QuotaKind::SevenDay, 22.0, 90),
                ],
            }],
        })
    }
    fn spawn(&self, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError> {
        let (commands, mut cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, events) = mpsc::unbounded_channel();
        let model_id = cfg.model_id.clone();
        let stage = self.stage.clone();
        stage
            .events
            .lock()
            .unwrap()
            .insert(model_id.clone(), ev_tx.clone());
        let claude = self.id == ProviderId::Claude;
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    SessionCommand::SendPrompt(_) => {
                        let _ = ev_tx.send(ProviderEvent::Started {
                            provider_session_id: format!("demo-{model_id}"),
                            model_id: model_id.clone(),
                        });
                        if claude {
                            for q in [
                                window(QuotaKind::FiveHour, 18.0, 4),
                                window(QuotaKind::SevenDay, 43.0, 34),
                            ] {
                                let _ = ev_tx.send(ProviderEvent::Quota(q));
                            }
                        }
                        *stage
                            .prompts
                            .lock()
                            .unwrap()
                            .entry(model_id.clone())
                            .or_default() += 1;
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
