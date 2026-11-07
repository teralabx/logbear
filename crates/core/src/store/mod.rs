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
pub mod wal;
pub mod write;

use crate::error::Result;
use crate::model::*;
use crate::parse::{vt, RawLine};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

pub use write::Writer;

/// A handle on one project's database.
#[derive(Clone)]
pub struct Db {
    /// Idle read-only connections, so a slow read never holds the capture writer's lock.
    ///
    /// Declared before `conn` so they are dropped first. The last connection to close folds
    /// the WAL in and removes it, which a read-only connection cannot do.
    readers: Arc<Mutex<Vec<Connection>>>,
    conn: Arc<Mutex<Connection>>,
    checkpointer: Arc<wal::Checkpointer>,
    /// Held for the length of a retention pass, so two cannot overlap.
    retention: Arc<Mutex<()>>,
    path: PathBuf,
    project_id: ProjectId,
    derived_stale: bool,
}

impl Db {
    /// Opens a project database, creating or migrating it as needed.
    ///
    /// A database from a newer build is refused with [`crate::Error::DatabaseTooNew`] and
    /// left byte for byte as it was.
    pub fn open(path: &Path, project_id: ProjectId) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::Error::io(parent, e))?;
        }
        crate::paths::create_private(path).map_err(|e| crate::Error::io(path, e))?;
        let mut conn = Connection::open(path)?;
        // First, because everything after this writes.
        schema::check_readable(&conn, path)?;
        apply_pragmas(&conn)?;
        wal::install(&conn)?;
        conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE);

        schema::migrate(&mut conn)?;
        let derived_stale = schema::prepare_derived(&mut conn)?;
        register_functions(&conn, path)?;
        create_views(&conn)?;

        Ok(Db {
            readers: Arc::new(Mutex::new(Vec::new())),
            conn: Arc::new(Mutex::new(conn)),
            checkpointer: Arc::new(wal::Checkpointer::new(path)),
            retention: Arc::new(Mutex::new(())),
            path: path.to_path_buf(),
            project_id,
            derived_stale,
        })
    }

    /// Whether the derived layer needs rebuilding from `chunks`, which includes a rebuild
    /// a crash interrupted.
    ///
    /// The caller does the rebuild, because it can take minutes on a large project and
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

    /// The writer's lock. A panic with it held has already rolled back through the
    /// transaction's drop, so the connection is sound and poison is not worth losing a
    /// project over.
    fn writer(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs a closure with the connection.
    ///
    /// Prefer the named methods in [`read`] and [`write`](mod@write), which keep query
    /// shapes in one reviewable place.
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.writer();
        let out = f(&conn);
        self.checkpointer.after_write(&conn, &self.conn);
        out
    }

    /// Runs a read on a pooled read-only connection.
    ///
    /// WAL lets readers proceed while the writer commits, but only on separate connections:
    /// funnelling a UI request that renders thousands of entries through [`Db::with`]
    /// would stall capture for as long as the render takes. Use this for anything that
    /// does not write and does not need to see the writer's uncommitted state.
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let pooled = self.readers.lock().unwrap_or_else(PoisonError::into_inner).pop();
        let conn = match pooled {
            Some(c) => c,
            None => {
                let c = Connection::open_with_flags(
                    &self.path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                c.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA cache_size = -4000;")?;
                c
            }
        };
        let out = f(&conn);
        let mut idle = self.readers.lock().unwrap_or_else(PoisonError::into_inner);
        if idle.len() < MAX_IDLE_READERS {
            idle.push(conn);
        }
        out
    }

    pub fn with_mut<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut conn = self.writer();
        let out = f(&mut conn);
        self.checkpointer.after_write(&conn, &self.conn);
        out
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

    /// Hands back what this database holds in memory: idle read connections and the
    /// writer's page cache. Worth calling after large work and on idle projects.
    pub fn release_memory(&self) {
        self.readers.lock().unwrap_or_else(PoisonError::into_inner).clear();
        let _ = self.writer().execute_batch("PRAGMA shrink_memory;");
    }

    /// Folds the WAL into the database and empties it, as far as the log's other users
    /// allow.
    ///
    /// Capture is stopped while this runs, so a reader on the log or a background
    /// checkpoint gets a few short tries rather than the usual timeout, with capture let
    /// through between them. What is left over goes in the next pass.
    pub fn checkpoint_truncate(&self) -> Result<()> {
        for _ in 0..TRUNCATE_TRIES {
            let done = {
                let conn = self.writer();
                conn.busy_timeout(TRUNCATE_WAIT)?;
                let done = wal::checkpoint(&conn, "TRUNCATE");
                conn.busy_timeout(BUSY_TIMEOUT)?;
                done?
            };
            if !done.busy {
                break;
            }
            std::thread::sleep(TRUNCATE_WAIT);
        }
        Ok(())
    }

    /// The right to run a retention pass, unless one is already running.
    pub(crate) fn try_retention(&self) -> Option<MutexGuard<'_, ()>> {
        match self.retention.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    /// Records what identifies this project, so the central index can be rebuilt from the
    /// project databases if it is ever lost. Writes only when something changed.
    pub fn stamp_identity(&self, project: &Project) -> Result<()> {
        let json = serde_json::to_string(project)?;
        self.with(|c| {
            if get_meta(c, schema::PROJECT_IDENTITY).as_deref() != Some(json.as_str()) {
                set_meta(c, schema::PROJECT_IDENTITY, &json)?;
            }
            Ok(())
        })
    }

    #[cfg(test)]
    pub(crate) fn checkpointer_shared_for_test(&self) -> Arc<impl Sized> {
        self.checkpointer.shared_for_test()
    }
}

