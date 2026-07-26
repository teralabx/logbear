//! Ingest. One [`Writer`] per live run.
//!
//! The writer is the only thing that mutates a project database. Everything it does is
//! batched into transactions flushed every 100 entries or 100 ms, whichever comes first —
//! a dev server that logs a hundred lines a second must not mean a hundred fsyncs, and a
//! server that logs one line a minute must not mean that line is invisible for a minute.

use crate::error::Result;
use crate::model::*;
use crate::parse::{ParsedEntry, Parser, RawLine};
use crate::model::RunStamp;
use crate::redact::Redactor;
use crate::store::{index::IndexDb, pack_chunk, Db};
use rusqlite::Connection;
use std::sync::Arc;

/// Flush thresholds. Entry count first, then time, then size — whichever trips.
pub const FLUSH_ENTRIES: usize = 100;
pub const FLUSH_MICROS: i64 = 100_000;
pub const FLUSH_BYTES: usize = 64 * 1024;

pub struct Writer {
    db: Db,
    run_id: RunId,
    parser: Parser,
    /// Lines captured but not yet written as a chunk.
    buffered_lines: Vec<RawLine>,
    buffered_bytes: usize,
    /// Entries grouped but not yet written.
    buffered_entries: Vec<ParsedEntry>,
    next_seq: i64,
    last_flush: i64,
    counters: Counters,
    total_lines: i64,
    total_bytes: i64,
    mode: DelimiterMode,
    mode_written: bool,
}

/// What a flush produced, so the daemon can fan it out to subscribers and decide on
/// notifications without re-reading the database.
#[derive(Debug, Default)]
pub struct Flushed {
    pub entries: Vec<EntryView>,
    pub counters: Counters,
}

