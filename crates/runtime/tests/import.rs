#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::{Arc, Mutex};

use time::{Duration, OffsetDateTime};
use tokio::sync::mpsc;
use xade_core::{
    AgentProvider, Effort, ImportedMessage, ImportedSession, ModelOption, ProviderError,
    ProviderEvent, ProviderId, Role, SessionCommand, SessionHandle, SessionOrigin, SessionStatus,
    SpawnConfig, UsageTotals,
};
use xade_runtime::{AppEventKind, ManagerCmd};

/// Replies to a prompt with a short scripted turn and serves a mutable list of importable sessions.
#[derive(Default)]
struct FakeProvider {
    import: Mutex<Vec<ImportedSession>>,
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
        "fake-default".into()
    }
    fn effort_levels(&self) -> Vec<Effort> {
        vec![Effort::Default]
    }
    fn preflight(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn spawn(&self, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError> {
        self.spawns.lock().unwrap().push(cfg);
        let (commands, mut cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, events) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    SessionCommand::SendPrompt(_) => {
                        for ev in [
                            ProviderEvent::Started {
                                provider_session_id: "xade-owned".into(),
                                model_id: "m".into(),
                            },
                            ProviderEvent::AssistantText {
                                text: "pong".into(),
                            },
                            ProviderEvent::TurnComplete {
                                usage: UsageTotals::default(),
                                is_error: false,
                                interrupted: false,
                            },
                        ] {
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
    fn import_sessions(&self) -> Vec<ImportedSession> {
        self.import.lock().unwrap().clone()
    }
}

fn imported(psid: &str, title: &str, cwd: &std::path::Path, age_secs: i64) -> ImportedSession {
    let now = OffsetDateTime::now_utc();
    let msg = |role, text: &str, off| ImportedMessage {
        role,
        content_text: text.into(),
        tool_name: None,
        tool_use_id: None,
        tool_payload_json: None,
        is_error: false,
        created_at: now - Duration::seconds(age_secs + off),
    };
    ImportedSession {
        provider_session_id: psid.into(),
        cwd: cwd.into(),
        title: title.into(),
        model_id: String::new(),
        created_at: now - Duration::seconds(age_secs + 5),
        updated_at: now - Duration::seconds(age_secs),
        usage: UsageTotals {
            context_tokens_used: 100,
            ..Default::default()
        },
        messages: vec![
            msg(Role::User, "question", 5),
            msg(Role::Assistant, "answer", 0),
        ],
    }
}

async fn import(h: &mut common::Harness) -> (u32, u32) {
    h.tx.send(ManagerCmd::Import).unwrap();
    let ev = h
        .until(5, |e| matches!(e.kind, AppEventKind::ImportDone { .. }))
        .await;
    match ev.last().unwrap().kind {
        AppEventKind::ImportDone { imported, updated } => (imported, updated),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn import_is_idempotent_and_groups_by_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::default());
    let mut h = common::start(dir.path(), provider.clone()).await;
    let elsewhere = dir.path().join("elsewhere");
    *provider.import.lock().unwrap() = vec![
        imported("a", "first", &h.cwd, 100),
        imported("b", "second", &elsewhere, 50),
    ];

    assert_eq!(import(&mut h).await, (2, 0));
    assert_eq!(import(&mut h).await, (0, 0));

    let page = h
        .reader
        .list_sessions(None, Some(SessionOrigin::ClaudeCode), 0, 24)
        .await
        .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(
        h.reader
            .list_sessions(None, Some(SessionOrigin::Xade), 0, 24)
            .await
            .unwrap()
            .total,
        0
    );
    let first = page.items.iter().find(|c| c.title == "first").unwrap();
    assert_eq!(
        first.workspace_id, h.workspace_id,
        "existing workspace reused by path"
    );
    let second = page.items.iter().find(|c| c.title == "second").unwrap();
    let workspaces = h.reader.list_workspaces().await.unwrap();
    assert_eq!(workspaces.len(), 2);
    assert!(workspaces
        .iter()
        .any(|w| w.id == second.workspace_id && w.root_path == elsewhere));

    let (row, msgs) = h.reader.load_session(first.id).await.unwrap();
    assert_eq!(row.origin, SessionOrigin::ClaudeCode);
    assert_eq!(row.provider_session_id.as_deref(), Some("a"));
    assert_eq!(
        row.model_id, "fake-default",
        "empty model falls back to the default"
    );
    assert_eq!(row.usage.context_tokens_used, 100);
    assert_eq!(msgs.iter().map(|m| m.seq).collect::<Vec<_>>(), [1, 2]);

    // A newer transcript replaces the messages of the same row.
    let mut newer = imported("a", "first renamed", &h.cwd, 1);
    newer.messages.truncate(1);
    provider.import.lock().unwrap()[0] = newer;
    assert_eq!(import(&mut h).await, (0, 1));
    let (row, msgs) = h.reader.load_session(first.id).await.unwrap();
    assert_eq!((row.title.as_str(), msgs.len()), ("first renamed", 1));
    assert_eq!(import(&mut h).await, (0, 0));
}

#[tokio::test]
async fn xade_sessions_and_live_sessions_are_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::default());
    let mut h = common::start(dir.path(), provider.clone()).await;

    // A Xade-origin session owns provider session id "xade-owned".
    h.create("haiku");
    let ev = h.until_status(5, SessionStatus::Idle).await;
    let xade_id = ev
        .iter()
        .find_map(|e| e.session_id)
        .expect("session id in events");
    h.tx.send(ManagerCmd::Send {
        session_id: xade_id,
        text: "hello".into(),
    })
    .unwrap();
    h.until_status(5, SessionStatus::Idle).await;
    xade_runtime::flush(&h.writer).await;

    provider
        .import
        .lock()
        .unwrap()
        .push(imported("xade-owned", "from transcript", &h.cwd, 0));
    provider
        .import
        .lock()
        .unwrap()
        .push(imported("cc", "cc one", &h.cwd, 100));
    assert_eq!(import(&mut h).await, (1, 0));
    let (row, msgs) = h.reader.load_session(xade_id).await.unwrap();
    assert_eq!(
        (row.title.as_str(), row.origin),
        ("hello", SessionOrigin::Xade)
    );
    assert_eq!(msgs.len(), 2);

    // Open the imported session: it resumes by its provider session id and becomes live.
    let cc = h
        .reader
        .list_sessions(None, Some(SessionOrigin::ClaudeCode), 0, 24)
        .await
        .unwrap()
        .items
        .remove(0);
    h.tx.send(ManagerCmd::Open { session_id: cc.id }).unwrap();
    h.until_status(5, SessionStatus::Idle).await;
    assert_eq!(
        provider
            .spawns
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .resume
            .as_deref(),
        Some("cc")
    );
    provider.import.lock().unwrap()[1] = imported("cc", "cc newer", &h.cwd, 0);
    assert_eq!(import(&mut h).await, (0, 0), "live session is skipped");
    assert_eq!(
        h.reader.load_session(cc.id).await.unwrap().0.title,
        "cc one"
    );

    h.tx.send(ManagerCmd::Close { session_id: cc.id }).unwrap();
    h.until_status(5, SessionStatus::Closed).await;
    assert_eq!(import(&mut h).await, (0, 1));
    assert_eq!(
        h.reader.load_session(cc.id).await.unwrap().0.title,
        "cc newer"
    );
}