/// Moves a damaged database and its sidecar files aside, returning where it went. The next
/// open at `path` starts empty; the file is kept, because it is the only copy of whatever
/// can still be salvaged from it.
pub fn quarantine(path: &Path) -> Result<PathBuf> {
    let aside = PathBuf::from(format!("{}.corrupt-{}", path.display(), crate::now_micros()));
    std::fs::rename(path, &aside).map_err(|e| crate::Error::io(path, e))?;
    for suffix in ["-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", path.display()));
        let _ = std::fs::rename(&from, format!("{}{suffix}", aside.display()));
    }
    Ok(aside)
}

/// Read connections kept open per project once their reads finish. Bursts beyond this
/// open a connection and close it afterwards rather than queueing. Each one is two file
/// descriptors.
const MAX_IDLE_READERS: usize = 2;

const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(5000);
const TRUNCATE_TRIES: usize = 40;
const TRUNCATE_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// Comfortably more than the statements on the write path, which are all prepared cached.
const STATEMENT_CACHE: usize = 48;

fn apply_pragmas(conn: &Connection) -> Result<()> {
    // Incremental rather than full: a bulk DELETE plus VACUUM would stall the daemon, and
    // until vacuumed would not return any disk space.
    //
    // It has to be set before anything is written, and switching to WAL writes the header,
    // so it comes first. Databases created by builds that set it afterwards never took it:
    // every page eviction freed stayed in the file, the file never shrank, and retention
    // read that as still over the cap and evicted run after run. Those are converted once,
    // with a VACUUM, which mostly copies free space away and so is quick on exactly the
    // files that need it.
    conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
    let mode: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)).unwrap_or(0);
    if mode != 2 {
        conn.execute_batch("VACUUM;")?;
    }

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
    Ok(())
}

/// Recreates the views if this build's differ from the stored ones.
///
/// Not on every open: that is a schema write, which costs a commit and throws away every
/// prepared statement on every other connection, and projects now open and close as they
/// come and go.
fn create_views(conn: &Connection) -> Result<()> {
    let hash = blake3::hash(schema::PROJECT_VIEWS.as_bytes()).to_hex().to_string();
    let current = get_meta(conn, "views_hash").as_deref() == Some(hash.as_str());
    if !current || !schema::table_exists(conn, "v_runs")? {
        conn.execute_batch(schema::PROJECT_VIEWS)?;
        set_meta(conn, "views_hash", &hash)?;
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

/// The lines of the chunk most recently read by [`render_range_cached`].
pub struct ChunkCache {
    run_id: RunId,
    first_line: i64,
    lines: Vec<Vec<u8>>,
}

/// [`render_range`] for a sequence of nearby ranges, such as a page of entries.
///
/// Entries are small and chunks hold hundreds of lines, so neighbouring entries almost
/// always share a chunk. Rendering a page with plain `render_range` re-reads and re-splits
/// the same blob once per entry; this reads it once.
pub fn render_range_cached(
    conn: &Connection,
    cache: &mut Option<ChunkCache>,
    run_id: RunId,
    start: i64,
    end: i64,
) -> Result<String> {
    fn slice(c: &ChunkCache, run_id: RunId, start: i64, end: i64) -> Option<String> {
        if c.run_id != run_id || start < c.first_line || end < start {
            return None;
        }
        let (a, b) = ((start - c.first_line) as usize, (end - c.first_line) as usize);
        let lines = c.lines.get(a..=b)?;
        Some(vt::render_lines(lines.iter().map(|l| l.as_slice())))
    }

    if let Some(text) = cache.as_ref().and_then(|c| slice(c, run_id, start, end)) {
        return Ok(text);
    }
    if end >= start {
        let row = conn
            .prepare_cached(
                "SELECT first_line, n_lines, bytes FROM chunks
                  WHERE run_id = ?1 AND first_line <= ?2 AND first_line + n_lines > ?2
                  ORDER BY first_line DESC LIMIT 1",
            )?
            .query_row((run_id, start), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Vec<u8>>(2)?))
            })
            .optional()?;
        if let Some((first_line, n_lines, bytes)) = row {
            let fresh = ChunkCache {
                run_id,
                first_line,
                lines: split_chunk(&bytes, n_lines as usize),
            };
            let hit = slice(&fresh, run_id, start, end);
            *cache = Some(fresh);
            if let Some(text) = hit {
                return Ok(text);
            }
        }
    }
    // Spans a chunk boundary: rare enough that the general path is fine.
    render_range(conn, run_id, start, end)
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

