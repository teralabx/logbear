//! Schema for a project database, and for the small central index.
//!
//! The shape is two layers. `chunks` is truth: append-only, never rewritten, holding the
//! captured bytes and nothing else. Everything else — `entries`, `fingerprints`, the
//! counters, the FTS index — is a derived projection that can be dropped and rebuilt.
//!
//! `entries` deliberately holds **no text**. It holds a line range. Text lives once, in
//! `chunks`, and rendered text is produced on demand by `runlog_render()`. That is what
//! keeps a reparse safe: if the row *were* the logical entry, regenerating would split and
//! merge rows, row identity would be unstable, and every reference to it — fingerprints,
//! session cursors, embeddings — would break.

/// Bumped whenever the derived layer's meaning changes. On a bump the daemon rebuilds the
/// index from chunks rather than migrating it, because it can.
pub const DERIVED_VERSION: i64 = 1;
/// Bumped when truth-layer tables change shape. Requires a real migration.
pub const SCHEMA_VERSION: i64 = 1;

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
  bytes_total     INTEGER NOT NULL DEFAULT 0
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
  is_error       INTEGER NOT NULL DEFAULT 0,
  error_reason   TEXT,
  fingerprint_id INTEGER REFERENCES fingerprints(id),
  boundaries     TEXT    NOT NULL DEFAULT 'inferred'
);
CREATE INDEX IF NOT EXISTS entries_run_time ON entries(run_id, captured_at);
CREATE INDEX IF NOT EXISTS entries_fp       ON entries(fingerprint_id, captured_at);
CREATE INDEX IF NOT EXISTS entries_err      ON entries(run_id, is_error, captured_at);
CREATE INDEX IF NOT EXISTS entries_time     ON entries(captured_at DESC);

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
  session_id    TEXT    NOT NULL,
  harness       TEXT    NOT NULL,                -- claude_code | codex | mcp
  last_entry_id INTEGER NOT NULL DEFAULT 0,
  last_seen_at  INTEGER NOT NULL,
  blocks_issued INTEGER NOT NULL DEFAULT 0,
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

/// Views and the functions they lean on. Recreated on every open, because they are pure
/// projections and the function set can grow between versions.
pub const PROJECT_VIEWS: &str = r#"
DROP VIEW IF EXISTS v_entries;
DROP VIEW IF EXISTS v_errors;
DROP VIEW IF EXISTS v_runs;

-- Rendered text, produced on demand from the bytes. This is why entries hold no text.
CREATE VIEW v_entries AS
SELECT e.id, e.run_id, e.start_line, e.end_line, e.captured_at, e.level, e.is_error,
       e.error_reason, e.fingerprint_id, e.boundaries,
       runlog_render(e.run_id, e.start_line, e.end_line) AS text
  FROM entries e;

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

/// The central registry. Deliberately tiny: it answers "which projects exist" and
/// "what is on the home screen", and nothing an agent would ever query.
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

/// Handed to an agent by `runlog_schema()` before it writes any SQL. Kept next to the DDL
/// so the two cannot drift.
pub const AGENT_NOTES: &str = r#"
## How this database is shaped

Two layers. `chunks` holds the captured bytes and is the only source of truth. Everything
else is a derived index that can be rebuilt from it, so do not be surprised if entry ids
change after a reparse — fingerprint hashes and run ids do not.

`entries` holds **no text**, only a line range. Query the `v_entries` view instead: it
calls `runlog_render()` to produce the text on demand, with ANSI resolved.

## Functions available to your SQL

  runlog_render(run_id, start_line, end_line) -> TEXT   VT-emulated, ANSI stripped
  runlog_raw(run_id, start_line, end_line)    -> TEXT   verbatim bytes, ANSI intact

## Views

  v_entries   every entry with its rendered `text`
  v_errors    v_entries restricted to errors, joined to its fingerprint template
  v_runs      runs with per-level counters, duration, and a `clean` flag
              (`clean` = exited 0 AND logged no errors — this is "it worked")

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
