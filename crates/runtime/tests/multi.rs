#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, Notify};
use xade_core::{
    AgentProvider, Effort, MultiMemberSpec, ProviderError, ProviderEvent, ProviderId,
    SessionCommand, SessionHandle, SessionStatus, SpawnConfig, TeamMessageKind, UsageTotals,
};
use xade_runtime::{AppEventKind, ManagerCmd};

/// Programmable fake provider that records every spawned config & prompt per model and can
/// optionally hold turns behind a per-model gate to deterministically test busy queuing/steering.
struct TeamProvider {
    spawns: Mutex<Vec<SpawnConfig>>,
    prompts: Mutex<Vec<(String, String)>>,
    replies: Mutex<HashMap<String, Vec<String>>>,
    gates: Mutex<HashMap<String, Arc<Notify>>>,
    deleted: Mutex<Vec<String>>,
    next_psid: Mutex<u32>,
}

impl TeamProvider {
    fn new() -> Self {
        Self {
            spawns: Mutex::default(),
            prompts: Mutex::default(),
            replies: Mutex::default(),
            gates: Mutex::default(),
            deleted: Mutex::default(),
            next_psid: Mutex::new(1),
        }
    }

    fn set_replies(&self, model: &str, list: Vec<&str>) {
        self.replies.lock().unwrap().insert(
            model.to_string(),
            list.into_iter().map(str::to_string).collect(),
        );
    }

    fn set_gate(&self, model: &str, gate: Arc<Notify>) {
        self.gates.lock().unwrap().insert(model.to_string(), gate);
    }

    fn prompts_for(&self, model: &str) -> Vec<String> {
        self.prompts
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == model)
            .map(|(_, p)| p.clone())
            .collect()
    }
}

#[derive(Clone)]
struct SharedProvider {
    id: ProviderId,
    inner: Arc<TeamProvider>,
}

impl SharedProvider {
    fn new(id: ProviderId) -> Self {
        Self {
            id,
            inner: Arc::new(TeamProvider::new()),
        }
    }
}

impl AgentProvider for SharedProvider {
    fn id(&self) -> ProviderId {
        self.id
    }
    fn display_name(&self) -> &'static str {
        match self.id {
            ProviderId::Claude => "Claude",
            ProviderId::Antigravity => "Antigravity",
            ProviderId::Codex => "Codex",
        }
    }
    fn models(&self) -> Vec<xade_core::ModelOption> {
        Vec::new()
    }
    fn default_model(&self) -> String {
        "default".into()
    }
    fn effort_levels(&self) -> Vec<Effort> {
        vec![Effort::Default]
    }
    fn preflight(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    fn spawn(&self, cfg: SpawnConfig) -> Result<SessionHandle, ProviderError> {
        let model = cfg.model_id.clone();
        let psid = cfg.resume.clone().unwrap_or_else(|| {
            let mut n = self.inner.next_psid.lock().unwrap();
            let id = format!("{:?}-{model}-{}", self.id, *n);
            *n += 1;
            id
        });
        self.inner.spawns.lock().unwrap().push(cfg);
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let mut in_turn = false;
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    SessionCommand::SendPrompt(prompt) => {
                        inner.prompts.lock().unwrap().push((model.clone(), prompt));
                        if in_turn {
                            // Steered mid-turn: folded into current turn; no second TurnComplete.
                            continue;
                        }
                        in_turn = true;
                        let _ = ev_tx.send(ProviderEvent::Started {
                            provider_session_id: psid.clone(),
                            model_id: model.clone(),
                        });
                        let gate = inner.gates.lock().unwrap().get(&model).cloned();
                        if let Some(gate) = gate {
                            // Keep receiving steered prompts or interrupts until gate opens.
                            loop {
                                tokio::select! {
                                    _ = gate.notified() => break,
                                    next = cmd_rx.recv() => match next {
                                        Some(SessionCommand::SendPrompt(p)) => {
                                            inner.prompts.lock().unwrap().push((model.clone(), p));
                                        }
                                        Some(SessionCommand::Interrupt) => {
                                            let _ = ev_tx.send(ProviderEvent::TurnComplete {
                                                usage: UsageTotals::default(),
                                                is_error: false,
                                                interrupted: true,
                                            });
                                            in_turn = false;
                                            break;
                                        }
                                        Some(SessionCommand::Shutdown) | None => {
                                            let _ = ev_tx.send(ProviderEvent::Exited { code: Some(0) });
                                            return;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            if !in_turn {
                                continue;
                            }
                        }
                        let reply = inner
                            .replies
                            .lock()
                            .unwrap()
                            .get_mut(&model)
                            .and_then(|v| {
                                if v.is_empty() {
                                    None
                                } else {
                                    Some(v.remove(0))
                                }
                            })
                            .unwrap_or_else(|| format!("ack from {model}"));
                        let _ = ev_tx.send(ProviderEvent::AssistantText { text: reply });
                        let _ = ev_tx.send(ProviderEvent::TurnComplete {
                            usage: UsageTotals::default(),
                            is_error: false,
                            interrupted: false,
                        });
                        in_turn = false;
                    }
                    SessionCommand::Interrupt => {
                        if in_turn {
                            let _ = ev_tx.send(ProviderEvent::TurnComplete {
                                usage: UsageTotals::default(),
                                is_error: false,
                                interrupted: true,
                            });
                            in_turn = false;
                        }
                    }
                    SessionCommand::Shutdown => {
                        let _ = ev_tx.send(ProviderEvent::Exited { code: Some(0) });
                        return;
                    }
                    SessionCommand::RespondPermission { .. } | SessionCommand::SetAutoMode(_) => {}
                }
            }
            let _ = ev_tx.send(ProviderEvent::Exited { code: Some(0) });
        });
        Ok(SessionHandle {
            commands: cmd_tx,
            events: ev_rx,
        })
    }

    fn delete_session(&self, psid: &str) -> Result<(), ProviderError> {
        self.inner.deleted.lock().unwrap().push(psid.to_string());
        Ok(())
    }
}

