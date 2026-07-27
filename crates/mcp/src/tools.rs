//! The five tools, and their dispatch.
//!
//! Every tool's JSON Schema is standing context for as long as the server is connected, so
//! the surface stays small: one schema tool whose output can grow without costing standing
//! context, one read-only SQL endpoint, and three things SQL cannot express — blocking on
//! new output, diffing two runs against git, and advancing a watermark.

use crate::backend::Backend;
use runlog_core::model::{EntryView, Level};
use runlog_core::proto::{ProjectRef, RunDiff, RunRef};
use serde_json::{json, Map, Value};

pub const TOOL_NAMES: [&str; 5] = [
    "runlog_schema",
    "runlog_query",
    "runlog_wait",
    "runlog_diff",
    "runlog_ack",
];

/// What a `tools/call` produced, before the banner is prefixed.
pub struct Outcome {
    pub text: String,
    pub is_error: bool,
}

impl Outcome {
    fn ok(text: impl Into<String>) -> Outcome {
        Outcome {
            text: text.into(),
            is_error: false,
        }
    }

    /// An error a model can act on.
    ///
    /// Never a bare Rust error string: `hint` must give a route to repair, and `detail`
    /// should echo the failing input.
    fn err(what: &str, detail: impl std::fmt::Display, hint: &str) -> Outcome {
        Outcome {
            text: format!("{what} failed: {detail}\n\n{hint}"),
            is_error: true,
        }
    }
}

const SCHEMA_HINT: &str =
    "Call runlog_schema() for the tables, views, SQL functions and worked examples \
     available to you, then retry.";

/// The `tools/list` payload.
pub fn descriptors() -> Value {
    json!([
        {
            "name": "runlog_schema",
            "title": "runlog schema",
            "description":
                "Full schema of this project's runlog database: DDL, the v_entries/v_errors/v_runs \
                 views, the SQL functions available to you, worked example queries, and this \
                 project's entry-delimiter mode and retention state. Call this before writing SQL \
                 for runlog_query.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "runlog_query",
            "title": "Query runlog",
            "description":
                "Run read-only SQL against the resolved project's runlog database and return \
                 columns plus rows. SELECT only: writes, ATTACH, PRAGMA and extension loading are \
                 refused, statements are cancelled after 5s, and results are capped. Prefer the \
                 v_entries / v_errors / v_runs views over the raw tables — entries hold no text, \
                 only a line range. Get the schema from runlog_schema() first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "A single SELECT statement. Use ?1, ?2 … for parameters."
                    },
                    "params": {
                        "type": "array",
                        "description": "Positional bindings for ?1, ?2 … Never interpolate values into the SQL.",
                        "items": {}
                    },
                    "max_rows": {
                        "type": "integer",
                        "description": "Row cap. Default 200, ceiling 2000.",
                        "minimum": 1,
                        "maximum": 2000
                    }
                },
                "required": ["sql"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "runlog_wait",
            "title": "Wait for new output",
            "description":
                "Block until a new log entry appears in this project or the timeout expires, and \
                 return whatever arrived. Use immediately after making a change so you observe the \
                 dev server's reaction instead of guessing; this is the one thing not expressible \
                 as a query.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "timeout_ms": {
                        "type": "integer",
                        "description": "How long to wait, in milliseconds.",
                        "minimum": 1,
                        "maximum": 600000
                    },
                    "level": {
                        "type": "string",
                        "description": "Only return entries at this level. Omit to match any entry.",
                        "enum": ["error", "warn", "info", "debug", "trace", "unknown"]
                    }
                },
                "required": ["timeout_ms"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "runlog_diff",
            "title": "Diff two runs",
            "description":
                "Compare two runs: error signatures new in B, signatures gone since A, count \
                 deltas, the git commits and files between the two SHAs, and which lockfiles \
                 moved. Defaults to last-known-good vs latest, which is what 'it worked yesterday' \
                 means. Needs git, so it is not expressible as SQL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "a": {
                        "description": "Baseline run: \"latest\", \"last_good\", \"last_bad\", or a run id. Default \"last_good\".",
                        "type": ["string", "integer"]
                    },
                    "b": {
                        "description": "Run to compare against the baseline. Default \"latest\".",
                        "type": ["string", "integer"]
                    }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "runlog_ack",
            "title": "Acknowledge errors",
            "description":
                "Mark this session's unread errors as seen, which clears the warning banner on \
                 runlog tool results. Call it once you have read and understood the errors — not \
                 to silence them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "entry_id": {
                        "type": "integer",
                        "description": "Acknowledge up to and including this entry id. Omit to acknowledge everything currently unread."
                    }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true }
        }
    ])
}

