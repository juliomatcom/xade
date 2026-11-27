//! Parsing for Codex `model/list` and `account/read` responses.

use serde_json::Value;
use xade_core::{Effort, ModelOption};

pub const FALLBACK_MODEL_ID: &str = "gpt-6-luna";

const LEVELS: [(&str, Effort); 5] = [
    ("low", Effort::Low),
    ("medium", Effort::Medium),
    ("high", Effort::High),
    ("xhigh", Effort::XHigh),
    ("max", Effort::Max),
];

fn parse_effort(s: &str) -> Option<Effort> {
    LEVELS.iter().find(|(k, _)| *k == s).map(|(_, e)| *e)
}

fn rank(e: Effort) -> usize {
    LEVELS
        .iter()
        .position(|(_, l)| *l == e)
        .unwrap_or(LEVELS.len())
}

/// Static fallback catalog used if preflight has not populated models.
pub fn fallback_models() -> Vec<ModelOption> {
    vec![ModelOption {
        id: FALLBACK_MODEL_ID.to_string(),
        label: "GPT-6-Luna".to_string(),
        description: String::new(),
        effort_levels: vec![
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
        ],
    }]
}

/// Parses a `model/list` result into `(models, default_model_id)`.
/// Hidden models and unknown/unsupported effort levels (e.g. `ultra`) are skipped.
pub fn parse_models(result: &Value) -> (Vec<ModelOption>, Option<String>) {
    let items = result
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut out = Vec::new();
    let mut default_id = None;
    for entry in items {
        if entry
            .get("hidden")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let Some(id) = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let label = entry
            .get("displayName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(id);
        let description = entry
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut effort_levels = Vec::new();
        if let Some(efforts) = entry
            .get("supportedReasoningEfforts")
            .and_then(Value::as_array)
        {
            for e in efforts {
                let Some(raw) = e.get("reasoningEffort").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(level) = parse_effort(raw) {
                    if !effort_levels.contains(&level) {
                        effort_levels.push(level);
                    }
                }
            }
        }
        effort_levels.sort_by_key(|e| rank(*e));
        if default_id.is_none()
            && entry
                .get("isDefault")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            default_id = Some(id.to_string());
        }
        out.push(ModelOption {
            id: id.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            effort_levels,
        });
    }
    if default_id.is_none() {
        default_id = out.first().map(|m| m.id.clone());
    }
    (out, default_id)
}

/// Auth state reported by `account/read`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountStatus {
    /// Signed in; optional display-only label (email for ChatGPT auth, `"API key"` for API key).
    LoggedIn(Option<String>),
    NotLoggedIn,
}

/// Parses an `account/read` result. Never logs the email.
pub fn parse_account(result: &Value) -> AccountStatus {
    match result.get("account") {
        Some(Value::Object(map)) => {
            let label = match map.get("type").and_then(Value::as_str) {
                Some("chatgpt") => map
                    .get("email")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from),
                Some("apiKey") => Some("API key".to_string()),
                _ => None,
            };
            AccountStatus::LoggedIn(label)
        }
        _ => {
            let requires = result
                .get("requiresOpenaiAuth")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if requires {
                AccountStatus::NotLoggedIn
            } else {
                AccountStatus::LoggedIn(None)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_models_skips_hidden_and_drops_ultra_effort() {
        let res = json!({
            "data": [
                {
                    "id": "gpt-6.2-sol",
                    "displayName": "GPT-6.2-Sol",
                    "description": "Fast model.",
                    "hidden": false,
                    "isDefault": true,
                    "supportedReasoningEfforts": [
                        {"reasoningEffort": "ultra"},
                        {"reasoningEffort": "high"},
                        {"reasoningEffort": "low"},
                        {"reasoningEffort": "max"},
                        {"reasoningEffort": "high"}
                    ]
                },
                {
                    "id": "secret-hidden",
                    "displayName": "Hidden",
                    "hidden": true,
                    "isDefault": false,
                    "supportedReasoningEfforts": []
                },
                {
                    "id": "gpt-5.5",
                    "displayName": "GPT-5.5",
                    "description": "Legacy.",
                    "hidden": false,
                    "isDefault": false,
                    "supportedReasoningEfforts": [
                        {"reasoningEffort": "low"},
                        {"reasoningEffort": "medium"},
                        {"reasoningEffort": "high"},
                        {"reasoningEffort": "xhigh"}
                    ]
                }
            ]
        });
        let (models, default_id) = parse_models(&res);
        assert_eq!(default_id.as_deref(), Some("gpt-6.2-sol"));
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-6.2-sol");
        assert_eq!(models[0].label, "GPT-6.2-Sol");
        assert_eq!(models[0].description, "Fast model.");
        assert_eq!(
            models[0].effort_levels,
            [Effort::Low, Effort::High, Effort::Max]
        );
        assert_eq!(models[1].id, "gpt-5.5");
        assert_eq!(
            models[1].effort_levels,
            [Effort::Low, Effort::Medium, Effort::High, Effort::XHigh]
        );
    }

    #[test]
    fn default_falls_back_to_first_model_when_none_marked_default() {
        let res = json!({"data": [{"id": "m1", "hidden": false, "isDefault": false}]});
        let (models, default_id) = parse_models(&res);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].label, "m1");
        assert_eq!(default_id.as_deref(), Some("m1"));
    }

    #[test]
    fn parses_account_variants() {
        assert_eq!(
            parse_account(&json!({"account": {"type": "apiKey"}, "requiresOpenaiAuth": true})),
            AccountStatus::LoggedIn(Some("API key".into()))
        );
        assert_eq!(
            parse_account(&json!({
                "account": {"type": "chatgpt", "email": "u@example.com", "planType": "plus"},
                "requiresOpenaiAuth": true
            })),
            AccountStatus::LoggedIn(Some("u@example.com".into()))
        );
        assert_eq!(
            parse_account(&json!({"account": null, "requiresOpenaiAuth": true})),
            AccountStatus::NotLoggedIn
        );
        assert_eq!(
            parse_account(&json!({"account": null, "requiresOpenaiAuth": false})),
            AccountStatus::LoggedIn(None)
        );
    }
}
