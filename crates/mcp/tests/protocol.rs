//! Protocol-level tests for the hand-rolled JSON-RPC layer.
//!
//! These run against a stub backend: the failure modes worth guarding — answering a
//! notification, dropping the banner on an error path, a malformed `tools/list` — are
//! framing bugs, and none of them need real log data.

use logbear_core::model::{EntryId, EntryView, Level};
use logbear_core::proto::{ProjectRef, QueryResult, RunDiff, RunRef, UnreadState};
use logbear_core::{Error, Result};
use logbear_mcp::backend::Backend;
use logbear_mcp::Server;
use serde_json::{json, Value};

/// A backend with no daemon behind it. `unread` is always non-empty so every test can
/// assert the banner, and a query mentioning "boom" fails so the error path is reachable.
#[derive(Default)]
struct Stub {
    last_project: Option<String>,
    fail_unread: bool,
}

impl Stub {
    fn note(&mut self, project: &ProjectRef) {
        if let ProjectRef::Path(p) = project {
            self.last_project = Some(p.clone());
        }
    }
}

impl Backend for Stub {
    fn unread(&mut self, project: ProjectRef, _session: &str) -> Result<UnreadState> {
        self.note(&project);
        if self.fail_unread {
            return Err(Error::other("daemon is down"));
        }
        Ok(UnreadState {
            new_errors: 6,
            new_signatures: 2,
            oldest_at: Some(logbear_core::now_micros() - 40_000_000),
            newest_entry_id: Some(99),
            blocks_issued: 0,
            blocks_remaining: 3,
            change_blocks_remaining: 2,
            push_enabled: true,
            unseen_lines: 0,
            server_running: false,
            entries: Vec::new(),
        })
    }

    fn schema(&mut self, project: ProjectRef) -> Result<Value> {
        self.note(&project);
        Ok(Value::String("CREATE TABLE runs (...);".into()))
    }

    fn query(
        &mut self,
        project: ProjectRef,
        sql: &str,
        _params: Vec<Value>,
        _max_rows: Option<usize>,
    ) -> Result<QueryResult> {
        self.note(&project);
        if sql.contains("boom") {
            return Err(Error::QueryRejected("DELETE is not permitted".into()));
        }
        Ok(QueryResult {
            columns: vec!["n".into()],
            rows: vec![vec![json!(1)]],
            row_count: 1,
            truncated: false,
            elapsed_ms: 1,
        })
    }

    fn wait(&mut self, _p: ProjectRef, _t: u64, _l: Option<Level>) -> Result<Vec<EntryView>> {
        Ok(Vec::new())
    }

    fn diff(&mut self, _p: ProjectRef, _a: Option<RunRef>, _b: Option<RunRef>) -> Result<RunDiff> {
        Err(Error::other("no run to compare"))
    }

    fn ack(&mut self, _p: ProjectRef, _s: &str, _e: Option<EntryId>) -> Result<UnreadState> {
        Ok(UnreadState::default())
    }
}

fn server() -> Server<Stub> {
    Server::new(Stub::default(), "mcp-test".into())
}

fn one(server: &mut Server<Stub>, line: &str) -> Value {
    let mut out = server.handle_line(line);
    assert_eq!(out.len(), 1, "expected exactly one message for {line}");
    out.remove(0)
}

fn call(server: &mut Server<Stub>, name: &str, args: Value) -> Value {
    one(
        server,
        &json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        })
        .to_string(),
    )
}

fn text_of(response: &Value) -> String {
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("a text content block")
        .to_string()
}

#[test]
fn initialize_reports_the_protocol_version_and_server_info() {
    let mut s = server();
    let r = one(
        &mut s,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
    );
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(r["result"]["serverInfo"]["name"], "logbear");
    assert!(r["result"]["serverInfo"]["version"].is_string());
    assert!(r["result"]["capabilities"]["tools"].is_object());
    assert_eq!(r["id"], 1);
    assert_eq!(r["jsonrpc"], "2.0");
}

#[test]
fn a_notification_gets_no_response() {
    let mut s = server();
    // No roots capability declared, so `initialized` has nothing to trigger either.
    s.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#);
    assert!(s
        .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .is_empty());
    assert!(s
        .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#)
        .is_empty());
    assert!(s
        .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/something/unheard/of"}"#)
        .is_empty());
}

