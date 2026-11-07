//! Upgrades, against databases as older builds really left them.

use super::*;
use crate::model::RunStamp;
use crate::proto::{EntryFilter, RunRef};
use crate::redact::Redactor;
use crate::store::index::IndexDb;
use crate::store::{read, retention, Db, Writer};
use std::collections::BTreeSet;
use std::sync::Arc;

/// The schema as version 1 shipped it, frozen. It is what a real user's file looks like, so
/// it must not follow [`PROJECT_SCHEMA`] when that changes.
const V1_SCHEMA: &str = r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE runs (
  id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, command TEXT NOT NULL, cwd TEXT NOT NULL,
  session_group INTEGER, started_at INTEGER NOT NULL, ended_at INTEGER, exit_code INTEGER,
  signal TEXT, git_sha TEXT, git_branch TEXT, git_dirty_hash TEXT,
  lockfile_hashes TEXT NOT NULL DEFAULT '{}', env_keys TEXT NOT NULL DEFAULT '[]',
  term_cols INTEGER NOT NULL DEFAULT 80, delimiter_mode TEXT NOT NULL DEFAULT 'fallback',
  chunks_evicted INTEGER NOT NULL DEFAULT 0, n_lines INTEGER NOT NULL DEFAULT 0,
  bytes_total INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX runs_started ON runs(started_at DESC);
CREATE INDEX runs_session ON runs(session_group, started_at);
CREATE TABLE chunks (
  run_id INTEGER NOT NULL, seq INTEGER NOT NULL, captured_at INTEGER NOT NULL,
  first_line INTEGER NOT NULL, n_lines INTEGER NOT NULL, bytes BLOB NOT NULL,
  PRIMARY KEY (run_id, seq)
) WITHOUT ROWID;
CREATE INDEX chunks_lines ON chunks(run_id, first_line);
CREATE TABLE entries (
  id INTEGER PRIMARY KEY, run_id INTEGER NOT NULL, start_line INTEGER NOT NULL,
  end_line INTEGER NOT NULL, captured_at INTEGER NOT NULL,
  level TEXT NOT NULL DEFAULT 'unknown', evidence TEXT NOT NULL DEFAULT 'unknown',
  is_error INTEGER NOT NULL DEFAULT 0, error_reason TEXT,
  fingerprint_id INTEGER REFERENCES fingerprints(id),
  boundaries TEXT NOT NULL DEFAULT 'inferred'
);
CREATE INDEX entries_run_time ON entries(run_id, captured_at);
CREATE INDEX entries_fp ON entries(fingerprint_id, captured_at);
CREATE INDEX entries_err ON entries(run_id, is_error, captured_at);
CREATE INDEX entries_time ON entries(captured_at DESC);
CREATE TABLE fingerprints (
  id INTEGER PRIMARY KEY, template TEXT NOT NULL, hash TEXT NOT NULL UNIQUE,
  level TEXT NOT NULL DEFAULT 'unknown', first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL, total_count INTEGER NOT NULL DEFAULT 0,
  run_count INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX fingerprints_last ON fingerprints(last_seen DESC);
CREATE TABLE run_fingerprints (
  run_id INTEGER NOT NULL, fingerprint_id INTEGER NOT NULL, count INTEGER NOT NULL DEFAULT 0,
  first_at INTEGER NOT NULL, last_at INTEGER NOT NULL, PRIMARY KEY (run_id, fingerprint_id)
) WITHOUT ROWID;
CREATE INDEX run_fp_by_fp ON run_fingerprints(fingerprint_id, run_id);
CREATE TABLE run_counters (
  run_id INTEGER PRIMARY KEY, n_entries INTEGER NOT NULL DEFAULT 0,
  n_error INTEGER NOT NULL DEFAULT 0, n_warn INTEGER NOT NULL DEFAULT 0,
  n_info INTEGER NOT NULL DEFAULT 0, n_debug INTEGER NOT NULL DEFAULT 0,
  n_trace INTEGER NOT NULL DEFAULT 0, n_unknown INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE session_cursors (
  session_id TEXT NOT NULL, harness TEXT NOT NULL, last_entry_id INTEGER NOT NULL DEFAULT 0,
  last_seen_at INTEGER NOT NULL, blocks_issued INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (session_id)
) WITHOUT ROWID;
CREATE TABLE session_blocks (
  session_id TEXT NOT NULL, fingerprint_id INTEGER NOT NULL, at INTEGER NOT NULL,
  PRIMARY KEY (session_id, fingerprint_id)
) WITHOUT ROWID;
CREATE TABLE notice_state (
  fingerprint_id INTEGER PRIMARY KEY, last_notified INTEGER NOT NULL,
  count_since INTEGER NOT NULL DEFAULT 0
);
CREATE VIRTUAL TABLE entries_fts USING fts5(
  text, content='', contentless_delete=1, tokenize='porter unicode61'
);
"#;

const AT: i64 = 1_700_000_000_000_000;

/// A version 1 database with one failed and one clean run, written by hand with what such a
/// file holds: no `user_version`, and only the meta stamps.
fn write_v1(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; PRAGMA journal_mode = WAL;").unwrap();
    conn.execute_batch(V1_SCHEMA).unwrap();
    conn.execute_batch(&format!(
        "INSERT INTO meta VALUES ('schema_version', '1'), ('derived_version', '{DERIVED_VERSION}');
         INSERT INTO runs (id, project_id, command, cwd, started_at, ended_at, exit_code, git_sha,
                           n_lines, bytes_total)
         VALUES (1, 7, '[\"mix\"]', '/w', {AT}, {AT} + 100, 1, 'abc', 2, 24),
                (2, 7, '[\"mix\"]', '/w', {AT} + 200, {AT} + 300, 0, 'def', 1, 5);
         INSERT INTO chunks VALUES (1, 0, {AT}, 0, 2, CAST('[error] boom' || char(10) || 'plain' || char(10) AS BLOB)),
                                   (2, 0, {AT} + 200, 0, 1, CAST('fine' || char(10) AS BLOB));
         INSERT INTO fingerprints VALUES (1, '[error] boom', 'h1', 'error', {AT}, {AT}, 1, 1),
                                         (2, 'plain', 'h2', 'unknown', {AT}, {AT} + 200, 2, 2);
         INSERT INTO entries (id, run_id, start_line, end_line, captured_at, level, evidence,
                              is_error, fingerprint_id)
         VALUES (1, 1, 0, 0, {AT}, 'error', 'declared', 1, 1),
                (2, 1, 1, 1, {AT}, 'unknown', 'unknown', 0, 2),
                (3, 2, 0, 0, {AT} + 200, 'unknown', 'unknown', 0, 2);
         INSERT INTO entries_fts (rowid, text) VALUES (1, '[error] boom'), (2, 'plain'), (3, 'fine');
         INSERT INTO run_fingerprints VALUES (1, 1, 1, {AT}, {AT}), (1, 2, 1, {AT}, {AT}),
                                             (2, 2, 1, {AT} + 200, {AT} + 200);
         INSERT INTO run_counters (run_id, n_entries, n_error, n_unknown)
         VALUES (1, 2, 1, 1), (2, 1, 0, 1);
         INSERT INTO session_cursors VALUES ('s', 'claude_code', 2, {AT}, 1);
         INSERT INTO session_blocks VALUES ('s', 1, {AT});"
    ))
    .unwrap();
}

/// Columns and indexes, which is what makes two databases the same shape. The DDL text
/// differs legitimately: a column added by `ALTER` sits at the end of its table's.
fn shape(conn: &Connection) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let names: Vec<(String, String)> = conn
        .prepare(
            "SELECT type, name FROM sqlite_master
              WHERE type IN ('table', 'index', 'view') AND name NOT LIKE 'sqlite_%'",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    for (kind, name) in names {
        out.insert(format!("{kind} {name}"));
        if kind == "table" {
            let mut stmt = conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{name}')"))
                .unwrap();
            let columns = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            for column in columns {
                out.insert(format!("{name}.{}", column.unwrap()));
            }
        }
    }
    out
}

fn raw(db: &Db) -> Connection {
    Connection::open(db.path()).unwrap()
}

fn version(path: &Path) -> i64 {
    user_version(&Connection::open(path).unwrap()).unwrap()
}

#[test]
fn the_steps_are_in_order_and_end_at_the_current_version() {
    let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
    assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");
    assert_eq!(versions.last().copied(), Some(SCHEMA_VERSION));
}

#[test]
fn a_new_database_starts_at_the_current_version_with_nothing_to_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("p.db");
    let db = Db::open(&path, 1).unwrap();
    assert_eq!(version(&path), SCHEMA_VERSION);
    assert!(!db.derived_stale());
}

#[test]
fn a_populated_v1_database_comes_up_to_current_with_everything_in_it() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("v1.db");
    write_v1(&path);
    assert_eq!(version(&path), 0, "setup: an old file has no user_version");

    let db = Db::open(&path, 7).unwrap();
    assert_eq!(version(&path), SCHEMA_VERSION);
    assert!(!db.derived_stale(), "a healthy old database needs no rebuild");

    let failed = read::get_run(&db, 1).unwrap();
    assert_eq!(failed.exit_code, Some(1));
    assert_eq!(failed.git_sha.as_deref(), Some("abc"));
    assert_eq!(failed.counters.n_error, 1);
    assert_eq!(failed.squashed_at, None, "the column added since is there");

    let all = EntryFilter { limit: 10, ascending: true, ..Default::default() };
    let entries = read::entries(&db, &all).unwrap();
    assert_eq!(
        entries.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
        ["[error] boom", "plain", "fine"]
    );
    let hits = read::entries(
        &db,
        &EntryFilter { search: Some("boom".into()), ..Default::default() },
    )
    .unwrap();
    assert_eq!(hits.len(), 1, "the search index came through");
    assert_eq!(read::errors_since(&db, 0).unwrap(), 1);
    assert_eq!(read::signatures_for_run(&db, 1).unwrap().len(), 2);
    assert_eq!(read::fingerprints(&db, 10, true).unwrap()[0].hash, "h1");
    assert_eq!(
        read::resolve_run(&db, &RunRef::LastGood).unwrap().unwrap().id,
        2,
        "the failed run is still the failed run"
    );

    // Session state: a cursor keeps its place, with the columns added since defaulted.
    let unread = read::unread(&db, "s", "claude_code", 3, 2, 10).unwrap();
    assert_eq!(unread.blocks_issued, 1);

    // And the result is the shape a new database has, or later steps would not hold.
    let fresh = tmp.path().join("fresh.db");
    drop(Db::open(&fresh, 7).unwrap());
    assert_eq!(shape(&raw(&db)), shape(&Connection::open(&fresh).unwrap()));

    // Captures carry on where it left off.
    drop(db);
    let db = Db::open(&path, 7).unwrap();
    let index = IndexDb::open(&tmp.path().join("index.db")).unwrap();
    let mut w = Writer::start(&db, &index, &RunStamp::default(), Arc::new(Redactor::disabled())).unwrap();
    assert_eq!(w.run_id(), 3);
    w.push(b"[error] boom\n", crate::now_micros()).unwrap();
    w.finish(&index, Some(1), None).unwrap();
    assert_eq!(read::fingerprints(&db, 10, true).unwrap()[0].total_count, 1, "hash differs, new row");
}

