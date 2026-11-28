//! The manager over the real `opencode`. Spends a little quota, so the test is `#[ignore]` and
//! also skips unless `XADE_E2E_OPENCODE=1`:
//!
//! `XADE_E2E_OPENCODE=1 cargo test -p xade-runtime --test e2e_opencode -- --ignored --test-threads=1`
//!
//! A cheap model, tiny prompts, a temp store and repo. Every session it creates is deleted
//! through `delete_session`, even when it fails.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::BTreeSet;
use std::process::Command;
use std::sync::Arc;

use common::Harness;
use xade_core::{AgentProvider, Effort, ProviderEvent, ProviderId, SessionId, SessionStatus};
use xade_opencode::OpenCodeProvider;
use xade_runtime::{AppEvent, AppEventKind, ManagerCmd};

const MODEL: &str = "opencode-go/glm-5.3-flash";
const WAIT: u64 = 180;
/// The prompts the app ships for the snapshot turn and the fork's first send.
const HANDOFF_PROMPT: &str = include_str!("../../../assets/prompts/fork-handoff.md");
const RESUME_PROMPT: &str = include_str!("../../../assets/prompts/fork-resume.md");

/// The provider and every session id seen; dropping deletes those sessions.
struct Cleanup {
    provider: Arc<OpenCodeProvider>,
    ids: BTreeSet<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for id in &self.ids {
            if let Err(e) = self.provider.delete_session(id) {
                eprintln!("could not delete session {id}: {e}");
            }
        }
    }
}

struct Ctx {
    h: Harness,
    cleanup: Cleanup,
}

impl Ctx {
    /// Events through the first one `stop` accepts; remembers every session started.
    async fn until(&mut self, stop: impl Fn(&AppEvent) -> bool) -> Vec<AppEvent> {
        let evs = self.h.until(WAIT, stop).await;
        for e in &evs {
            if let AppEventKind::Provider(ProviderEvent::Started {
                provider_session_id,
                ..
            }) = &e.kind
            {
                self.cleanup.ids.insert(provider_session_id.clone());
            }
        }
        evs
    }

    fn prompt(&self, id: SessionId, text: &str) {
        self.h
            .tx
            .send(ManagerCmd::Send {
                session_id: id,
                text: text.into(),
                delivery: None,
                context: None,
            })
            .unwrap();
    }

    fn set_auto(&self, id: SessionId, on: bool) {
        self.h
            .tx
            .send(ManagerCmd::SetAutoMode { session_id: id, on })
            .unwrap();
    }

    /// Sends and returns the events through the turn's end, which must be a good one, without
    /// any permission request.
    async fn unasked(&mut self, id: SessionId, text: &str) -> Vec<AppEvent> {
        self.prompt(id, text);
        let evs = self
            .until(|e| is_status(e, &SessionStatus::Idle) || is_permission(e))
            .await;
        assert!(!evs.iter().any(is_permission), "{evs:?}");
        assert_eq!(turns(&evs), [(false, false)], "{evs:?}");
        evs
    }
}

fn is_status(e: &AppEvent, want: &SessionStatus) -> bool {
    matches!(&e.kind, AppEventKind::Status(s) if s == want)
}

fn is_permission(e: &AppEvent) -> bool {
    matches!(
        &e.kind,
        AppEventKind::Provider(ProviderEvent::PermissionRequest { .. })
    )
}

fn turns(evs: &[AppEvent]) -> Vec<(bool, bool)> {
    evs.iter()
        .filter_map(|e| match &e.kind {
            AppEventKind::Provider(ProviderEvent::TurnComplete {
                is_error,
                interrupted,
                ..
            }) => Some((*is_error, *interrupted)),
            _ => None,
        })
        .collect()
}

