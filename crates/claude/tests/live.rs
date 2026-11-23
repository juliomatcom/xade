//! Live smoke test against the real CLI. Costs a few cents: run with `-- --ignored`.

#![allow(clippy::expect_used)]

use std::time::Duration;

use tokio::time::timeout;
use xade_claude::ClaudeProvider;
use xade_core::{AgentProvider, Effort, ProviderEvent, SessionCommand, SpawnConfig};

#[tokio::test]
#[ignore]
async fn one_prompt_then_shutdown() {
    let provider = ClaudeProvider::new();
    provider.preflight().expect("preflight");
    let cwd = std::env::temp_dir().join(format!("xade-claude-live-{}", std::process::id()));
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

/// Free: `initialize` carries no prompt. Run with `-- --ignored`.
#[test]
#[ignore]
fn preflight_loads_versioned_models() {
    let provider = ClaudeProvider::new();
    provider.preflight().expect("preflight");
    let models = provider.models();
    assert!(
        models.iter().any(|m| m.label.starts_with("Opus 5.5")),
        "labels: {:?}",
        models.iter().map(|m| &m.label).collect::<Vec<_>>()
    );
    let default = provider.default_model();
    assert!(
        default.starts_with("claude-"),
        "default is a model id: {default}"
    );
    assert!(models.iter().any(|m| m.id == default));
    assert!(models.iter().all(|m| !m.label.is_empty()));
    println!("models: {}, default: {default}", models.len());
}

/// Reads the real `~/.claude/projects`; prints aggregate numbers only.
#[test]
#[ignore]
fn import_real_projects_smoke() {
    let root =
        std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".claude/projects");
    let started = std::time::Instant::now();
    let scan = xade_claude::transcript::scan_projects(&root);
    println!(
        "{:?} in {:.2}s; sessions with title: {}",
        scan.stats,
        started.elapsed().as_secs_f64(),
        scan.sessions.iter().filter(|s| !s.title.is_empty()).count()
    );
    assert_eq!(scan.stats.read_errors, 0);
    assert_eq!(scan.stats.parsed, scan.sessions.len());
}
