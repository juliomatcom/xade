//! Parsing for `account/rateLimits/read` (`GetAccountRateLimitsResponse`, schema-only / unverified
//! live on ChatGPT accounts because API-key accounts return `-32600`).

use serde_json::Value;
use time::OffsetDateTime;
use xade_core::{QuotaGroup, QuotaKind, QuotaWindow};

const FIVE_HOURS_MINS: i64 = 300;
const SEVEN_DAYS_MINS: i64 = 10_080;

/// Parses a `GetAccountRateLimitsResponse` result into `QuotaGroup`s.
/// Prefers `rateLimitsByLimitId` when present and non-empty, falling back to `rateLimits`.
/// Snapshots with no recognized 5-hour or weekly window are dropped.
pub fn parse_rate_limits(result: &Value) -> Vec<QuotaGroup> {
    if let Some(map) = result
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .filter(|m| !m.is_empty())
    {
        let main_id = result
            .pointer("/rateLimits/limitId")
            .and_then(Value::as_str);
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort_by(|a, b| {
            let a_main = Some(a.as_str()) == main_id || a.as_str() == "codex";
            let b_main = Some(b.as_str()) == main_id || b.as_str() == "codex";
            b_main.cmp(&a_main).then_with(|| a.cmp(b))
        });
        let mut out = Vec::new();
        for key in keys {
            if let Some(snap) = map.get(key) {
                if let Some(g) = parse_snapshot(snap) {
                    out.push(g);
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    result
        .get("rateLimits")
        .and_then(parse_snapshot)
        .into_iter()
        .collect()
}

fn parse_snapshot(snapshot: &Value) -> Option<QuotaGroup> {
    let name = snapshot
        .get("limitName")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let mut windows = Vec::new();
    for key in ["primary", "secondary"] {
        if let Some(w) = snapshot.get(key).and_then(parse_window) {
            if !windows
                .iter()
                .any(|existing: &QuotaWindow| existing.kind == w.kind)
            {
                windows.push(w);
            }
        }
    }
    (!windows.is_empty()).then_some(QuotaGroup { name, windows })
}

fn parse_window(w: &Value) -> Option<QuotaWindow> {
    let kind = match w.get("windowDurationMins").and_then(Value::as_i64)? {
        FIVE_HOURS_MINS => QuotaKind::FiveHour,
        SEVEN_DAYS_MINS => QuotaKind::SevenDay,
        _ => return None,
    };
    let used = w.get("usedPercent")?.as_f64()?;
    let resets_at = w
        .get("resetsAt")
        .and_then(Value::as_i64)
        .and_then(|s| OffsetDateTime::from_unix_timestamp(s).ok());
    Some(QuotaWindow {
        kind,
        utilization_pct: used.clamp(0.0, 100.0) as f32,
        resets_at,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_main_snapshot_with_five_hour_and_weekly_windows() {
        let res = json!({
            "rateLimits": {
                "limitId": "codex",
                "limitName": null,
                "primary": {
                    "usedPercent": 42,
                    "windowDurationMins": 300,
                    "resetsAt": 1_800_000_000
                },
                "secondary": {
                    "usedPercent": 17,
                    "windowDurationMins": 10080,
                    "resetsAt": 1_800_500_000
                }
            },
            "rateLimitsByLimitId": null
        });
        let groups = parse_rate_limits(&res);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "");
        assert_eq!(groups[0].windows.len(), 2);
        assert_eq!(groups[0].windows[0].kind, QuotaKind::FiveHour);
        assert_eq!(groups[0].windows[0].utilization_pct, 42.0);
        assert_eq!(
            groups[0].windows[0].resets_at,
            Some(OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap())
        );
        assert_eq!(groups[0].windows[1].kind, QuotaKind::SevenDay);
        assert_eq!(groups[0].windows[1].utilization_pct, 17.0);
    }

    #[test]
    fn prefers_rate_limits_by_limit_id_and_orders_main_first() {
        let res = json!({
            "rateLimits": {
                "limitId": "codex",
                "primary": {"usedPercent": 10, "windowDurationMins": 300}
            },
            "rateLimitsByLimitId": {
                "spark": {
                    "limitId": "spark",
                    "limitName": "GPT-Spark",
                    "primary": {"usedPercent": 75, "windowDurationMins": 300},
                    "secondary": {"usedPercent": 30, "windowDurationMins": 10080}
                },
                "codex": {
                    "limitId": "codex",
                    "limitName": "Codex",
                    "primary": {"usedPercent": 10, "windowDurationMins": 300}
                }
            }
        });
        let groups = parse_rate_limits(&res);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "Codex");
        assert_eq!(groups[0].windows.len(), 1);
        assert_eq!(groups[0].windows[0].utilization_pct, 10.0);
        assert_eq!(groups[1].name, "GPT-Spark");
        assert_eq!(groups[1].windows.len(), 2);
        assert_eq!(groups[1].windows[0].utilization_pct, 75.0);
        assert_eq!(groups[1].windows[1].utilization_pct, 30.0);
    }

    #[test]
    fn skips_missing_windows_and_unknown_durations() {
        let res = json!({
            "rateLimits": {
                "primary": {"usedPercent": 50, "windowDurationMins": 60},
                "secondary": null
            }
        });
        assert!(parse_rate_limits(&res).is_empty());

        let partial = json!({
            "rateLimits": {
                "primary": {"usedPercent": 120, "windowDurationMins": 10080},
                "secondary": {"usedPercent": 5, "windowDurationMins": 1440}
            }
        });
        let groups = parse_rate_limits(&partial);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].windows.len(), 1);
        assert_eq!(groups[0].windows[0].kind, QuotaKind::SevenDay);
        assert_eq!(groups[0].windows[0].utilization_pct, 100.0);
        assert_eq!(groups[0].windows[0].resets_at, None);
    }

    #[test]
    fn empty_or_unrecognized_by_limit_id_falls_back_to_rate_limits() {
        let res = json!({
            "rateLimits": {
                "primary": {"usedPercent": 20, "windowDurationMins": 300}
            },
            "rateLimitsByLimitId": {
                "other": {"primary": {"usedPercent": 5, "windowDurationMins": 60}}
            }
        });
        let groups = parse_rate_limits(&res);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].windows[0].utilization_pct, 20.0);
    }
}
