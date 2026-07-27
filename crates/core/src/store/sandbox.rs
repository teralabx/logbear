//! The read-only SQL endpoint an agent's queries execute in.
//!
//! Untrusted model output executes here, so enforcement is layered and each layer is
//! independently sufficient:
//!
//! 1. Read-only handle — opened `SQLITE_OPEN_READ_ONLY` with `PRAGMA query_only`.
//! 2. Authorizer allowlist — `SELECT`/`READ`/`FUNCTION` on named functions only; `ATTACH`,
//!    `PRAGMA`, extension loading and every write verb are denied.
//! 3. Interrupt timer — statements are cancelled after a fixed budget, so no query can pin
//!    the daemon.
//! 4. Result caps — row cap and byte cap, both reporting truncation rather than silently
//!    cutting the answer.
//! 5. One project per connection — with `ATTACH` denied, a query cannot reach another
//!    project's data or any other file on disk.
//!
//! Rejections name the construct that failed and echo the SQL, so an agent can repair its
//! own query.

use crate::error::{Error, Result};
use crate::proto::QueryResult;
use crate::store::{register_functions, ALLOWED_FUNCTIONS};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::time::{Duration, Instant};

pub const DEFAULT_MAX_ROWS: usize = 200;
pub const CEILING_MAX_ROWS: usize = 2000;
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
pub const QUERY_BUDGET_MS: u64 = 5_000;

/// Runs one read-only statement against a project database.
pub fn query(
    db_path: &Path,
    sql: &str,
    params: &[serde_json::Value],
    max_rows: Option<usize>,
) -> Result<QueryResult> {
    let max_rows = max_rows.unwrap_or(DEFAULT_MAX_ROWS).clamp(1, CEILING_MAX_ROWS);
    reject_obvious(sql)?;

    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.execute_batch("PRAGMA query_only = ON; PRAGMA busy_timeout = 2000;")?;
    // Registration must precede the authorizer: it is itself a privileged act the
    // authorizer would otherwise have to permit.
    register_functions(&conn, db_path)?;
    install_authorizer(&conn)?;

    // Cancelled from another thread rather than trusting the query to finish: a recursive
    // CTE or a cartesian join can otherwise run unbounded.
    let handle = conn.get_interrupt_handle();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watch_done = done.clone();
    let watchdog = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(QUERY_BUDGET_MS);
        while Instant::now() < deadline {
            if watch_done.load(std::sync::atomic::Ordering::Relaxed) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if !watch_done.load(std::sync::atomic::Ordering::Relaxed) {
            handle.interrupt();
            return true;
        }
        false
    });

    let started = Instant::now();
    let outcome = run_statement(&conn, sql, params, max_rows);
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let interrupted = watchdog.join().unwrap_or(false);

    match outcome {
        Ok(mut r) => {
            r.elapsed_ms = started.elapsed().as_millis() as u64;
            Ok(r)
        }
        Err(_) if interrupted => Err(Error::QueryTimeout(QUERY_BUDGET_MS)),
        Err(e) => Err(explain(e, sql)),
    }
}

fn run_statement(
    conn: &Connection,
    sql: &str,
    params: &[serde_json::Value],
    max_rows: usize,
) -> Result<QueryResult> {
    let mut stmt = conn.prepare(sql)?;
    let bound: Vec<rusqlite::types::Value> = params.iter().map(json_to_sql).collect();
    let refs: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v as &dyn rusqlite::ToSql).collect();

    let columns: Vec<String> = stmt.column_names().into_iter().map(str::to_string).collect();
    let n_cols = columns.len();

    let mut rows = stmt.query(refs.as_slice())?;
    let mut out: Vec<Vec<serde_json::Value>> = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;

    while let Some(row) = rows.next()? {
        if out.len() >= max_rows {
            truncated = true;
            break;
        }
        let mut r = Vec::with_capacity(n_cols);
        for i in 0..n_cols {
            let v = sql_to_json(row.get_ref(i)?);
            bytes += approx_size(&v);
            r.push(v);
        }
        // Cap the response so a single query cannot return the whole log.
        if bytes > MAX_RESPONSE_BYTES {
            truncated = true;
            break;
        }
        out.push(r);
    }

    Ok(QueryResult {
        row_count: out.len(),
        columns,
        rows: out,
        truncated,
        elapsed_ms: 0,
    })
}