pub fn dispatch<B: Backend>(
    backend: &mut B,
    project: ProjectRef,
    session_id: &str,
    name: &str,
    args: &Value,
) -> Outcome {
    let args = match args {
        Value::Null => Map::new(),
        Value::Object(m) => m.clone(),
        other => {
            return Outcome::err(
                name,
                format!("arguments must be an object, got {other}"),
                "Pass tool arguments as a JSON object, e.g. {\"sql\": \"SELECT 1\"}.",
            )
        }
    };

    match name {
        "runlog_schema" => schema(backend, project),
        "runlog_query" => query(backend, project, &args),
        "runlog_wait" => wait(backend, project, &args),
        "runlog_diff" => diff(backend, project, &args),
        "runlog_ack" => ack(backend, project, session_id, &args),
        _ => Outcome::err(
            "tools/call",
            format!("no tool named {name:?}"),
            &format!(
                "runlog exposes exactly these tools: {}.",
                TOOL_NAMES.join(", ")
            ),
        ),
    }
}

fn schema<B: Backend>(backend: &mut B, project: ProjectRef) -> Outcome {
    match backend.schema(project) {
        // The daemon composes this as prose; unwrap the JSON string so the model reads the
        // schema rather than an escaped blob.
        Ok(Value::String(s)) => Outcome::ok(s),
        Ok(other) => Outcome::ok(
            serde_json::to_string_pretty(&other).unwrap_or_else(|_| other.to_string()),
        ),
        Err(e) => Outcome::err(
            "runlog_schema",
            e,
            "The runlog daemon could not describe this project. It may have no captured runs \
             yet — a run is created by wrapping a command with `runlog <cmd>`.",
        ),
    }
}

fn query<B: Backend>(backend: &mut B, project: ProjectRef, args: &Map<String, Value>) -> Outcome {
    let sql = match args.get("sql").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => {
            return Outcome::err(
                "runlog_query",
                "the required `sql` argument is missing or empty",
                SCHEMA_HINT,
            )
        }
    };
    let params = match args.get("params") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(other) => {
            return Outcome::err(
                "runlog_query",
                format!("`params` must be an array of positional bindings, got {other}"),
                "Bind values positionally: sql \"SELECT * FROM v_errors WHERE run_id = ?1\", \
                 params [42].",
            )
        }
    };
    let max_rows = args.get("max_rows").and_then(|v| v.as_u64()).map(|n| n as usize);

    match backend.query(project, &sql, params, max_rows) {
        Ok(result) => {
            let mut text = serde_json::to_string(&result)
                .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
            if result.truncated {
                text.push_str(
                    "\n\nTruncated: the row or byte cap cut this result short. Narrow the query, \
                     aggregate, or raise max_rows (ceiling 2000).",
                );
            }
            Outcome::ok(text)
        }
        Err(e) => Outcome::err(
            "runlog_query",
            e,
            &format!("Your SQL was:\n{sql}\n\n{SCHEMA_HINT}"),
        ),
    }
}

