//! Manual harness: `cargo run -p aw-claude --example chat -- [cwd] [model]`.
//! Lines are prompts; `/stop` interrupts, `/auto on|off` toggles auto mode,
//! `y`/`n` answers a pending permission, `/quit` shuts down. EOF waits for the
//! outstanding turn, then shuts down.

#![allow(clippy::expect_used)]

use std::collections::VecDeque;
use std::io::Write;

use aw_claude::ClaudeProvider;
use aw_core::{AgentProvider, Effort, ProviderEvent, SessionCommand, SpawnConfig};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let cwd = args
        .next()
        .map(Into::into)
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let provider = ClaudeProvider::new();
    let model = args.next().unwrap_or_else(|| provider.default_model());
    if let Err(e) = provider.preflight() {
        eprintln!("preflight failed: {e}");
        return;
    }
    let mut h = match provider.spawn(SpawnConfig {
        cwd,
        model_id: model,
        effort: Effort::Default,
        auto_mode: false,
        resume: None,
    }) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("spawn failed: {e}");
            return;
        }
    };

    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut input_open = true;
    let mut outstanding = 0usize;
    let mut permissions: VecDeque<String> = VecDeque::new();
    let mut shutting_down = false;

    loop {
        tokio::select! {
            line = stdin.next_line(), if input_open => match line {
                Ok(Some(line)) => {
                    let line = line.trim().to_string();
                    let cmd = match line.as_str() {
                        "" => None,
                        "/stop" => Some(SessionCommand::Interrupt),
                        "/auto on" => Some(SessionCommand::SetAutoMode(true)),
                        "/auto off" => Some(SessionCommand::SetAutoMode(false)),
                        "/quit" => { shutting_down = true; Some(SessionCommand::Shutdown) }
                        "y" | "n" if !permissions.is_empty() => {
                            permissions.pop_front().map(|request_id| SessionCommand::RespondPermission {
                                request_id,
                                allow: line == "y",
                            })
                        }
                        _ => { outstanding += 1; Some(SessionCommand::SendPrompt(line)) }
                    };
                    if let Some(c) = cmd { let _ = h.commands.send(c); }
                }
                _ => {
                    input_open = false;
                    if outstanding == 0 && !shutting_down {
                        shutting_down = true;
                        let _ = h.commands.send(SessionCommand::Shutdown);
                    }
                }
            },
            ev = h.events.recv() => {
                let Some(ev) = ev else { break };
                match &ev {
                    ProviderEvent::TextDelta(t) => { print!("{t}"); let _ = std::io::stdout().flush(); }
                    ProviderEvent::PermissionRequest { request_id, tool_name, input_json, .. } => {
                        permissions.push_back(request_id.clone());
                        println!("\n[permission] {tool_name} {input_json}  (y/n)");
                    }
                    ProviderEvent::TurnComplete { .. } => {
                        outstanding = outstanding.saturating_sub(1);
                        println!("\n{ev:?}");
                        if !input_open && outstanding == 0 && !shutting_down {
                            shutting_down = true;
                            let _ = h.commands.send(SessionCommand::Shutdown);
                        }
                    }
                    ProviderEvent::Exited { .. } => { println!("{ev:?}"); break }
                    _ => println!("{ev:?}"),
                }
            }
        }
    }
}