fn install_authorizer(conn: &Connection) -> Result<()> {
    conn.authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
        AuthAction::Select => Authorization::Allow,
        AuthAction::Read { .. } => Authorization::Allow,
        AuthAction::Function { function_name } => {
            if ALLOWED_FUNCTIONS
                .iter()
                .any(|f| f.eq_ignore_ascii_case(function_name))
            {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        // Recursive CTEs are needed for analytical queries; the interrupt timer bounds a
        // runaway one.
        AuthAction::Recursive => Authorization::Allow,
        _ => Authorization::Deny,
    }))?;
    Ok(())
}

/// A cheap textual pass before SQLite sees the string.
///
/// The authorizer is the actual defence; this only turns common mistakes into a message an
/// agent can act on rather than an opaque `not authorized`.
fn reject_obvious(sql: &str) -> Result<()> {
    let trimmed = sql.trim_start();
    let lower = trimmed.to_ascii_lowercase();

    if !(lower.starts_with("select") || lower.starts_with("with") || lower.starts_with("explain")) {
        return Err(Error::QueryRejected(format!(
            "only SELECT (or WITH … SELECT) is allowed here; this statement starts with {:?}. \
             runlog_query is read-only — use runlog_ack to advance your cursor.",
            trimmed.split_whitespace().next().unwrap_or("")
        )));
    }
    for word in ["attach", "detach", "pragma", "vacuum", "load_extension"] {
        if lower.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').any(|t| t == word) {
            return Err(Error::QueryRejected(format!(
                "`{word}` is not permitted. Queries are bound to this project's database only."
            )));
        }
    }
    // One statement per call: a statement after a semicolon is how a benign-looking SELECT
    // smuggles something else in.
    if sql.trim_end().trim_end_matches(';').contains(';') {
        return Err(Error::QueryRejected(
            "one statement per call; remove the extra `;`".into(),
        ));
    }
    Ok(())
}

fn explain(e: Error, sql: &str) -> Error {
    let msg = e.to_string();
    let hint = if msg.contains("not authorized") {
        "This query touched something the sandbox denies (a write, ATTACH, PRAGMA, or an \
         unlisted function). Call runlog_schema() for the allowed surface."
    } else if msg.contains("no such table") || msg.contains("no such column") {
        "Call runlog_schema() for the current tables, views and columns."
    } else {
        "Fix the statement and retry."
    };
    Error::QueryRejected(format!("{msg}\n{hint}\n\nSQL was:\n{sql}"))
}

fn json_to_sql(v: &serde_json::Value) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Integer(*b as i64),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::Integer)
            .or_else(|| n.as_f64().map(Value::Real))
            .unwrap_or(Value::Null),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        other => Value::Text(other.to_string()),
    }
}

fn sql_to_json(v: rusqlite::types::ValueRef<'_>) -> serde_json::Value {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(i) => serde_json::Value::from(i),
        ValueRef::Real(f) => serde_json::Value::from(f),
        ValueRef::Text(t) => serde_json::Value::from(String::from_utf8_lossy(t).into_owned()),
        // Blobs are chunk payloads; report their size and let the caller use
        // runlog_render.
        ValueRef::Blob(b) => serde_json::Value::from(format!("<{} bytes>", b.len())),
    }
}