#[test]
fn opening_twice_migrates_once() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("v1.db");
    write_v1(&path);
    drop(Db::open(&path, 7).unwrap());
    let db = Db::open(&path, 7).unwrap();
    assert_eq!(read::get_run(&db, 1).unwrap().counters.n_error, 1);
    assert_eq!(version(&path), SCHEMA_VERSION);
}

#[test]
fn a_step_that_fails_leaves_the_database_as_it_was() {
    fn half_then_fail(conn: &Connection) -> Result<()> {
        conn.execute_batch("DROP TABLE run_counters; CREATE TABLE half_done(x);")?;
        Err(Error::other("died part way"))
    }
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("v1.db");
    write_v1(&path);

    let mut conn = Connection::open(&path).unwrap();
    let step = Migration { version: 2, name: "fails", up: half_then_fail };
    assert!(apply(&mut conn, &step).is_err());

    assert_eq!(user_version(&conn).unwrap(), 0);
    assert!(table_exists(&conn, "run_counters").unwrap());
    assert!(!table_exists(&conn, "half_done").unwrap());
}

#[test]
fn a_database_from_a_newer_build_is_refused_and_not_touched() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("p.db");
    drop(Db::open(&path, 1).unwrap());
    {
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    }
    let before = std::fs::read(&path).unwrap();

    let err = Db::open(&path, 1).err().expect("a newer database must be refused");
    assert!(matches!(err, Error::DatabaseTooNew { .. }), "{err}");
    assert!(!err.is_corrupt(), "and must never be mistaken for a damaged one");
    assert!(err.to_string().contains("Upgrade logbear"), "unhelpful: {err}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "not one byte may change");
}

