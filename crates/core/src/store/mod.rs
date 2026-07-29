//! The SQLite store. One database per project, plus a small central index.
//!
//! A database per project keeps retention per project and makes a single-project query an
//! ordinary one, with no attaching or fan-out. It also means an agent's SQL cannot reach
//! another project's data.

pub mod index;
pub mod read;
pub mod retention;
pub mod sandbox;
pub mod schema;
pub mod write;

use crate::error::Result;
use crate::model::*;
use crate::parse::{vt, RawLine};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use write::Writer;

/// A handle on one project's database.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
    project_id: ProjectId,
    derived_stale: bool,
}

impl Db {
    /// Opens a project database, creating it and applying the schema if needed.
    pub fn open(path: &Path, project_id: ProjectId) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::Error::io(parent, e))?;
        }
        let conn = Connection::open(path)?;
        apply_pragmas(&conn)?;

        // The stored version has to be read before the schema is applied, and the derived
        // tables dropped before they are recreated: `CREATE TABLE IF NOT EXISTS` leaves an
        // existing table at its old shape, so without this a bump silently does nothing.
        let existing: Option<i64> = conn
            .query_row("SELECT value FROM meta WHERE key = 'derived_version'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|v| v.parse().ok());
        // The version stamp records intent; the table records reality. They diverge when a
        // build writes the stamp without changing the shape, and then a correct build sees
        // a current version over a stale table and rebuilds nothing. Only the shape is
        // authoritative, so it is checked too.
        let stale =
            existing.is_some_and(|v| v != schema::DERIVED_VERSION) || !entries_shape_current(&conn);
        if stale {
            conn.execute_batch(schema::DROP_DERIVED)?;
        }

        conn.execute_batch(schema::PROJECT_SCHEMA)?;
        register_functions(&conn, path)?;
        conn.execute_batch(schema::PROJECT_VIEWS)?;
        set_meta(&conn, "schema_version", &schema::SCHEMA_VERSION.to_string())?;
        set_meta(&conn, "derived_version", &schema::DERIVED_VERSION.to_string())?;

        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
            path: path.to_path_buf(),
            project_id,
            derived_stale: stale,
        })
    }

    /// Whether the derived layer was dropped on open and needs rebuilding from `chunks`.
    ///
    /// The caller does the rebuild, because it can take seconds on a large project and
    /// `open` is on the request path.
    pub fn derived_stale(&self) -> bool {
        self.derived_stale
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    /// Runs a closure with the connection.
    ///
    /// Prefer the named methods in [`read`] and [`write`](mod@write), which keep query
    /// shapes in one reviewable place.
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().map_err(|_| crate::Error::other("db mutex poisoned"))?;
        f(&conn)
    }

    pub fn with_mut<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().map_err(|_| crate::Error::other("db mutex poisoned"))?;
        f(&mut conn)
    }

    /// Size on disk, including the WAL and shm files, as counted by retention.
    pub fn size_bytes(&self) -> i64 {
        let mut total = 0i64;
        for suffix in ["", "-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{suffix}", self.path.display()));
            if let Ok(m) = std::fs::metadata(&p) {
                total += m.len() as i64;
            }
        }
        total
    }
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    // WAL so readers never block the capture writer. NORMAL synchronous trades the last
    // few milliseconds of a log on power loss for not fsyncing every batch.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -8000;",
    )?;
    // Incremental rather than full: a bulk DELETE plus VACUUM would stall the daemon, and
    // until vacuumed would not return any disk space.
    let mode: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)).unwrap_or(0);
    if mode != 2 {
        conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
    }
    Ok(())
}

/// Columns `entries` must carry for the current code to read it.
///
/// Kept beside the open path rather than derived from the DDL: the point is to catch a
/// table whose DDL was never applied, so reading the expectation out of the database would
/// defeat it.
const ENTRIES_COLUMNS: &[&str] = &[
    "id",
    "run_id",
    "start_line",
    "end_line",
    "captured_at",
    "level",
    "evidence",
    "is_error",
    "error_reason",
    "fingerprint_id",
    "boundaries",
];

/// True when `entries` is absent (a fresh database) or has every expected column.
fn entries_shape_current(conn: &Connection) -> bool {
    let Ok(mut stmt) = conn.prepare("SELECT name FROM pragma_table_info('entries')") else {
        return true;
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
        return true;
    };
    let found: std::collections::HashSet<String> = rows.filter_map(|r| r.ok()).collect();
    // No rows means no table, which `PROJECT_SCHEMA` is about to create correctly.
    found.is_empty() || ENTRIES_COLUMNS.iter().all(|c| found.contains(*c))
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    )?;
    Ok(())
}

pub fn get_meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .ok()
}

// ---------------------------------------------------------------------------
// Line access
// ---------------------------------------------------------------------------

/// Reads the raw bytes of a line range, one `Vec<u8>` per line.
///
/// Chunks are line-aligned, so only the chunks overlapping the range are scanned.
pub fn read_lines(conn: &Connection, run_id: RunId, start: i64, end: i64) -> Result<Vec<Vec<u8>>> {
    if end < start {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare_cached(
        "SELECT first_line, n_lines, bytes FROM chunks
          WHERE run_id = ?1 AND first_line <= ?3 AND first_line + n_lines > ?2
          ORDER BY first_line",
    )?;
    let mut out: Vec<Vec<u8>> = Vec::new();
    let rows = stmt.query_map((run_id, start, end), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    for row in rows {
        let (first_line, n_lines, bytes) = row?;
        for (i, line) in split_chunk(&bytes, n_lines as usize).into_iter().enumerate() {
            let index = first_line + i as i64;
            if index >= start && index <= end {
                out.push(line);
            }
        }
    }
    Ok(out)
}

/// Splits a chunk payload back into lines.
///
/// Every line in a chunk is LF-terminated, including the last, so a trailing empty segment
/// is an artifact of the split rather than a blank line.
pub fn split_chunk(bytes: &[u8], expect: usize) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(expect);
    let mut start = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            out.push(bytes[start..i].to_vec());
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(bytes[start..].to_vec());
    }
    out
}

