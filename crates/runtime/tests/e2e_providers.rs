//! The manager over the real `claude` and `agy`. Spends a little quota, so each test is
//! `#[ignore]` and also skips unless `XADE_E2E_AGY=1`:
//!
//! `XADE_E2E_AGY=1 cargo test -p xade-runtime --test e2e_providers -- --ignored --test-threads=1`
//!
//! Cheap models only (`haiku`, `gemini-3.8-flash`), tiny prompts, a temp store. Every provider
//! conversation a test created is deleted through `delete_session`, even when it fails.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::Harness;
use tokio::sync::oneshot;
use xade_antigravity::AntigravityProvider;
use xade_claude::ClaudeProvider;
use xade_codex::CodexProvider;
use xade_core::{AgentProvider, Effort, ProviderEvent, ProviderId, Role, SessionId, SessionStatus};
use xade_runtime::{AppEvent, AppEventKind, ManagerCmd};

const CLAUDE: &str = "haiku";
const AGY: &str = "gemini-3.8-flash";
const CODEX: &str = "gpt-6-luna";
const WAIT: u64 = 180;
const REMEMBER: &str = "Remember the word kiwi. Reply with just: ok";
/// The prompt the app ships for the snapshot turn.
const HANDOFF_PROMPT: &str = include_str!("../../../assets/prompts/fork-handoff.md");
const RESUME_PROMPT: &str = include_str!("../../../assets/prompts/fork-resume.md");
const RECALL: &str = "What word did I ask you to remember? Reply with one word.";

fn enabled() -> bool {
    std::env::var("XADE_E2E_AGY").is_ok_and(|v| v == "1")
}

macro_rules! e2e {
    () => {
        if !enabled() {
            eprintln!("skipped: set XADE_E2E_AGY=1");
            return;
        }
    };
}

/// One at a time: the tests count this process's children.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Conversations seen in `Started` events; dropping deletes them (twice after a panic, since a
/// child still shutting down may write its files once more).
struct Cleanup {
    claude: Arc<ClaudeProvider>,
    agy: Arc<AntigravityProvider>,
    ids: Mutex<Vec<(ProviderId, String)>>,
}

impl Cleanup {
    fn delete_all(&self) {
        for (provider, id) in self.ids.lock().unwrap().iter() {
            let res = match provider {
                ProviderId::Claude => self.claude.delete_session(id),
                ProviderId::Antigravity => self.agy.delete_session(id),
                ProviderId::Codex => CodexProvider::new().delete_session(id),
            };
            if let Err(e) = res {
                eprintln!("could not delete conversation {id}: {e}");
            }
        }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.delete_all();
        if std::thread::panicking() {
            std::thread::sleep(Duration::from_secs(4));
            self.delete_all();
        }
    }
}

struct Ctx {
    h: Harness,
    cleanup: Arc<Cleanup>,
    _dir: tempfile::TempDir,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

async fn ctx() -> Ctx {
    let serial = SERIAL.lock().await;
    let claude = Arc::new(ClaudeProvider::new());
    let agy = Arc::new(AntigravityProvider::new());
    claude
        .preflight()
        .expect("claude is installed and signed in");
    agy.preflight().expect("agy is installed and signed in");
    let dir = tempfile::tempdir().unwrap();
    let h = common::start_with(dir.path(), vec![claude.clone(), agy.clone()]).await;
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&h.cwd)
        .status()
        .expect("git");
    assert!(git.success());
    Ctx {
        h,
        cleanup: Arc::new(Cleanup {
            claude,
            agy,
            ids: Mutex::default(),
        }),
        _dir: dir,
        _serial: serial,
    }
}

fn model(p: ProviderId) -> &'static str {
    match p {
        ProviderId::Claude => CLAUDE,
        ProviderId::Antigravity => AGY,
        ProviderId::Codex => CODEX,
    }
}

impl Ctx {
    /// Events through the first one `stop` accepts; remembers every conversation started.
    async fn until(&mut self, stop: impl Fn(&AppEvent) -> bool) -> Vec<AppEvent> {
        let evs = self.h.until(WAIT, stop).await;
        for e in &evs {
            if let (
                Some(id),
                AppEventKind::Provider(ProviderEvent::Started {
                    provider_session_id,
                    ..
                }),
            ) = (e.session_id, &e.kind)
            {
                let provider = self.provider_of(id).await;
                let mut ids = self.cleanup.ids.lock().unwrap();
                let key = (provider, provider_session_id.clone());
                if !ids.contains(&key) {
                    ids.push(key);
                }
            }
        }
        evs
    }

    async fn provider_of(&self, id: SessionId) -> ProviderId {
        xade_runtime::flush(&self.h.writer).await;
        self.h.reader.load_session(id).await.unwrap().0.provider_id
    }

    async fn create(&mut self, p: ProviderId) -> SessionId {
        self.h.create_on(p, model(p), Effort::Default, true);
        let evs = self.until(|e| is_status(e, &SessionStatus::Idle)).await;
        evs.iter()
            .find_map(|e| match &e.kind {
                AppEventKind::SessionCreated(s) => Some(s.id),
                _ => None,
            })
            .expect("SessionCreated")
    }