impl Writer {
    /// Register a run and open a writer for it.
    pub fn start(
        db: &Db,
        index: &IndexDb,
        stamp: &RunStamp,
        redactor: Arc<Redactor>,
    ) -> Result<Writer> {
        let now = crate::now_micros();
        let project_id = db.project_id();
        let command = serde_json::to_string(&stamp.command)?;
        let env_keys = serde_json::to_string(&stamp.env_keys)?;
        let lockfiles = serde_json::to_string(&stamp.lockfile_hashes)?;

        let run_id = db.with(|conn| {
            let session_group = session_group_for(conn, &command)?;
            conn.execute(
                "INSERT INTO runs (project_id, command, cwd, session_group, started_at,
                                   git_sha, git_branch, git_dirty_hash, lockfile_hashes,
                                   env_keys, term_cols, delimiter_mode)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'fallback')",
                rusqlite::params![
                    project_id,
                    command,
                    stamp.cwd,
                    session_group,
                    now,
                    stamp.git_sha,
                    stamp.git_branch,
                    stamp.git_dirty_hash,
                    lockfiles,
                    env_keys,
                    stamp.term_cols as i64,
                ],
            )?;
            let id = conn.last_insert_rowid();
            // A session is a chain of identical commands, so the first run of a chain
            // names it. Doing this after the insert avoids reserving ids up front.
            if session_group.is_none() {
                conn.execute("UPDATE runs SET session_group = ?1 WHERE id = ?1", [id])?;
            }
            Ok(id)
        })?;

        index.record_run_start(project_id, run_id, &command, now)?;

        Ok(Writer {
            db: db.clone(),
            run_id,
            parser: Parser::new().with_redactor(redactor),
            buffered_lines: Vec::new(),
            buffered_bytes: 0,
            buffered_entries: Vec::new(),
            next_seq: 0,
            last_flush: now,
            counters: Counters::default(),
            total_lines: 0,
            total_bytes: 0,
            mode: DelimiterMode::Fallback,
            mode_written: false,
        })
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    pub fn mode(&self) -> DelimiterMode {
        self.mode
    }

    /// Feed captured bytes. Returns anything a flush produced.
    pub fn push(&mut self, bytes: &[u8], captured_at: i64) -> Result<Option<Flushed>> {
        let out = self.parser.push(bytes, captured_at);
        self.absorb(out);
        if self.should_flush(captured_at) {
            return self.flush().map(Some);
        }
        Ok(None)
    }

    /// Close an entry that has gone quiet, so the live view is not held hostage by a
    /// server that stopped talking mid-stack-trace.
    pub fn tick(&mut self, now: i64) -> Result<Option<Flushed>> {
        let out = self.parser.tick(now);
        let had = !out.entries.is_empty() || !out.lines.is_empty();
        self.absorb(out);
        if had || self.should_flush(now) {
            return self.flush().map(Some);
        }
        Ok(None)
    }

    /// End of run. Flushes everything, stamps the outcome, and folds run-level failure
    /// signals into the record so a silent crash still reads as a failure.
    pub fn finish(
        &mut self,
        index: &IndexDb,
        exit_code: Option<i32>,
        signal: Option<&str>,
    ) -> Result<Flushed> {
        let now = crate::now_micros();
        let out = self.parser.finish(now);
        self.absorb(out);
        let flushed = self.flush()?;

        let run_id = self.run_id;
        let total_lines = self.total_lines;
        let total_bytes = self.total_bytes;
        let mode = self.mode.as_str();
        self.db.with(|conn| {
            conn.execute(
                "UPDATE runs SET ended_at = ?2, exit_code = ?3, signal = ?4,
                                 n_lines = ?5, bytes_total = ?6, delimiter_mode = ?7
                  WHERE id = ?1",
                rusqlite::params![run_id, now, exit_code, signal, total_lines, total_bytes, mode],
            )?;
            Ok(())
        })?;

        index.record_run_end(
            self.db.project_id(),
            run_id,
            now,
            exit_code,
            self.counters.n_error,
        )?;
        Ok(flushed)
    }

    fn absorb(&mut self, mut out: crate::parse::ParseOutput) {
        if let Some(mode) = out.mode_settled {
            self.mode = mode;
        }
        // Lines arrive already redacted: the parser masks them before rendering, so the
        // secret is absent from the entry text and the fingerprint template as well as
        // from the bytes about to be written.
        for line in out.lines.iter() {
            self.buffered_bytes += line.bytes.len() + 1;
            self.total_bytes += line.bytes.len() as i64 + 1;
            self.total_lines = self.total_lines.max(line.index + 1);
        }
        self.buffered_lines.append(&mut out.lines);
        self.buffered_entries.append(&mut out.entries);
    }

    fn should_flush(&self, now: i64) -> bool {
        if self.buffered_lines.is_empty() && self.buffered_entries.is_empty() {
            return false;
        }
        self.buffered_entries.len() >= FLUSH_ENTRIES
            || self.buffered_bytes >= FLUSH_BYTES
            || now - self.last_flush >= FLUSH_MICROS
    }

    /// Write everything buffered in a single transaction.
    pub fn flush(&mut self) -> Result<Flushed> {
        if self.buffered_lines.is_empty() && self.buffered_entries.is_empty() {
            return Ok(Flushed {
                counters: self.counters,
                ..Default::default()
            });
        }

        let run_id = self.run_id;
        let lines = std::mem::take(&mut self.buffered_lines);
        let entries = std::mem::take(&mut self.buffered_entries);
        let seq = self.next_seq;
        let mode = self.mode;
        let write_mode = !self.mode_written && !lines.is_empty();
        self.buffered_bytes = 0;
        self.last_flush = crate::now_micros();

        let mut counters = self.counters;
        let views = self.db.with_mut(|conn| {
            let tx = conn.transaction()?;

            if !lines.is_empty() {
                tx.execute(
                    "INSERT INTO chunks (run_id, seq, captured_at, first_line, n_lines, bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        run_id,
                        seq,
                        lines[0].captured_at,
                        lines[0].index,
                        lines.len() as i64,
                        pack_chunk(&lines),
                    ],
                )?;
            }

            if write_mode {
                tx.execute(
                    "UPDATE runs SET delimiter_mode = ?2 WHERE id = ?1",
                    rusqlite::params![run_id, mode.as_str()],
                )?;
            }

            let mut views = Vec::with_capacity(entries.len());
            for e in &entries {
                let (fp_id, is_new) = upsert_fingerprint(&tx, e)?;
                tx.execute(
                    "INSERT INTO entries (run_id, start_line, end_line, captured_at, level,
                                          is_error, error_reason, fingerprint_id, boundaries)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        run_id,
                        e.start_line,
                        e.end_line,
                        e.captured_at,
                        e.level.as_str(),
                        e.is_error as i64,
                        e.error_reason,
                        fp_id,
                        e.boundaries.as_str(),
                    ],
                )?;
                let entry_id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO entries_fts (rowid, text) VALUES (?1, ?2)",
                    rusqlite::params![entry_id, e.text],
                )?;
                bump_run_fingerprint(&tx, run_id, fp_id, e.captured_at)?;
                counters.bump(e.level);

                views.push(EntryView {
                    entry: Entry {
                        id: entry_id,
                        run_id,
                        start_line: e.start_line,
                        end_line: e.end_line,
                        captured_at: e.captured_at,
                        level: e.level,
                        is_error: e.is_error,
                        error_reason: e.error_reason.clone(),
                        fingerprint_id: Some(fp_id),
                        boundaries: e.boundaries,
                    },
                    text: e.text.clone(),
                    template: Some(e.template.clone()),
                    is_new_signature: is_new,
                });
            }

            tx.execute(
                "INSERT INTO run_counters (run_id, n_entries, n_error, n_warn, n_info,
                                           n_debug, n_trace, n_unknown)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(run_id) DO UPDATE SET
                   n_entries = excluded.n_entries, n_error = excluded.n_error,
                   n_warn = excluded.n_warn, n_info = excluded.n_info,
                   n_debug = excluded.n_debug, n_trace = excluded.n_trace,
                   n_unknown = excluded.n_unknown",
                rusqlite::params![
                    run_id,
                    counters.n_entries,
                    counters.n_error,
                    counters.n_warn,
                    counters.n_info,
                    counters.n_debug,
                    counters.n_trace,
                    counters.n_unknown,
                ],
            )?;

            tx.commit()?;
            Ok(views)
        })?;

        if !lines.is_empty() {
            self.next_seq += 1;
        }
        if write_mode {
            self.mode_written = true;
        }
        self.counters = counters;

        Ok(Flushed {
            entries: views,
            counters,
        })
    }
}