/// Packs lines into a chunk payload.
pub fn pack_chunk(lines: &[RawLine]) -> Vec<u8> {
    let mut out = Vec::with_capacity(lines.iter().map(|l| l.bytes.len() + 1).sum());
    for line in lines {
        out.extend_from_slice(&line.bytes);
        out.push(b'\n');
    }
    out
}

/// Rendered text for a line range: ANSI resolved, progress bars collapsed.
pub fn render_range(conn: &Connection, run_id: RunId, start: i64, end: i64) -> Result<String> {
    let lines = read_lines(conn, run_id, start, end)?;
    Ok(vt::render_lines(lines.iter().map(|l| l.as_slice())))
}

/// Verbatim bytes for a line range, CRLF-terminated so it replays into a terminal.
///
/// The pty's `ONLCR` is normalised away on ingest and reapplied here, keeping the stored
/// form deterministic without breaking replay.
pub fn raw_range(conn: &Connection, run_id: RunId, start: i64, end: i64) -> Result<Vec<u8>> {
    let lines = read_lines(conn, run_id, start, end)?;
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(&line);
        out.extend_from_slice(b"\r\n");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// SQL functions
// ---------------------------------------------------------------------------

/// Registers `logbear_render` and `logbear_raw` on a connection.
///
/// Both read `chunks` while a query is already executing on the same connection, which
/// SQLite disallows, so each holds its own read-only connection to the file.
pub fn register_functions(conn: &Connection, path: &Path) -> Result<()> {
    use rusqlite::functions::FunctionFlags;

    // Must be INNOCUOUS rather than DIRECTONLY: these are used inside v_entries, and
    // SQLite refuses a non-innocuous function in a view under `trusted_schema = OFF`.
    let flags = FunctionFlags::SQLITE_UTF8
        | FunctionFlags::SQLITE_INNOCUOUS
        | FunctionFlags::SQLITE_DETERMINISTIC;

    let render_conn = open_reader(path)?;
    conn.create_scalar_function("logbear_render", 3, flags, move |ctx| {
        let run_id: i64 = ctx.get(0)?;
        let start: i64 = ctx.get(1)?;
        let end: i64 = ctx.get(2)?;
        let guard = render_conn
            .lock()
            .map_err(|_| rusqlite::Error::UserFunctionError("reader poisoned".into()))?;
        render_range(&guard, run_id, start, end)
            .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))
    })?;

    let raw_conn = open_reader(path)?;
    conn.create_scalar_function("logbear_raw", 3, flags, move |ctx| {
        let run_id: i64 = ctx.get(0)?;
        let start: i64 = ctx.get(1)?;
        let end: i64 = ctx.get(2)?;
        let guard = raw_conn
            .lock()
            .map_err(|_| rusqlite::Error::UserFunctionError("reader poisoned".into()))?;
        let bytes = raw_range(&guard, run_id, start, end)
            .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })?;

    Ok(())
}

fn open_reader(path: &Path) -> Result<Arc<Mutex<Connection>>> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
    Ok(Arc::new(Mutex::new(conn)))
}

/// Function names an agent's SQL may call. Anything absent is denied.
pub const ALLOWED_FUNCTIONS: &[&str] = &[
    "logbear_render",
    "logbear_raw",
    // Core SQLite scalar and aggregate functions.
    "abs", "avg", "cast", "char", "coalesce", "count", "date", "datetime", "format",
    "group_concat", "hex", "ifnull", "iif", "instr", "json", "json_array",
    "json_array_length", "json_each", "json_extract", "json_object", "json_type",
    "json_valid", "julianday", "length", "like", "lower", "ltrim", "max", "min", "nullif",
    "printf", "quote", "random", "replace", "round", "rtrim", "strftime", "substr",
    "sum", "time", "total", "trim", "typeof", "unicode", "unixepoch", "upper",
    "row_number", "rank", "dense_rank", "lag", "lead", "first_value", "last_value",
    "ntile", "percent_rank", "cume_dist", "nth_value",
    // FTS5 auxiliary functions.
    "bm25", "highlight", "snippet", "rank",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_packing_round_trips() {
        let lines = vec![
            RawLine { index: 0, captured_at: 1, bytes: b"one".to_vec() },
            RawLine { index: 1, captured_at: 2, bytes: b"".to_vec() },
            RawLine { index: 2, captured_at: 3, bytes: b"three".to_vec() },
        ];
        let packed = pack_chunk(&lines);
        let back = split_chunk(&packed, 3);
        assert_eq!(back.len(), 3);
        assert_eq!(back[0], b"one");
        assert_eq!(back[1], b"");
        assert_eq!(back[2], b"three");
    }

    #[test]
    fn a_line_containing_escapes_survives_packing() {
        let lines = vec![RawLine {
            index: 0,
            captured_at: 1,
            bytes: b"\x1b[31mred\x1b[0m".to_vec(),
        }];
        let back = split_chunk(&pack_chunk(&lines), 1);
        assert_eq!(back[0], b"\x1b[31mred\x1b[0m");
    }
}
