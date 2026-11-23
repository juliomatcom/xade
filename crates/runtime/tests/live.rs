//! Real CLI through the manager and store. Costs a few cents: run with `-- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use tokio::sync::oneshot;
use xade_claude::ClaudeProvider;
use xade_core::{AgentProvider, Role, SessionStatus};
use xade_runtime::{AppEventKind, ManagerCmd};

#[tokio::test]
#[ignore]
async fn create_send_close_resume() {
    let provider = Arc::new(ClaudeProvider::new());
    provider.preflight().expect("preflight");
    let dir = tempfile::tempdir().unwrap();
    let mut h = common::start(dir.path(), provider).await;

    h.create("haiku");
    let ev = h.until_status(30, SessionStatus::Idle).await;
    let id = ev
        .iter()
        .find_map(|e| e.session_id)
        .expect("session id in events");

    h.tx.send(ManagerCmd::Send {
        session_id: id,
        text: "Reply with exactly: pong".into(),
        delivery: None,
    })
    .unwrap();
    h.until_status(120, SessionStatus::Idle).await;
    xade_runtime::flush(&h.writer).await;
    let (row, msgs) = h.reader.load_session(id).await.unwrap();
    assert!(
        row.provider_session_id.is_some(),
        "provider_session_id stored"
    );
    assert_eq!(msgs[0].role, Role::User);
    let reply = msgs
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("assistant message");
    assert!(reply.content_text.to_lowercase().contains("pong"));
    let before = msgs.len();

    h.tx.send(ManagerCmd::Close { session_id: id }).unwrap();
    h.until_status(30, SessionStatus::Closed).await;

    h.tx.send(ManagerCmd::Open { session_id: id }).unwrap();
    let ev = h.until_status(30, SessionStatus::Idle).await;
    assert!(ev.iter().any(|e| matches!(
        &e.kind,
        AppEventKind::SessionOpened { messages, .. } if messages.len() == before
    )));
    h.tx.send(ManagerCmd::Send {
        session_id: id,
        text: "Reply with exactly: pong again".into(),
        delivery: None,
    })
    .unwrap();
    h.until_status(120, SessionStatus::Idle).await;
    xade_runtime::flush(&h.writer).await;
    let (row2, msgs2) = h.reader.load_session(id).await.unwrap();
    assert_eq!(row2.provider_session_id, row.provider_session_id);
    assert!(msgs2.len() > before + 1);

    let (done_tx, done_rx) = oneshot::channel();
    h.tx.send(ManagerCmd::ShutdownAll(done_tx)).unwrap();
    done_rx.await.unwrap();
}
