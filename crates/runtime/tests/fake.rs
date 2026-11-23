#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::{Arc, Mutex};

use aw_core::{
    AgentProvider, Effort, ModelOption, ProviderError, ProviderEvent, ProviderId, QuotaKind,
    QuotaWindow, Role, SessionCommand, SessionHandle, SessionStatus, SpawnConfig, UsageTotals,
};
use aw_runtime::{AppEventKind, ManagerCmd};
use tokio::sync::{mpsc, oneshot};

/// Replies to every prompt with a scripted turn: text, one tool call, a final answer.
#[derive(Default)]
struct FakeProvider {
    spawns: Mutex<Vec<SpawnConfig>>,
}

impl AgentProvider for FakeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Claude
    }
    fn display_name(&self) -> &'static str {
        "Fake"
    }
    fn models(&self) -> Vec<ModelOption> {
        Vec::new()
    }
    fn default_model(&self) -> String {
        "fake".into()
    }
    fn effort_levels(&self) -> Vec<Effort> {
        vec![Effort::Default]
    }
    fn preflight(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn spawn(&self, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError> {
        let model_id = cfg.model_id.clone();
        self.spawns.lock().unwrap().push(cfg);
        let (commands, mut cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, events) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    SessionCommand::SendPrompt(_) => {
                        let script = [
                            ProviderEvent::Started {
                                provider_session_id: "fake-session".into(),
                                model_id: model_id.clone(),
                            },
                            ProviderEvent::TextDelta("po".into()),
                            ProviderEvent::TextDelta("ng".into()),
                            ProviderEvent::ToolUse {
                                tool_use_id: "t1".into(),
                                name: "Read".into(),
                                input_json: r#"{"file_path":"a.rs"}"#.into(),
                            },
                            ProviderEvent::ToolResult {
                                tool_use_id: "t1".into(),
                                output: "contents".into(),
                                is_error: false,
                                structured_json: Some(r#"{"x":1}"#.into()),
                            },
                            ProviderEvent::AssistantText {
                                text: "pong".into(),
                            },
                            ProviderEvent::Quota(QuotaWindow {
                                kind: QuotaKind::FiveHour,
                                utilization_pct: 42.0,
                                resets_at: None,
                            }),
                            ProviderEvent::TurnComplete {
                                usage: UsageTotals {
                                    input_tokens: 10,
                                    output_tokens: 5,
                                    cost_usd: 0.25,
                                    context_tokens_used: 100,
                                    context_window_max: 1000,
                                    ..Default::default()
                                },
                                is_error: false,
                                interrupted: false,
                            },
                        ];
                        for ev in script {
                            let _ = ev_tx.send(ev);
                        }
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

#[tokio::test]
async fn create_send_close_open_resume() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::default());
    let mut h = common::start(dir.path(), provider.clone()).await;

    h.create("haiku");
    let ev = h.until_status(5, SessionStatus::Idle).await;
    let session = ev
        .iter()
        .find_map(|e| match &e.kind {
            AppEventKind::SessionCreated(s) => Some(s.clone()),
            _ => None,
        })
        .expect("SessionCreated");
    let id = session.id;

    h.tx.send(ManagerCmd::Send {
        session_id: id,
        text: "Say pong\nsecond line".into(),
    })
    .unwrap();
    let ev = h.until_status(5, SessionStatus::Idle).await;
    assert_eq!(
        common::statuses(&ev),
        vec![SessionStatus::Streaming, SessionStatus::Idle]
    );
    assert!(ev.iter().any(|e| matches!(
        &e.kind,
        AppEventKind::Quota(q) if e.session_id.is_none() && q.utilization_pct == 42.0
    )));
    assert!(ev
        .iter()
        .any(|e| matches!(&e.kind, AppEventKind::Provider(ProviderEvent::TextDelta(_)))));

    aw_runtime::flush(&h.writer).await;
    let (row, msgs) = h.reader.load_session(id).await.unwrap();
    assert_eq!(row.title, "Say pong");
    assert_eq!(row.provider_session_id.as_deref(), Some("fake-session"));
    assert_eq!(row.usage.input_tokens, 10);
    assert_eq!(row.usage.context_window_max, 1000);
    let roles: Vec<_> = msgs.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            Role::User,
            Role::ToolCall,
            Role::ToolResult,
            Role::Assistant
        ]
    );
    assert_eq!(
        msgs.iter().map(|m| m.seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(msgs[2].tool_payload_json.as_deref(), Some(r#"{"x":1}"#));
    assert_eq!(msgs[3].content_text, "pong");

    // Close, then reopen: resumes with the stored provider session id; seq continues.
    h.tx.send(ManagerCmd::Close { session_id: id }).unwrap();
    h.until_status(5, SessionStatus::Closed).await;
    h.tx.send(ManagerCmd::Open { session_id: id }).unwrap();
    let ev = h.until_status(5, SessionStatus::Idle).await;
    assert!(ev.iter().any(|e| matches!(
        &e.kind,
        AppEventKind::SessionOpened { messages, .. } if messages.len() == 4
    )));
    assert_eq!(
        provider
            .spawns
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .resume
            .as_deref(),
        Some("fake-session")
    );

    h.tx.send(ManagerCmd::Send {
        session_id: id,
        text: "again".into(),
    })
    .unwrap();
    h.until_status(5, SessionStatus::Idle).await;
    aw_runtime::flush(&h.writer).await;
    let (_, msgs) = h.reader.load_session(id).await.unwrap();
    assert_eq!(msgs.len(), 8);
    assert_eq!(msgs.last().unwrap().seq, 8);

    // A model change is applied by respawning with resume.
    h.tx.send(ManagerCmd::SetModel {
        session_id: id,
        model_id: "opus".into(),
    })
    .unwrap();
    for _ in 0..50 {
        if provider.spawns.lock().unwrap().last().unwrap().model_id == "opus" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    aw_runtime::flush(&h.writer).await;
    {
        let spawns = provider.spawns.lock().unwrap();
        let last = spawns.last().unwrap();
        assert_eq!(last.model_id, "opus");
        assert_eq!(last.resume.as_deref(), Some("fake-session"));
    }

    let (done_tx, done_rx) = oneshot::channel();
    h.tx.send(ManagerCmd::ShutdownAll(done_tx)).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), done_rx)
        .await
        .unwrap()
        .unwrap();
}
