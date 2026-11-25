//! The quota report of `agy -p /usage --output-format json`: model groups with a 5-hour and a
//! weekly window each, as the share still remaining.

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use xade_core::{QuotaGroup, QuotaKind, QuotaWindow};

/// Parses the JSON envelope; `None` when it has no `command.data.groups`. Buckets of an unknown
/// window and malformed fields are skipped, a group left without windows is dropped.
pub fn parse_usage(stdout: &str) -> Option<Vec<QuotaGroup>> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    let groups = value.pointer("/command/data/groups")?.as_array()?;
    let mut out = Vec::new();
    for g in groups {
        let name = g.get("name").and_then(Value::as_str).unwrap_or("").trim();
        let windows: Vec<QuotaWindow> = g
            .get("buckets")
            .and_then(Value::as_array)
            .map(|b| b.iter().filter_map(bucket).collect())
            .unwrap_or_default();
        if !windows.is_empty() {
            out.push(QuotaGroup {
                name: name.to_string(),
                windows,
            });
        }
    }
    Some(out)
}

fn bucket(b: &Value) -> Option<QuotaWindow> {
    let kind = match b.get("window")?.as_str()? {
        "5h" => QuotaKind::FiveHour,
        "weekly" => QuotaKind::SevenDay,
        _ => return None,
    };
    let remaining = b.get("remaining_fraction")?.as_f64()?;
    Some(QuotaWindow {
        kind,
        utilization_pct: ((1.0 - remaining) * 100.0).clamp(0.0, 100.0) as f32,
        resets_at: b
            .get("reset_time")
            .and_then(Value::as_str)
            .and_then(|t| OffsetDateTime::parse(t, &Rfc3339).ok()),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Recorded from `agy -p /usage --output-format json` 1.5.17 (descriptions trimmed).
    const SAMPLE: &str = r#"{"conversation_id":"","status":"SUCCESS","response":"x","command":{"name":"usage","data":{"description":"d","groups":[{"name":"Gemini Models","buckets":[{"id":"gemini-weekly","name":"Weekly Limit Remaining","window":"weekly","remaining_fraction":0.75,"reset_time":"2026-12-01T22:40:00Z"},{"id":"gemini-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":0.9679359197616577,"reset_time":"2026-11-25T13:06:58Z"}]},{"name":"Claude and GPT models","buckets":[{"id":"3p-weekly","window":"weekly","remaining_fraction":1,"reset_time":"2026-12-02T10:24:26Z"},{"id":"3p-5h","window":"5h","remaining_fraction":1,"reset_time":"2026-11-25T15:24:26Z"}]}]}}}"#;

    #[test]
    fn groups_and_windows_become_used_percentages() {
        let groups = parse_usage(SAMPLE).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "Gemini Models");
        assert_eq!(groups[0].windows[0].kind, QuotaKind::SevenDay);
        assert_eq!(groups[0].windows[0].utilization_pct, 25.0);
        assert!((groups[0].windows[1].utilization_pct - 3.2).abs() < 0.1);
        assert!(groups[0].windows[0].resets_at.is_some());
        assert_eq!(groups[1].windows[0].utilization_pct, 0.0);
    }

    #[test]
    fn anything_else_is_skipped_or_none() {
        assert_eq!(parse_usage("not json"), None);
        assert_eq!(parse_usage(r#"{"command":{"name":"usage"}}"#), None);
        let odd = r#"{"command":{"data":{"groups":[{"name":"G","buckets":[{"window":"daily","remaining_fraction":0.5},{"window":"5h"}]}]}}}"#;
        assert_eq!(parse_usage(odd), Some(vec![]));
    }
}