#[test]
fn a_newer_derived_layer_is_refused_rather_than_dropped_and_stamped_down() {
    // How this used to lose data: any version that was not exactly this build's, older or
    // newer, dropped the derived tables.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("v1.db");
    write_v1(&path);
    Connection::open(&path)
        .unwrap()
        .execute("UPDATE meta SET value = ?1 WHERE key = 'derived_version'", [DERIVED_VERSION + 1])
        .unwrap();

    assert!(matches!(Db::open(&path, 7), Err(Error::DatabaseTooNew { .. })));

    let conn = Connection::open(&path).unwrap();
    let n: i64 = conn.query_row("SELECT count(*) FROM entries", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 3);
    assert_eq!(user_version(&conn).unwrap(), 0);
}

#[test]
fn entries_left_empty_by_an_old_crash_are_noticed_and_rebuilt() {
    // A crash after the old open dropped the derived tables and stamped the new version, and
    // before anything rebuilt them. Every later open then saw a current stamp.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("v1.db");
    write_v1(&path);
    Connection::open(&path)
        .unwrap()
        .execute_batch("DELETE FROM entries; DELETE FROM entries_fts WHERE rowid IN (1, 2, 3);")
        .unwrap();

    let db = Db::open(&path, 7).unwrap();
    assert!(db.derived_stale());
    assert_eq!(retention::rebuild_derived(&db).unwrap(), 3);
    assert_eq!(read::errors_since(&db, 0).unwrap(), 1);
    assert!(!Db::open(&path, 7).unwrap().derived_stale(), "and it settles");
}