fn wait<B: Backend>(backend: &mut B, project: ProjectRef, args: &Map<String, Value>) -> Outcome {
    let timeout_ms = match args.get("timeout_ms").and_then(|v| v.as_u64()) {
        Some(n) if n > 0 => n,
        _ => {
            return Outcome::err(
                "runlog_wait",
                "the required `timeout_ms` argument is missing or not a positive integer",
                "Call it as runlog_wait({\"timeout_ms\": 15000}), optionally with \
                 \"level\": \"error\".",
            )
        }
    };
    let level = match args.get("level") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => match Level::parse(s) {
            // `Level::parse` maps anything it does not know to Unknown, so only an explicit
            // "unknown" may resolve to that level.
            Level::Unknown if s != "unknown" => {
                return Outcome::err(
                    "runlog_wait",
                    format!("{s:?} is not a level"),
                    "Levels are: error, warn, info, debug, trace, unknown.",
                )
            }
            other => Some(other),
        },
        Some(other) => {
            return Outcome::err(
                "runlog_wait",
                format!("`level` must be a string, got {other}"),
                "Levels are: error, warn, info, debug, trace, unknown.",
            )
        }
    };

    match backend.wait(project, timeout_ms, level) {
        Ok(entries) if entries.is_empty() => Outcome::ok(format!(
            "No matching entry appeared within {timeout_ms}ms. The process may be idle, or it may \
             not be running under runlog at all — check with \
             runlog_query(\"SELECT id, command, started_at, ended_at FROM v_runs ORDER BY \
             started_at DESC LIMIT 3\")."
        )),
        Ok(entries) => Outcome::ok(
            serde_json::to_string(&entries.iter().map(entry_json).collect::<Vec<_>>())
                .unwrap_or_default(),
        ),
        Err(e) => Outcome::err(
            "runlog_wait",
            e,
            "The runlog daemon did not answer. Fall back to runlog_query for entries already \
             captured.",
        ),
    }
}

fn diff<B: Backend>(backend: &mut B, project: ProjectRef, args: &Map<String, Value>) -> Outcome {
    let a = match run_ref(args.get("a")) {
        Ok(v) => v,
        Err(e) => return Outcome::err("runlog_diff", e, RUN_REF_HINT),
    };
    let b = match run_ref(args.get("b")) {
        Ok(v) => v,
        Err(e) => return Outcome::err("runlog_diff", e, RUN_REF_HINT),
    };
    // Defaults live here rather than in the daemon so this tool's documented contract holds
    // regardless of what other callers of `diff` default to.
    let a = Some(a.unwrap_or(RunRef::LastGood));
    let b = Some(b.unwrap_or(RunRef::Latest));

    match backend.diff(project, a, b) {
        Ok(d) => Outcome::ok(serde_json::to_string(&diff_json(&d)).unwrap_or_default()),
        Err(e) => Outcome::err(
            "runlog_diff",
            e,
            "There may be no run to compare against yet. \
             runlog_query(\"SELECT id, started_at, exit_code, clean FROM v_runs ORDER BY \
             started_at DESC LIMIT 10\") shows what exists.",
        ),
    }
}

const RUN_REF_HINT: &str =
    "A run reference is \"latest\", \"last_good\", \"last_bad\", or a numeric run id.";

fn run_ref(value: Option<&Value>) -> Result<Option<RunRef>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .map(|id| Some(RunRef::Id(id)))
            .ok_or_else(|| format!("{n} is not a run id")),
        Some(Value::String(s)) => match s.as_str() {
            "latest" => Ok(Some(RunRef::Latest)),
            "last_good" => Ok(Some(RunRef::LastGood)),
            "last_bad" => Ok(Some(RunRef::LastBad)),
            other => other
                .parse::<i64>()
                .map(|id| Some(RunRef::Id(id)))
                .map_err(|_| format!("{other:?} is not a run reference")),
        },
        Some(other) => Err(format!("{other} is not a run reference")),
    }
}

