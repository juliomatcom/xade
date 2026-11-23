//! Versioned model list from the CLI's `initialize` control request (no prompt, no cost).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;
use xade_core::{Effort, ModelOption};

use crate::frames::OutFrame;

const REQUEST_ID: &str = "req_xade_init";
const EXIT_WAIT: Duration = Duration::from_secs(3);

/// Models offered, and the `resolvedModel` the CLI's "default" entry points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    pub models: Vec<ModelOption>,
    pub default_model: String,
}

/// Parses the `control_response` frame of an `initialize` request.
/// Entries are deduped by `resolvedModel`; the non-"default" entry's label wins.
pub fn parse_catalog(frame: &Value) -> Option<Catalog> {
    let entries = frame
        .pointer("/response/response/models")?
        .as_array()
        .filter(|a| !a.is_empty())?;
    let mut models: Vec<ModelOption> = Vec::new();
    let mut default_model = None;
    for e in entries {
        let value = e.get("value").and_then(Value::as_str).unwrap_or_default();
        let id = e
            .get("resolvedModel")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(value);
        if id.is_empty() {
            continue;
        }
        let is_default = value == "default";
        if is_default {
            default_model = Some(id.to_string());
        }
        let text = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or_default().into();
        let option = ModelOption {
            id: id.to_string(),
            label: text("displayName"),
            description: text("description"),
            effort_levels: e
                .get("supportedEffortLevels")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| serde_json::from_value::<Effort>(v.clone()).ok())
                        .filter(|l| *l != Effort::Default)
                        .collect()
                })
                .unwrap_or_default(),
        };
        match models.iter_mut().find(|m| m.id == option.id) {
            Some(existing) if !is_default => *existing = option,
            Some(_) => {}
            None => models.push(option),
        }
    }
    let default_model = default_model.or_else(|| models.first().map(|m| m.id.clone()))?;
    Some(Catalog {
        models,
        default_model,
    })
}

/// Runs one short `claude` process: initialize, read the response, close stdin.
/// `None` on any failure; the caller keeps its fallback list.
pub fn fetch_catalog(binary: &Path, timeout: Duration) -> Option<Catalog> {
    let mut child = Command::new(binary)
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut line = OutFrame::initialize(REQUEST_ID).0.to_string();
    line.push('\n');
    let sent = stdin
        .write_all(line.as_bytes())
        .and_then(|()| stdin.flush())
        .is_ok();
    let catalog = if sent {
        read_catalog(&rx, Instant::now() + timeout)
    } else {
        None
    };

    drop(stdin);
    let end = Instant::now() + EXIT_WAIT;
    while !matches!(child.try_wait(), Ok(Some(_))) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = child.kill();
        let _ = child.wait();
    }
    catalog
}

/// Waits for the `initialize` reply until `deadline`.
fn read_catalog(rx: &mpsc::Receiver<String>, deadline: Instant) -> Option<Catalog> {
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        let line = rx.recv_timeout(left).ok()?;
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if frame.get("type").and_then(Value::as_str) == Some("control_response")
            && frame
                .pointer("/response/request_id")
                .and_then(Value::as_str)
                == Some(REQUEST_ID)
        {
            return parse_catalog(&frame);
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn fixture_frame() -> Value {
        let path = format!("{}/fixtures/init_first.ndjson", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path)
            .expect("fixture")
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).expect("line"))
            .find(|l| l["dir"] == "out" && l["frame"]["type"] == "control_response")
            .expect("control_response")["frame"]
            .clone()
    }

    #[test]
    fn init_fixture_gives_versioned_deduped_models() {
        let c = parse_catalog(&fixture_frame()).expect("catalog");
        assert_eq!(c.default_model, "claude-opus-5-5");
        let opus = c
            .models
            .iter()
            .find(|m| m.id == "claude-opus-5-5")
            .expect("opus 5.5");
        assert_eq!(opus.label, "Opus 5.5");
        assert_eq!(opus.description, "For complex work and everyday tasks");
        assert_eq!(
            opus.effort_levels,
            vec![
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::XHigh,
                Effort::Max
            ]
        );
        let ids: Vec<_> = c.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids.iter().filter(|i| **i == "claude-opus-5-5").count(), 1);
        for want in ["claude-sonnet-5-5", "claude-haiku-5-5", "claude-opus-5"] {
            assert!(ids.contains(&want), "{want}");
        }
        assert!(c.models.iter().all(|m| !m.label.starts_with("Default")));
        let haiku4 = c
            .models
            .iter()
            .find(|m| m.id == "claude-haiku-4-5-20251001")
            .expect("haiku 4.5");
        assert!(haiku4.effort_levels.is_empty());
    }

    #[test]
    fn malformed_or_empty_responses_yield_none() {
        assert!(parse_catalog(&serde_json::json!({"type": "control_response"})).is_none());
        let empty = serde_json::json!({"response": {"response": {"models": []}}});
        assert!(parse_catalog(&empty).is_none());
    }

    #[test]
    fn without_default_entry_first_model_is_default() {
        let f = serde_json::json!({"response": {"response": {"models": [
            {"value": "a", "resolvedModel": "m-a", "displayName": "A"},
            {"value": "b", "resolvedModel": "m-b", "displayName": "B"}]}}});
        let c = parse_catalog(&f).expect("catalog");
        assert_eq!(c.default_model, "m-a");
        assert_eq!(c.models.len(), 2);
    }
}
