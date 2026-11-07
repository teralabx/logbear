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
//! 6. Memory limits — a cap on any one value, and a heap budget per query. Queries run inside
//!    the daemon, so one that grows without bound takes capture down with it.
//!
//! The budget is not `sqlite3_hard_heap_limit64`, because that limit is process-wide: reaching
//! it fails every allocation in the process, including the capture writer's, which has no way
//! to retry and would lose lines. SQLite has no per-connection allocator accounting, so the
//! budget is a progress handler watching growth in the process's SQLite heap. It stops only
//! the query it runs on, and stopping it frees what it held. It sees SQLite's heap and nothing
//! else, so the functions that allocate on the Rust side cap their own results.
//!
//! Rejections name the construct that failed and echo the SQL, so an agent can repair its
//! own query.

use crate::error::{Error, Result};
use crate::proto::QueryResult;
use crate::store::{register_functions, ALLOWED_FUNCTIONS};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::{ffi, Connection, OpenFlags};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_MAX_ROWS: usize = 200;
pub const CEILING_MAX_ROWS: usize = 2000;
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
pub const QUERY_BUDGET_MS: u64 = 5_000;
/// Largest string or blob a query may build. Sixteen times over any result it could return.
pub const MAX_VALUE_BYTES: i32 = 4 * 1024 * 1024;
pub const MAX_SQL_BYTES: i32 = 1024 * 1024;
/// Columns in a result or subquery. Together with [`MAX_VALUE_BYTES`] this bounds what one row
/// can hold, which is what the heap budget cannot do: SQLite only runs the progress handler at
/// jumps, so a row of computed columns is built whole before it can fire. The worst case is
/// about 1 GiB, held briefly, and it is only reachable on purpose.
pub const MAX_COLUMNS: i32 = 256;
/// SQLite heap one query may add, checked between rows.
pub const QUERY_HEAP_BUDGET: i64 = 128 * 1024 * 1024;
/// Page cache per connection. Sorters and temporary tables spill to disk past this, which is
/// what keeps ordinary `ORDER BY` and `DISTINCT` queries out of the budget's way.
const CACHE_KIB: i64 = 2048;
/// VM steps between heap checks. Reading the heap counter takes a lock, so not every step.
const HEAP_CHECK_OPS: std::ffi::c_int = 100;
/// Heap use past which a finished query's memory is pushed back to the OS.
///
/// Worth doing only after a heavy query: it also evicts the writer's cached pages, and the
/// allocator otherwise keeps freed memory, which is how a query that had already ended could
/// leave the daemon gigabytes larger for minutes.
const RELEASE_ABOVE: i64 = 16 * 1024 * 1024;

/// Queries running now. They share one process-wide counter, so each is allowed the sum of
/// their budgets; otherwise a hog would get an innocent query running beside it cancelled.
static ACTIVE_QUERIES: AtomicI64 = AtomicI64::new(0);

/// Watches one query's growth against [`QUERY_HEAP_BUDGET`].
struct HeapWatch {
    start: i64,
    peak: AtomicI64,
    exceeded: AtomicBool,
}

impl HeapWatch {
    fn start() -> HeapWatch {
        ACTIVE_QUERIES.fetch_add(1, Ordering::SeqCst);
        HeapWatch {
            start: heap_used(),
            peak: AtomicI64::new(0),
            exceeded: AtomicBool::new(false),
        }
    }

    /// Whether the query should stop.
    fn over_budget(&self) -> bool {
        let grown = heap_used() - self.start;
        self.peak.fetch_max(grown, Ordering::Relaxed);
        let allowed = QUERY_HEAP_BUDGET * ACTIVE_QUERIES.load(Ordering::SeqCst).max(1);
        if grown > allowed {
            self.exceeded.store(true, Ordering::SeqCst);
        }
        grown > allowed
    }
}

impl Drop for HeapWatch {
    fn drop(&mut self) {
        ACTIVE_QUERIES.fetch_sub(1, Ordering::SeqCst);
    }
}

fn heap_used() -> i64 {
    unsafe { ffi::sqlite3_memory_used() }
}

