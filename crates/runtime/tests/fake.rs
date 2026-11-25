#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use xade_core::{
    AgentProvider, Effort, ModelOption, ProviderError, ProviderEvent, ProviderId, QuotaKind,
    QuotaWindow, Role, SessionCommand, SessionHandle, SessionStatus, SpawnConfig, UsageTotals,
};
use xade_runtime::{AppEventKind, ManagerCmd};

/// Replies to every prompt with a scripted turn: text, one tool call, a final answer. A
/// session it starts anew gets its own provider session id (the first is "fake-session").
struct FakeProvider {
    id: ProviderId,
    models: Vec<ModelOption>,
    spawns: Mutex<Vec<SpawnConfig>>,
    prompts: Arc<Mutex<Vec<String>>>,
    started: AtomicUsize,
    /// How the next prompt goes instead of the scripted turn.
    next_turn: Arc<Mutex<Option<Turn>>>,
}

#[derive(Clone, Copy)]
enum Turn {
    /// Stopped before any session was started (no `Started`).
    StopBeforeStart,
    /// `Started`, then stopped with no output.
    StartedThenStop,
    /// Ends in error with no output.
    ErrorNoOutput,
    /// No reply until an interrupt, which ends the turn stopped.
    Hold,
}

fn turn_complete(is_error: bool, interrupted: bool) -> ProviderEvent {
    ProviderEvent::TurnComplete {
        usage: UsageTotals::default(),
        is_error,
        interrupted,
    }
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::new(ProviderId::Claude)
    }
}