fn stale(path: &Path) {
    let conn = Connection::open(path).unwrap();
    crate::store::set_meta(&conn, "derived_version", "1").unwrap();
}

fn entry_count(db: &Db) -> i64 {
    db.read(|c| Ok(c.query_row("SELECT count(*) FROM entries", [], |r| r.get(0))?)).unwrap()
}

#[test]
fn a_crash_before_the_rebuild_is_picked_up_by_the_next_open() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("v1.db");
    write_v1(&path);
    drop(Db::open(&path, 7).unwrap());
    stale(&path);

    // The open drops the derived layer and the process dies before the rebuild runs.
    let db = Db::open(&path, 7).unwrap();
    assert!(db.derived_stale());
    assert_eq!(entry_count(&db), 0);
    drop(db);

    let db = Db::open(&path, 7).unwrap();
    assert!(db.derived_stale(), "the outstanding rebuild must survive the restart");
    assert_eq!(retention::rebuild_derived(&db).unwrap(), 3);
    assert_eq!(entry_count(&db), 3);
    assert!(!Db::open(&path, 7).unwrap().derived_stale());
}

#[test]
fn a_crash_part_way_through_a_rebuild_resumes_after_the_last_finished_run() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("v1.db");
    write_v1(&path);
    drop(Db::open(&path, 7).unwrap());
    stale(&path);

    let db = Db::open(&path, 7).unwrap();
    // What a crash after the first of the two runs leaves.
    retention::reparse_run(&db, 1).unwrap();
    db.with(|c| crate::store::set_meta(c, REBUILD_AFTER, "1")).unwrap();
    drop(db);

    let db = Db::open(&path, 7).unwrap();
    assert!(db.derived_stale());
    assert_eq!(retention::rebuild_derived(&db).unwrap(), 1, "only the run still to do");
    assert_eq!(entry_count(&db), 3);
    let fps = read::fingerprints(&db, 10, false).unwrap();
    assert!(fps.iter().all(|f| f.total_count > 0 && f.run_count > 0), "{fps:?}");
}

