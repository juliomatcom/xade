//! Live smoke test against the real CLI. Costs a few cents: run with `-- --ignored`.

#![allow(clippy::expect_used)]

use std::time::Duration;

use aw_claude::ClaudeProvider;
use aw_core::{AgentProvider, Effort, ProviderEvent, SessionCommand, SpawnConfig};
use tokio::time::timeout;

#[tokio::test]
#[ignore]
async fn one_prompt_then_shutdown() {
    let provider = ClaudeProvider::new();
    provider.preflight().expect("preflight");
    let cwd = std::env::temp_dir().join(format!("aw-claude-live-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("cwd");
    let mut h = provider
        .spawn(SpawnConfig {
            cwd: cwd.clone(),
            model_id: "haiku".into(),
            effort: Effort::Default,
            auto_mode: false,
            resume: None,
        })
        .expect("spawn");
    h.commands
        .send(SessionCommand::SendPrompt(
            "Reply with exactly: pong".into(),
        ))
        .expect("send");

    let mut saw_text = false;
    let mut saw_turn = false;
    let mut exited = false;
    let result = timeout(Duration::from_secs(90), async {
        while let Some(ev) = h.events.recv().await {
            match ev {
                ProviderEvent::AssistantText { .. } => saw_text = true,
                ProviderEvent::TurnComplete { is_error, .. } => {
                    assert!(!is_error);
                    saw_turn = true;
                    h.commands.send(SessionCommand::Shutdown).expect("shutdown");
                }
                ProviderEvent::Exited { .. } => {
                    exited = true;
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    let _ = std::fs::remove_dir_all(&cwd);
    result.expect("timed out");
    assert!(saw_text && saw_turn && exited);
}
