//! Live sessions from Claude Code's registry `~/.claude/sessions/<pid>.json`.

use std::path::Path;

use serde::Deserialize;
use xade_core::{RunState, RunningSession};

#[derive(Deserialize)]
struct Entry {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    status: Option<String>,
    entrypoint: Option<String>,
}

fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn location(entrypoint: Option<&str>) -> String {
    match entrypoint {
        Some("claude-vscode") => "VS Code".into(),
        Some("cli") => "terminal".into(),
        Some("sdk-cli" | "sdk-ts" | "sdk-py") => "SDK".into(),
        Some(other) => other.into(),
        None => String::new(),
    }
}

/// Reads only `*.json` files (never the `.key` files). Stale files of dead pids and
/// unparsable files are skipped.
pub fn scan_running(dir: &Path) -> Vec<RunningSession> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<RunningSession> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| serde_json::from_slice::<Entry>(&std::fs::read(p).ok()?).ok())
        .filter(|e| pid_alive(e.pid))
        .map(|e| RunningSession {
            provider_session_id: e.session_id,
            state: if e.status.as_deref() == Some("busy") {
                RunState::Busy
            } else {
                RunState::Idle
            },
            location: location(e.entrypoint.as_deref()),
            pid: e.pid,
        })
        .collect();
    out.sort_by_key(|s| s.pid);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("xade-running-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("dir");
        d
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("write");
    }

    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
        let pid = child.id();
        child.wait().expect("wait");
        pid
    }

    #[test]
    fn keeps_alive_skips_dead_bad_json_and_key_files() {
        let dir = temp_dir("scan");
        let me = std::process::id();
        write(
            &dir,
            &format!("{me}.json"),
            &format!(
                r#"{{"pid":{me},"sessionId":"live","status":"busy","entrypoint":"claude-vscode"}}"#
            ),
        );
        let dead = dead_pid();
        write(
            &dir,
            &format!("{dead}.json"),
            &format!(r#"{{"pid":{dead},"sessionId":"dead","status":"idle"}}"#),
        );
        write(&dir, "1.json", "{not json");
        write(&dir, "2.json", r#"{"pid":2}"#);
        write(
            &dir,
            &format!("{me}.abc123.key"),
            &format!(r#"{{"pid":{me},"sessionId":"key"}}"#),
        );
        let found = scan_running(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            found,
            vec![RunningSession {
                provider_session_id: "live".into(),
                state: RunState::Busy,
                location: "VS Code".into(),
                pid: me,
            }]
        );
    }

    #[test]
    fn maps_status_and_entrypoint() {
        let dir = temp_dir("map");
        let me = std::process::id();
        let cases = [
            ("busy", Some("cli"), RunState::Busy, "terminal"),
            ("idle", Some("sdk-cli"), RunState::Idle, "SDK"),
            ("waiting", Some("sdk-ts"), RunState::Idle, "SDK"),
            ("busy", Some("sdk-py"), RunState::Busy, "SDK"),
            ("busy", Some("jetbrains"), RunState::Busy, "jetbrains"),
        ];
        for (i, (status, ep, _, _)) in cases.iter().enumerate() {
            let ep = ep.map_or(String::new(), |e| format!(r#","entrypoint":"{e}""#));
            write(
                &dir,
                &format!("{i}.json"),
                &format!(r#"{{"pid":{me},"sessionId":"s{i}","status":"{status}"{ep}}}"#),
            );
        }
        write(
            &dir,
            "x.json",
            &format!(r#"{{"pid":{me},"sessionId":"nostatus"}}"#),
        );
        let mut found = scan_running(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        found.sort_by(|a, b| a.provider_session_id.cmp(&b.provider_session_id));
        for (i, (_, _, state, loc)) in cases.iter().enumerate() {
            let s = found
                .iter()
                .find(|s| s.provider_session_id == format!("s{i}"))
                .expect("case present");
            assert_eq!((s.state, s.location.as_str()), (*state, *loc), "case {i}");
        }
        let n = found
            .iter()
            .find(|s| s.provider_session_id == "nostatus")
            .expect("nostatus");
        assert_eq!(n.state, RunState::Idle);
    }

    #[test]
    fn missing_dir_is_empty() {
        assert!(scan_running(Path::new("/nonexistent/xade-sessions")).is_empty());
    }

    /// Prints only counts; run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_scan_counts() {
        let home = std::env::var_os("HOME").expect("HOME");
        let found = scan_running(&Path::new(&home).join(".claude/sessions"));
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        for s in &found {
            *counts
                .entry(format!("{:?}/{}", s.state, s.location))
                .or_default() += 1;
        }
        println!("running total={} by state/location={counts:?}", found.len());
    }
}
