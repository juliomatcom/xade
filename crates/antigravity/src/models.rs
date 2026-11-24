//! The model list from `agy models`, grouped to match the ACP `model` config values.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use xade_core::{Effort, ModelOption};

/// Levels an `agy models` slug can end in, in the order they are listed per model.
const LEVELS: [(&str, &str, Effort); 5] = [
    ("low", "Low", Effort::Low),
    ("medium", "Medium", Effort::Medium),
    ("high", "High", Effort::High),
    ("xhigh", "XHigh", Effort::XHigh),
    ("max", "Max", Effort::Max),
];

fn rank(e: Effort) -> usize {
    LEVELS
        .iter()
        .position(|(_, _, l)| *l == e)
        .unwrap_or(LEVELS.len())
}

/// `gemini-4-neon-high` -> (`gemini-4-neon`, High).
fn split_slug(slug: &str) -> Option<(&str, Effort)> {
    let (base, suffix) = slug.rsplit_once('-')?;
    let (_, _, level) = LEVELS.iter().find(|(s, _, _)| *s == suffix)?;
    (!base.is_empty()).then_some((base, *level))
}

/// `Gemini 4 Neon (High)` -> `Gemini 4 Neon`.
fn strip_label_level(label: &str) -> &str {
    for (_, name, _) in LEVELS {
        if let Some(l) = label.strip_suffix(&format!(" ({name})")) {
            return l;
        }
    }
    label
}

/// Parses `agy models` stdout: `<slug>\t<label>` per line, anything else ignored.
/// Slugs sharing a base (two or more) become one option with the levels seen; the rest stay as is.
/// First-seen order.
pub fn parse_models(stdout: &str) -> Vec<ModelOption> {
    struct Row<'a> {
        slug: &'a str,
        label: &'a str,
        split: Option<(&'a str, Effort)>,
    }
    let rows: Vec<Row> = stdout
        .lines()
        .filter_map(|l| {
            let (slug, label) = l.split_once('\t')?;
            let (slug, label) = (slug.trim(), label.trim());
            (!slug.is_empty()).then(|| Row {
                slug,
                label,
                split: split_slug(slug),
            })
        })
        .collect();
    let shared = |base: &str| {
        rows.iter()
            .filter(|r| r.split.is_some_and(|(b, _)| b == base))
            .count()
            >= 2
    };

    let mut out: Vec<ModelOption> = Vec::new();
    for row in &rows {
        match row.split {
            Some((base, level)) if shared(base) => {
                let label = strip_label_level(row.label);
                match out.iter_mut().find(|m| m.id == base) {
                    Some(m) => {
                        if !m.effort_levels.contains(&level) {
                            m.effort_levels.push(level);
                        }
                    }
                    None => out.push(ModelOption {
                        id: base.to_string(),
                        label: label.to_string(),
                        description: String::new(),
                        effort_levels: vec![level],
                    }),
                }
            }
            _ => {
                if !out.iter().any(|m| m.id == row.slug) {
                    out.push(ModelOption {
                        id: row.slug.to_string(),
                        label: row.label.to_string(),
                        description: String::new(),
                        effort_levels: vec![],
                    });
                }
            }
        }
    }
    for m in &mut out {
        m.effort_levels.sort_by_key(|e| rank(*e));
    }
    out
}

/// Outcome of one `agy models` run.
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

