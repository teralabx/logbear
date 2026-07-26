//! The SQLite store. One database per project, plus a small central index.
//!
//! One database per project rather than one global file: projects stay isolated, retention
//! applies per project, and the query that actually matters — "what changed in *this*
//! project since yesterday" — is one ordinary query with no attaching and no fan-out. It
//! also means an agent's SQL is structurally unable to reach another project's data.

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
}

impl Db {
    /// Open (creating if needed) a project database and bring it up to schema.
    pub fn open(path: &Path, project_id: ProjectId) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::Error::io(parent, e))?;
        }
        let conn = Connection::open(path)?;
        apply_pragmas(&conn)?;
        conn.execute_batch(schema::PROJECT_SCHEMA)?;
        register_functions(&conn, path)?;
        conn.execute_batch(schema::PROJECT_VIEWS)?;
        set_meta(&conn, "schema_version", &schema::SCHEMA_VERSION.to_string())?;
        set_meta(&conn, "derived_version", &schema::DERIVED_VERSION.to_string())?;

        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
            path: path.to_path_buf(),
            project_id,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    /// Run a closure with the connection. Kept private-ish on purpose: everything the rest
    /// of the system needs is a named method in [`read`] or [`write`], so query shapes are
    /// reviewable in one place rather than scattered.
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().map_err(|_| crate::Error::other("db mutex poisoned"))?;
        f(&conn)
    }

    pub fn with_mut<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().map_err(|_| crate::Error::other("db mutex poisoned"))?;
        f(&mut conn)
    }

    /// Size on disk, including the WAL, which is what a retention cap has to count.
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
    // WAL so a reader (the app, an agent's query) never blocks the writer (capture).
    // NORMAL synchronous because losing the last few milliseconds of a dev log to a power
    // cut is an acceptable trade for not fsyncing on every batch.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -8000;",
    )?;
    // Incremental rather than full: a bulk DELETE plus VACUUM would stall the daemon and,
    // until vacuumed, would not return the disk at all.
    let mode: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)).unwrap_or(0);
    if mode != 2 {
        conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
    }
    Ok(())
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

/// Fetch the raw bytes of a line range, one `Vec<u8>` per line.
///
/// Chunks are line-aligned batches, so a range is served by scanning the few chunks that
/// overlap it rather than the whole run.
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

/// A chunk is its lines, each terminated by LF. The final terminator is always present,
/// so a trailing empty segment is an artifact of the split, not a blank line.
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

/// Pack lines into a chunk payload.
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

/// Verbatim bytes for a line range, CRLF-terminated so it replays correctly into a
/// terminal. The pty's ONLCR is normalised away on ingest and reapplied here, which is
/// what makes the stored form deterministic without making replay wrong.
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

/// Register `runlog_render` and `runlog_raw` on a connection.
///
/// They need to read `chunks` while a query is already executing on this connection, which
/// SQLite will not allow re-entrantly, so each function holds its own read-only connection
/// to the same file. WAL makes that concurrent read free.
pub fn register_functions(conn: &Connection, path: &Path) -> Result<()> {
    use rusqlite::functions::FunctionFlags;

    // Not DIRECTONLY: these are used inside v_entries, and SQLite refuses a
    // non-innocuous function in a view under the default `trusted_schema = OFF`.
    let flags = FunctionFlags::SQLITE_UTF8
        | FunctionFlags::SQLITE_INNOCUOUS
        | FunctionFlags::SQLITE_DETERMINISTIC;

    let render_conn = open_reader(path)?;
    conn.create_scalar_function("runlog_render", 3, flags, move |ctx| {
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
    conn.create_scalar_function("runlog_raw", 3, flags, move |ctx| {
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

/// Names an agent's SQL is allowed to call. Anything absent is denied outright rather
/// than merely unsupported, so adding a function is a deliberate act.
pub const ALLOWED_FUNCTIONS: &[&str] = &[
    "runlog_render",
    "runlog_raw",
    // Core SQLite scalar / aggregate surface an analytical query genuinely needs.
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
