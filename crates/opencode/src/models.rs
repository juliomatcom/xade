//! The model list from `opencode models --standalone`, and the bounded runner for such one-shot
//! `opencode` commands.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use xade_core::{Effort, ModelOption};

/// Every level the picker offers. Which of them a model accepts is only known to a live session,
/// so the adapter filters against its `configOptions` instead.
pub const EFFORT_LEVELS: [Effort; 5] = [
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::XHigh,
    Effort::Max,
];

/// Slug words that keep a model from being the default (free, data-sharing or unfinished ones).
const NOT_DEFAULT: [&str; 3] = ["free", "contributor", "preview"];

/// How long to wait for the output pipes after the command itself is gone.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Parses the output: one `provider/model` id per line, anything else ignored. First-seen order.
pub fn parse_models(stdout: &str) -> Vec<ModelOption> {
    let mut out: Vec<ModelOption> = Vec::new();
    for id in stdout.lines().map(str::trim) {
        let is_id = id
            .split_once('/')
            .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
            && !id.contains(|c: char| c.is_whitespace() || c.is_control());
        if is_id && !out.iter().any(|m| m.id == id) {
            out.push(ModelOption {
                id: id.to_string(),
                label: id.to_string(),
                description: String::new(),
                effort_levels: EFFORT_LEVELS.to_vec(),
            });
        }
    }
    out
}

/// The first `opencode-go/*` model that is not free, contributor or preview; else the first model
/// that is none of those; else the first; else empty.
pub fn default_model(models: &[ModelOption]) -> String {
    let regular = |m: &&ModelOption| {
        let slug = m.id.split_once('/').map_or(m.id.as_str(), |(_, s)| s);
        let slug = slug.to_lowercase();
        !NOT_DEFAULT.iter().any(|w| slug.contains(w))
    };
    models
        .iter()
        .filter(|m| m.id.starts_with("opencode-go/"))
        .find(regular)
        .or_else(|| models.iter().find(regular))
        .or_else(|| models.first())
        .map(|m| m.id.clone())
        .unwrap_or_default()
}

/// Outcome of one `opencode` command.
#[derive(Debug)]
pub enum Fetch {
    Done {
        success: bool,
        stdout: String,
        stderr: String,
    },
    TimedOut,
    SpawnFailed(String),
}

/// Runs `opencode models --standalone` (see [`run_opencode`]).
pub fn run_models(binary: &Path, timeout: Duration) -> Fetch {
    run_opencode(binary, &["models", "--standalone"], timeout)
}