/// Runs `agy models` with stdin closed and a deadline. The child leads its own process group so a
/// timeout kills it along with anything it started.
pub fn run_models(binary: &Path, timeout: Duration) -> Fetch {
    let mut child = match Command::new(binary)
        .arg("models")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Fetch::SpawnFailed(e.to_string()),
    };
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as _));

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
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    match status {
        Some(st) => Fetch::Done {
            success: st.success(),
            stdout,
            stderr,
        },
        None => Fetch::TimedOut,
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
mod tests {
    use super::*;

    /// Output recorded from `agy models` 1.5.16 (stdout only; the spinner goes to stderr).
    const SAMPLE: &str = "gemini-4-neon-high\tGemini 4 Neon (High)
gemini-4-neon-xhigh\tGemini 4 Neon (XHigh)
gemini-4-neon-medium\tGemini 4 Neon (Medium)
gemini-4-neon-low\tGemini 4 Neon (Low)
gemini-4-carbon-medium\tGemini 4 Carbon (Medium)
gemini-4-carbon-high\tGemini 4 Carbon (High)
gemini-4-carbon-low\tGemini 4 Carbon (Low)
gemini-4-argon-medium\tGemini 4 Argon (Medium)
gemini-4-argon-high\tGemini 4 Argon (High)
gemini-4-argon-low\tGemini 4 Argon (Low)
gemini-4-hydrogen-high\tGemini 4 Hydrogen (High)
gemini-4-hydrogen-xhigh\tGemini 4 Hydrogen (XHigh)
gemini-4-hydrogen-medium\tGemini 4 Hydrogen (Medium)
gemini-4-hydrogen-low\tGemini 4 Hydrogen (Low)
gemini-pro-orchestrator-neon\tOrchestrator (Neon)
gemini-pro-orchestrator\tOrchestrator (Carbon)
gemini-pro-orchestrator-argon\tOrchestrator (Argon)
gemini-3.8-flash-high\tGemini 3.8 Flash (High)
gemini-3.8-flash-medium\tGemini 3.8 Flash (Medium)
gemini-3.8-flash-low\tGemini 3.8 Flash (Low)
claude-opus-5-5-low\tClaude Opus 5.5 (Low)
claude-opus-5-5-medium\tClaude Opus 5.5 (Medium)
claude-opus-5-5-high\tClaude Opus 5.5 (High)
claude-sonnet-5-5-low\tClaude Sonnet 5.5 (Low)
claude-sonnet-5-5-medium\tClaude Sonnet 5.5 (Medium)
claude-sonnet-5-5-high\tClaude Sonnet 5.5 (High)
gpt-oss-120b-medium\tGPT-OSS 120B (Medium)
";

    fn ids(models: &[ModelOption]) -> Vec<&str> {
        models.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn groups_slugs_into_the_acp_model_values() {
        let m = parse_models(SAMPLE);
        // Same list, same order, as `configOptions.model` in the set_config fixture.
        assert_eq!(
            ids(&m),
            [
                "gemini-4-neon",
                "gemini-4-carbon",
                "gemini-4-argon",
                "gemini-4-hydrogen",
                "gemini-pro-orchestrator-neon",
                "gemini-pro-orchestrator",
                "gemini-pro-orchestrator-argon",
                "gemini-3.8-flash",
                "claude-opus-5-5",
                "claude-sonnet-5-5",
                "gpt-oss-120b-medium",
            ]
        );
    }

    #[test]
    fn grouped_models_list_their_levels_in_order() {
        let m = parse_models(SAMPLE);
        assert_eq!(m[0].label, "Gemini 4 Neon");
        assert_eq!(
            m[0].effort_levels,
            [Effort::Low, Effort::Medium, Effort::High, Effort::XHigh]
        );
        assert_eq!(
            m[1].effort_levels,
            [Effort::Low, Effort::Medium, Effort::High]
        );
        assert_eq!(m[7].label, "Gemini 3.8 Flash");
        assert_eq!(m[8].label, "Claude Opus 5.5");
    }

    #[test]
    fn a_lone_or_unsuffixed_slug_stays_as_is() {
        let m = parse_models(SAMPLE);
        let lone = &m[10];
        assert_eq!(lone.label, "GPT-OSS 120B (Medium)");
        assert!(lone.effort_levels.is_empty());
        let plain = &m[5];
        assert_eq!(plain.label, "Orchestrator (Carbon)");
        assert!(plain.effort_levels.is_empty());
    }

    #[test]
    fn lines_without_a_tab_and_blank_lines_are_ignored() {
        let m = parse_models("Fetching available models...\n\n  \nfoo\tFoo\nbar-low only\n");
        assert_eq!(ids(&m), ["foo"]);
        assert!(parse_models("").is_empty());
    }

    #[test]
    fn levels_sort_low_to_max_whatever_the_input_order() {
        let m = parse_models("x-max\tX (Max)\nx-low\tX (Low)\nx-high\tX (High)\n");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].effort_levels, [Effort::Low, Effort::High, Effort::Max]);
        assert_eq!(m[0].label, "X");
    }

    #[test]
    fn auth_hints_are_found_case_insensitively() {
        assert!(mentions_auth("Error: Please Sign In first"));
        assert!(mentions_auth("not authenticated"));
        assert!(!mentions_auth("network unreachable"));
    }
}
