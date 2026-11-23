//! Parent-death probe: `cargo run -p xade-claude --example parent_death`.
//! Spawns a helper that starts a `claude` session, SIGKILLs the helper, then checks
//! that no `claude` process with the helper's cwd survives. Starts no turn, so it costs nothing.

#![allow(clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use xade_claude::ClaudeProvider;
use xade_core::{AgentProvider, Effort, SpawnConfig};

/// Pids of processes whose cwd is `cwd` (`lsof -Fpn` emits `p<pid>` then `n<path>`).
fn pids_in(cwd: &Path) -> Vec<String> {
    let out = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-Fpn"])
        .output()
        .expect("lsof");
    let (mut pid, mut found) = (String::new(), Vec::new());
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.to_string();
        } else if line.strip_prefix('n').is_some_and(|n| Path::new(n) == cwd) {
            found.push(pid.clone());
        }
    }
    found
}

fn helper(cwd: &str) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let _handle = ClaudeProvider::new()
        .spawn(SpawnConfig {
            cwd: cwd.into(),
            model_id: "haiku".into(),
            effort: Effort::Default,
            auto_mode: false,
            resume: None,
        })
        .expect("spawn");
    println!("ready");
    std::io::stdout().flush().expect("flush");
    std::thread::park();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("helper") {
        return helper(&args[2]);
    }
    let cwd = std::env::temp_dir().join(format!("xade-parent-death-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("cwd");
    let cwd = cwd.canonicalize().expect("canonicalize");
    let mut parent = Command::new(std::env::current_exe().expect("exe"))
        .args(["helper", &cwd.to_string_lossy()])
        .stdout(Stdio::piped())
        .spawn()
        .expect("helper");
    let mut line = String::new();
    BufReader::new(parent.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("ready line");
    std::thread::sleep(Duration::from_secs(1));
    let before = pids_in(&cwd);
    println!("claude pids before kill: {before:?}");
    assert!(
        !before.is_empty(),
        "no claude child found: probe inconclusive"
    );

    parent.kill().expect("SIGKILL helper");
    let _ = parent.wait();
    let start = Instant::now();
    let mut survivors = pids_in(&cwd);
    while !survivors.is_empty() && start.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(100));
        survivors = pids_in(&cwd);
    }
    if survivors.is_empty() {
        println!(
            "OK: child exited {:?} after parent SIGKILL",
            start.elapsed()
        );
    } else {
        println!("FAIL: children survived parent death: {survivors:?}");
        for p in &survivors {
            let _ = Command::new("kill").args(["-9", p]).status();
        }
    }
    let _ = std::fs::remove_dir_all(&cwd);
    assert!(survivors.is_empty());
}
