//! JSON-RPC 2.0 line framing for `codex app-server`: one JSON object per line, in both directions.
//!
//! Frames stay `serde_json::Value`: app-server carries many fields we never read, and unknown ones
//! must be skipped, never fatal. Responses omit `"jsonrpc"` and notifications may carry top-level
//! `"emittedAtMs"`; both are optional. Ids from codex are integers starting at 0; keep the original
//! `Value` to answer with and use [`id_key`] where a string key is needed.

use serde_json::{json, Value};

/// A JSON-RPC error object (`{"code", "message"}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// What codex sent us.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// A call from codex that expects a reply (has an `id`).
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

/// A string key for a codex request id (`0` and `"0"` both give `"0"`), used to find a pending request.
pub fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// True when `params` carries `threadId == thread_id`.
pub fn for_thread(params: &Value, thread_id: &str) -> bool {
    params.get("threadId").and_then(Value::as_str) == Some(thread_id)
}

/// A request line, newline included.
pub fn request(id: u64, method: &str, params: Value) -> String {
    line(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
}

/// A notification line (no `id`), newline included. `None` omits `"params"` (e.g. `initialized`).
pub fn notification(method: &str, params: Option<Value>) -> String {
    match params {
        Some(p) => line(json!({"jsonrpc": "2.0", "method": method, "params": p})),
        None => line(json!({"jsonrpc": "2.0", "method": method})),
    }
}

/// A success reply to a codex request. `id` is the original value, so a numeric id stays numeric.
pub fn response_ok(id: &Value, result: Value) -> String {
    line(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// An error reply to a codex request.
pub fn response_err(id: &Value, code: i64, message: &str) -> String {
    line(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
}

/// The reply for any codex server request we do not implement; never leave one unanswered, codex would block.
pub fn method_not_found(id: &Value) -> String {
    response_err(id, -32601, "unsupported")
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
    fn classifies_the_three_codex_shapes() {
        let req = parse_line(
            r#"{"method":"item/commandExecution/requestApproval","id":0,"params":{"itemId":"x"}}"#,
        );
        assert!(
            matches!(&req, Some(Incoming::Request { id, method, .. }) if *id == json!(0) && method == "item/commandExecution/requestApproval")
        );
        let note = parse_line(
            r#"{"method":"item/agentMessage/delta","params":{"delta":"hi"},"emittedAtMs":1234}"#,
        );
        assert!(matches!(note, Some(Incoming::Notification { .. })));
        let ok = parse_line(r#"{"id":1,"result":{"userAgent":"xade"}}"#);
        assert_eq!(
            ok,
            Some(Incoming::Response {
                id: json!(1),
                result: Ok(json!({"userAgent": "xade"}))
            })
        );
        let err = parse_line(
            r#"{"error":{"code":-32600,"message":"no rollout found for thread id x"},"id":2}"#,
        );
        assert_eq!(
            err,
            Some(Incoming::Response {
                id: json!(2),
                result: Err(RpcError {
                    code: -32600,
                    message: "no rollout found for thread id x".into()
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
    fn numeric_id_zero_round_trips_unchanged() {
        let Some(Incoming::Request { id, .. }) =
            parse_line(r#"{"method":"item/fileChange/requestApproval","id":0,"params":{}}"#)
        else {
            panic!("not a request");
        };
        assert_eq!(id_key(&id), "0");
        let reply = response_ok(&id, json!({"decision": "accept"}));
        let v: Value = serde_json::from_str(&reply).expect("json");
        assert_eq!(v["id"], json!(0));
        assert!(reply.ends_with('\n'));
    }

    #[test]
    fn unsupported_request_replies_with_method_not_found() {
        let Some(Incoming::Request { id, .. }) = parse_line(r#"{"method":"x/y","id":42}"#) else {
            panic!("not a request");
        };
        let v: Value = serde_json::from_str(&method_not_found(&id)).expect("json");
        assert_eq!(v["id"], json!(42));
        assert_eq!(v["error"]["code"], json!(-32601));
    }

    #[test]
    fn requests_and_notifications_serialize_as_single_lines() {
        let r = request(3, "turn/start", json!({"threadId": "t"}));
        assert_eq!(r.matches('\n').count(), 1);
        let v: Value = serde_json::from_str(&r).expect("json");
        assert_eq!(
            (v["jsonrpc"].as_str(), v["id"].as_u64()),
            (Some("2.0"), Some(3))
        );
        let n: Value = serde_json::from_str(&notification("initialized", None)).expect("json");
        assert!(n.get("id").is_none());
        assert!(n.get("params").is_none());
        let np: Value =
            serde_json::from_str(&notification("x", Some(json!({"a": 1})))).expect("json");
        assert_eq!(np["params"], json!({"a": 1}));
    }

    #[test]
    fn thread_filter_matches_only_the_active_thread() {
        assert!(for_thread(&json!({"threadId": "a"}), "a"));
        assert!(!for_thread(&json!({"threadId": "b"}), "a"));
        assert!(!for_thread(&json!({}), "a"));
    }
}
