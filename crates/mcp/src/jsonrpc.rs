//! JSON-RPC 2.0, hand-rolled.
//!
//! The MCP surface runlog needs — `initialize`, `tools/list`, `tools/call`, `ping`, and a
//! server-to-client `roots/list` — is a few hundred lines of a protocol that has not
//! changed since 2010. The official Rust SDK is at `3.0.0-beta` with an API that is still
//! moving, and a churning dependency on the critical path of the product's most visible
//! surface is a worse trade than owning this file.
//!
//! One rule dominates the whole crate: **stdout carries protocol frames and nothing else.**
//! A stray `println!` corrupts the stream and the client disconnects with no useful error,
//! so every diagnostic in this crate goes to stderr.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// A JSON-RPC id. The spec allows a string or a number and forbids reusing one; we echo
/// back whatever the client sent rather than normalising, because clients index on it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Num(i64),
    Str(String),
}

/// A parsed inbound message.
///
/// The distinction that matters most is `Request` vs `Notification`: a notification has no
/// `id` and **must never** be answered. Replying to one is the single most common way to
/// wedge an MCP client, because it then has a response it cannot correlate.
#[derive(Debug)]
pub enum Incoming {
    Request { id: Id, method: String, params: Value },
    Notification { method: String, params: Value },
    /// The client answering a request *we* sent, e.g. `roots/list`.
    Response { id: Id, result: Option<Value>, error: Option<Value> },
    /// Well-formed JSON that is not a well-formed JSON-RPC message.
    Invalid { id: Option<Id>, reason: String },
}

/// Parse one line of the stream.
///
/// `Err` is reserved for "this was not JSON at all", which is the only case that maps to
/// `-32700`; everything else is a structural complaint carried in [`Incoming::Invalid`] so
/// the caller can still echo the id back.
pub fn parse(line: &str) -> Result<Incoming, String> {
    let value: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;

    // Protocol revision 2025-06-18 removed JSON-RPC batching. Naming that explicitly beats
    // a generic "invalid request" when a client built against an older revision connects.
    if value.is_array() {
        return Ok(Incoming::Invalid {
            id: None,
            reason: "JSON-RPC batching was removed in protocol version 2025-06-18; \
                     send one message per line"
                .into(),
        });
    }

    let obj = match value.as_object() {
        Some(o) => o,
        None => {
            return Ok(Incoming::Invalid {
                id: None,
                reason: "a JSON-RPC message must be an object".into(),
            })
        }
    };

    let id = obj.get("id").and_then(|v| match v {
        Value::Number(n) => n.as_i64().map(Id::Num),
        Value::String(s) => Some(Id::Str(s.clone())),
        _ => None,
    });

    match obj.get("method").and_then(|m| m.as_str()) {
        Some(method) => {
            let params = obj.get("params").cloned().unwrap_or(Value::Null);
            let method = method.to_string();
            Ok(match id {
                Some(id) => Incoming::Request { id, method, params },
                None => Incoming::Notification { method, params },
            })
        }
        None if obj.contains_key("result") || obj.contains_key("error") => match id {
            Some(id) => Ok(Incoming::Response {
                id,
                result: obj.get("result").cloned(),
                error: obj.get("error").cloned(),
            }),
            None => Ok(Incoming::Invalid {
                id: None,
                reason: "a response must carry an id".into(),
            }),
        },
        None => Ok(Incoming::Invalid {
            id,
            reason: "a message must carry either a method or a result/error".into(),
        }),
    }
}

pub fn result(id: &Id, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error(id: Option<&Id>, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.cloned().map(|i| serde_json::to_value(i).unwrap_or(Value::Null)),
        "error": { "code": code, "message": message.into() },
    })
}

/// A request from server to client. Only `roots/list` uses this.
pub fn request(id: &Id, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notification_has_no_id() {
        match parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap() {
            Incoming::Notification { method, .. } => {
                assert_eq!(method, "notifications/initialized")
            }
            other => panic!("expected a notification, got {other:?}"),
        }
    }

    #[test]
    fn a_request_keeps_its_id_type() {
        match parse(r#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#).unwrap() {
            Incoming::Request { id, .. } => assert_eq!(id, Id::Str("abc".into())),
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[test]
    fn a_client_response_is_not_a_request() {
        match parse(r#"{"jsonrpc":"2.0","id":1,"result":{"roots":[]}}"#).unwrap() {
            Incoming::Response { id, result, .. } => {
                assert_eq!(id, Id::Num(1));
                assert!(result.is_some());
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn garbage_is_a_parse_error() {
        assert!(parse("{not json").is_err());
    }

    #[test]
    fn batches_are_refused_by_name() {
        match parse(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).unwrap() {
            Incoming::Invalid { reason, .. } => assert!(reason.contains("batching"), "{reason}"),
            other => panic!("expected invalid, got {other:?}"),
        }
    }
}