/// Insert or update a fingerprint. Returns its id and whether this is the first time the
/// project has ever seen it — the "(1 new)" that is the most valuable number in the UI.
fn upsert_fingerprint(conn: &Connection, e: &ParsedEntry) -> Result<(FingerprintId, bool)> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM fingerprints WHERE hash = ?1",
            [&e.hash],
            |r| r.get(0),
        )
        .ok();

    match existing {
        Some(id) => {
            conn.execute(
                "UPDATE fingerprints
                    SET last_seen = max(last_seen, ?2), total_count = total_count + 1
                  WHERE id = ?1",
                rusqlite::params![id, e.captured_at],
            )?;
            Ok((id, false))
        }
        None => {
            conn.execute(
                "INSERT INTO fingerprints (template, hash, level, first_seen, last_seen,
                                           total_count, run_count)
                 VALUES (?1, ?2, ?3, ?4, ?4, 1, 0)",
                rusqlite::params![e.template, e.hash, e.level.as_str(), e.captured_at],
            )?;
            Ok((conn.last_insert_rowid(), true))
        }
    }
}

fn bump_run_fingerprint(
    conn: &Connection,
    run_id: RunId,
    fp_id: FingerprintId,
    at: i64,
) -> Result<()> {
    let changed = conn.execute(
        "UPDATE run_fingerprints SET count = count + 1, last_at = max(last_at, ?3)
          WHERE run_id = ?1 AND fingerprint_id = ?2",
        rusqlite::params![run_id, fp_id, at],
    )?;
    if changed == 0 {
        conn.execute(
            "INSERT INTO run_fingerprints (run_id, fingerprint_id, count, first_at, last_at)
             VALUES (?1, ?2, 1, ?3, ?3)",
            rusqlite::params![run_id, fp_id, at],
        )?;
        // run_count tracks distinct runs, so it moves only when the pair is new.
        conn.execute(
            "UPDATE fingerprints SET run_count = run_count + 1 WHERE id = ?1",
            [fp_id],
        )?;
    }
    Ok(())
}