impl FakeProvider {
    fn new(id: ProviderId) -> Self {
        Self {
            id,
            models: Vec::new(),
            spawns: Mutex::default(),
            prompts: Arc::default(),
            started: AtomicUsize::new(0),
            next_turn: Arc::default(),
        }
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

impl AgentProvider for FakeProvider {
    fn id(&self) -> ProviderId {
        self.id
    }
    fn display_name(&self) -> &'static str {
        "Fake"
    }
    fn models(&self) -> Vec<ModelOption> {
        self.models.clone()
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
        let n = self.started.fetch_add(1, Ordering::SeqCst);
        let psid = cfg.resume.clone().unwrap_or_else(|| match n {
            0 => "fake-session".to_string(),
            n => format!("fake-session-{n}"),
        });
        let prompts = self.prompts.clone();
        let next_turn = self.next_turn.clone();
        self.spawns.lock().unwrap().push(cfg);
        let (commands, mut cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, events) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut holding = false;
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    SessionCommand::SendPrompt(text) => {
                        prompts.lock().unwrap().push(text);
                        if holding {
                            continue;
                        }
                        let turn = next_turn.lock().unwrap().take();
                        match turn {
                            Some(Turn::StopBeforeStart) => {
                                let _ = ev_tx.send(turn_complete(false, true));
                                continue;
                            }
                            Some(Turn::StartedThenStop) => {
                                let _ = ev_tx.send(ProviderEvent::Started {
                                    provider_session_id: psid.clone(),
                                    model_id: model_id.clone(),
                                });
                                let _ = ev_tx.send(turn_complete(false, true));
                                continue;
                            }
                            Some(Turn::ErrorNoOutput) => {
                                let _ = ev_tx.send(turn_complete(true, false));
                                continue;
                            }
                            Some(Turn::Hold) => {
                                holding = true;
                                continue;
                            }
                            None => {}
                        }
                        let script = [
                            ProviderEvent::Started {
                                provider_session_id: psid.clone(),
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
                    SessionCommand::Interrupt if holding => {
                        holding = false;
                        let _ = ev_tx.send(turn_complete(false, true));
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
        delivery: None,
        context: None,
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

    xade_runtime::flush(&h.writer).await;
    let quota = h.reader.load_quota().await.unwrap();
    assert_eq!(quota.len(), 1);
    assert_eq!(
        (quota[0].kind, quota[0].utilization_pct),
        (QuotaKind::FiveHour, 42.0)
    );
    let (row, msgs) = h.reader.load_session(id).await.unwrap();
    assert_eq!(row.title, "Say pong");
    assert_eq!(row.origin, xade_core::SessionOrigin::Xade);
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
    // Opening starts no process; settings changed meanwhile apply to the first spawn.
    let spawned_before = provider.spawns.lock().unwrap().len();
    h.tx.send(ManagerCmd::SetModel {
        session_id: id,
        model_id: "haiku".into(),
    })
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(provider.spawns.lock().unwrap().len(), spawned_before);

    h.tx.send(ManagerCmd::Send {
        session_id: id,
        text: "again".into(),
        delivery: None,
        context: None,
    })
    .unwrap();
    h.until_status(5, SessionStatus::Idle).await;
    {
        let spawns = provider.spawns.lock().unwrap();
        assert_eq!(spawns.len(), spawned_before + 1);
        let last = spawns.last().unwrap();
        assert_eq!(last.resume.as_deref(), Some("fake-session"));
        assert_eq!(last.model_id, "haiku");
    }
    xade_runtime::flush(&h.writer).await;
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
    xade_runtime::flush(&h.writer).await;
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

#[tokio::test]
async fn context_goes_to_the_provider_but_not_into_the_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::default());
    let mut h = common::start(dir.path(), provider.clone()).await;

    h.create("haiku");
    let ev = h.until_status(5, SessionStatus::Idle).await;
    let id = ev
        .iter()
        .find_map(|e| match &e.kind {
            AppEventKind::SessionCreated(s) => Some(s.id),
            _ => None,
        })
        .expect("SessionCreated");

    h.tx.send(ManagerCmd::Send {
        session_id: id,
        text: "fix it".into(),
        delivery: None,
        context: Some("<ide_opened_file>a.rs</ide_opened_file>".into()),
    })
    .unwrap();
    h.until_status(5, SessionStatus::Idle).await;
    xade_runtime::flush(&h.writer).await;

    assert_eq!(
        *provider.prompts.lock().unwrap(),
        vec!["<ide_opened_file>a.rs</ide_opened_file>\nfix it".to_string()]
    );
    let (row, msgs) = h.reader.load_session(id).await.unwrap();
    assert_eq!(row.title, "fix it");
    let user = msgs.iter().find(|m| m.role == Role::User).unwrap();
    assert_eq!(user.content_text, "fix it");
}

// ----- fork -----

use common::Harness;
use xade_core::SessionId;

fn pair() -> (Arc<FakeProvider>, Arc<FakeProvider>) {
    (
        Arc::new(FakeProvider::new(ProviderId::Claude)),
        Arc::new(FakeProvider::new(ProviderId::Antigravity)),
    )
}

async fn both(dir: &std::path::Path) -> (Harness, Arc<FakeProvider>, Arc<FakeProvider>) {
    let (claude, agy) = pair();
    let h = common::start_with(dir, vec![claude.clone(), agy.clone()]).await;
    (h, claude, agy)
}

async fn create(h: &mut Harness, provider: ProviderId) -> SessionId {
    h.create_on(provider, "m", Effort::Default, true);
    let ev = h.until_status(5, SessionStatus::Idle).await;
    ev.iter()
        .find_map(|e| match &e.kind {
            AppEventKind::SessionCreated(s) => Some(s.id),
            _ => None,
        })
        .expect("SessionCreated")
}

async fn send(h: &mut Harness, id: SessionId, text: &str) {
    h.tx.send(ManagerCmd::Send {
        session_id: id,
        text: text.into(),
        delivery: None,
        context: None,
    })
    .unwrap();
    h.until_status(5, SessionStatus::Idle).await;
}

async fn open(h: &mut Harness, id: SessionId) {
    h.tx.send(ManagerCmd::Open { session_id: id }).unwrap();
    h.until_status(5, SessionStatus::Idle).await;
}

async fn close(h: &mut Harness, id: SessionId) {
    h.tx.send(ManagerCmd::Close { session_id: id }).unwrap();
    h.until_status(5, SessionStatus::Closed).await;
}

/// Forks `id` and returns the new session id.
async fn fork(h: &mut Harness, id: SessionId, to: ProviderId) -> SessionId {
    h.tx.send(ManagerCmd::Fork {
        session_id: id,
        provider: to,
    })
    .unwrap();
    let ev = h
        .until(5, |e| {
            matches!(
                e.kind,
                AppEventKind::SessionForked { .. } | AppEventKind::Error(_)
            )
        })
        .await;
    match &ev.last().unwrap().kind {
        AppEventKind::SessionForked { from, session_id } => {
            assert_eq!(*from, id);
            *session_id
        }
        other => panic!("fork failed: {other:?}"),
    }
}

const HANDOFF_TAG: &str = "<previous_conversation>";

#[tokio::test]
async fn fork_copies_the_history_into_a_new_xade_session_of_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, _claude, agy) = both(dir.path()).await;
    let src = create(&mut h, ProviderId::Claude).await;
    send(&mut h, src, "remember the word apple").await;
    close(&mut h, src).await;

    let new = fork(&mut h, src, ProviderId::Antigravity).await;
    assert_ne!(new, src);
    let (row, msgs) = h.reader.load_session(new).await.unwrap();
    let (src_row, src_msgs) = h.reader.load_session(src).await.unwrap();
    assert_eq!(row.provider_id, ProviderId::Antigravity);
    assert_eq!(row.provider_session_id, None);
    assert_eq!(row.origin, xade_core::SessionOrigin::Xade);
    assert_eq!(row.workspace_id, src_row.workspace_id);
    assert_eq!(row.title, "Fork: remember the word apple");
    assert!(row.auto_mode, "auto mode is copied");
    assert_eq!(row.usage, UsageTotals::default());
    assert_eq!(row.sent_model_id, "");
    // Nothing is sent: no provider process for the target yet.
    assert!(agy.spawns.lock().unwrap().is_empty());
    assert_eq!(msgs.len(), src_msgs.len());
    assert_eq!(msgs.len(), 4);
    for (i, (a, b)) in src_msgs.iter().zip(&msgs).enumerate() {
        assert_ne!(a.id, b.id);
        assert_eq!(b.session_id, new);
        assert_eq!(b.seq, i as i64 + 1);
        assert_eq!(
            (a.role, &a.content_text, &a.tool_name, &a.tool_payload_json),
            (b.role, &b.content_text, &b.tool_name, &b.tool_payload_json)
        );
        assert_eq!(a.is_error, b.is_error);
    }
    // The source is unchanged, and deleting it does not touch the fork.
    assert_eq!(src_msgs.len(), 4);
    assert_eq!(src_row.provider_session_id.as_deref(), Some("fake-session"));
    h.tx.send(ManagerCmd::DeleteSession { session_id: src })
        .unwrap();
    h.until(5, |e| matches!(e.kind, AppEventKind::SessionDeleted { .. }))
        .await;
    let (_, still) = h.reader.load_session(new).await.unwrap();
    assert_eq!(still.len(), 4);
}

#[tokio::test]
async fn the_first_send_to_a_fork_carries_the_handoff_once() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, _claude, agy) = both(dir.path()).await;
    let src = create(&mut h, ProviderId::Claude).await;
    send(&mut h, src, "remember the word apple").await;
    close(&mut h, src).await;
    let new = fork(&mut h, src, ProviderId::Antigravity).await;

    open(&mut h, new).await;
    send(&mut h, new, "what word was it?").await;
    let prompts = agy.prompts();
    assert_eq!(prompts.len(), 1);
    let p = &prompts[0];
    assert!(p.starts_with(HANDOFF_TAG), "{p}");
    assert!(p.contains("[user] remember the word apple"));
    assert!(p.contains(r#"[tool_call Read] {"file_path":"a.rs"}"#));
    assert!(p.contains("[tool_result] contents"));
    assert!(p.contains("[assistant] pong"));
    assert!(p.ends_with("</previous_conversation>\nwhat word was it?"));

    xade_runtime::flush(&h.writer).await;
    let (row, msgs) = h.reader.load_session(new).await.unwrap();
    let last_user = msgs.iter().rfind(|m| m.role == Role::User).unwrap();
    assert_eq!(last_user.content_text, "what word was it?");
    assert!(msgs.iter().all(|m| !m.content_text.contains(HANDOFF_TAG)));
    assert!(row.provider_session_id.is_some());

    send(&mut h, new, "and again").await;
    assert_eq!(agy.prompts()[1], "and again");
    // Once the provider stored its session id, reopening never repeats it.
    close(&mut h, new).await;
    open(&mut h, new).await;
    send(&mut h, new, "third").await;
    assert_eq!(agy.prompts()[2], "third");
}

#[tokio::test]
async fn a_first_turn_stopped_before_the_provider_started_keeps_the_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, _claude, agy) = both(dir.path()).await;
    let src = create(&mut h, ProviderId::Claude).await;
    send(&mut h, src, "remember the word apple").await;
    close(&mut h, src).await;
    let new = fork(&mut h, src, ProviderId::Antigravity).await;
    open(&mut h, new).await;
    *agy.next_turn.lock().unwrap() = Some(Turn::StopBeforeStart);
    send(&mut h, new, "first try").await;
    send(&mut h, new, "second try").await;
    let prompts = agy.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[0].starts_with(HANDOFF_TAG));
    assert!(prompts[1].starts_with(HANDOFF_TAG), "{}", prompts[1]);
    send(&mut h, new, "third").await;
    assert_eq!(agy.prompts()[2], "third");
}

