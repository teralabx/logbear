//! Schema for a project database, and for the small central index.
//!
//! Two layers. `chunks` is the truth layer, holding the captured bytes and nothing else.
//! `entries` and the FTS index are a derived projection that can be dropped and rebuilt
//! from it. `fingerprints` and the counters are derived the same way but outlive the bytes,
//! so they are kept across a rebuild; see [`DROP_DERIVED`].
//!
//! `entries` stores a line range and no text; text lives once in `chunks` and is rendered
//! on demand by `logbear_render()`. A reparse therefore rewrites only the derived layer and
//! can never lose or alter captured output.

use crate::error::{Error, Result};
use crate::store::{get_meta, set_meta};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;

/// Bumped whenever the derived layer's meaning changes. The daemon then rebuilds the index
/// from chunks rather than migrating it.
///
/// * 2 — severity is read rather than inferred. Adds `entries.evidence` and recomputes
///   every existing entry's level.
/// * 3 — in prefix mode an unprefixed exception banner starts its own entry instead of
///   continuing the one before it, so existing entries are regrouped.
pub const DERIVED_VERSION: i64 = 3;

/// The shape this build reads, held in `PRAGMA user_version`. Moving it means adding a step
/// to [`MIGRATIONS`] and bringing [`PROJECT_SCHEMA`] to the same shape.
///
/// * 1 — every database written before migrations existed. They carry `user_version` 0
///   and differ among themselves only in [`LATE_COLUMNS`].
/// * 2 — bounded indexes on `entries`, a signature index for error lookups, and the repair
///   of derived layers left empty or uncounted by a crash or an earlier rebuild.
pub const SCHEMA_VERSION: i64 = 2;

/// One step up. Each runs in its own transaction with the version bump, so a crash leaves
/// the database wholly before or wholly after the step.
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub up: fn(&Connection) -> Result<()>,
}

/// In order, and never edited once shipped: a database may stop at any of them.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 2,
    name: "bounded entry indexes and derived-layer repair",
    up: migrate_to_2,
}];

/// Refuses a database from a newer build before anything writes to it.
///
/// Reads only, and is the first thing done with a file, so it also surfaces a damaged one
/// as an error rather than as a half-applied open.
pub fn check_readable(conn: &Connection, path: &Path) -> Result<()> {
    let refuse = |found: i64, supported: i64| Error::DatabaseTooNew {
        path: path.to_path_buf(),
        found,
        supported,
    };
    let version = user_version(conn)?;
    if version > SCHEMA_VERSION {
        return Err(refuse(version, SCHEMA_VERSION));
    }
    if table_exists(conn, "meta")? {
        // Builds before `user_version` was read wrote these stamps and nothing else.
        let stamp = |key| get_meta(conn, key).and_then(|v| v.parse::<i64>().ok());
        if let Some(found) = stamp("schema_version").filter(|v| *v > SCHEMA_VERSION) {
            return Err(refuse(found, SCHEMA_VERSION));
        }
        if let Some(found) = stamp("derived_version").filter(|v| *v > DERIVED_VERSION) {
            return Err(refuse(found, DERIVED_VERSION));
        }
    }
    Ok(())
}

/// Brings a database to [`SCHEMA_VERSION`].
pub fn migrate(conn: &mut Connection) -> Result<()> {
    let mut version = user_version(conn)?;
    if version == 0 {
        if !table_exists(conn, "runs")? {
            return create(conn);
        }
        // A database from before migrations. Nothing is written yet: the first step is the
        // one that moves `user_version`.
        version = 1;
    }
    for step in MIGRATIONS.iter().filter(|m| m.version > version) {
        apply(conn, step).map_err(|e| match e {
            // Keep the code visible, so a damaged file is still recognised as one.
            Error::Sqlite(_) => e,
            other => Error::other(format!("migration to {} ({}): {other}", step.version, step.name)),
        })?;
    }
    Ok(())
}

fn apply(conn: &mut Connection, step: &Migration) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Taken after the write lock, so a second process cannot apply the same step.
    if user_version(&tx)? < step.version {
        (step.up)(&tx)?;
        stamp(&tx, step.version)?;
    }
    tx.commit()?;
    Ok(())
}