/// A Phoenix server restarted forty times in a day is forty runs and one session. Without
/// this, "recent errors" degenerates into "errors since the last Ctrl-C ninety seconds
/// ago" and the run list drowns.
fn session_group_for(conn: &Connection, command: &str) -> Result<Option<i64>> {
    let last: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT command, session_group FROM runs ORDER BY started_at DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    Ok(match last {
        Some((cmd, group)) if cmd == command => group,
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::read;

    struct Harness {
        _tmp: tempfile::TempDir,
        db: Db,
        index: IndexDb,
    }

    fn harness() -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let index = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let project = index
            .register(&tmp.path().join("proj"), "proj", &tmp.path().join("p.db"))
            .unwrap();
        let db = Db::open(&tmp.path().join("p.db"), project.id).unwrap();
        Harness { _tmp: tmp, db, index }
    }

    fn stamp() -> RunStamp {
        RunStamp {
            command: vec!["mix".into(), "phx.server".into()],
            cwd: "/tmp/proj".into(),
            project_root: "/tmp/proj".into(),
            project_name: "proj".into(),
            term_cols: 120,
            ..Default::default()
        }
    }

    #[test]
    fn a_run_captures_lines_entries_and_counters() {
        let h = harness();
        let mut w = Writer::start(&h.db, &h.index, &stamp(), Arc::new(Redactor::disabled())).unwrap();
        w.push(b"GET /users\nError: boom\n    at a (/app/x.js:1:1)\n", 1).unwrap();
        w.finish(&h.index, Some(0), None).unwrap();

        let run = read::get_run(&h.db, w.run_id()).unwrap();
        assert_eq!(run.counters.n_entries, 2);
        assert_eq!(run.counters.n_error, 1);

        let entries = read::entries(&h.db, &Default::default()).unwrap();
        assert_eq!(entries.len(), 2);
        // Rendered text comes back through the chunks, so the stack frame is attached.
        let err = entries.iter().find(|e| e.entry.is_error).unwrap();
        assert!(err.text.contains("at a (/app/x.js:1:1)"), "{}", err.text);
    }

    #[test]
    fn the_first_sighting_of_a_signature_is_flagged_new() {
        let h = harness();
        let mut w = Writer::start(&h.db, &h.index, &stamp(), Arc::new(Redactor::disabled())).unwrap();
        w.push(b"Error: connection refused\n", 1).unwrap();
        // A single line has not settled the delimiter mode yet; the idle tick is what
        // makes a quiet run's first entry visible, and it is what the daemon calls.
        w.tick(crate::now_micros()).unwrap();
        let all = read::entries(&h.db, &Default::default()).unwrap();
        assert!(all[0].is_new_signature);

        // Same error again, in a second run: known, not new.
        w.finish(&h.index, Some(1), None).unwrap();
        let mut w2 = Writer::start(&h.db, &h.index, &stamp(), Arc::new(Redactor::disabled())).unwrap();
        w2.push(b"Error: connection refused\n", 2).unwrap();
        w2.finish(&h.index, Some(1), None).unwrap();
        let latest = read::entries(
            &h.db,
            &crate::proto::EntryFilter {
                run_id: Some(w2.run_id()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!latest[0].is_new_signature);
    }

    #[test]
    fn repeated_runs_of_one_command_share_a_session() {
        let h = harness();
        let mut a = Writer::start(&h.db, &h.index, &stamp(), Arc::new(Redactor::disabled())).unwrap();
        a.finish(&h.index, Some(0), None).unwrap();
        let mut b = Writer::start(&h.db, &h.index, &stamp(), Arc::new(Redactor::disabled())).unwrap();
        b.finish(&h.index, Some(0), None).unwrap();

        let ra = read::get_run(&h.db, a.run_id()).unwrap();
        let rb = read::get_run(&h.db, b.run_id()).unwrap();
        assert_eq!(ra.session_group, rb.session_group);
        assert_eq!(ra.session_group, Some(a.run_id()));

        // A different command starts a new session.
        let mut other = stamp();
        other.command = vec!["npm".into(), "test".into()];
        let mut c = Writer::start(&h.db, &h.index, &other, Arc::new(Redactor::disabled())).unwrap();
        c.finish(&h.index, Some(0), None).unwrap();
        let rc = read::get_run(&h.db, c.run_id()).unwrap();
        assert_ne!(rc.session_group, ra.session_group);
    }

    #[test]
    fn secrets_never_reach_the_chunks() {
        let h = harness();
        let redactor = Arc::new(Redactor::new(true, &[]));
        let mut w = Writer::start(&h.db, &h.index, &stamp(), redactor).unwrap();
        w.push(b"connecting to postgres://admin:hunter2@db:5432/app\n", 1).unwrap();
        w.finish(&h.index, Some(0), None).unwrap();

        let raw = h
            .db
            .with(|c| crate::store::raw_range(c, w.run_id(), 0, 100))
            .unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(!text.contains("hunter2"), "secret stored verbatim: {text}");
        assert!(text.contains('•'), "nothing was masked: {text}");
    }

    #[test]
    fn a_run_with_no_output_still_records_cleanly() {
        let h = harness();
        let mut w = Writer::start(&h.db, &h.index, &stamp(), Arc::new(Redactor::disabled())).unwrap();
        w.finish(&h.index, Some(0), None).unwrap();
        let run = read::get_run(&h.db, w.run_id()).unwrap();
        assert!(run.is_clean());
        assert_eq!(run.counters.n_entries, 0);
    }

    #[test]
    fn counters_survive_many_flushes() {
        let h = harness();
        let mut w = Writer::start(&h.db, &h.index, &stamp(), Arc::new(Redactor::disabled())).unwrap();
        for i in 0..250 {
            w.push(format!("line {i}\n").as_bytes(), i as i64).unwrap();
        }
        w.finish(&h.index, Some(0), None).unwrap();
        let run = read::get_run(&h.db, w.run_id()).unwrap();
        assert_eq!(run.counters.n_entries, 250);
    }
}

/// Close runs that were live when the daemon last stopped.
///
/// If the daemon is starting up, nothing can still be running: it owns every writer in
/// memory, so a run with no `ended_at` is a run whose daemon was killed, upgraded, or lost
/// to a power cut. Left alone it shows as "running" on the home screen forever, which is
/// worse than a wrong exit code because it is a claim about *now*.
///
/// The end time is the last byte actually captured, not the current clock. A run that died
/// at 20:28 and is reconciled at 23:34 lasted four minutes, not three hours, and recording
/// the latter would poison every duration statistic that touches it.
pub fn close_orphaned_runs(db: &Db) -> Result<usize> {
    let orphans: Vec<(RunId, i64)> = db.with(|conn| {
        let mut stmt = conn.prepare(
            "SELECT r.id, COALESCE(
                      (SELECT max(c.captured_at) FROM chunks c WHERE c.run_id = r.id),
                      r.started_at)
               FROM runs r WHERE r.ended_at IS NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })?;

    if orphans.is_empty() {
        return Ok(0);
    }
    db.with_mut(|conn| {
        let tx = conn.transaction()?;
        for (run_id, ended_at) in &orphans {
            // No exit code and no signal is exactly how an unexpected death is recorded
            // elsewhere, so `run_failed` already classifies these correctly.
            tx.execute(
                "UPDATE runs SET ended_at = ?2 WHERE id = ?1 AND ended_at IS NULL",
                rusqlite::params![run_id, ended_at],
            )?;
        }
        tx.commit()?;
        Ok(())
    })?;
    Ok(orphans.len())
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::store::{index::IndexDb, read};

    #[test]
    fn a_run_abandoned_by_a_dead_daemon_is_closed_on_the_next_start() {
        let tmp = tempfile::tempdir().unwrap();
        let index = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let p = index.register(&root, "proj", &tmp.path().join("p.db")).unwrap();
        let db = Db::open(&tmp.path().join("p.db"), p.id).unwrap();

        // A run that starts, produces output, and is never finished — the daemon died.
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
        let output_at = crate::now_micros();
        w.push(b"listening on port 4000\n", output_at).unwrap();
        w.flush().unwrap();
        drop(w);

        assert_eq!(read::active_runs(&db).unwrap().len(), 1, "setup failed");

        assert_eq!(close_orphaned_runs(&db).unwrap(), 1);
        assert!(
            read::active_runs(&db).unwrap().is_empty(),
            "the home screen would still claim this run is live"
        );

        let run = read::get_run(&db, 1).unwrap();
        assert!(!run.is_clean(), "an abandoned run did not succeed");
        assert_eq!(run.exit_code, None);
        // Ends when output stopped, not when we noticed.
        let ended = run.ended_at.unwrap();
        assert!(
            (ended - output_at).abs() < 1_000_000,
            "duration was stretched to the reconciliation time"
        );
    }

    #[test]
    fn reconciliation_leaves_finished_runs_alone_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let index = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let p = index.register(&root, "proj", &tmp.path().join("p.db")).unwrap();
        let db = Db::open(&tmp.path().join("p.db"), p.id).unwrap();

        let mut w = Writer::start(&db, &index, &RunStamp::default(), Arc::new(Redactor::disabled()))
            .unwrap();
        w.push(b"all fine\n", crate::now_micros()).unwrap();
        w.finish(&index, Some(0), None).unwrap();
        let before = read::get_run(&db, w.run_id()).unwrap();

        assert_eq!(close_orphaned_runs(&db).unwrap(), 0);
        assert_eq!(close_orphaned_runs(&db).unwrap(), 0);

        let after = read::get_run(&db, w.run_id()).unwrap();
        assert_eq!(before.ended_at, after.ended_at);
        assert_eq!(after.exit_code, Some(0));
    }
}