/// Forks a Claude session into a fresh open agy session, the first send still to come.
async fn fresh_fork(dir: &std::path::Path) -> (Harness, Arc<FakeProvider>, SessionId) {
    let (mut h, _claude, agy) = both(dir).await;
    let src = create(&mut h, ProviderId::Claude).await;
    send(&mut h, src, "remember the word apple").await;
    close(&mut h, src).await;
    let new = fork(&mut h, src, ProviderId::Antigravity).await;
    open(&mut h, new).await;
    (h, agy, new)
}

#[tokio::test]
async fn a_steer_before_the_provider_answers_does_not_drop_the_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, agy, new) = fresh_fork(dir.path()).await;
    *agy.next_turn.lock().unwrap() = Some(Turn::Hold);
    h.tx.send(ManagerCmd::Send {
        session_id: new,
        text: "first".into(),
        delivery: None,
        context: None,
    })
    .unwrap();
    h.until_status(5, SessionStatus::Streaming).await;
    // The steer carries no hand-off, and must not erase the one in flight.
    h.tx.send(ManagerCmd::Send {
        session_id: new,
        text: "steer".into(),
        delivery: None,
        context: None,
    })
    .unwrap();
    h.tx.send(ManagerCmd::Interrupt { session_id: new })
        .unwrap();
    h.until_status(5, SessionStatus::Idle).await;
    send(&mut h, new, "again").await;
    let prompts = agy.prompts();
    assert!(prompts[0].starts_with(HANDOFF_TAG));
    assert_eq!(prompts[1], "steer");
    assert!(prompts[2].starts_with(HANDOFF_TAG), "{}", prompts[2]);
    send(&mut h, new, "last").await;
    assert_eq!(agy.prompts()[3], "last");
}