/// Runs `opencode <args>` with stdin closed and a deadline, from `/` (never `/tmp`: OpenCode
/// executes `.opencode/plugin(s)/*.js` found in ancestors of its cwd, and `/tmp` is world-writable
/// on shared Linux). The child leads its own process group so a timeout kills it along with what it
/// started in that group. Callers pass `--standalone`: without it the command reaches for the
/// user's background server.
pub fn run_opencode(binary: &Path, args: &[&str], timeout: Duration) -> Fetch {
    let mut child = match Command::new(binary)
        .args(args)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Fetch::SpawnFailed(e.to_string()),
    };
    let out = Drain::start(child.stdout.take());
    let err = Drain::start(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) | Err(_) => {
                // SAFETY: plain syscall; the child leads its own group (process_group(0)).
                unsafe {
                    libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    match status {
        Some(st) => {
            let until = Instant::now() + DRAIN_GRACE;
            Fetch::Done {
                success: st.success(),
                stdout: out.finish(until),
                stderr: err.finish(until),
            }
        }
        None => Fetch::TimedOut,
    }
}

/// A pipe read on its own thread. The wait for it is bounded: `opencode` starts a private server
/// in another process group, which could hold the pipe open after the command exited.
struct Drain {
    data: Arc<Mutex<Vec<u8>>>,
    done: mpsc::Receiver<()>,
}

impl Drain {
    fn start(pipe: Option<impl Read + Send + 'static>) -> Self {
        let data = Arc::new(Mutex::new(Vec::new()));
        let (tx, done) = mpsc::channel();
        let sink = data.clone();
        std::thread::spawn(move || {
            if let Some(mut pipe) = pipe {
                let mut chunk = [0u8; 8192];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => sink
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .extend_from_slice(&chunk[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            }
            let _ = tx.send(());
        });
        Self { data, done }
    }

    fn finish(self, until: Instant) -> String {
        let _ = self
            .done
            .recv_timeout(until.saturating_duration_since(Instant::now()));
        let data = self.data.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&data).into_owned()
    }
}

/// True when output reads like a missing login. Case-insensitive.
pub fn mentions_auth(text: &str) -> bool {
    let t = text.to_lowercase();
    ["auth", "sign in", "sign-in", "log in", "login"]
        .iter()
        .any(|k| t.contains(k))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// Lines from `opencode models --standalone` 2.4.2, in its order.
    const SAMPLE: &str = "opencode-go/beam-free
opencode-go/claude-haiku-5-5
opencode-go/glm-5.3-flash
opencode-go/gpt-6-luna
opencode-go/hy4-preview
opencode-go/muse-spark-1.2-contributor
opencode/beam-free
opencode/big-pickle
opencode/claude-sonnet-5-5
";

    fn ids(models: &[ModelOption]) -> Vec<&str> {
        models.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn every_line_is_a_model_with_every_effort_level() {
        let m = parse_models(SAMPLE);
        assert_eq!(ids(&m), SAMPLE.lines().collect::<Vec<_>>());
        assert_eq!(m[1].label, "opencode-go/claude-haiku-5-5");
        assert!(m.iter().all(|m| m.effort_levels == EFFORT_LEVELS));
    }

    #[test]
    fn noise_blank_lines_and_duplicates_are_ignored() {
        let m = parse_models(
            "Loading models...\n\n  a/b  \nError: no/such thing\na/b\n/x\ny/\nplain\n\u{1b}[1ma/c\nopenrouter/meta/llama\n",
        );
        assert_eq!(ids(&m), ["a/b", "openrouter/meta/llama"]);
        assert!(parse_models("").is_empty());
    }

    #[test]
    fn the_default_is_the_first_regular_go_model() {
        assert_eq!(
            default_model(&parse_models(SAMPLE)),
            "opencode-go/claude-haiku-5-5"
        );
    }

    #[test]
    fn without_go_the_default_is_the_first_regular_model() {
        let m = parse_models("opencode/beam-free\nopencode/X-Preview\nopencode/big-pickle\n");
        assert_eq!(default_model(&m), "opencode/big-pickle");
        // A regular model elsewhere beats a free Go one.
        let m = parse_models("opencode-go/beam-free\nanthropic/claude\n");
        assert_eq!(default_model(&m), "anthropic/claude");
    }

    #[test]
    fn with_only_free_models_the_first_is_the_default() {
        let m = parse_models("opencode/beam-free\nopencode/hy4-flash-free\n");
        assert_eq!(default_model(&m), "opencode/beam-free");
        assert_eq!(default_model(&[]), "");
    }

    #[test]
    fn auth_hints_are_found_case_insensitively() {
        assert!(mentions_auth("Error: Please Sign In first"));
        assert!(mentions_auth("run `opencode auth login`"));
        assert!(!mentions_auth("network unreachable"));
    }

    fn sh(script: &str, timeout: Duration) -> Fetch {
        run_opencode(Path::new("/bin/sh"), &["-c", script], timeout)
    }

    #[test]
    fn a_command_reports_its_status_and_both_streams() {
        match sh("echo out; echo err >&2; exit 3", Duration::from_secs(10)) {
            Fetch::Done {
                success,
                stdout,
                stderr,
            } => assert_eq!(
                (success, stdout.as_str(), stderr.as_str()),
                (false, "out\n", "err\n")
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_hung_command_times_out() {
        let start = Instant::now();
        assert!(matches!(
            sh("sleep 30", Duration::from_millis(200)),
            Fetch::TimedOut
        ));
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_pipe_held_open_by_another_process_does_not_block() {
        let start = Instant::now();
        // The background sleep inherits stdout and outlives the shell.
        match sh("sleep 6 & echo listed", Duration::from_secs(10)) {
            Fetch::Done { stdout, .. } => assert_eq!(stdout, "listed\n"),
            other => panic!("{other:?}"),
        }
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_binary_is_a_spawn_failure() {
        assert!(matches!(
            run_models(Path::new("/nonexistent/opencode"), Duration::from_secs(1)),
            Fetch::SpawnFailed(_)
        ));
    }
}