fn approx_size(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::String(s) => s.len() + 2,
        serde_json::Value::Null => 4,
        _ => 8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RunStamp;
    use crate::redact::Redactor;
    use crate::store::{index::IndexDb, Db, Writer};
    use std::sync::Arc;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let index = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let db_path = tmp.path().join("p.db");
        let p = index.register(&root, "proj", &db_path).unwrap();
        let db = Db::open(&db_path, p.id).unwrap();
        let mut w = Writer::start(
            &db,
            &index,
            &RunStamp {
                command: vec!["mix".into(), "phx.server".into()],
                ..Default::default()
            },
            Arc::new(Redactor::disabled()),
        )
        .unwrap();
        w.push(
            b"GET /users\nError: connection refused\n    at a (/app/x.js:1:1)\nGET /posts\n",
            crate::now_micros(),
        )
        .unwrap();
        w.finish(&index, Some(1), None).unwrap();
        (tmp, db_path)
    }

    fn q(path: &Path, sql: &str) -> Result<QueryResult> {
        query(path, sql, &[], None)
    }

    #[test]
    fn a_normal_select_works_and_renders_entry_text() {
        let (_t, path) = fixture();
        let r = q(&path, "SELECT text FROM v_errors ORDER BY captured_at DESC").unwrap();
        assert_eq!(r.row_count, 1);
        let text = r.rows[0][0].as_str().unwrap();
        assert!(text.contains("connection refused"), "{text}");
        assert!(text.contains("at a"), "whole group expected, got: {text}");
    }

    #[test]
    fn the_views_an_agent_is_told_about_all_exist() {
        let (_t, path) = fixture();
        for sql in [
            "SELECT count(*) FROM v_entries",
            "SELECT count(*) FROM v_errors",
            "SELECT count(*) FROM v_runs WHERE clean = 0",
            "SELECT runlog_render(1, 0, 1)",
            "SELECT runlog_raw(1, 0, 0)",
        ] {
            assert!(q(&path, sql).is_ok(), "documented surface failed: {sql}");
        }
    }

    #[test]
    fn every_write_verb_is_refused() {
        let (_t, path) = fixture();
        for sql in [
            "DELETE FROM entries",
            "UPDATE runs SET exit_code = 0",
            "INSERT INTO entries (run_id) VALUES (1)",
            "DROP TABLE chunks",
            "CREATE TABLE evil (x)",
            "ALTER TABLE runs ADD COLUMN evil TEXT",
        ] {
            assert!(q(&path, sql).is_err(), "write slipped through: {sql}");
        }
    }

    #[test]
    fn attach_pragma_and_extensions_are_refused() {
        let (_t, path) = fixture();
        for sql in [
            "ATTACH DATABASE '/etc/passwd' AS evil",
            "SELECT * FROM pragma_database_list",
            "PRAGMA table_info(runs)",
            "SELECT load_extension('/tmp/evil.so')",
        ] {
            assert!(q(&path, sql).is_err(), "escape slipped through: {sql}");
        }
    }

    #[test]
    fn a_second_statement_cannot_be_smuggled_in() {
        let (_t, path) = fixture();
        assert!(q(&path, "SELECT 1; DELETE FROM entries").is_err());
    }

    #[test]
    fn readfile_style_functions_are_not_available() {
        let (_t, path) = fixture();
        assert!(q(&path, "SELECT readfile('/etc/passwd')").is_err());
    }

    #[test]
    fn row_caps_are_enforced_and_reported() {
        let (_t, path) = fixture();
        let r = query(
            &path,
            "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i < 100000)
             SELECT i FROM c",
            &[],
            Some(10),
        )
        .unwrap();
        assert_eq!(r.row_count, 10);
        assert!(r.truncated, "truncation must be reported, never silent");
    }

    #[test]
    fn an_unbounded_query_is_cancelled_rather_than_pinning_the_daemon() {
        let (_t, path) = fixture();
        // No LIMIT and no row the cap can stop at: this must hit the timer.
        let r = query(
            &path,
            "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c)
             SELECT count(*) FROM c",
            &[],
            Some(2000),
        );
        assert!(matches!(r, Err(Error::QueryTimeout(_))), "{r:?}");
    }

    #[test]
    fn rejections_tell_the_agent_how_to_repair_the_query() {
        let (_t, path) = fixture();
        let err = q(&path, "SELECT * FROM no_such_table").unwrap_err().to_string();
        assert!(err.contains("runlog_schema"), "unhelpful rejection: {err}");
        assert!(err.contains("SQL was:"), "the failing SQL was not echoed: {err}");
    }

    #[test]
    fn parameters_bind_rather_than_being_interpolated() {
        let (_t, path) = fixture();
        let r = query(
            &path,
            "SELECT count(*) FROM v_entries WHERE run_id = ?1",
            &[serde_json::json!(1)],
            None,
        )
        .unwrap();
        assert_eq!(r.rows[0][0].as_i64().unwrap(), 3);
    }
}
