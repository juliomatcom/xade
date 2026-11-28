//! JSON-RPC 2.0 line framing for `opencode acp`: one JSON object per line, in both directions.
//!
//! Frames stay `serde_json::Value`: ACP carries many fields we never read, and unknown ones must be
//! skipped, never fatal. Ids from OpenCode may be numbers or strings; keep the original `Value`
//! to answer with and use [`id_key`] where a string key is needed.

use serde_json::{json, Value};

/// A JSON-RPC error object (`{"code", "message"}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// What OpenCode sent us.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// A call from OpenCode that expects a reply (has an `id`).
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// The answer to one of our requests.
    Response {
        id: Value,
        result: Result<Value, RpcError>,
    },
}

/// Classifies one stdout line. Invalid JSON and frames that are none of the three shapes give `None`.
pub fn parse_line(line: &str) -> Option<Incoming> {
    let v: Value = serde_json::from_str(line).ok()?;
    parse_frame(&v)
}

pub fn parse_frame(v: &Value) -> Option<Incoming> {
    let id = v.get("id").filter(|id| !id.is_null()).cloned();
    if let Some(method) = v.get("method").and_then(Value::as_str) {
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        let method = method.to_string();
        return Some(match id {
            Some(id) => Incoming::Request { id, method, params },
            None => Incoming::Notification { method, params },
        });
    }
    let id = id?;
    if let Some(err) = v.get("error") {
        return Some(Incoming::Response {
            id,
            result: Err(RpcError {
                code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
        });
    }
    let result = v.get("result")?.clone();
    Some(Incoming::Response {
        id,
        result: Ok(result),
    })
}

/// A string key for an OpenCode request id (`"1"` and `1` both give `1`), used to find a pending request.
pub fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The `update` object of a `session/update` notification, only when it is for `session_id`.
pub fn update_for<'a>(params: &'a Value, session_id: &str) -> Option<&'a Value> {
    if params.get("sessionId").and_then(Value::as_str) != Some(session_id) {
        return None;
    }
    params.get("update")
}

/// A request line, newline included.
pub fn request(id: u64, method: &str, params: Value) -> String {
    line(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
}

/// A notification line (no `id`), newline included.
pub fn notification(method: &str, params: Value) -> String {
    line(json!({"jsonrpc": "2.0", "method": method, "params": params}))
}

/// A success reply to an OpenCode request. `id` is the original value, so a string id stays a string.
pub fn response_ok(id: &Value, result: Value) -> String {
    line(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// An error reply to an OpenCode request.
pub fn response_err(id: &Value, code: i64, message: &str) -> String {
    line(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
}

/// The reply for any OpenCode request we do not implement; never leave one unanswered, OpenCode
/// would block.
pub fn method_not_found(id: &Value) -> String {
    response_err(id, -32601, "Method not found")
}

fn line(v: Value) -> String {
    let mut s = v.to_string();
    s.push('\n');
    s
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn classifies_the_three_shapes() {
        let req = parse_line(
            r#"{"jsonrpc":"2.0","method":"session/request_permission","params":{"a":1},"id":"1"}"#,
        );
        assert!(
            matches!(&req, Some(Incoming::Request { id, method, .. }) if id == "1" && method == "session/request_permission")
        );
        let note = parse_line(r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#);
        assert!(matches!(note, Some(Incoming::Notification { .. })));
        let ok = parse_line(r#"{"jsonrpc":"2.0","result":{"x":1},"id":7}"#);
        assert_eq!(
            ok,
            Some(Incoming::Response {
                id: json!(7),
                result: Ok(json!({"x": 1}))
            })
        );
        let err = parse_line(
            r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"Invalid params: session not found: x"},"id":3}"#,
        );
        assert_eq!(
            err,
            Some(Incoming::Response {
                id: json!(3),
                result: Err(RpcError {
                    code: -32602,
                    message: "Invalid params: session not found: x".into()
                })
            })
        );
    }

    #[test]
    fn junk_is_skipped() {
        assert_eq!(parse_line("not json"), None);
        assert_eq!(parse_line(r#"{"jsonrpc":"2.0"}"#), None);
        assert_eq!(parse_line(r#"{"id":1}"#), None);
        assert_eq!(parse_line("[]"), None);
    }

    #[test]
    fn a_string_id_round_trips_unchanged() {
        let Some(Incoming::Request { id, .. }) =
            parse_line(r#"{"method":"session/request_permission","id":"1"}"#)
        else {
            panic!("not a request");
        };
        assert_eq!(id_key(&id), "1");
        let reply = response_ok(&id, json!({"outcome": {"outcome": "cancelled"}}));
        let v: Value = serde_json::from_str(&reply).expect("json");
        assert_eq!(v["id"], json!("1"));
        assert!(reply.ends_with('\n'));
    }

    #[test]
    fn a_numeric_id_round_trips_unchanged() {
        let Some(Incoming::Request { id, .. }) = parse_line(r#"{"method":"x/y","id":42}"#) else {
            panic!("not a request");
        };
        assert_eq!(id_key(&id), "42");
        let v: Value = serde_json::from_str(&method_not_found(&id)).expect("json");
        assert_eq!(v["id"], json!(42));
        assert_eq!(v["error"]["code"], json!(-32601));
    }

    #[test]
    fn requests_and_notifications_serialize_as_single_lines() {
        let r = request(3, "session/prompt", json!({"sessionId": "s"}));
        assert_eq!(r.matches('\n').count(), 1);
        let v: Value = serde_json::from_str(&r).expect("json");
        assert_eq!(
            (v["jsonrpc"].as_str(), v["id"].as_u64()),
            (Some("2.0"), Some(3))
        );
        let n: Value =
            serde_json::from_str(&notification("session/cancel", json!({}))).expect("json");
        assert!(n.get("id").is_none());
    }

    #[test]
    fn updates_for_another_session_are_dropped() {
        let params = json!({"sessionId": "a", "update": {"sessionUpdate": "usage_update"}});
        assert!(update_for(&params, "a").is_some());
        assert!(update_for(&params, "b").is_none());
        assert!(update_for(&json!({"update": {}}), "a").is_none());
    }
}
