//! The MCP state machine: initialize, roots, tools.
//!
//! The whole server is `handle_line(&str) -> Vec<Value>` — messages in, messages out, no
//! I/O — so the framing rules are testable without a daemon, a pipe, or a client.
//!
//! Server-to-client requests go through the same loop rather than a nested read: a
//! `roots/list` records its outstanding id and returns, and the client's reply arrives as
//! another line. A nested read would deadlock as soon as a client interleaved a
//! `tools/call` with its answer.

use crate::backend::Backend;
use crate::jsonrpc::{self, Id, Incoming};
use crate::tools;
use runlog_core::proto::ProjectRef;
use serde_json::{json, Value};
use std::path::PathBuf;

pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const SERVER_NAME: &str = "runlog";

/// Standing context sent with `initialize`. Without it a model reaches for `entries`, which
/// holds no text, instead of `v_entries`.
const INSTRUCTIONS: &str = "runlog captures this project's dev-server output. Call \
    runlog_schema() once, then answer questions with runlog_query(). Tool results are \
    prefixed with a warning when errors have appeared that you have not seen yet.";

pub struct Server<B: Backend> {
    backend: B,
    session_id: String,
    /// Workspace roots declared by the client. First one wins for project resolution.
    roots: Vec<PathBuf>,
    client_supports_roots: bool,
    next_outgoing_id: i64,
    /// Id of the `roots/list` we are waiting on, if any.
    pending_roots: Option<Id>,
}

impl<B: Backend> Server<B> {
    pub fn new(backend: B, session_id: String) -> Server<B> {
        Server {
            backend,
            session_id,
            roots: Vec::new(),
            client_supports_roots: false,
            next_outgoing_id: 1,
            pending_roots: None,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Which project the caller means.
    ///
    /// Roots first: a globally-configured MCP server need not be spawned in the project
    /// directory, so the process cwd is only a fallback. Resolving a subdirectory to its
    /// git root is the daemon's job.
    fn project(&self) -> ProjectRef {
        match self.roots.first() {
            Some(root) => ProjectRef::Path(root.to_string_lossy().into_owned()),
            None => ProjectRef::cwd(),
        }
    }

    pub fn handle_line(&mut self, line: &str) -> Vec<Value> {
        match jsonrpc::parse(line) {
            Ok(msg) => self.handle(msg),
            Err(e) => vec![jsonrpc::error(
                None,
                jsonrpc::PARSE_ERROR,
                format!("invalid JSON: {e}"),
            )],
        }
    }

    pub fn handle(&mut self, msg: Incoming) -> Vec<Value> {
        match msg {
            Incoming::Request { id, method, params } => self.request(id, &method, params),
            Incoming::Notification { method, params } => self.notification(&method, params),
            Incoming::Response { id, result, error } => self.client_response(id, result, error),
            Incoming::Invalid { id, reason } => {
                vec![jsonrpc::error(
                    id.as_ref(),
                    jsonrpc::INVALID_REQUEST,
                    reason,
                )]
            }
        }
    }

    fn request(&mut self, id: Id, method: &str, params: Value) -> Vec<Value> {
        match method {
            "initialize" => vec![jsonrpc::result(&id, self.initialize(&params))],
            "ping" => vec![jsonrpc::result(&id, json!({}))],
            "tools/list" => vec![jsonrpc::result(
                &id,
                json!({ "tools": tools::descriptors() }),
            )],
            "tools/call" => vec![self.tools_call(&id, &params)],
            // A conformant client never sends these, since no `resources`/`prompts`
            // capability is advertised, but many probe anyway.
            other => vec![jsonrpc::error(
                Some(&id),
                jsonrpc::METHOD_NOT_FOUND,
                format!("unknown method {other:?}"),
            )],
        }
    }

    fn notification(&mut self, method: &str, _params: Value) -> Vec<Value> {
        match method {
            // A client is not obliged to serve requests before it reports initialized.
            "notifications/initialized" if self.client_supports_roots => {
                vec![self.request_roots()]
            }
            "notifications/roots/list_changed" => {
                self.client_supports_roots = true;
                vec![self.request_roots()]
            }
            // A notification carries no id and must never be answered.
            _ => Vec::new(),
        }
    }

    fn client_response(&mut self, id: Id, result: Option<Value>, error: Option<Value>) -> Vec<Value> {
        if self.pending_roots.as_ref() == Some(&id) {
            self.pending_roots = None;
            match (result, error) {
                (Some(r), _) => self.set_roots(&r),
                (None, Some(e)) => {
                    // A client may advertise roots and still refuse to list them.
                    eprintln!("runlog-mcp: roots/list refused ({e}); falling back to cwd");
                }
                _ => {}
            }
        }
        Vec::new()
    }

    fn request_roots(&mut self) -> Value {
        let id = Id::Num(self.next_outgoing_id);
        self.next_outgoing_id += 1;
        self.pending_roots = Some(id.clone());
        jsonrpc::request(&id, "roots/list", json!({}))
    }

    fn set_roots(&mut self, result: &Value) {
        let roots = result
            .get("roots")
            .and_then(|r| r.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]);
        self.roots = roots
            .iter()
            .filter_map(|r| r.get("uri").and_then(|u| u.as_str()))
            .filter_map(file_uri_to_path)
            .collect();
        eprintln!("runlog-mcp: {} workspace root(s) from the client", self.roots.len());
    }

    fn initialize(&mut self, params: &Value) -> Value {
        self.client_supports_roots = params
            .get("capabilities")
            .and_then(|c| c.get("roots"))
            .is_some();

        // Not in the specification, but some clients put concrete roots in the initialize
        // params, which saves a round trip.
        if let Some(list) = params.get("roots").and_then(|r| r.as_array()) {
            let declared: Vec<PathBuf> = list
                .iter()
                .filter_map(|r| {
                    r.get("uri")
                        .and_then(|u| u.as_str())
                        .and_then(file_uri_to_path)
                })
                .collect();
            if !declared.is_empty() {
                self.roots = declared;
            }
        }

        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        })
    }

    fn tools_call(&mut self, id: &Id, params: &Value) -> Value {
        let name = match params.get("name").and_then(|n| n.as_str()) {
            Some(n) => n.to_string(),
            None => {
                return jsonrpc::error(
                    Some(id),
                    jsonrpc::INVALID_PARAMS,
                    "tools/call requires a `name`",
                )
            }
        };
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);

        // The banner is computed before the call and prefixed to the result even when the
        // call fails: in a client with no hook support it is the entire push mechanism.
        let banner = self.banner();
        let project = self.project();
        let session_id = self.session_id.clone();
        let outcome = tools::dispatch(&mut self.backend, project, &session_id, &name, &args);

        let text = match banner {
            Some(b) => format!("{b}\n\n{}", outcome.text),
            None => outcome.text,
        };
        jsonrpc::result(
            id,
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": outcome.is_error,
            }),
        )
    }

    fn banner(&mut self) -> Option<String> {
        match self.backend.unread(self.project(), &self.session_id) {
            Ok(state) => state.banner(),
            Err(e) => {
                eprintln!("runlog-mcp: unread state unavailable ({e})");
                None
            }
        }
    }
}