fn spec(provider_id: ProviderId, model_id: &str) -> MultiMemberSpec {
    MultiMemberSpec {
        provider_id,
        model_id: model_id.into(),
        effort: Effort::Default,
        auto_mode: true,
    }
}

#[tokio::test]
async fn lazy_spawn_targeted_send_and_broadcast_routing() {
    let dir = tempfile::tempdir().unwrap();
    let claude = SharedProvider::new(ProviderId::Claude);
    let agy = SharedProvider::new(ProviderId::Antigravity);
    claude.inner.set_replies(
        "claude-opus-4-6",
        vec!["Planning for the user.\n<broadcast>Sonnet do A, Neon do B</broadcast>"],
    );
    claude
        .inner
        .set_replies("claude-sonnet-4-6", vec!["Starting part A (user only)."]);
    agy.inner
        .set_replies("gemini-4-neon", vec!["Starting part B (user only)."]);

    let mut h = common::start_with(
        dir.path(),
        vec![Arc::new(claude.clone()), Arc::new(agy.clone())],
    )
    .await;

    h.tx.send(ManagerCmd::CreateMulti {
        workspace_id: h.workspace_id,
        cwd: h.cwd.clone(),
        title: String::new(),
        members: vec![
            spec(ProviderId::Claude, "claude-opus-4-6"),
            spec(ProviderId::Claude, "claude-sonnet-4-6"),
            spec(ProviderId::Antigravity, "gemini-4-neon"),
        ],
    })
    .unwrap();
    let evs = h
        .until(5, |e| matches!(e.kind, AppEventKind::MultiCreated { .. }))
        .await;
    let (multi_id, member_ids) = evs
        .iter()
        .find_map(|e| match &e.kind {
            AppEventKind::MultiCreated { multi, members } => {
                Some((multi.id, members.iter().map(|m| m.id).collect::<Vec<_>>()))
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(member_ids.len(), 3);
    assert_eq!(claude.inner.spawns.lock().unwrap().len(), 0, "all dormant");
    assert_eq!(agy.inner.spawns.lock().unwrap().len(), 0);

    // Send targeted message to #opus-1 only: Opus broadcasts, waking Sonnet and Neon.
    h.tx.send(ManagerCmd::SendMulti {
        multi_id,
        text: "#opus-1: Plan Feature X".into(),
        steer: false,
        context: None,
    })
    .unwrap();

    // Wait until all 3 members have completed their turns.
    let mut completed = std::collections::HashSet::new();
    while completed.len() < 3 {
        let evs = h
            .until(5, |e| {
                matches!(
                    e.kind,
                    AppEventKind::Provider(ProviderEvent::TurnComplete { .. })
                )
            })
            .await;
        for e in evs {
            if let (Some(sid), AppEventKind::Provider(ProviderEvent::TurnComplete { .. })) =
                (e.session_id, e.kind)
            {
                completed.insert(sid);
            }
        }
    }
    xade_runtime::flush(&h.writer).await;

    // Opus got the user prompt + team preamble; Sonnet & Neon got ONLY the broadcast + preamble.
    let opus_prompts = claude.inner.prompts_for("claude-opus-4-6");
    assert_eq!(opus_prompts.len(), 1);
    assert!(opus_prompts[0].contains("You are #opus-1"));
    assert!(opus_prompts[0].contains("Plan Feature X"));

    let sonnet_prompts = claude.inner.prompts_for("claude-sonnet-4-6");
    assert_eq!(sonnet_prompts.len(), 1);
    assert!(sonnet_prompts[0].contains("You are #sonnet-1"));
    assert!(sonnet_prompts[0].contains("Sonnet do A, Neon do B"));
    assert!(!sonnet_prompts[0].contains("Planning for the user"));

    let neon_prompts = agy.inner.prompts_for("gemini-4-neon");
    assert_eq!(neon_prompts.len(), 1);
    assert!(neon_prompts[0].contains("You are #neon-1"));
    assert!(neon_prompts[0].contains("Sonnet do A, Neon do B"));

    // Store persisted title derivation, team messages, and sender attribution on member messages.
    let loaded = h.reader.load_multi_session(multi_id).await.unwrap();
    assert_eq!(loaded.multi.title, "Plan Feature X");
    assert_eq!(loaded.team_messages.len(), 2);
    assert_eq!(
        loaded.team_messages[0].kind,
        TeamMessageKind::User {
            target: Some("opus-1".into())
        }
    );
    assert_eq!(
        loaded.team_messages[1].kind,
        TeamMessageKind::Broadcast {
            sender_handle: "opus-1".into(),
            sender_provider: ProviderId::Claude,
        }
    );
    assert_eq!(
        loaded.members[1].1[0].sender_handle.as_deref(),
        Some("opus-1")
    );
}

#[tokio::test]
async fn broadcasts_queue_and_coalesce_while_user_can_steer() {
    let dir = tempfile::tempdir().unwrap();
    let claude = SharedProvider::new(ProviderId::Claude);
    let sonnet_gate = Arc::new(Notify::new());
    claude
        .inner
        .set_gate("claude-sonnet-4-6", sonnet_gate.clone());
    claude.inner.set_replies(
        "claude-opus-4-6",
        vec![
            "<broadcast>First update</broadcast>",
            "<broadcast>Second update</broadcast>",
        ],
    );
    claude
        .inner
        .set_replies("claude-sonnet-4-6", vec!["done first", "done coalesced"]);

    let mut h = common::start(dir.path(), Arc::new(claude.clone())).await;
    h.tx.send(ManagerCmd::CreateMulti {
        workspace_id: h.workspace_id,
        cwd: h.cwd.clone(),
        title: "Queue Test".into(),
        members: vec![
            spec(ProviderId::Claude, "claude-opus-4-6"),
            spec(ProviderId::Claude, "claude-sonnet-4-6"),
        ],
    })
    .unwrap();
    let evs = h
        .until(5, |e| matches!(e.kind, AppEventKind::MultiCreated { .. }))
        .await;
    let (multi_id, opus_id, sonnet_id) = evs
        .iter()
        .find_map(|e| match &e.kind {
            AppEventKind::MultiCreated { multi, members } => {
                Some((multi.id, members[0].id, members[1].id))
            }
            _ => None,
        })
        .unwrap();

    // Start a held turn on sonnet-1.
    h.tx.send(ManagerCmd::SendMulti {
        multi_id,
        text: "#sonnet-1: long task".into(),
        steer: false,
        context: None,
    })
    .unwrap();
    h.until(5, |e| {
        e.session_id == Some(sonnet_id)
            && matches!(e.kind, AppEventKind::Status(SessionStatus::Streaming))
    })
    .await;

    // While sonnet-1 is busy, opus-1 sends two broadcasts in separate turns.
    for msg in ["#opus-1: send 1", "#opus-1: send 2"] {
        h.tx.send(ManagerCmd::SendMulti {
            multi_id,
            text: msg.into(),
            steer: false,
            context: None,
        })
        .unwrap();
        h.until(5, |e| {
            e.session_id == Some(opus_id)
                && matches!(e.kind, AppEventKind::Status(SessionStatus::Idle))
        })
        .await;
    }

    // Sonnet must NOT have received either broadcast while Streaming.
    assert_eq!(claude.inner.prompts_for("claude-sonnet-4-6").len(), 1);

    // User CAN steer sonnet-1 mid-turn.
    h.tx.send(ManagerCmd::SendMulti {
        multi_id,
        text: "#sonnet-1: user steer now".into(),
        steer: true,
        context: None,
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mid_prompts = claude.inner.prompts_for("claude-sonnet-4-6");
    assert_eq!(mid_prompts.len(), 2, "user steer delivered mid-turn");
    assert!(mid_prompts[1].contains("user steer now"));

    // Release sonnet-1's gate: once Idle, both queued broadcasts coalesce into ONE turn.
    claude
        .inner
        .gates
        .lock()
        .unwrap()
        .remove("claude-sonnet-4-6");
    sonnet_gate.notify_one();
    // First Idle ends the steered turn; second Idle ends the coalesced broadcast turn.
    let mut sonnet_idles = 0;
    while sonnet_idles < 2 {
        h.until(5, |e| {
            e.session_id == Some(sonnet_id)
                && matches!(e.kind, AppEventKind::Status(SessionStatus::Idle))
        })
        .await;
        sonnet_idles += 1;
    }

    let final_prompts = claude.inner.prompts_for("claude-sonnet-4-6");
    assert_eq!(final_prompts.len(), 3, "two broadcasts coalesced into one");
    assert!(final_prompts[2].contains("First update"));
    assert!(final_prompts[2].contains("Second update"));
}

#[tokio::test]
async fn hop_cap_pauses_broadcast_loops_and_interrupt_clears_queues() {
    let dir = tempfile::tempdir().unwrap();
    let claude = SharedProvider::new(ProviderId::Claude);
    // Both agents keep replying with <broadcast>, simulating a ping-pong loop.
    claude
        .inner
        .set_replies("claude-opus-4-6", vec!["<broadcast>ping</broadcast>"; 20]);
    claude
        .inner
        .set_replies("claude-sonnet-4-6", vec!["<broadcast>pong</broadcast>"; 20]);

    let mut h = common::start(dir.path(), Arc::new(claude.clone())).await;
    h.tx.send(ManagerCmd::CreateMulti {
        workspace_id: h.workspace_id,
        cwd: h.cwd.clone(),
        title: "Loop Cap".into(),
        members: vec![
            spec(ProviderId::Claude, "claude-opus-4-6"),
            spec(ProviderId::Claude, "claude-sonnet-4-6"),
        ],
    })
    .unwrap();
    let evs = h
        .until(5, |e| matches!(e.kind, AppEventKind::MultiCreated { .. }))
        .await;
    let multi_id = evs
        .iter()
        .find_map(|e| match &e.kind {
            AppEventKind::MultiCreated { multi, .. } => Some(multi.id),
            _ => None,
        })
        .unwrap();

    h.tx.send(ManagerCmd::SendMulti {
        multi_id,
        text: "#opus-1: start ping pong".into(),
        steer: false,
        context: None,
    })
    .unwrap();

    // Must hit the hop cap and emit MultiBroadcastPaused { paused: true }.
    h.until(5, |e| {
        matches!(
            e.kind,
            AppEventKind::MultiBroadcastPaused { paused: true, .. }
        )
    })
    .await;

    // InterruptMultiAll clears queues and resets pause state.
    h.tx.send(ManagerCmd::InterruptMultiAll { multi_id })
        .unwrap();
    h.until(5, |e| {
        matches!(
            e.kind,
            AppEventKind::MultiBroadcastPaused { paused: false, .. }
        )
    })
    .await;
}

#[tokio::test]
async fn close_reopen_model_lock_and_delete_multi() {
    let dir = tempfile::tempdir().unwrap();
    let claude = SharedProvider::new(ProviderId::Claude);
    let mut h = common::start(dir.path(), Arc::new(claude.clone())).await;

    h.tx.send(ManagerCmd::CreateMulti {
        workspace_id: h.workspace_id,
        cwd: h.cwd.clone(),
        title: "Persist Team".into(),
        members: vec![
            spec(ProviderId::Claude, "claude-opus-4-6"),
            spec(ProviderId::Claude, "claude-sonnet-4-6"),
        ],
    })
    .unwrap();
    let evs = h
        .until(5, |e| matches!(e.kind, AppEventKind::MultiCreated { .. }))
        .await;
    let (multi_id, opus_id) = evs
        .iter()
        .find_map(|e| match &e.kind {
            AppEventKind::MultiCreated { multi, members } => Some((multi.id, members[0].id)),
            _ => None,
        })
        .unwrap();

    // Model changes on multi-session members are ignored.
    h.tx.send(ManagerCmd::SetModel {
        session_id: opus_id,
        model_id: "claude-haiku-4-5".into(),
    })
    .unwrap();
    h.tx.send(ManagerCmd::SendMulti {
        multi_id,
        text: "#opus-1: hello".into(),
        steer: false,
        context: None,
    })
    .unwrap();
    h.until(5, |e| {
        e.session_id == Some(opus_id) && matches!(e.kind, AppEventKind::Status(SessionStatus::Idle))
    })
    .await;
    xade_runtime::flush(&h.writer).await;
    assert_eq!(
        claude.inner.spawns.lock().unwrap()[0].model_id,
        "claude-opus-4-6"
    );

    // Close and reopen: stays dormant until next send, and resumes with provider_session_id.
    h.tx.send(ManagerCmd::CloseMulti { multi_id }).unwrap();
    h.until(5, |e| matches!(e.kind, AppEventKind::MultiClosed { .. }))
        .await;
    let spawns_before = claude.inner.spawns.lock().unwrap().len();
    h.tx.send(ManagerCmd::OpenMulti { multi_id }).unwrap();
    let opened = h
        .until(5, |e| matches!(e.kind, AppEventKind::MultiOpened { .. }))
        .await;
    assert_eq!(
        claude.inner.spawns.lock().unwrap().len(),
        spawns_before,
        "reopen stays dormant"
    );
    assert!(opened.iter().any(
        |e| matches!(&e.kind, AppEventKind::MultiOpened { members, .. } if members.len() == 2)
    ));

    // Sending after reopen: opus-1 resumes with provider_session_id (no duplicate preamble),
    // while sonnet-1 (never spawned before) spawns fresh with its team preamble.
    h.tx.send(ManagerCmd::SendMulti {
        multi_id,
        text: "continue after reopen".into(),
        steer: false,
        context: None,
    })
    .unwrap();
    let mut completed = std::collections::HashSet::new();
    while completed.len() < 2 {
        let evs = h
            .until(5, |e| {
                matches!(
                    e.kind,
                    AppEventKind::Provider(ProviderEvent::TurnComplete { .. })
                )
            })
            .await;
        for e in evs {
            if let (Some(sid), AppEventKind::Provider(ProviderEvent::TurnComplete { .. })) =
                (e.session_id, e.kind)
            {
                completed.insert(sid);
            }
        }
    }
    let spawns = claude.inner.spawns.lock().unwrap().clone();
    assert_eq!(spawns.len(), 3);
    assert!(
        spawns[1..]
            .iter()
            .any(|s| s.model_id == "claude-opus-4-6" && s.resume.is_some()),
        "reopened opus-1 must resume"
    );
    assert!(
        spawns[1..]
            .iter()
            .any(|s| s.model_id == "claude-sonnet-4-6" && s.resume.is_none()),
        "first-time sonnet-1 spawns fresh"
    );
    let opus_prompts = claude.inner.prompts_for("claude-opus-4-6");
    assert!(
        !opus_prompts[1].contains("[Team Session Instructions]"),
        "resumed member should not repeat preamble"
    );
    let sonnet_prompts = claude.inner.prompts_for("claude-sonnet-4-6");
    assert!(sonnet_prompts[0].contains("You are #sonnet-1"));

    // DeleteMulti deletes provider transcript(s) and cascades DB rows.
    h.tx.send(ManagerCmd::DeleteMulti { multi_id }).unwrap();
    h.until(5, |e| matches!(e.kind, AppEventKind::MultiDeleted { .. }))
        .await;
    assert_eq!(claude.inner.deleted.lock().unwrap().len(), 2);
    assert!(h.reader.load_multi_session(multi_id).await.is_err());
}

#[tokio::test]
async fn queued_user_beats_broadcasts_and_member_guards_reject_bypass() {
    let dir = tempfile::tempdir().unwrap();
    let claude = SharedProvider::new(ProviderId::Claude);
    let sonnet_gate = Arc::new(Notify::new());
    claude
        .inner
        .set_gate("claude-sonnet-4-6", sonnet_gate.clone());
    claude
        .inner
        .set_replies("claude-opus-4-6", vec!["<broadcast>from opus</broadcast>"]);
    claude
        .inner
        .set_replies("claude-sonnet-4-6", vec!["t1", "t2", "t3"]);

    let mut h = common::start(dir.path(), Arc::new(claude.clone())).await;
    h.tx.send(ManagerCmd::CreateMulti {
        workspace_id: h.workspace_id,
        cwd: h.cwd.clone(),
        title: "Guards".into(),
        members: vec![
            spec(ProviderId::Claude, "claude-opus-4-6"),
            spec(ProviderId::Claude, "claude-sonnet-4-6"),
        ],
    })
    .unwrap();
    let evs = h
        .until(5, |e| matches!(e.kind, AppEventKind::MultiCreated { .. }))
        .await;
    let (multi_id, opus_id, sonnet_id) = evs
        .iter()
        .find_map(|e| match &e.kind {
            AppEventKind::MultiCreated { multi, members } => {
                Some((multi.id, members[0].id, members[1].id))
            }
            _ => None,
        })
        .unwrap();

    // Direct Send / DeleteSession / Fork on a member must be rejected.
    h.tx.send(ManagerCmd::Send {
        session_id: opus_id,
        text: "bypass".into(),
        delivery: None,
        context: None,
    })
    .unwrap();
    h.until(5, |e| matches!(e.kind, AppEventKind::Error(_)))
        .await;

    h.tx.send(ManagerCmd::DeleteSession {
        session_id: opus_id,
    })
    .unwrap();
    h.until(5, |e| matches!(e.kind, AppEventKind::Error(_)))
        .await;

    h.tx.send(ManagerCmd::Fork {
        session_id: opus_id,
        provider: ProviderId::Claude,
        handoff_prompt: String::new(),
        resume_prompt: String::new(),
    })
    .unwrap();
    h.until(5, |e| matches!(e.kind, AppEventKind::ForkFailed { .. }))
        .await;

    // Hold sonnet-1 busy, enqueue a broadcast first, then a non-steering user message.
    h.tx.send(ManagerCmd::SendMulti {
        multi_id,
        text: "#sonnet-1: first".into(),
        steer: false,
        context: None,
    })
    .unwrap();
    h.until(5, |e| {
        e.session_id == Some(sonnet_id)
            && matches!(e.kind, AppEventKind::Status(SessionStatus::Streaming))
    })
    .await;

    h.tx.send(ManagerCmd::SendMulti {
        multi_id,
        text: "#opus-1: broadcast now".into(),
        steer: false,
        context: None,
    })
    .unwrap();
    h.until(5, |e| {
        e.session_id == Some(opus_id) && matches!(e.kind, AppEventKind::Status(SessionStatus::Idle))
    })
    .await;

    h.tx.send(ManagerCmd::SendMulti {
        multi_id,
        text: "#sonnet-1: user queued".into(),
        steer: false,
        context: None,
    })
    .unwrap();

    // Release sonnet-1: user queued prompt must run BEFORE the queued broadcast.
    claude
        .inner
        .gates
        .lock()
        .unwrap()
        .remove("claude-sonnet-4-6");
    sonnet_gate.notify_one();
    for _ in 0..3 {
        h.until(5, |e| {
            e.session_id == Some(sonnet_id)
                && matches!(e.kind, AppEventKind::Status(SessionStatus::Idle))
        })
        .await;
    }
    let prompts = claude.inner.prompts_for("claude-sonnet-4-6");
    assert_eq!(prompts.len(), 3);
    assert!(prompts[1].contains("user queued"));
    assert!(prompts[2].contains("from opus"));
}