    /// Sends and returns what the assistant said (lower-cased); the turn must not be an error.
    async fn send(&mut self, id: SessionId, text: &str) -> String {
        self.h
            .tx
            .send(ManagerCmd::Send {
                session_id: id,
                text: text.into(),
                delivery: None,
                context: None,
            })
            .unwrap();
        let evs = self.until(|e| is_status(e, &SessionStatus::Idle)).await;
        let turns: Vec<_> = evs
            .iter()
            .filter_map(|e| match &e.kind {
                AppEventKind::Provider(ProviderEvent::TurnComplete {
                    is_error,
                    interrupted,
                    ..
                }) => Some((*is_error, *interrupted)),
                _ => None,
            })
            .collect();
        assert_eq!(turns, [(false, false)], "{evs:?}");
        evs.iter()
            .filter_map(|e| match &e.kind {
                AppEventKind::Provider(ProviderEvent::AssistantText { text }) => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase()
    }

    async fn open(&mut self, id: SessionId) {
        self.h.tx.send(ManagerCmd::Open { session_id: id }).unwrap();
        self.until(|e| is_status(e, &SessionStatus::Idle)).await;
    }

    async fn close(&mut self, id: SessionId) {
        self.h
            .tx
            .send(ManagerCmd::Close { session_id: id })
            .unwrap();
        self.until(|e| is_status(e, &SessionStatus::Closed)).await;
    }

    async fn fork(&mut self, id: SessionId, to: ProviderId) -> SessionId {
        self.h
            .tx
            .send(ManagerCmd::Fork {
                session_id: id,
                provider: to,
                handoff_prompt: HANDOFF_PROMPT.into(),
                resume_prompt: RESUME_PROMPT.into(),
            })
            .unwrap();
        let evs = self
            .until(|e| {
                matches!(
                    e.kind,
                    AppEventKind::SessionForked { .. } | AppEventKind::ForkFailed { .. }
                )
            })
            .await;
        match &evs.last().unwrap().kind {
            AppEventKind::SessionForked { session_id, .. } => *session_id,
            other => panic!("fork failed: {other:?}"),
        }
    }

    /// Every provider child gone after `ShutdownAll`.
    async fn shutdown(&mut self) {
        let (done_tx, done_rx) = oneshot::channel();
        self.h.tx.send(ManagerCmd::ShutdownAll(done_tx)).unwrap();
        tokio::time::timeout(Duration::from_secs(30), done_rx)
            .await
            .expect("shutdown in time")
            .unwrap();
        assert!(
            wait_children(0).await.is_empty(),
            "children left: {:?}",
            children()
        );
    }

    async fn stored_user_texts(&self, id: SessionId) -> Vec<String> {
        xade_runtime::flush(&self.h.writer).await;
        let (_, msgs) = self.h.reader.load_session(id).await.unwrap();
        msgs.into_iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.content_text)
            .collect()
    }
}

fn is_status(e: &AppEvent, want: &SessionStatus) -> bool {
    matches!(&e.kind, AppEventKind::Status(s) if s == want)
}

/// Pids of this process's children (`claude -p` and `agy acp`; the tests run one at a time).
fn children() -> Vec<u32> {
    let out = Command::new("pgrep")
        .args(["-P", &std::process::id().to_string()])
        .output()
        .expect("pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

async fn wait_children(want: usize) -> Vec<u32> {
    let start = Instant::now();
    loop {
        let c = children();
        if c.len() == want || start.elapsed() > Duration::from_secs(10) {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn agy_conversation(id: &str) -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME"))
        .join(".gemini/antigravity-cli/conversations")
        .join(format!("{id}.db"))
}

/// Create, send, close, reopen (dormant) and send again: the resumed session remembers.
async fn resume_keeps_context(p: ProviderId) {
    let mut c = ctx().await;
    let id = c.create(p).await;
    c.send(id, REMEMBER).await;
    c.close(id).await;
    c.open(id).await;
    assert!(
        wait_children(0).await.is_empty(),
        "opening a session starts no process"
    );
    let said = c.send(id, RECALL).await;
    assert!(said.contains("kiwi"), "{said}");
    c.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn claude_create_send_close_open_send_resumes_with_context() {
    e2e!();
    resume_keeps_context(ProviderId::Claude).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn antigravity_create_send_close_open_send_resumes_with_context() {
    e2e!();
    resume_keeps_context(ProviderId::Antigravity).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn antigravity_set_model_respawns_and_keeps_the_conversation() {
    e2e!();
    let mut c = ctx().await;
    let id = c.create(ProviderId::Antigravity).await;
    c.send(id, REMEMBER).await;
    let (before, _) = {
        xade_runtime::flush(&c.h.writer).await;
        let (row, _) = c.h.reader.load_session(id).await.unwrap();
        (row.provider_session_id, row.model_id)
    };
    let pids = wait_children(1).await;
    assert_eq!(pids.len(), 1);

    c.h.tx
        .send(ManagerCmd::SetModel {
            session_id: id,
            model_id: "gemini-4-hydrogen".into(),
        })
        .unwrap();
    // The respawn happens at once (the session is idle); wait for the new child.
    let start = Instant::now();
    while children() == pids && start.elapsed() < Duration::from_secs(15) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_ne!(children(), pids, "a new process after the model change");
    let said = c.send(id, RECALL).await;
    assert!(said.contains("kiwi"), "{said}");
    xade_runtime::flush(&c.h.writer).await;
    let (row, _) = c.h.reader.load_session(id).await.unwrap();
    assert_eq!(row.model_id, "gemini-4-hydrogen");
    assert_eq!(row.provider_session_id, before, "same conversation");
    c.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn delete_session_removes_the_antigravity_conversation() {
    e2e!();
    let mut c = ctx().await;
    let id = c.create(ProviderId::Antigravity).await;
    c.send(id, "Reply with just: ok").await;
    xade_runtime::flush(&c.h.writer).await;
    let psid =
        c.h.reader
            .load_session(id)
            .await
            .unwrap()
            .0
            .provider_session_id
            .expect("provider session id");
    assert!(agy_conversation(&psid).exists());
    c.h.tx
        .send(ManagerCmd::DeleteSession { session_id: id })
        .unwrap();
    c.until(|e| matches!(e.kind, AppEventKind::SessionDeleted { .. }))
        .await;
    assert!(!agy_conversation(&psid).exists(), "conversation file gone");
    c.shutdown().await;
}

/// Names of the conversations the providers hold for the test's folder (Claude) and in
/// total (agy, whose files are not tied to a folder): a throwaway one left behind shows up.
fn conversations(c: &Ctx) -> (Vec<String>, Vec<String>) {
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let key: String =
        c.h.cwd
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| {
                n.to_string_lossy()
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect()
            })
            .unwrap_or_default();
    let names = |dir: &std::path::Path, ext: &str| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == ext))
            .map(|p| p.display().to_string())
            .collect();
        v.sort();
        v
    };
    let mut claude = Vec::new();
    for project in std::fs::read_dir(home.join(".claude/projects"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name: String = project
            .file_name()
            .to_string_lossy()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect();
        if !key.is_empty() && name.contains(&key) {
            claude.extend(names(&project.path(), "jsonl"));
        }
    }
    (
        claude,
        names(&home.join(".gemini/antigravity-cli/conversations"), "db"),
    )
}

/// Fork `from` to `to`: the copy shows the history and the first send answers from it.
async fn fork_recalls(from: ProviderId, to: ProviderId) {
    let mut c = ctx().await;
    let src = c.create(from).await;
    c.send(src, REMEMBER).await;
    let before = conversations(&c);
    let new = c.fork(src, to).await;
    // The snapshot turn ran and its throwaway conversation is gone again.
    assert_eq!(
        conversations(&c),
        before,
        "a snapshot conversation was left behind"
    );
    let snapshot = c.h.forks.join(format!("{}.snapshot.md", new.0));
    let text = std::fs::read_to_string(&snapshot).expect("snapshot file written");
    assert!(
        text.to_lowercase().contains("kiwi"),
        "snapshot lacks the state: {text}"
    );
    {
        xade_runtime::flush(&c.h.writer).await;
        let (row, msgs) = c.h.reader.load_session(new).await.unwrap();
        assert_eq!(row.provider_id, to);
        assert_eq!(row.provider_session_id, None);
        assert_eq!(
            msgs.len(),
            c.h.reader.load_session(src).await.unwrap().1.len()
        );
    }
    c.open(new).await;
    // A fork starts on the target's default model; keep the run cheap.
    c.h.tx
        .send(ManagerCmd::SetModel {
            session_id: new,
            model_id: model(to).into(),
        })
        .unwrap();
    let said = c.send(new, RECALL).await;
    assert!(
        said.contains("kiwi"),
        "fork did not answer from the history: {said}"
    );

    // The hand-off is not a message, and a later send needs none: the provider kept the turn.
    let users = c.stored_user_texts(new).await;
    assert_eq!(
        users,
        [REMEMBER, RECALL],
        "only the user's own text is stored"
    );
    let (row, _) = c.h.reader.load_session(new).await.unwrap();
    assert!(row.provider_session_id.is_some());
    let said = c
        .send(
            new,
            "How many questions have I asked you in total? Reply with just a number.",
        )
        .await;
    assert!(!said.is_empty());
    c.close(new).await;
    c.open(new).await;
    let said = c.send(new, RECALL).await;
    assert!(said.contains("kiwi"), "{said}");
    c.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn fork_claude_to_antigravity_answers_from_the_copied_history() {
    e2e!();
    fork_recalls(ProviderId::Claude, ProviderId::Antigravity).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn fork_antigravity_to_claude_answers_from_the_copied_history() {
    e2e!();
    fork_recalls(ProviderId::Antigravity, ProviderId::Claude).await;
}