#[tokio::test]
async fn a_turn_started_then_stopped_without_output_keeps_the_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, agy, new) = fresh_fork(dir.path()).await;
    *agy.next_turn.lock().unwrap() = Some(Turn::StartedThenStop);
    send(&mut h, new, "first try").await;
    send(&mut h, new, "second try").await;
    let prompts = agy.prompts();
    assert!(prompts[0].starts_with(HANDOFF_TAG));
    assert!(prompts[1].starts_with(HANDOFF_TAG), "{}", prompts[1]);
    send(&mut h, new, "third").await;
    assert_eq!(agy.prompts()[2], "third");
}

#[tokio::test]
async fn an_errored_first_turn_without_output_keeps_the_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, agy, new) = fresh_fork(dir.path()).await;
    *agy.next_turn.lock().unwrap() = Some(Turn::ErrorNoOutput);
    send(&mut h, new, "first try").await;
    send(&mut h, new, "second try").await;
    assert!(agy.prompts()[1].starts_with(HANDOFF_TAG));
    send(&mut h, new, "third").await;
    assert_eq!(agy.prompts()[2], "third");
}

#[tokio::test]
async fn the_handoff_survives_closing_the_fork_before_its_first_send() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, claude, _agy) = both(dir.path()).await;
    let src = create(&mut h, ProviderId::Claude).await;
    send(&mut h, src, "first task").await;
    close(&mut h, src).await;
    // Same provider: forking works through the hand-off too.
    let new = fork(&mut h, src, ProviderId::Claude).await;

    open(&mut h, new).await;
    close(&mut h, new).await;
    open(&mut h, new).await;
    // Context is prepended after the hand-off.
    h.tx.send(ManagerCmd::Send {
        session_id: new,
        text: "go on".into(),
        delivery: None,
        context: Some("<ide_opened_file>a.rs</ide_opened_file>".into()),
    })
    .unwrap();
    h.until_status(5, SessionStatus::Idle).await;
    let p = claude.prompts().pop().unwrap();
    assert!(
        p.starts_with(HANDOFF_TAG) && p.contains("[user] first task"),
        "{p}"
    );
    assert!(p.ends_with("</previous_conversation>\n<ide_opened_file>a.rs</ide_opened_file>\ngo on"));
}