/// A new database goes straight to the current shape, in one transaction, so a crash during
/// creation leaves an empty file rather than a half-made schema.
fn create(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(PROJECT_SCHEMA)?;
    stamp(&tx, SCHEMA_VERSION)?;
    set_meta(&tx, "derived_version", &DERIVED_VERSION.to_string())?;
    tx.commit()?;
    Ok(())
}

fn stamp(conn: &Connection, version: i64) -> Result<()> {
    conn.pragma_update(None, "user_version", version)?;
    // Still written: a build from before `user_version` reads nothing else.
    set_meta(conn, "schema_version", &version.to_string())
}

pub fn user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

pub(crate) fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM sqlite_master WHERE name = ?1", [name], |_| Ok(()))
        .optional()?
        .is_some())
}

/// Version 2.
///
/// The indexes dropped here were most of the cost of an entry: `entries_err` and
/// `entries_time` covered every row to serve the rare error, and a partial index does that
/// for almost nothing. See [`PROJECT_SCHEMA`] for what replaced them.
///
/// It also repairs what older builds could leave behind. A crash between dropping the
/// derived layer and rebuilding it left empty entries with a current version stamp, and an
/// old rebuild dropped squashed runs' counters; either way the data to fix it is in
/// `chunks`, so a rebuild is scheduled.
fn migrate_to_2(conn: &Connection) -> Result<()> {
    add_late_columns(conn);
    conn.execute_batch(
        "DROP INDEX IF EXISTS entries_err;
         DROP INDEX IF EXISTS entries_time;
         DROP INDEX IF EXISTS entries_fp;",
    )?;
    conn.execute_batch(PROJECT_SCHEMA)?;

    let needs_rebuild: bool = conn.query_row(
        "SELECT (EXISTS (SELECT 1 FROM chunks) AND NOT EXISTS (SELECT 1 FROM entries))
             OR EXISTS (SELECT 1 FROM runs r
                         WHERE r.squashed_at IS NOT NULL AND r.chunks_evicted = 0
                           AND NOT EXISTS (SELECT 1 FROM run_counters c WHERE c.run_id = r.id))",
        [],
        |r| r.get(0),
    )?;
    if needs_rebuild {
        mark_rebuild(conn)?;
    }
    Ok(())
}

/// Drops whatever of the derived layer is at a stale version, in one transaction with the
/// record that it needs rebuilding. Returns true while a rebuild is outstanding, including
/// one a crash interrupted.
pub fn prepare_derived(conn: &mut Connection) -> Result<bool> {
    let stored: Option<i64> = get_meta(conn, "derived_version").and_then(|v| v.parse().ok());
    // The version stamp records intent; the table records reality. They diverge when a
    // build writes the stamp without changing the shape, and then a correct build sees a
    // current version over a stale table and rebuilds nothing. Only the shape is
    // authoritative, so it is checked too.
    let stale = stored.is_some_and(|v| v != DERIVED_VERSION) || !entries_shape_current(conn);
    if stale {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(DROP_DERIVED)?;
        tx.execute_batch(PROJECT_SCHEMA)?;
        mark_rebuild(&tx)?;
        set_meta(&tx, "derived_version", &DERIVED_VERSION.to_string())?;
        tx.commit()?;
    } else if stored.is_none() {
        set_meta(conn, "derived_version", &DERIVED_VERSION.to_string())?;
    }
    Ok(get_meta(conn, REBUILD_UPTO).is_some())
}

/// Meta key: the newest run the outstanding rebuild covers. Its presence is what says a
/// rebuild is outstanding. Runs after it are written by the current parser and left alone.
pub const REBUILD_UPTO: &str = "derived_rebuild_upto";
/// Meta key: the last run the outstanding rebuild finished, where a resume picks up.
pub const REBUILD_AFTER: &str = "derived_rebuild_after";

/// Meta key: this project's row in the central index, as JSON. See
/// [`Db::stamp_identity`](crate::store::Db::stamp_identity).
pub const PROJECT_IDENTITY: &str = "project_identity";