fn entry_json(e: &EntryView) -> Value {
    json!({
        "entry_id": e.entry.id,
        "run_id": e.entry.run_id,
        "captured_at": e.entry.captured_at,
        "level": e.entry.level.as_str(),
        "is_error": e.entry.is_error,
        "error_reason": e.entry.error_reason,
        "boundaries": e.entry.boundaries.as_str(),
        "template": e.template,
        "new_signature": e.is_new_signature,
        "text": e.text,
    })
}

/// Trim a `RunDiff` down to the identity of each side and the signatures that moved; the
/// full `Run` structs carry far more than the model needs.
fn diff_json(d: &RunDiff) -> Value {
    let side = |r: &Option<runlog_core::model::Run>| match r {
        None => Value::Null,
        Some(r) => json!({
            "run_id": r.id,
            "command": r.command_line(),
            "started_at": r.started_at,
            "exit_code": r.exit_code,
            "signal": r.signal,
            "git_sha": r.git_sha,
            "git_branch": r.git_branch,
            "clean": r.is_clean(),
            "n_error": r.counters.n_error,
            "chunks_evicted": r.chunks_evicted,
        }),
    };
    let sigs = |v: &Vec<runlog_core::proto::DiffSignature>| {
        v.iter()
            .map(|s| {
                json!({
                    "template": s.template,
                    "level": s.level.as_str(),
                    "count_a": s.count_a,
                    "count_b": s.count_b,
                    "sample": s.sample,
                })
            })
            .collect::<Vec<_>>()
    };
    json!({
        "a": side(&d.a),
        "b": side(&d.b),
        "new_signatures": sigs(&d.new_signatures),
        "resolved_signatures": sigs(&d.resolved_signatures),
        "changed_counts": sigs(&d.changed_counts),
        "git": {
            "sha_a": d.git.sha_a,
            "sha_b": d.git.sha_b,
            "branch_a": d.git.branch_a,
            "branch_b": d.git.branch_b,
            "dirty_changed": d.git.dirty_changed,
            "commits": d.git.commits.iter().map(|c| json!({
                "sha": c.sha, "subject": c.subject, "author": c.author, "at": c.at
            })).collect::<Vec<_>>(),
            "files_changed": d.git.files_changed,
        },
        "lockfiles_changed": d.lockfiles_changed.iter().map(|l| json!({
            "file": l.file, "changed": l.hash_a != l.hash_b
        })).collect::<Vec<_>>(),
        "env_added": d.env_added,
        "env_removed": d.env_removed,
        "command_changed": d.command_changed,
    })
}

fn ack<B: Backend>(
    backend: &mut B,
    project: ProjectRef,
    session_id: &str,
    args: &Map<String, Value>,
) -> Outcome {
    let entry_id = match args.get("entry_id") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => match n.as_i64() {
            Some(id) => Some(id),
            None => {
                return Outcome::err(
                    "runlog_ack",
                    format!("{n} is not an entry id"),
                    "Omit entry_id to acknowledge everything currently unread.",
                )
            }
        },
        Some(other) => {
            return Outcome::err(
                "runlog_ack",
                format!("`entry_id` must be an integer, got {other}"),
                "Omit entry_id to acknowledge everything currently unread.",
            )
        }
    };

    match backend.ack(project, session_id, entry_id) {
        Ok(state) if state.is_empty() => {
            Outcome::ok("Acknowledged. Nothing is unread for this session.")
        }
        Ok(state) => Outcome::ok(format!(
            "Acknowledged. {} error{} still unread ({} signature{}).",
            state.new_errors,
            if state.new_errors == 1 { "" } else { "s" },
            state.new_signatures,
            if state.new_signatures == 1 { "" } else { "s" },
        )),
        Err(e) => Outcome::err(
            "runlog_ack",
            e,
            "The watermark was not advanced, so the banner will keep reporting these errors.",
        ),
    }
}