#[tokio::test]
async fn a_fork_of_a_fork_does_not_nest_the_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, claude, agy) = both(dir.path()).await;
    let src = create(&mut h, ProviderId::Claude).await;
    send(&mut h, src, "start here").await;
    close(&mut h, src).await;
    let one = fork(&mut h, src, ProviderId::Antigravity).await;
    open(&mut h, one).await;
    send(&mut h, one, "second question").await;
    close(&mut h, one).await;
    assert!(agy.prompts()[0].starts_with(HANDOFF_TAG));

    let two = fork(&mut h, one, ProviderId::Claude).await;
    let (_, msgs) = h.reader.load_session(two).await.unwrap();
    assert_eq!(msgs.len(), 8);
    assert!(msgs.iter().all(|m| !m.content_text.contains(HANDOFF_TAG)));
    open(&mut h, two).await;
    send(&mut h, two, "third question").await;
    let p = claude.prompts().pop().unwrap();
    assert_eq!(p.matches(HANDOFF_TAG).count(), 1, "{p}");
    assert!(p.contains("[user] start here") && p.contains("[user] second question"));
    assert_eq!(p.matches("[user] start here").count(), 1);
}

#[tokio::test]
async fn forking_to_a_missing_provider_fails_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::default());
    let mut h = common::start(dir.path(), provider).await;
    let src = create(&mut h, ProviderId::Claude).await;
    send(&mut h, src, "hello").await;

    h.tx.send(ManagerCmd::Fork {
        session_id: src,
        provider: ProviderId::Antigravity,
    })
    .unwrap();
    let ev = h
        .until(5, |e| matches!(e.kind, AppEventKind::Error(_)))
        .await;
    assert!(matches!(
        &ev.last().unwrap().kind,
        AppEventKind::Error(m) if m.starts_with("Could not fork the session:")
    ));
    xade_runtime::flush(&h.writer).await;
    let page = h.reader.list_sessions(None, None, 0, 24).await.unwrap();
    assert_eq!(page.total, 1);
}

#[tokio::test]
async fn forking_a_live_session_flushes_its_latest_messages_first() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, _claude, _agy) = both(dir.path()).await;
    let src = create(&mut h, ProviderId::Claude).await;
    // Still live: no Close, and no flush of ours before the fork.
    send(&mut h, src, "live question").await;
    let new = fork(&mut h, src, ProviderId::Antigravity).await;
    let (_, msgs) = h.reader.load_session(new).await.unwrap();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs.last().unwrap().content_text, "pong");
}

#[tokio::test]
async fn a_session_without_messages_cannot_be_forked() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, _claude, _agy) = both(dir.path()).await;
    let src = create(&mut h, ProviderId::Claude).await;
    h.tx.send(ManagerCmd::Fork {
        session_id: src,
        provider: ProviderId::Antigravity,
    })
    .unwrap();
    h.until(5, |e| matches!(e.kind, AppEventKind::Error(_)))
        .await;
}