/// Hands freed memory back to the OS: SQLite's caches first, then the allocator's free lists.
fn release_memory() {
    unsafe {
        ffi::sqlite3_release_memory(i32::MAX);
    }
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // A null zone means all of them, and a zero goal means as much as possible.
        unsafe {
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }
}

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
    conn.execute_batch(&format!(
        "PRAGMA query_only = ON; PRAGMA busy_timeout = 2000;
         PRAGMA cache_size = -{CACHE_KIB}; PRAGMA temp_store = FILE; PRAGMA mmap_size = 0;"
    ))?;
    conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_VALUE_BYTES)?;
    conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, MAX_SQL_BYTES)?;
    conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, MAX_COLUMNS)?;
    // Registration must precede the authorizer: it is itself a privileged act the
    // authorizer would otherwise have to permit.
    register_functions(&conn, db_path)?;
    install_authorizer(&conn)?;

    let heap = Arc::new(HeapWatch::start());
    let watch = heap.clone();
    conn.progress_handler(HEAP_CHECK_OPS, Some(move || watch.over_budget()))?;

    // Cancelled from another thread rather than trusting the query to finish: a recursive
    // CTE or a cartesian join can otherwise run unbounded.
    //
    // The timer is *signalled* when the statement finishes, not polled. A poll loop makes
    // every query — including the trivial ones the UI issues by the dozen — wait for a
    // sleeping thread to be rescheduled, which costs far more than the query itself.
    let handle = conn.get_interrupt_handle();
    let done = Arc::new((Mutex::new(false), Condvar::new()));
    let watch = done.clone();
    let watchdog = std::thread::spawn(move || {
        let (lock, signal) = &*watch;
        let Ok(guard) = lock.lock() else { return false };
        let Ok((finished, timeout)) = signal.wait_timeout_while(
            guard,
            Duration::from_millis(QUERY_BUDGET_MS),
            |finished| !*finished,
        ) else {
            return false;
        };
        if timeout.timed_out() && !*finished {
            handle.interrupt();
            return true;
        }
        false
    });

    let started = Instant::now();
    let outcome = run_statement(&conn, sql, params, max_rows);
    let elapsed = started.elapsed();
    {
        let (lock, signal) = &*done;
        if let Ok(mut finished) = lock.lock() {
            *finished = true;
        }
        signal.notify_all();
    }
    let interrupted = watchdog.join().unwrap_or(false);

    // Closing frees everything the statement held, which has to precede the release.
    drop(conn);
    let exceeded = heap.exceeded.load(Ordering::SeqCst);
    let too_big = matches!(&outcome, Err(e) if e.to_string().contains("too big"));
    if exceeded || too_big || heap.peak.load(Ordering::Relaxed) > RELEASE_ABOVE {
        release_memory();
    }

    match outcome {
        Ok(mut r) => {
            r.elapsed_ms = elapsed.as_millis() as u64;
            Ok(r)
        }
        Err(_) if exceeded => Err(Error::QueryRejected(format!(
            "this query needed more than {} MiB of memory and was stopped.\n{NARROW_HINT}\n\n\
             SQL was:\n{sql}",
            QUERY_HEAP_BUDGET / (1024 * 1024),
        ))),
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
            // Cap the response so a single query cannot return the whole log. Checked per
            // cell, so that one wide row is not copied out in full just to be discarded.
            if bytes > MAX_RESPONSE_BYTES {
                truncated = true;
                break;
            }
            r.push(v);
        }
        if truncated {
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
             logbear_query is read-only — use logbear_ack to advance your cursor.",
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

const NARROW_HINT: &str = "Narrow the query with WHERE or LIMIT, aggregate over fewer rows, \
     or use substr() so it builds smaller values.";

fn explain(e: Error, sql: &str) -> Error {
    let msg = e.to_string();
    let hint = if msg.contains("not authorized") {
        "This query touched something the sandbox denies (a write, ATTACH, PRAGMA, or an \
         unlisted function). Call logbear_schema() for the allowed surface."
    } else if msg.contains("no such table") || msg.contains("no such column") {
        "Call logbear_schema() for the current tables, views and columns."
    } else if msg.contains("too big") {
        NARROW_HINT
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
        // logbear_render.
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
    use crate::store::MAX_RENDERED_BYTES;
    use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

    /// Heap accounting is process-wide, so a test that reads the high-water mark, or that
    /// provokes the budget, runs alone. Every other test holds this shared: a neighbouring hog
    /// can legitimately get a concurrent query stopped.
    static HEAP: RwLock<()> = RwLock::new(());

    enum Turn {
        #[allow(dead_code)]
        Shared(RwLockReadGuard<'static, ()>),
        #[allow(dead_code)]
        Alone(RwLockWriteGuard<'static, ()>),
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        _turn: Turn,
    }

    fn fixture() -> (Fixture, std::path::PathBuf) {
        fixture_with(Turn::Shared(HEAP.read().unwrap_or_else(|e| e.into_inner())))
    }

    fn fixture_alone() -> (Fixture, std::path::PathBuf) {
        fixture_with(alone())
    }

    fn alone() -> Turn {
        Turn::Alone(HEAP.write().unwrap_or_else(|e| e.into_inner()))
    }

    fn fixture_with(turn: Turn) -> (Fixture, std::path::PathBuf) {
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
        (Fixture { _tmp: tmp, _turn: turn }, db_path)
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
            "SELECT logbear_render(1, 0, 1)",
            "SELECT logbear_raw(1, 0, 0)",
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
    fn a_trivial_query_does_not_pay_for_the_timeout_machinery() {
        let (_t, path) = fixture();
        // The fastest of many, not the mean: a polled watchdog puts a scheduler round trip
        // under *every* query, so it raises the floor. A mean or a single sample measures
        // whatever else the machine was doing, and fails when the suite runs in parallel.
        let best = (0..20)
            .map(|_| {
                let started = Instant::now();
                q(&path, "SELECT 1").unwrap();
                started.elapsed()
            })
            .min()
            .unwrap();
        assert!(
            best < Duration::from_millis(5),
            "the cheapest of 20 trivial queries still cost {best:?}; \
             the cancellation timer is being waited on rather than signalled"
        );
    }

    #[test]
    fn elapsed_ms_reports_the_statement_not_the_bookkeeping() {
        let (_t, path) = fixture();
        let best = (0..20)
            .map(|_| q(&path, "SELECT 1").unwrap().elapsed_ms)
            .min()
            .unwrap();
        assert!(
            best < 5,
            "the cheapest of 20 trivial queries reported {best}ms; \
             elapsed_ms is measuring the timeout bookkeeping, not the statement"
        );
    }

    #[test]
    fn rejections_tell_the_agent_how_to_repair_the_query() {
        let (_t, path) = fixture();
        let err = q(&path, "SELECT * FROM no_such_table").unwrap_err().to_string();
        assert!(err.contains("logbear_schema"), "unhelpful rejection: {err}");
        assert!(err.contains("SQL was:"), "the failing SQL was not echoed: {err}");
    }

    /// Runs a query and reports the most heap SQLite held while it ran.
    fn with_peak(path: &Path, sql: &str) -> (Result<QueryResult>, i64) {
        unsafe { rusqlite::ffi::sqlite3_memory_highwater(1) };
        let r = q(path, sql);
        (r, unsafe { rusqlite::ffi::sqlite3_memory_highwater(0) })
    }

    const MIB: i64 = 1024 * 1024;

    fn assert_fails_cleanly(path: &Path, sql: &str) {
        let (r, peak) = with_peak(path, sql);
        assert!(peak < 256 * MIB, "reached {} MiB: {sql}", peak / MIB);
        match r {
            Err(Error::QueryRejected(msg)) => {
                assert!(msg.contains("Narrow"), "no way forward offered: {msg}")
            }
            other => panic!("expected a clean rejection of {sql}, got {other:?}"),
        }
    }

    #[test]
    fn a_runaway_group_concat_fails_instead_of_growing() {
        let (_t, path) = fixture_alone();
        assert_fails_cleanly(
            &path,
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 2000000)
             SELECT length(group_concat(printf('%.*c', 4000, 'x'))) FROM n",
        );
    }

    #[test]
    fn one_enormous_value_fails_instead_of_growing() {
        let (_t, path) = fixture_alone();
        assert_fails_cleanly(&path, "SELECT length(printf('%.*c', 2000000000, 'x'))");
    }

    #[test]
    fn repeated_doubling_fails_instead_of_growing() {
        let (_t, path) = fixture_alone();
        assert_fails_cleanly(
            &path,
            "WITH RECURSIVE c(s, i) AS (SELECT 'xxxxxxxx', 0 UNION ALL
                 SELECT replace(s, 'x', 'xx'), i+1 FROM c WHERE i < 40)
             SELECT max(length(s)) FROM c",
        );
    }

    #[test]
    fn many_large_values_at_once_fail_instead_of_growing() {
        let (_t, path) = fixture_alone();
        // Each value is under the per-value limit; it is the sum that has to be stopped. The
        // outer query reads every column, or SQLite drops the unused ones unevaluated, and
        // the LIMIT stops it flattening the subquery away.
        let columns: Vec<String> = (0..40)
            .map(|i| format!("printf('%.*c', 3900000, 'x') AS c{i}"))
            .collect();
        let lengths: Vec<String> = (0..40).map(|i| format!("length(c{i})")).collect();
        let sql = format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 3000)
             SELECT sum({}) FROM (SELECT i, {} FROM n LIMIT 3000)",
            lengths.join(" + "),
            columns.join(", "),
        );
        let (r, peak) = with_peak(&path, &sql);
        assert!(peak < 512 * MIB, "reached {} MiB", peak / MIB);
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("more than 128 MiB"), "{msg}");
        assert!(msg.contains("Narrow"), "{msg}");
    }

    #[test]
    fn the_column_cap_bounds_what_one_row_can_hold() {
        let (_t, path) = fixture_alone();
        let columns = vec!["printf('%.*c', 3900000, 'x')"; MAX_COLUMNS as usize + 1].join(", ");
        let err = q(&path, &format!("SELECT {columns}")).unwrap_err().to_string();
        assert!(err.contains("too many columns"), "{err}");
    }

    #[test]
    fn ordinary_analytical_queries_stay_inside_the_limits() {
        let (_t, path) = fixture_alone();
        for sql in [
            "SELECT level, count(*), max(length(text)) FROM v_entries GROUP BY level",
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 200000)
             SELECT i % 7, count(*), sum(i) FROM n GROUP BY 1 ORDER BY 3 DESC",
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 200000)
             SELECT count(DISTINCT printf('%050d', i)) FROM n",
        ] {
            assert!(q(&path, sql).is_ok(), "{sql}");
        }
    }

    #[test]
    fn a_wide_row_stops_at_the_byte_cap_rather_than_after_copying_all_of_itself() {
        let (_t, path) = fixture_alone();
        let columns = vec!["printf('%.*c', 1000000, 'x')"; 40].join(", ");
        let r = q(&path, &format!("SELECT {columns}")).unwrap();
        assert!(r.truncated, "truncation must be reported");
        assert_eq!(r.row_count, 0);
    }

    #[test]
    fn memory_is_given_back_after_a_hostile_query() {
        let (_t, path) = fixture_alone();
        let before = unsafe { rusqlite::ffi::sqlite3_memory_used() };
        let _ = q(&path, "SELECT length(printf('%.*c', 2000000000, 'x'))");
        let after = unsafe { rusqlite::ffi::sqlite3_memory_used() };
        assert!(after - before < 4 * MIB, "{} MiB retained", (after - before) / MIB);
    }

    /// Why this is not `sqlite3_hard_heap_limit64`: that limit is process-wide, so the query
    /// that reaches it would make the capture writer's allocations fail too.
    #[test]
    fn ingest_keeps_writing_while_a_query_hits_its_limit() {
        let _turn = alone();
        let tmp = tempfile::tempdir().unwrap();
        let index = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let db_path = tmp.path().join("p.db");
        let p = index.register(&root, "proj", &db_path).unwrap();
        let db = Db::open(&db_path, p.id).unwrap();
        let stamp = RunStamp { command: vec!["x".into()], ..Default::default() };
        let mut w = Writer::start(&db, &index, &stamp, Arc::new(Redactor::disabled())).unwrap();

        let hog = {
            let path = db_path.clone();
            std::thread::spawn(move || {
                let columns = vec!["printf('%.*c', 3900000, 'x')"; 200].join(", ");
                for _ in 0..5 {
                    let _ = q(&path, &format!("SELECT count(*) FROM (SELECT {columns})"));
                }
            })
        };
        while !hog.is_finished() {
            w.push(b"Error: still capturing\n", crate::now_micros()).unwrap();
        }
        hog.join().unwrap();
        w.finish(&index, Some(0), None).unwrap();
    }

    #[test]
    fn rendering_a_whole_run_is_capped_rather_than_read_into_memory() {
        let _turn = alone();
        let tmp = tempfile::tempdir().unwrap();
        let index = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let db_path = tmp.path().join("p.db");
        let p = index.register(&root, "proj", &db_path).unwrap();
        let db = Db::open(&db_path, p.id).unwrap();
        let stamp = RunStamp { command: vec!["x".into()], ..Default::default() };
        let mut w = Writer::start(&db, &index, &stamp, Arc::new(Redactor::disabled())).unwrap();
        let line = format!("{}\n", "y".repeat(60_000));
        for _ in 0..200 {
            w.push(line.as_bytes(), crate::now_micros()).unwrap();
        }
        w.finish(&index, Some(0), None).unwrap();

        for sql in [
            "SELECT logbear_render(1, 0, 1000000000)",
            "SELECT logbear_raw(1, 0, 1000000000)",
        ] {
            let inner = sql.strip_prefix("SELECT ").unwrap();
            let r = q(&db_path, &format!("SELECT length({inner})")).unwrap();
            let n = r.rows[0][0].as_i64().unwrap();
            // Stops at the first line past the cap, not inside it.
            assert!(n <= MAX_RENDERED_BYTES as i64 + 128 * 1024, "{sql} returned {n} bytes");
            assert!(n > 1024 * 1024, "{sql} returned only {n} bytes");
        }
    }

    #[test]
    fn a_short_range_is_rendered_whole() {
        let (_t, path) = fixture();
        let r = q(&path, "SELECT logbear_render(1, 1, 2)").unwrap();
        assert_eq!(
            r.rows[0][0].as_str().unwrap(),
            "Error: connection refused\n    at a (/app/x.js:1:1)"
        );
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