/// `file:///Users/x/my%20project` → `/Users/x/my project`.
///
/// Hand-rolled rather than pulling in a URL crate: the only scheme MCP roots may use is
/// `file:`, and the only escaping seen in practice is percent-encoded spaces.
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///path` leaves a leading `/`; `file://host/path` is unsupported.
    let rest = match rest.find('/') {
        Some(0) => rest,
        _ => return None,
    };
    let mut out = Vec::with_capacity(rest.len());
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

/// A session identity, stable for the life of this process and distinct from every other
/// runlog-mcp on the machine.
///
/// Hashes pid, start time and cwd together: a pid alone is reused by the OS, and a reused
/// pid would inherit a stale watermark and swallow another session's errors.
pub fn derive_session_id() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let seed = format!("{}:{}:{cwd}", std::process::id(), runlog_core::now_micros());
    format!("mcp-{}", &blake3::hash(seed.as_bytes()).to_hex()[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_decode() {
        assert_eq!(
            file_uri_to_path("file:///Users/x/proj"),
            Some(PathBuf::from("/Users/x/proj"))
        );
        assert_eq!(
            file_uri_to_path("file:///Users/x/my%20proj"),
            Some(PathBuf::from("/Users/x/my proj"))
        );
        assert_eq!(file_uri_to_path("https://example.com"), None);
    }

    #[test]
    fn session_ids_are_distinct() {
        assert_ne!(derive_session_id(), derive_session_id());
    }
}