fn mark_rebuild(conn: &Connection) -> Result<()> {
    let upto: i64 = conn.query_row("SELECT COALESCE(max(id), 0) FROM runs", [], |r| r.get(0))?;
    set_meta(conn, REBUILD_UPTO, &upto.to_string())?;
    set_meta(conn, REBUILD_AFTER, "0")
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

/// Drops the part of the derived layer a rebuild can reproduce, so it can be recreated at
/// the current shape.
///
/// Only that part. `fingerprints`, `run_fingerprints` and `run_counters` are derived too,
/// but they outlive the bytes: a squashed run's counts describe output no longer stored,
/// and an evicted run has no bytes left at all. Dropping them erased those runs' signatures
/// and zeroed their error counts, so a run that failed could read as the last good one.
/// A rebuild re-derives them for every run whose bytes survive and leaves the rest alone.
///
/// Truth — `runs`, `chunks`, `meta` — is untouched, so a rebuild reproduces the index from
/// the captured bytes. `CREATE TABLE IF NOT EXISTS` cannot add a column to an existing
/// table, so dropping is what makes a shape change possible at all.
pub const DROP_DERIVED: &str = r#"
DROP TABLE IF EXISTS entries;
DROP TABLE IF EXISTS entries_fts;
"#;

/// Columns added to existing tables after their table shipped.
///
/// `CREATE TABLE IF NOT EXISTS` leaves an older database with the older column set, so the
/// additions have to be applied separately. These predate migrations, which is why version 1
/// comes in several shapes; [`migrate_to_2`] evens them out. New columns belong in a
/// migration.
const LATE_COLUMNS: &[&str] = &[
    "ALTER TABLE session_cursors ADD COLUMN nudge_entry_id INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE session_cursors ADD COLUMN change_blocks_issued INTEGER NOT NULL DEFAULT 0",
    // When the run's stored output was squashed to its unique entries; null if it never was.
    "ALTER TABLE runs ADD COLUMN squashed_at INTEGER",
];

/// Adds any of [`LATE_COLUMNS`] this database does not have.
///
/// Adding a column that already exists is an error rather than a no-op, so each is attempted
/// and its rejection ignored — cheaper and harder to get wrong than reading `PRAGMA
/// table_info` back and diffing it.
fn add_late_columns(conn: &rusqlite::Connection) {
    for stmt in LATE_COLUMNS {
        let _ = conn.execute(stmt, []);
    }
}

pub const PROJECT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- ---------------------------------------------------------------- truth
CREATE TABLE IF NOT EXISTS runs (
  id              INTEGER PRIMARY KEY,
  project_id      INTEGER NOT NULL,
  command         TEXT    NOT NULL,              -- argv as a JSON array
  cwd             TEXT    NOT NULL,
  session_group   INTEGER,
  started_at      INTEGER NOT NULL,              -- unix micros
  ended_at        INTEGER,
  exit_code       INTEGER,
  signal          TEXT,
  git_sha         TEXT,
  git_branch      TEXT,
  git_dirty_hash  TEXT,
  lockfile_hashes TEXT    NOT NULL DEFAULT '{}', -- JSON object
  env_keys        TEXT    NOT NULL DEFAULT '[]', -- JSON array of NAMES ONLY
  term_cols       INTEGER NOT NULL DEFAULT 80,
  delimiter_mode  TEXT    NOT NULL DEFAULT 'fallback',
  chunks_evicted  INTEGER NOT NULL DEFAULT 0,
  n_lines         INTEGER NOT NULL DEFAULT 0,
  bytes_total     INTEGER NOT NULL DEFAULT 0,
  -- When the run's stored output was squashed to its unique entries; null if it never was.
  squashed_at     INTEGER
);
CREATE INDEX IF NOT EXISTS runs_started ON runs(started_at DESC);
CREATE INDEX IF NOT EXISTS runs_session ON runs(session_group, started_at);

-- Captured bytes, batched at line boundaries. Concatenating every chunk of a run in seq
-- order reproduces the stream, one line per LF, with the pty's ONLCR artifact normalised.
CREATE TABLE IF NOT EXISTS chunks (
  run_id      INTEGER NOT NULL,
  seq         INTEGER NOT NULL,
  captured_at INTEGER NOT NULL,
  first_line  INTEGER NOT NULL,
  n_lines     INTEGER NOT NULL,
  bytes       BLOB    NOT NULL,                  -- verbatim, ANSI intact
  PRIMARY KEY (run_id, seq)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS chunks_lines ON chunks(run_id, first_line);

-- ---------------------------------------------------------------- derived index
CREATE TABLE IF NOT EXISTS entries (
  id             INTEGER PRIMARY KEY,
  run_id         INTEGER NOT NULL,
  start_line     INTEGER NOT NULL,
  end_line       INTEGER NOT NULL,
  captured_at    INTEGER NOT NULL,
  level          TEXT    NOT NULL DEFAULT 'unknown',
  -- How `level` was arrived at: declared | structural | unknown. `unknown` means the
  -- producer said nothing, which is an answer and not a missing value. Severity is never
  -- inferred from the shape of the text, so there is no fourth value.
  evidence       TEXT    NOT NULL DEFAULT 'unknown',
  is_error       INTEGER NOT NULL DEFAULT 0,
  error_reason   TEXT,
  fingerprint_id INTEGER REFERENCES fingerprints(id),
  boundaries     TEXT    NOT NULL DEFAULT 'inferred'
);
-- Each index on this table is written for every line captured, so there are as few as will
-- do. Errors are rare, so the index that finds them is partial and costs nearly nothing.
CREATE INDEX IF NOT EXISTS entries_run_time ON entries(run_id, captured_at);
CREATE INDEX IF NOT EXISTS entries_signature ON entries(fingerprint_id);
CREATE INDEX IF NOT EXISTS entries_errors   ON entries(captured_at) WHERE is_error = 1;

CREATE TABLE IF NOT EXISTS fingerprints (
  id          INTEGER PRIMARY KEY,
  template    TEXT    NOT NULL,                  -- readable; this is the human summary
  hash        TEXT    NOT NULL UNIQUE,
  level       TEXT    NOT NULL DEFAULT 'unknown',
  first_seen  INTEGER NOT NULL,
  last_seen   INTEGER NOT NULL,
  total_count INTEGER NOT NULL DEFAULT 0,
  run_count   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS fingerprints_last ON fingerprints(last_seen DESC);
CREATE INDEX IF NOT EXISTS fingerprints_errors ON fingerprints(id) WHERE level = 'error';

-- Per-run rollup. A few KB per run, so it outlives chunk eviction and keeps
-- "it worked three weeks ago" answerable long after the bytes are gone.
CREATE TABLE IF NOT EXISTS run_fingerprints (
  run_id         INTEGER NOT NULL,
  fingerprint_id INTEGER NOT NULL,
  count          INTEGER NOT NULL DEFAULT 0,
  first_at       INTEGER NOT NULL,
  last_at        INTEGER NOT NULL,
  PRIMARY KEY (run_id, fingerprint_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS run_fp_by_fp ON run_fingerprints(fingerprint_id, run_id);

CREATE TABLE IF NOT EXISTS run_counters (
  run_id    INTEGER PRIMARY KEY,
  n_entries INTEGER NOT NULL DEFAULT 0,
  n_error   INTEGER NOT NULL DEFAULT 0,
  n_warn    INTEGER NOT NULL DEFAULT 0,
  n_info    INTEGER NOT NULL DEFAULT 0,
  n_debug   INTEGER NOT NULL DEFAULT 0,
  n_trace   INTEGER NOT NULL DEFAULT 0,
  n_unknown INTEGER NOT NULL DEFAULT 0
);

-- One watermark per agent session, never global: two Claude Code windows on the same
-- project must not steal each other's notifications.
CREATE TABLE IF NOT EXISTS session_cursors (
  session_id           TEXT    NOT NULL,
  harness              TEXT    NOT NULL,         -- claude_code | codex | mcp
  last_entry_id        INTEGER NOT NULL DEFAULT 0,
  last_seen_at         INTEGER NOT NULL,
  blocks_issued        INTEGER NOT NULL DEFAULT 0,
  -- Advanced when a verify nudge fires, never by the error path. Separate from
  -- last_entry_id so telling the agent to go look cannot consume errors the Stop
  -- hook has yet to report.
  nudge_entry_id       INTEGER NOT NULL DEFAULT 0,
  change_blocks_issued INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (session_id)
) WITHOUT ROWID;

-- A fingerprint blocks a given session at most once, however many times it recurs.
CREATE TABLE IF NOT EXISTS session_blocks (
  session_id     TEXT    NOT NULL,
  fingerprint_id INTEGER NOT NULL,
  at             INTEGER NOT NULL,
  PRIMARY KEY (session_id, fingerprint_id)
) WITHOUT ROWID;

-- Notification bookkeeping, so a burst updates a counter instead of stacking banners.
CREATE TABLE IF NOT EXISTS notice_state (
  fingerprint_id INTEGER PRIMARY KEY,
  last_notified  INTEGER NOT NULL,
  count_since    INTEGER NOT NULL DEFAULT 0
);

-- Contentless: the text lives in chunks and is re-rendered on demand, so FTS holds only
-- the index. `contentless_delete` is required or eviction cannot remove stale rows, and
-- search would return hits whose text no longer exists.
CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
  text,
  content='',
  contentless_delete=1,
  tokenize='porter unicode61'
);
"#;

/// Read-only views over the derived layer.
///
/// Dropped and recreated on every open, since they are pure projections and the function
/// set they call can change between versions.
pub const PROJECT_VIEWS: &str = r#"
DROP VIEW IF EXISTS v_entries;
DROP VIEW IF EXISTS v_errors;
DROP VIEW IF EXISTS v_novel;
DROP VIEW IF EXISTS v_runs;

-- Rendered text, produced on demand from the bytes. This is why entries hold no text.
CREATE VIEW v_entries AS
SELECT e.id, e.run_id, e.start_line, e.end_line, e.captured_at, e.level, e.evidence,
       e.is_error, e.error_reason, e.fingerprint_id, e.boundaries,
       logbear_render(e.run_id, e.start_line, e.end_line) AS text
  FROM entries e;

-- Output that has never been seen in this project before, at any severity. This is where
-- `IO.inspect` / `print` / `console.log` debugging surfaces: a line a developer added
-- while chasing something has no severity to read, but its template being brand new is a
-- fact rather than a guess. Deliberately not restricted to errors.
CREATE VIEW v_novel AS
SELECT v.*, f.template, f.first_seen, f.total_count
  FROM v_entries v
  JOIN fingerprints f ON f.id = v.fingerprint_id
 WHERE f.first_seen >= (SELECT started_at FROM runs WHERE id = v.run_id)
   AND f.total_count > 0;

CREATE VIEW v_errors AS
SELECT v.*, f.template, f.total_count, f.first_seen, f.last_seen
  FROM v_entries v
  JOIN fingerprints f ON f.id = v.fingerprint_id
 WHERE v.is_error = 1;

CREATE VIEW v_runs AS
SELECT r.id, r.project_id, r.command, r.cwd, r.session_group, r.started_at, r.ended_at,
       r.exit_code, r.signal, r.git_sha, r.git_branch, r.git_dirty_hash,
       r.lockfile_hashes, r.term_cols, r.delimiter_mode, r.chunks_evicted,
       COALESCE(c.n_entries, 0) AS n_entries,
       COALESCE(c.n_error, 0)   AS n_error,
       COALESCE(c.n_warn, 0)    AS n_warn,
       COALESCE(c.n_info, 0)    AS n_info,
       (r.ended_at - r.started_at) AS duration_micros,
       (r.exit_code = 0 AND COALESCE(c.n_error, 0) = 0) AS clean
  FROM runs r
  LEFT JOIN run_counters c ON c.run_id = r.id;
"#;

/// Schema for the central registry: which projects exist, plus a per-run summary for the
/// home screen. Never queried by agent SQL.
pub const INDEX_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS projects (
  id              INTEGER PRIMARY KEY,
  name            TEXT    NOT NULL,
  root            TEXT    NOT NULL UNIQUE,
  db_path         TEXT    NOT NULL,
  created_at      INTEGER NOT NULL,
  last_seen_at    INTEGER NOT NULL,
  retention_bytes INTEGER,                       -- NULL inherits the global default
  push_enabled    INTEGER NOT NULL DEFAULT 1,
  notify_enabled  INTEGER NOT NULL DEFAULT 1
);

-- Denormalised summary so the home screen is one query against one small database
-- instead of a fan-out across every project file.
CREATE TABLE IF NOT EXISTS run_summaries (
  project_id INTEGER NOT NULL,
  run_id     INTEGER NOT NULL,
  command    TEXT    NOT NULL,
  started_at INTEGER NOT NULL,
  ended_at   INTEGER,
  exit_code  INTEGER,
  n_error    INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (project_id, run_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS run_summaries_time ON run_summaries(started_at DESC);
"#;

/// Documentation handed to an agent by `logbear_schema()` before it writes any SQL. Kept
/// next to the DDL so the two cannot drift.
pub const AGENT_NOTES: &str = r#"
## How this database is shaped

Two layers. `chunks` holds the captured bytes and is the only source of truth. Everything
else is a derived index that can be rebuilt from it, so do not be surprised if entry ids
change after a reparse — fingerprint hashes and run ids do not.

`entries` holds **no text**, only a line range. Query the `v_entries` view instead: it
calls `logbear_render()` to produce the text on demand, with ANSI resolved.

## Functions available to your SQL

  logbear_render(run_id, start_line, end_line) -> TEXT   VT-emulated, ANSI stripped
  logbear_raw(run_id, start_line, end_line)    -> TEXT   verbatim bytes, ANSI intact

## Views

  v_entries   every entry with its rendered `text`
  v_errors    v_entries restricted to errors, joined to its fingerprint template
  v_novel     entries whose signature is being seen for the first time in this project,
              at ANY severity — this is where print-debugging output shows up
  v_runs      runs with per-level counters, duration, and a `clean` flag
              (`clean` = exited 0 AND logged no errors — this is "it worked")

## How severity works, so you can trust it

`level` is **read from the output, never inferred from its shape**. `evidence` says how:

  declared    the process emitted a severity token — `[error]`, `level=error`,
              `{"level":"error"}` — and we read it verbatim
  structural  no token, but the runtime printed a banner it only prints when something
              threw: `** (`, `Traceback (most recent call last):`, `panic:`
  unknown     the process said nothing about severity

`unknown` is an answer, not a missing value, and it is **never** an error. A bare
`print` / `IO.inspect` / `console.log` line is `unknown` by construction — no language
attaches a severity to those. Do not read `level = 'unknown'` as "probably fine"; read it
as "nobody said". Reach for `v_novel` and the surrounding entries instead.

`is_error` is true only for a declared error/fatal or a structural banner. Nothing is an
error because it looked scary.

## Worked examples

  -- the last 5 errors, whole entries, never half a stack trace
  SELECT captured_at, text FROM v_errors ORDER BY captured_at DESC LIMIT 5;

  -- distinct error signatures in one run, by frequency
  SELECT template, count(*) AS n FROM v_errors WHERE run_id = ?1
   GROUP BY 1 ORDER BY n DESC;

  -- full-text search across entries
  SELECT v.text FROM entries_fts JOIN v_entries v ON v.id = entries_fts.rowid
   WHERE entries_fts MATCH 'timeout NEAR pool' LIMIT 20;

  -- what is failing now that was not failing before
  SELECT f.template, f.first_seen, f.total_count FROM fingerprints f
   WHERE f.level = 'error' AND f.first_seen > (SELECT started_at FROM runs
                                                ORDER BY started_at DESC LIMIT 1 OFFSET 1);

  -- output that appeared for the first time in this run, whatever its severity.
  -- This is how you find the print statements someone just added.
  SELECT captured_at, level, text FROM v_novel WHERE run_id = ?1 ORDER BY captured_at;

  -- everything printed in the 5 seconds before a run died: the evidence window.
  -- Use this instead of reading the whole log — the lines around a failure are the ones
  -- that explain it, and they are usually `unknown` severity.
  SELECT v.captured_at, v.level, v.text FROM v_entries v
    JOIN runs r ON r.id = v.run_id
   WHERE r.id = ?1 AND v.captured_at >= r.ended_at - 5000000
   ORDER BY v.captured_at;

  -- signatures present in every failed run but no clean one
  SELECT f.template FROM fingerprints f
   WHERE f.id IN (SELECT rf.fingerprint_id FROM run_fingerprints rf
                    JOIN v_runs r ON r.id = rf.run_id WHERE r.clean = 0)
     AND f.id NOT IN (SELECT rf.fingerprint_id FROM run_fingerprints rf
                        JOIN v_runs r ON r.id = rf.run_id WHERE r.clean = 1);

## Limits

Read-only. SELECT only — no writes, no ATTACH, no PRAGMA, no extension loading. Statements
are cancelled after 5 seconds. Rows are capped (default 200, ceiling 2000) and the response
is capped at 256 KB; both report truncation rather than silently cutting.

If a query is rejected the error names the construct that failed, so repair it and retry
rather than giving up.
"#;

#[cfg(test)]
#[path = "migration_tests.rs"]
mod tests;