#[test]
fn an_unknown_method_is_method_not_found() {
    let mut s = server();
    let r = one(&mut s, r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#);
    assert_eq!(r["error"]["code"], -32601);
    assert_eq!(r["id"], 4);
    assert!(r["result"].is_null());
}

#[test]
fn malformed_input_is_reported_without_killing_the_stream() {
    let mut s = server();
    assert_eq!(one(&mut s, "{not json").get("error").unwrap()["code"], -32700);
    // Batching was removed in 2025-06-18 and must not be silently half-handled.
    assert_eq!(one(&mut s, r#"[{"id":1,"method":"ping"}]"#)["error"]["code"], -32600);
    // The stream still works afterwards.
    assert_eq!(
        one(&mut s, r#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#)["result"],
        json!({})
    );
}

#[test]
fn tools_list_is_the_five_tools_with_valid_schemas() {
    let mut s = server();
    let r = one(&mut s, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    let tools = r["result"]["tools"].as_array().expect("a tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "logbear_schema",
            "logbear_query",
            "logbear_wait",
            "logbear_diff",
            "logbear_ack"
        ]
    );
    for t in tools {
        assert!(t["description"].as_str().is_some_and(|d| d.len() > 40));
        assert_eq!(t["inputSchema"]["type"], "object");
        assert!(t["inputSchema"]["properties"].is_object());
    }
    // The tools with mandatory arguments say so.
    let by_name = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap().clone();
    assert_eq!(by_name("logbear_query")["inputSchema"]["required"], json!(["sql"]));
    assert_eq!(
        by_name("logbear_wait")["inputSchema"]["required"],
        json!(["timeout_ms"])
    );
    // Only logbear_ack writes.
    assert_eq!(by_name("logbear_ack")["annotations"]["readOnlyHint"], false);
    assert_eq!(by_name("logbear_query")["annotations"]["readOnlyHint"], true);
}

#[test]
fn the_banner_rides_on_a_successful_result() {
    let mut s = server();
    let r = call(&mut s, "logbear_query", json!({ "sql": "SELECT 1" }));
    assert_eq!(r["result"]["isError"], false);
    let text = text_of(&r);
    assert!(text.starts_with("⚠️ 6 new errors"), "{text}");
    assert!(text.contains("2 new signatures"), "{text}");
    assert!(text.contains("\"row_count\":1"), "{text}");
}

#[test]
fn the_banner_rides_on_an_error_result_too() {
    let mut s = server();
    let r = call(&mut s, "logbear_query", json!({ "sql": "DELETE FROM boom" }));
    assert_eq!(r["result"]["isError"], true);
    let text = text_of(&r);
    assert!(text.starts_with("⚠️ 6 new errors"), "{text}");
    assert!(text.contains("DELETE FROM boom"), "{text}");
    assert!(text.contains("logbear_schema()"), "{text}");
}

#[test]
fn a_tool_error_is_never_a_bare_rust_error() {
    let mut s = server();
    for (tool, args) in [
        ("logbear_query", json!({})),
        ("logbear_wait", json!({})),
        ("logbear_diff", json!({ "a": "yesterday" })),
        ("logbear_ack", json!({ "entry_id": "nope" })),
        ("logbear_nonexistent", json!({})),
    ] {
        let r = call(&mut s, tool, args);
        assert_eq!(r["result"]["isError"], true, "{tool}");
        let text = text_of(&r);
        assert!(text.starts_with("⚠️"), "{tool}: banner missing from {text}");
        // Two lines minimum: what went wrong, and what to do about it.
        assert!(text.lines().count() >= 3, "{tool}: {text}");
    }
}

#[test]
fn an_unknown_tool_names_the_real_ones() {
    let mut s = server();
    let text = text_of(&call(&mut s, "logbear_tail", json!({})));
    assert!(text.contains("logbear_query"), "{text}");
    assert!(text.contains("logbear_schema"), "{text}");
}

#[test]
fn a_dead_daemon_costs_the_banner_but_not_the_response() {
    let mut s = Server::new(
        Stub {
            fail_unread: true,
            ..Default::default()
        },
        "mcp-test".into(),
    );
    let r = call(&mut s, "logbear_schema", json!({}));
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(text_of(&r), "CREATE TABLE runs (...);");
}

#[test]
fn roots_are_requested_after_initialized_and_used_for_routing() {
    let mut s = server();
    s.handle_line(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{"roots":{"listChanged":true}}}}"#,
    );

    let asked = one(&mut s, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    assert_eq!(asked["method"], "roots/list");
    let request_id = asked["id"].clone();
    assert!(asked["result"].is_null(), "a request, not a response");

    // The client's answer is itself a message on the same stream.
    let answer = json!({
        "jsonrpc": "2.0", "id": request_id,
        "result": { "roots": [{ "uri": "file:///tmp/my%20project", "name": "my project" }] }
    });
    assert!(s.handle_line(&answer.to_string()).is_empty());
    assert_eq!(s.roots(), [std::path::PathBuf::from("/tmp/my project")]);

    // And the project handed to the daemon is the root, not the process cwd.
    call(&mut s, "logbear_schema", json!({}));
    assert_eq!(
        s.backend().last_project.as_deref(),
        Some("/tmp/my project")
    );
}

#[test]
fn a_roots_change_notification_re_requests() {
    let mut s = server();
    let out = s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/roots/list_changed"}"#);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["method"], "roots/list");
}

#[test]
fn no_roots_falls_back_to_the_process_cwd() {
    let mut s = server();
    s.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#);
    call(&mut s, "logbear_schema", json!({}));
    let cwd = std::env::current_dir().unwrap().to_string_lossy().into_owned();
    assert_eq!(s.backend().last_project.as_deref(), Some(cwd.as_str()));
}

#[test]
fn ping_is_an_empty_object() {
    let mut s = server();
    assert_eq!(
        one(&mut s, r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#),
        json!({ "jsonrpc": "2.0", "id": "p", "result": {} })
    );
}