#[tokio::test]
async fn a_fork_starts_from_the_targets_last_model_and_a_supported_effort() {
    let dir = tempfile::tempdir().unwrap();
    let claude = Arc::new(FakeProvider::new(ProviderId::Claude));
    let mut agy = FakeProvider::new(ProviderId::Antigravity);
    agy.models = vec![ModelOption {
        id: "gem".into(),
        label: "Gem".into(),
        description: String::new(),
        effort_levels: vec![Effort::Low],
    }];
    let agy = Arc::new(agy);
    let mut h = common::start_with(dir.path(), vec![claude.clone(), agy.clone()]).await;

    // Target has no session yet: its default model, effort Default.
    let src = create(&mut h, ProviderId::Claude).await;
    send(&mut h, src, "hi").await;
    let first = fork(&mut h, src, ProviderId::Antigravity).await;
    let (row, _) = h.reader.load_session(first).await.unwrap();
    assert_eq!(
        (row.model_id.as_str(), row.effort),
        ("fake", Effort::Default)
    );

    // Target's latest session used "gem" with an effort it no longer lists (Max).
    h.create_on(ProviderId::Antigravity, "gem", Effort::Max, false);
    h.until_status(5, SessionStatus::Idle).await;
    xade_runtime::flush(&h.writer).await;
    let second = fork(&mut h, src, ProviderId::Antigravity).await;
    let (row, _) = h.reader.load_session(second).await.unwrap();
    assert_eq!(
        (row.model_id.as_str(), row.effort),
        ("gem", Effort::Default)
    );
    assert!(
        row.auto_mode,
        "auto mode comes from the source, not the target's last"
    );

    // A level the model lists is kept.
    h.create_on(ProviderId::Antigravity, "gem", Effort::Low, false);
    h.until_status(5, SessionStatus::Idle).await;
    xade_runtime::flush(&h.writer).await;
    let third = fork(&mut h, src, ProviderId::Antigravity).await;
    let (row, _) = h.reader.load_session(third).await.unwrap();
    assert_eq!((row.model_id.as_str(), row.effort), ("gem", Effort::Low));
}

#[tokio::test]
async fn an_imported_session_forks_into_an_xade_session() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, _claude, agy) = both(dir.path()).await;
    let id = SessionId::new();
    let now = time::OffsetDateTime::now_utc();
    let session = xade_core::Session {
        id,
        workspace_id: h.workspace_id,
        provider_id: ProviderId::Claude,
        provider_session_id: Some("imported-1".into()),
        origin: xade_core::SessionOrigin::ClaudeCode,
        title: String::new(),
        preview_snippet: String::new(),
        model_id: "m".into(),
        effort: Effort::Default,
        sent_model_id: String::new(),
        sent_effort: Effort::Default,
        auto_mode: false,
        usage: UsageTotals::default(),
        created_at: now,
        updated_at: now,
    };
    let message = |seq, role, text: &str| xade_core::Message {
        id: uuid::Uuid::new_v4(),
        session_id: id,
        seq,
        role,
        content_text: text.into(),
        thinking_text: None,
        tool_name: None,
        tool_use_id: None,
        tool_payload_json: None,
        is_error: false,
        created_at: now,
    };
    h.writer
        .send(xade_store::WriteOp::ImportSession {
            session,
            messages: vec![message(1, Role::User, "an old task")],
            workspace_root: h.cwd.clone(),
        })
        .unwrap();
    xade_runtime::flush(&h.writer).await;

    let new = fork(&mut h, id, ProviderId::Antigravity).await;
    let (row, msgs) = h.reader.load_session(new).await.unwrap();
    assert_eq!(row.origin, xade_core::SessionOrigin::Xade);
    assert_eq!(row.title, "Fork: an old task");
    assert_eq!(msgs.len(), 1);
    open(&mut h, new).await;
    send(&mut h, new, "continue").await;
    assert!(agy.prompts()[0].contains("[user] an old task"));
}