/// Most that `logbear_render` or `logbear_raw` will return. They read and build on the Rust
/// side, where neither SQLite's length limit nor its heap counter can see them, and their
/// range is whatever a query passes: `logbear_raw(1, 0, 1e9)` asks for the whole run.
pub const MAX_RENDERED_BYTES: usize = 2 * 1024 * 1024;
const RENDER_CUT: &str = "\n… (truncated)";

/// [`read_lines`], stopping once `max_bytes` have been read. The flag says whether it stopped.
fn read_lines_capped(
    conn: &Connection,
    run_id: RunId,
    start: i64,
    end: i64,
    max_bytes: usize,
) -> Result<(Vec<Vec<u8>>, bool)> {
    let mut stmt = conn.prepare_cached(
        "SELECT first_line, n_lines, bytes FROM chunks
          WHERE run_id = ?1 AND first_line <= ?3 AND first_line + n_lines > ?2
          ORDER BY first_line",
    )?;
    let mut rows = stmt.query((run_id, start, end))?;
    let (mut out, mut read) = (Vec::new(), 0usize);
    while let Some(row) = rows.next()? {
        let (first_line, n_lines): (i64, i64) = (row.get(0)?, row.get(1)?);
        let bytes: Vec<u8> = row.get(2)?;
        for (i, line) in split_chunk(&bytes, n_lines as usize).into_iter().enumerate() {
            let index = first_line + i as i64;
            if index >= start && index <= end {
                read += line.len();
                out.push(line);
                if read >= max_bytes {
                    return Ok((out, true));
                }
            }
        }
    }
    Ok((out, false))
}

fn render_capped(conn: &Connection, run_id: RunId, start: i64, end: i64) -> Result<String> {
    if end < start {
        return Ok(String::new());
    }
    let (lines, mut cut) = read_lines_capped(conn, run_id, start, end, MAX_RENDERED_BYTES)?;
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&vt::render_line(line));
        // Rendering can outgrow its input, since cursor motion costs less than the blanks it
        // leaves behind.
        if out.len() >= MAX_RENDERED_BYTES {
            cut = true;
            break;
        }
    }
    if cut {
        out.push_str(RENDER_CUT);
    }
    Ok(out)
}

fn raw_capped(conn: &Connection, run_id: RunId, start: i64, end: i64) -> Result<String> {
    if end < start {
        return Ok(String::new());
    }
    let (lines, cut) = read_lines_capped(conn, run_id, start, end, MAX_RENDERED_BYTES)?;
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(&line);
        out.extend_from_slice(b"\r\n");
    }
    let mut text = String::from_utf8_lossy(&out).into_owned();
    if cut {
        text.push_str(RENDER_CUT);
    }
    Ok(text)
}

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

    let render_conn = LazyReader::new(path);
    conn.create_scalar_function("logbear_render", 3, flags, move |ctx| {
        let run_id: i64 = ctx.get(0)?;
        let start: i64 = ctx.get(1)?;
        let end: i64 = ctx.get(2)?;
        render_conn
            .with(|c| render_capped(c, run_id, start, end))
            .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))
    })?;

    let raw_conn = LazyReader::new(path);
    conn.create_scalar_function("logbear_raw", 3, flags, move |ctx| {
        let run_id: i64 = ctx.get(0)?;
        let start: i64 = ctx.get(1)?;
        let end: i64 = ctx.get(2)?;
        raw_conn
            .with(|c| raw_capped(c, run_id, start, end))
            .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))
    })?;

    Ok(())
}

/// A read-only connection opened the first time it is needed.
///
/// Registering a function must not cost a connection: every open project would hold two
/// more file descriptors each for functions the writer's connection never calls.
struct LazyReader {
    path: PathBuf,
    conn: Mutex<Option<Connection>>,
}

impl LazyReader {
    fn new(path: &Path) -> LazyReader {
        LazyReader {
            path: path.to_path_buf(),
            conn: Mutex::new(None),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let mut slot = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            let conn = Connection::open_with_flags(
                &self.path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
            *slot = Some(conn);
        }
        match slot.as_ref() {
            Some(conn) => f(conn),
            None => Err(crate::Error::other("reader did not open")),
        }
    }
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
    fn a_database_and_its_sidecars_are_private_under_any_umask() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("p.db");
        let index_path = tmp.path().join("index.db");

        // Not the daemon's umask: the desktop app and the shim open stores too.
        let db = Db::open(&db_path, 1).unwrap();
        let index = index::IndexDb::open(&index_path).unwrap();
        db.with(|c| Ok(c.execute_batch("CREATE TABLE IF NOT EXISTS t (x); INSERT INTO t VALUES (1)")?))
            .unwrap();
        index
            .register(tmp.path(), "proj", &db_path)
            .unwrap();

        for name in ["p.db", "p.db-wal", "p.db-shm", "index.db", "index.db-wal", "index.db-shm"] {
            let path = tmp.path().join(name);
            let mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o600, "{name}");
        }
    }

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
