#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
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
struct FakeProvider {
    provider_id: ProviderId,
    import: Mutex<Vec<ImportedSession>>,
    spawns: Mutex<Vec<SpawnConfig>>,
    /// Provider session ids passed to `delete_session`; a success also drops the transcript.
    deleted: Mutex<Vec<String>>,
    delete_fails: AtomicBool,
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::for_provider(ProviderId::Claude)
    }
}

impl FakeProvider {
    fn for_provider(provider_id: ProviderId) -> Self {
        Self {
            provider_id,
            import: Mutex::default(),
            spawns: Mutex::default(),
            deleted: Mutex::default(),
            delete_fails: AtomicBool::new(false),
        }
    }
}

impl AgentProvider for FakeProvider {
    fn id(&self) -> ProviderId {
        self.provider_id
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
    fn delete_session(&self, id: &str) -> Result<(), ProviderError> {
        if self.delete_fails.load(Ordering::SeqCst) {
            return Err(ProviderError::Protocol("disk on fire".into()));
        }
        self.deleted.lock().unwrap().push(id.into());
        self.import
            .lock()
            .unwrap()
            .retain(|s| s.provider_session_id != id);
        Ok(())
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

/// Imports skip temp dirs, so these tests live under the target dir instead.
fn non_temp_dir() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

async fn import(h: &mut common::Harness) -> (u32, u32) {
    import_from(h, 1).await
}

/// Each provider's batch is applied as it arrives and ends with its own `ImportDone`.
async fn import_from(h: &mut common::Harness, providers: usize) -> (u32, u32) {
    h.tx.send(ManagerCmd::Import).unwrap();
    let (mut imported, mut updated) = (0, 0);
    for _ in 0..providers {
        let ev = h
            .until(5, |e| matches!(e.kind, AppEventKind::ImportDone { .. }))
            .await;
        if let AppEventKind::ImportDone {
            imported: i,
            updated: u,
        } = ev.last().unwrap().kind
        {
            imported += i;
            updated += u;
        }
    }
    (imported, updated)
}

#[tokio::test]
async fn import_is_idempotent_and_groups_by_workspace() {
    let dir = non_temp_dir();
    let provider = Arc::new(FakeProvider::default());
    let mut h = common::start(dir.path(), provider.clone()).await;
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
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

    let first_id = first.session_id().unwrap();
    let (row, msgs) = h.reader.load_session(first_id).await.unwrap();
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
    let (row, msgs) = h.reader.load_session(first_id).await.unwrap();
    assert_eq!((row.title.as_str(), msgs.len()), ("first renamed", 1));
    assert_eq!(import(&mut h).await, (0, 0));
}

#[tokio::test]
async fn xade_sessions_and_live_sessions_are_never_overwritten() {
    let dir = non_temp_dir();
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
        delivery: None,
        context: None,
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

    // Open the imported session: it is live for import purposes but starts no process yet.
    let cc = h
        .reader
        .list_sessions(None, Some(SessionOrigin::ClaudeCode), 0, 24)
        .await
        .unwrap()
        .items
        .remove(0);
    let cc_id = cc.session_id().unwrap();
    let spawned_before = provider.spawns.lock().unwrap().len();
    h.tx.send(ManagerCmd::Open { session_id: cc_id }).unwrap();
    h.until_status(5, SessionStatus::Idle).await;
    assert_eq!(
        provider.spawns.lock().unwrap().len(),
        spawned_before,
        "opening spawns nothing"
    );
    provider.import.lock().unwrap()[1] = imported("cc", "cc newer", &h.cwd, 0);
    assert_eq!(import(&mut h).await, (0, 0), "live session is skipped");
    assert_eq!(
        h.reader.load_session(cc_id).await.unwrap().0.title,
        "cc one"
    );

    h.tx.send(ManagerCmd::Close { session_id: cc_id }).unwrap();
    h.until_status(5, SessionStatus::Closed).await;
    assert_eq!(import(&mut h).await, (0, 1));
    assert_eq!(
        h.reader.load_session(cc_id).await.unwrap().0.title,
        "cc newer"
    );
}

#[tokio::test]
async fn throwaway_cwds_are_skipped_and_earlier_leftovers_pruned() {
    let dir = non_temp_dir();
    let provider = Arc::new(FakeProvider::default());
    let mut h = common::start(dir.path(), provider.clone()).await;
    let temp = tempfile::tempdir().unwrap();
    let gone = dir.path().join("gone");
    *provider.import.lock().unwrap() = vec![
        imported("ok", "kept", &h.cwd, 100),
        imported("tmp", "in temp", temp.path(), 90),
        imported("gone", "missing dir", &gone, 80),
    ];
    assert_eq!(import(&mut h).await, (1, 0));
    let all = h.reader.list_sessions(None, None, 0, 24).await.unwrap();
    assert_eq!(all.items.len(), 1);
    assert_eq!(h.reader.list_workspaces().await.unwrap().len(), 1);

    // A leftover from an older run (directory gone since) is removed with its workspace.
    let leftover = dir.path().join("leftover");
    std::fs::create_dir_all(&leftover).unwrap();
    *provider.import.lock().unwrap() = vec![imported("late", "late", &leftover, 70)];
    assert_eq!(import(&mut h).await, (1, 0));
    assert_eq!(h.reader.list_workspaces().await.unwrap().len(), 2);
    std::fs::remove_dir(&leftover).unwrap();
    assert_eq!(import(&mut h).await, (0, 0));
    assert_eq!(h.reader.list_workspaces().await.unwrap().len(), 1);
    assert_eq!(
        h.reader
            .list_sessions(None, None, 0, 24)
            .await
            .unwrap()
            .total,
        1
    );
}

#[tokio::test]
async fn delete_closes_live_session_aborts_on_provider_error_and_stays_deleted() {
    let dir = non_temp_dir();
    let provider = Arc::new(FakeProvider::default());
    let mut h = common::start(dir.path(), provider.clone()).await;
    h.create("haiku");
    let ev = h.until_status(5, SessionStatus::Idle).await;
    let id = ev.iter().find_map(|e| e.session_id).unwrap();
    h.tx.send(ManagerCmd::Send {
        session_id: id,
        text: "hello".into(),
        delivery: None,
        context: None,
    })
    .unwrap();
    h.until_status(5, SessionStatus::Idle).await;
    // The transcript the CLI would have written, which an import would otherwise bring back.
    provider
        .import
        .lock()
        .unwrap()
        .push(imported("xade-owned", "hello", &h.cwd, 0));

    // A provider failure deletes nothing, but the live session was closed first.
    provider.delete_fails.store(true, Ordering::SeqCst);
    h.tx.send(ManagerCmd::DeleteSession { session_id: id })
        .unwrap();
    let ev = h
        .until(5, |e| matches!(e.kind, AppEventKind::Error(_)))
        .await;
    assert!(
        matches!(&ev.last().unwrap().kind, AppEventKind::Error(m) if m.contains("disk on fire"))
    );
    assert!(common::statuses(&ev).contains(&SessionStatus::Closed));
    assert_eq!(h.reader.load_session(id).await.unwrap().1.len(), 2);
    assert!(provider.deleted.lock().unwrap().is_empty());

    provider.delete_fails.store(false, Ordering::SeqCst);
    h.tx.send(ManagerCmd::DeleteSession { session_id: id })
        .unwrap();
    let ev = h
        .until(5, |e| matches!(e.kind, AppEventKind::SessionDeleted { .. }))
        .await;
    assert_eq!(ev.last().unwrap().session_id, Some(id));
    assert_eq!(*provider.deleted.lock().unwrap(), ["xade-owned"]);
    assert!(h.reader.load_session(id).await.is_err());

    // Import after a delete does not bring it back.
    assert_eq!(import(&mut h).await, (0, 0));
    assert_eq!(
        h.reader
            .list_sessions(None, None, 0, 24)
            .await
            .unwrap()
            .total,
        0
    );
}

#[tokio::test]
async fn delete_refuses_sessions_not_created_in_xade() {
    let dir = non_temp_dir();
    let provider = Arc::new(FakeProvider::default());
    let mut h = common::start(dir.path(), provider.clone()).await;
    *provider.import.lock().unwrap() = vec![imported("cc", "from the CLI", &h.cwd, 10)];
    assert_eq!(import(&mut h).await, (1, 0));
    let cc = h
        .reader
        .list_sessions(None, None, 0, 24)
        .await
        .unwrap()
        .items
        .remove(0);

    let cc_id = cc.session_id().unwrap();
    h.tx.send(ManagerCmd::DeleteSession { session_id: cc_id })
        .unwrap();
    h.until(5, |e| matches!(e.kind, AppEventKind::Error(_)))
        .await;
    assert!(provider.deleted.lock().unwrap().is_empty());
    assert!(h.reader.load_session(cc_id).await.is_ok());
}

#[tokio::test]
async fn codex_import_gets_codex_origin_while_claude_keeps_claude_code() {
    let dir = non_temp_dir();
    let claude = Arc::new(FakeProvider::for_provider(ProviderId::Claude));
    let codex = Arc::new(FakeProvider::for_provider(ProviderId::Codex));
    let mut h = common::start_with(dir.path(), vec![claude.clone(), codex.clone()]).await;

    *claude.import.lock().unwrap() = vec![imported("cc-1", "from claude", &h.cwd, 20)];
    *codex.import.lock().unwrap() = vec![imported("cx-1", "from codex", &h.cwd, 10)];

    assert_eq!(import_from(&mut h, 2).await, (2, 0));

    let cc_page = h
        .reader
        .list_sessions(None, Some(SessionOrigin::ClaudeCode), 0, 24)
        .await
        .unwrap();
    assert_eq!(cc_page.total, 1);
    assert_eq!(cc_page.items[0].title, "from claude");
    assert_eq!(cc_page.items[0].origin, SessionOrigin::ClaudeCode);

    let cx_page = h
        .reader
        .list_sessions(None, Some(SessionOrigin::Codex), 0, 24)
        .await
        .unwrap();
    assert_eq!(cx_page.total, 1);
    assert_eq!(cx_page.items[0].title, "from codex");
    assert_eq!(cx_page.items[0].origin, SessionOrigin::Codex);

    let cx_id = cx_page.items[0].session_id().unwrap();
    let (row, _) = h.reader.load_session(cx_id).await.unwrap();
    assert_eq!(row.provider_id, ProviderId::Codex);
    assert_eq!(row.origin, SessionOrigin::Codex);

    // Imported Codex sessions cannot be deleted from Xade, matching Claude Code imports.
    h.tx.send(ManagerCmd::DeleteSession { session_id: cx_id })
        .unwrap();
    h.until(5, |e| matches!(e.kind, AppEventKind::Error(_)))
        .await;
    assert!(codex.deleted.lock().unwrap().is_empty());
    assert!(h.reader.load_session(cx_id).await.is_ok());
}