fn said(evs: &[AppEvent]) -> String {
    evs.iter()
        .filter_map(|e| match &e.kind {
            AppEventKind::Provider(ProviderEvent::AssistantText { text }) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "spends quota; set XADE_E2E_OPENCODE=1"]
async fn auto_changes_respawn_and_resume_and_a_fork_reads_its_hand_off() {
    if !std::env::var("XADE_E2E_OPENCODE").is_ok_and(|v| v == "1") {
        eprintln!("skipped: set XADE_E2E_OPENCODE=1");
        return;
    }
    let provider = Arc::new(OpenCodeProvider::new());
    provider.preflight().expect("opencode 2 is installed");
    let dir = tempfile::tempdir().unwrap();
    let h = common::start_with(dir.path(), vec![provider.clone()]).await;
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&h.cwd)
        .status()
        .expect("git");
    assert!(git.success());
    let cwd = h.cwd.clone();
    let mut c = Ctx {
        h,
        cleanup: Cleanup {
            provider,
            ids: BTreeSet::new(),
        },
    };

    // Auto off from the start.
    c.h.create_on(ProviderId::OpenCode, MODEL, Effort::Default, false);
    let evs = c.until(|e| is_status(e, &SessionStatus::Idle)).await;
    let id = evs
        .iter()
        .find_map(|e| match &e.kind {
            AppEventKind::SessionCreated(s) => Some(s.id),
            _ => None,
        })
        .expect("SessionCreated");
    c.unasked(id, "Remember the word kiwi. Reply with just: ok")
        .await;

    // On, while idle: the new process runs a command unasked and still knows the word.
    c.set_auto(id, true);
    let evs = c
        .unasked(
            id,
            "Use the shell tool to run exactly `touch auto.txt`, then tell me the word I asked you \
             to remember.",
        )
        .await;
    assert!(cwd.join("auto.txt").exists());
    assert!(said(&evs).contains("kiwi"), "{evs:?}");

    // Off again: the command asks. "Approve & Enable Auto" runs it.
    c.set_auto(id, false);
    c.prompt(
        id,
        "Use the shell tool to run exactly `touch gated.txt`. Then reply DONE.",
    );
    let evs = c
        .until(|e| is_status(e, &SessionStatus::Idle) || is_permission(e))
        .await;
    let request = evs.iter().find_map(|e| match &e.kind {
        AppEventKind::Provider(ProviderEvent::PermissionRequest { request_id, .. }) => {
            Some(request_id.clone())
        }
        _ => None,
    });
    let request = request.unwrap_or_else(|| panic!("Auto off did not ask: {evs:?}"));
    assert!(!cwd.join("gated.txt").exists(), "ran before the approval");
    c.h.tx
        .send(ManagerCmd::RespondPermission {
            session_id: id,
            request_id: request,
            allow: true,
            enable_auto: true,
        })
        .unwrap();
    let evs = c.until(|e| is_status(e, &SessionStatus::Idle)).await;
    assert_eq!(turns(&evs), [(false, false)], "{evs:?}");
    assert!(cwd.join("gated.txt").exists());
    // The respawn that followed the turn keeps the conversation and no longer asks.
    c.unasked(id, "Use the shell tool to run exactly `touch after.txt`.")
        .await;
    assert!(cwd.join("after.txt").exists());

    // Fork onto OpenCode: its snapshot turn (Auto off) reads a file outside the project.
    c.h.tx
        .send(ManagerCmd::Fork {
            session_id: id,
            provider: ProviderId::OpenCode,
            handoff_prompt: HANDOFF_PROMPT.into(),
            resume_prompt: RESUME_PROMPT.into(),
        })
        .unwrap();
    let evs = c
        .until(|e| {
            matches!(
                e.kind,
                AppEventKind::SessionForked { .. } | AppEventKind::ForkFailed { .. }
            )
        })
        .await;
    let fork = match &evs.last().unwrap().kind {
        AppEventKind::SessionForked { session_id, .. } => *session_id,
        other => panic!("fork failed: {other:?}"),
    };
    let snapshot = c.h.forks.join(format!("{}.snapshot.md", fork.0));
    assert!(snapshot.exists(), "the snapshot turn failed");
    c.h.tx.send(ManagerCmd::Open { session_id: fork }).unwrap();
    c.until(|e| is_status(e, &SessionStatus::Idle)).await;
    let evs = c
        .unasked(
            fork,
            "What word did I ask you to remember? Reply with one word.",
        )
        .await;
    assert!(said(&evs).contains("kiwi"), "{evs:?}");

    for session_id in [fork, id] {
        c.h.tx
            .send(ManagerCmd::DeleteSession { session_id })
            .unwrap();
    }
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    c.h.tx.send(ManagerCmd::ShutdownAll(done_tx)).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(60), done_rx)
        .await
        .expect("shutdown in time")
        .unwrap();
}