#[test]
fn a_run_started_during_a_rebuild_is_left_to_its_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("v1.db");
    write_v1(&path);
    drop(Db::open(&path, 7).unwrap());
    stale(&path);

    let db = Db::open(&path, 7).unwrap();
    let index = IndexDb::open(&tmp.path().join("index.db")).unwrap();
    let mut w = Writer::start(&db, &index, &RunStamp::default(), Arc::new(Redactor::disabled())).unwrap();
    w.push(b"[error] during the rebuild\n", crate::now_micros()).unwrap();
    w.finish(&index, Some(1), None).unwrap();

    assert_eq!(retention::rebuild_derived(&db).unwrap(), 3, "the rebuild covers only old runs");
    assert_eq!(entry_count(&db), 4);
    let unread = read::unread(&db, "s", "claude_code", 3, 2, 10).unwrap();
    assert!(
        unread.entries.iter().any(|e| e.text.contains("during the rebuild")),
        "parking the cursor must not skip an error written alongside the rebuild"
    );
}

/// Squashes the failed run of a v1 database and evicts the clean one.
fn squashed_and_evicted(path: &Path) -> Db {
    write_v1(path);
    let db = Db::open(path, 7).unwrap();
    retention::squash_run(&db, 1).unwrap();
    retention::evict_run(&db, 2).unwrap();
    db
}

#[test]
fn a_rebuild_keeps_the_counters_of_squashed_and_evicted_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("v1.db");
    let db = squashed_and_evicted(&path);
    // More errors than survive in its bytes, as a real squashed run would have.
    db.with(|c| {
        c.execute("UPDATE run_counters SET n_entries = 500, n_error = 400 WHERE run_id = 1", [])?;
        Ok(())
    })
    .unwrap();
    drop(db);
    stale(&path);

    let db = Db::open(&path, 7).unwrap();
    retention::rebuild_derived(&db).unwrap();

    let failed = read::get_run(&db, 1).unwrap();
    assert_eq!(failed.counters.n_error, 400, "counts describe the run as captured");
    assert_eq!(failed.counters.n_entries, 500);
    assert!(!failed.is_clean());
    assert_eq!(read::get_run(&db, 2).unwrap().counters.n_entries, 1);
}

#[test]
fn a_rebuild_keeps_the_signatures_of_evicted_runs() {
    // "Kept effectively forever", until a rebuild dropped the tables they live in.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("v1.db");
    let db = squashed_and_evicted(&path);
    let before = read::signatures_for_run(&db, 2).unwrap();
    assert_eq!(before.len(), 1);
    drop(db);
    stale(&path);

    let db = Db::open(&path, 7).unwrap();
    retention::rebuild_derived(&db).unwrap();
    assert_eq!(read::signatures_for_run(&db, 2).unwrap(), before);
}

#[test]
fn a_failed_squashed_run_never_becomes_the_last_good_one() {
    // An older rebuild dropped `run_counters`, and squashed runs were skipped when it was
    // rebuilt, so a run that logged errors and exited 0 came back spotless.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("v1.db");
    write_v1(&path);
    {
        let db = Db::open(&path, 7).unwrap();
        retention::squash_run(&db, 1).unwrap();
        retention::evict_run(&db, 2).unwrap();
    }
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "UPDATE runs SET exit_code = 0 WHERE id = 1;
             DELETE FROM run_counters;
             PRAGMA user_version = 0;
             UPDATE meta SET value = '1' WHERE key = 'schema_version';",
        )
        .unwrap();
    }

    let db = Db::open(&path, 7).unwrap();
    assert!(db.derived_stale(), "a squashed run with no counters needs repair");
    retention::rebuild_derived(&db).unwrap();

    assert_eq!(read::get_run(&db, 1).unwrap().counters.n_error, 1, "restored from what survives");
    assert_eq!(
        read::resolve_run(&db, &RunRef::LastGood).unwrap().map(|r| r.id),
        None,
        "and the evicted run with no counters left cannot vouch for itself either"
    );
}
