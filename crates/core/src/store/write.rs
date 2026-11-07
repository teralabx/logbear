//! Ingest. One [`Writer`] per live run.
//!
//! The writer is the only thing that mutates a project database. Work is batched into
//! transactions flushed on whichever of the time and byte thresholds trips first. At a low
//! rate that is the clock, so an entry is visible almost at once; under load it is the byte
//! threshold, so batches grow with the rate and a firehose commits far less often.
//!
//! A flush that fails loses nothing: the batch stays buffered and is retried, so a full
//! disk costs capture at most what no longer fits in [`MAX_BUFFERED_BYTES`].

use crate::error::Result;
use crate::model::*;
use crate::parse::{ParsedEntry, Parser, RawLine};
use crate::model::RunStamp;
use crate::redact::Redactor;
use crate::store::{index::IndexDb, pack_chunk, read, Db};
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
use std::sync::Arc;

/// Flush thresholds; a flush happens when either trips.
pub const FLUSH_MICROS: i64 = 100_000;
pub const FLUSH_BYTES: usize = 1024 * 1024;

/// What a writer holds while storage is failing, past which the oldest output goes. One
/// flush's worth is the normal maximum, so this is only ever reached during an outage.
pub const MAX_BUFFERED_BYTES: usize = 32 * 1024 * 1024;

/// How long after a failed flush the next one is tried, doubling up to the maximum. A full
/// disk fails slowly, and the capture thread should not spend its time on that.
const RETRY_MIN_MICROS: i64 = 250_000;
const RETRY_MAX_MICROS: i64 = 5_000_000;

/// A run this large has churned the page cache, so its end is worth a release.
const RELEASE_AFTER_BYTES: i64 = 8 * 1024 * 1024;

/// How far a resumed run's start may sit from the moment its shim asked for it.
pub const RESUME_WINDOW_MICROS: i64 = 60_000_000;

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
    /// When a flush may next be tried; zero unless the last one failed.
    retry_at: i64,
    retry_delay: i64,
    /// What may be held while storage is failing.
    max_buffered: usize,
    /// Lines given up on during this outage, reported when it ends.
    lines_lost: i64,
    /// When the run ended, fixed by the first attempt so a retry does not move it.
    finished_at: Option<i64>,
}

/// What a flush produced, so subscribers can be updated without re-reading the database.
#[derive(Debug, Default)]
pub struct Flushed {
    pub entries: Vec<EntryView>,
    pub counters: Counters,
}

impl Writer {
    /// Registers a run and opens a writer for it.
    pub fn start(
        db: &Db,
        index: &IndexDb,
        stamp: &RunStamp,
        redactor: Arc<Redactor>,
    ) -> Result<Writer> {
        let now = crate::now_micros();
        let project_id = db.project_id();
        let command = serde_json::to_string(&redactor.redact_argv(&stamp.command))?;
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
            // The first run of a chain names the session; done after the insert so no id
            // has to be reserved up front.
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
            retry_at: 0,
            retry_delay: 0,
            max_buffered: MAX_BUFFERED_BYTES,
            lines_lost: 0,
            finished_at: None,
        })
    }

    /// Reopens a run that lost its live writer, so the rest of its output lands in it
    /// rather than in a new run.
    ///
    /// Returns `None` when the run cannot be continued: it is gone, its chunks were
    /// evicted, or it already ended with an outcome, in which case appending would put
    /// output after an exit the user has already seen.
    ///
    /// Run ids are rowids, which SQLite hands out again once the highest one is deleted, so
    /// the id alone does not prove this is the run the shim started. It must also have the
    /// same command and directory and have started within [`RESUME_WINDOW_MICROS`] of
    /// `shim_started_at`, the moment the shim asked for it.
    ///
    /// Line count and sequence are read from `chunks` rather than from the run row: a run
    /// closed by [`close_orphaned_runs`] never had its totals written. Counters come from
    /// `run_counters`, which every flush keeps current.
    pub fn resume(
        db: &Db,
        run_id: RunId,
        stamp: &RunStamp,
        shim_started_at: i64,
        redactor: Arc<Redactor>,
    ) -> Result<Option<Writer>> {
        let command = serde_json::to_string(&redactor.redact_argv(&stamp.command))?;
        let found = db.with(|conn| {
            let row: Option<(String, i64, Option<i32>, Option<String>)> = conn
                .query_row(
                    "SELECT delimiter_mode, chunks_evicted OR squashed_at IS NOT NULL, exit_code, signal
                       FROM runs
                      WHERE id = ?1 AND command = ?2 AND cwd = ?3
                        AND started_at BETWEEN ?4 - ?5 AND ?4 + ?5",
                    rusqlite::params![run_id, command, stamp.cwd, shim_started_at, RESUME_WINDOW_MICROS],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let Some((mode, evicted, exit_code, signal)) = row else {
                return Ok(None);
            };
            if evicted != 0 || exit_code.is_some() || signal.is_some() {
                return Ok(None);
            }
            let (total_lines, next_seq, total_bytes): (i64, i64, i64) = conn.query_row(
                "SELECT COALESCE(max(first_line + n_lines), 0), COALESCE(max(seq) + 1, 0),
                        COALESCE(sum(length(bytes)), 0)
                   FROM chunks WHERE run_id = ?1",
                [run_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let counters = conn
                .query_row(
                    "SELECT n_entries, n_error, n_warn, n_info, n_debug, n_trace, n_unknown
                       FROM run_counters WHERE run_id = ?1",
                    [run_id],
                    |r| {
                        Ok(Counters {
                            n_entries: r.get(0)?,
                            n_error: r.get(1)?,
                            n_warn: r.get(2)?,
                            n_info: r.get(3)?,
                            n_debug: r.get(4)?,
                            n_trace: r.get(5)?,
                            n_unknown: r.get(6)?,
                        })
                    },
                )
                .optional()?
                .unwrap_or_default();
            Ok(Some((DelimiterMode::parse(&mode), total_lines, next_seq, total_bytes, counters)))
        })?;

        let Some((mode, total_lines, next_seq, total_bytes, counters)) = found else {
            return Ok(None);
        };

        // Reopened, so the home screen shows it live again until the spool is replayed.
        db.with(|conn| {
            conn.execute("UPDATE runs SET ended_at = NULL WHERE id = ?1", [run_id])?;
            Ok(())
        })?;
        Ok(Some(Writer {
            db: db.clone(),
            run_id,
            parser: Parser::resume(total_lines, mode).with_redactor(redactor),
            buffered_lines: Vec::new(),
            buffered_bytes: 0,
            buffered_entries: Vec::new(),
            next_seq,
            last_flush: crate::now_micros(),
            counters,
            total_lines,
            total_bytes,
            mode,
            mode_written: total_lines > 0,
            retry_at: 0,
            retry_delay: 0,
            max_buffered: MAX_BUFFERED_BYTES,
            lines_lost: 0,
            finished_at: None,
        }))
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

    /// Feeds captured bytes, returning anything a flush produced.
    ///
    /// The clock here is the real one rather than `captured_at`: a spool replay feeds
    /// timestamps from the past, and the flush interval is about now.
    pub fn push(&mut self, bytes: &[u8], captured_at: i64) -> Result<Option<Flushed>> {
        let out = self.parser.push(bytes, captured_at);
        self.absorb(out);
        if self.should_flush(crate::now_micros()) {
            return self.flush().map(Some);
        }
        Ok(None)
    }

    /// Closes an entry that has gone quiet, so a producer that stops mid-entry does not
    /// stall the live view.
    pub fn tick(&mut self, now: i64) -> Result<Option<Flushed>> {
        let out = self.parser.tick(now);
        let had = !out.entries.is_empty() || !out.lines.is_empty();
        self.absorb(out);
        if (had && now >= self.retry_at) || self.should_flush(now) {
            return self.flush().map(Some);
        }
        Ok(None)
    }

    /// Ends the run: flushes everything and stamps the outcome on the run row.
    ///
    /// Safe to call again after a failure: nothing is lost, and the run keeps the end time
    /// of the first attempt.
    pub fn finish(
        &mut self,
        index: &IndexDb,
        exit_code: Option<i32>,
        signal: Option<&str>,
    ) -> Result<Flushed> {
        let now = *self.finished_at.get_or_insert_with(crate::now_micros);
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
        if self.total_bytes >= RELEASE_AFTER_BYTES {
            self.db.release_memory();
        }
        Ok(flushed)
    }

    fn absorb(&mut self, mut out: crate::parse::ParseOutput) {
        if let Some(mode) = out.mode_settled {
            self.mode = mode;
        }
        // Lines arrive already redacted: the parser masks them before rendering, so
        // secrets are absent from the entry text and template as well as from the bytes.
        for line in out.lines.iter() {
            self.buffered_bytes += line.bytes.len() + 1;
            self.total_bytes += line.bytes.len() as i64 + 1;
            self.total_lines = self.total_lines.max(line.index + 1);
        }
        self.buffered_lines.append(&mut out.lines);
        self.buffered_entries.append(&mut out.entries);
        if self.buffered_bytes > self.max_buffered {
            self.shed_oldest();
        }
    }

    /// Storage has been failing long enough that memory is the next thing to go, so the
    /// oldest half of what is held makes way for what is still arriving. The newest output
    /// is the output that explains whatever is about to happen.
    fn shed_oldest(&mut self) {
        let target = self.max_buffered / 2;
        let mut freed = 0;
        let cut = self
            .buffered_lines
            .iter()
            .position(|l| {
                freed += l.bytes.len() + 1;
                self.buffered_bytes - freed <= target
            })
            .map_or(self.buffered_lines.len(), |i| i + 1);
        let last_lost = self.buffered_lines[..cut].last().map_or(-1, |l| l.index);
        self.buffered_lines.drain(..cut);
        self.buffered_entries.retain(|e| e.end_line > last_lost);
        self.buffered_bytes -= freed;
        self.lines_lost += cut as i64;
    }

    fn should_flush(&self, now: i64) -> bool {
        if self.buffered_lines.is_empty() && self.buffered_entries.is_empty() {
            return false;
        }
        if now < self.retry_at {
            return false;
        }
        self.buffered_bytes >= FLUSH_BYTES || now - self.last_flush >= FLUSH_MICROS
    }

    /// Writes everything buffered in a single transaction.
    ///
    /// On failure the batch stays buffered, whole, for the next attempt: the parser has
    /// moved on, so dropping it would leave entries pointing at lines that were never
    /// stored.
    pub fn flush(&mut self) -> Result<Flushed> {
        if self.buffered_lines.is_empty() && self.buffered_entries.is_empty() {
            return Ok(Flushed {
                counters: self.counters,
                ..Default::default()
            });
        }

        let write_mode = !self.mode_written && !self.buffered_lines.is_empty();
        let now = crate::now_micros();
        self.last_flush = now;

        let mut counters = self.counters;
        let lines = &self.buffered_lines;
        let entries = &self.buffered_entries;
        let (run_id, seq, mode) = (self.run_id, self.next_seq, self.mode);
        let written = self.db.with_mut(|conn| {
            let tx = conn.transaction()?;
            let ids = write_batch(
                &tx,
                Batch {
                    run_id,
                    chunk: Some((seq, lines)),
                    mode: write_mode.then_some(mode),
                    entries,
                    counters: Some(&mut counters),
                    recount: true,
                },
            )?;
            tx.commit()?;
            Ok(ids)
        });
        let ids = match written {
            Ok(ids) => ids,
            Err(e) => {
                self.retry_delay = (self.retry_delay * 2).clamp(RETRY_MIN_MICROS, RETRY_MAX_MICROS);
                self.retry_at = now + self.retry_delay;
                return Err(e);
            }
        };

        if !self.buffered_lines.is_empty() {
            self.next_seq += 1;
        }
        if write_mode {
            self.mode_written = true;
        }
        let views = self
            .buffered_entries
            .drain(..)
            .zip(ids)
            .map(|(e, (id, fp_id, is_new))| EntryView {
                entry: Entry {
                    id,
                    run_id,
                    start_line: e.start_line,
                    end_line: e.end_line,
                    captured_at: e.captured_at,
                    level: e.level,
                    evidence: e.evidence,
                    is_error: e.is_error,
                    error_reason: e.error_reason,
                    fingerprint_id: Some(fp_id),
                    boundaries: e.boundaries,
                },
                text: e.text,
                template: Some(e.template),
                is_new_signature: is_new,
            })
            .collect();
        self.buffered_lines.clear();
        self.buffered_bytes = 0;
        self.counters = counters;
        self.retry_at = 0;
        self.retry_delay = 0;
        if self.lines_lost > 0 {
            eprintln!(
                "logbeard: run {run_id} lost {} line(s) while storage was failing",
                self.lines_lost
            );
            self.lines_lost = 0;
        }

        Ok(Flushed {
            entries: views,
            counters,
        })
    }
}

/// What one batch adds to one signature.
struct SignatureDelta {
    id: FingerprintId,
    count: i64,
    first_at: i64,
    last_at: i64,
}

/// One transaction's worth of derived rows.
pub(crate) struct Batch<'a> {
    pub run_id: RunId,
    /// The stored output, if this batch has any, and its sequence number.
    pub chunk: Option<(i64, &'a [RawLine])>,
    /// Written to the run row when set.
    pub mode: Option<DelimiterMode>,
    pub entries: &'a [ParsedEntry],
    /// The run's running totals, stored with the batch. `None` leaves stored totals alone.
    pub counters: Option<&'a mut Counters>,
    /// Whether these entries add to signature counts. A squashed run's are already recorded
    /// from before the squash, and adding the survivors would count them twice.
    pub recount: bool,
}

/// The statements of one flush, returning each entry's id, its signature's id and whether
/// that signature is new to the project.
///
/// Signatures are totalled across the batch and written once each, since most lines in a
/// batch repeat a template and the alternative is two updates per line.
pub(crate) fn write_batch(
    tx: &Connection,
    batch: Batch<'_>,
) -> Result<Vec<(EntryId, FingerprintId, bool)>> {
    let Batch {
        run_id,
        chunk,
        mode,
        entries,
        mut counters,
        recount,
    } = batch;

    if let Some((seq, lines)) = chunk.filter(|(_, lines)| !lines.is_empty()) {
        let first = &lines[0];
        tx.prepare_cached(
            "INSERT INTO chunks (run_id, seq, captured_at, first_line, n_lines, bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?
        .execute(rusqlite::params![
            run_id,
            seq,
            first.captured_at,
            first.index,
            lines.len() as i64,
            pack_chunk(lines),
        ])?;
    }

    if let Some(mode) = mode {
        tx.prepare_cached("UPDATE runs SET delimiter_mode = ?2 WHERE id = ?1")?
            .execute(rusqlite::params![run_id, mode.as_str()])?;
    }

    let mut find_fp = tx.prepare_cached("SELECT id FROM fingerprints WHERE hash = ?1")?;
    let mut new_fp = tx.prepare_cached(
        "INSERT INTO fingerprints (template, hash, level, first_seen, last_seen,
                                   total_count, run_count)
         VALUES (?1, ?2, ?3, ?4, ?4, 0, 0)",
    )?;
    let mut new_entry = tx.prepare_cached(
        "INSERT INTO entries (run_id, start_line, end_line, captured_at, level,
                              evidence, is_error, error_reason, fingerprint_id,
                              boundaries)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    let mut index_text =
        tx.prepare_cached("INSERT INTO entries_fts (rowid, text) VALUES (?1, ?2)")?;

    let mut signatures: HashMap<&str, SignatureDelta> = HashMap::new();
    let mut ids = Vec::with_capacity(entries.len());
    for e in entries {
        let (fp_id, is_new) = match signatures.get_mut(e.hash.as_str()) {
            Some(delta) => {
                delta.count += 1;
                delta.first_at = delta.first_at.min(e.captured_at);
                delta.last_at = delta.last_at.max(e.captured_at);
                (delta.id, false)
            }
            None => {
                let known: Option<i64> =
                    find_fp.query_row([&e.hash], |r| r.get(0)).optional()?;
                let id = match known {
                    Some(id) => id,
                    None => {
                        new_fp.execute(rusqlite::params![
                            e.template,
                            e.hash,
                            e.level.as_str(),
                            e.captured_at
                        ])?;
                        tx.last_insert_rowid()
                    }
                };
                signatures.insert(
                    &e.hash,
                    SignatureDelta {
                        id,
                        count: 1,
                        first_at: e.captured_at,
                        last_at: e.captured_at,
                    },
                );
                (id, known.is_none())
            }
        };

        new_entry.execute(rusqlite::params![
            run_id,
            e.start_line,
            e.end_line,
            e.captured_at,
            e.level.as_str(),
            e.evidence.as_str(),
            e.is_error as i64,
            e.error_reason,
            fp_id,
            e.boundaries.as_str(),
        ])?;
        let entry_id = tx.last_insert_rowid();
        index_text.execute(rusqlite::params![entry_id, e.text])?;
        if let Some(c) = counters.as_deref_mut() {
            c.bump(e.level);
        }

        ids.push((entry_id, fp_id, is_new));
    }

    let mut bump_run = tx.prepare_cached(
        "UPDATE run_fingerprints SET count = count + ?3, last_at = max(last_at, ?4)
          WHERE run_id = ?1 AND fingerprint_id = ?2",
    )?;
    let mut has_run = tx.prepare_cached(
        "SELECT count(*) FROM run_fingerprints WHERE run_id = ?1 AND fingerprint_id = ?2",
    )?;
    let mut new_run = tx.prepare_cached(
        "INSERT INTO run_fingerprints (run_id, fingerprint_id, count, first_at, last_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut bump_fp = tx.prepare_cached(
        "UPDATE fingerprints
            SET first_seen = min(first_seen, ?2), last_seen = max(last_seen, ?3),
                total_count = total_count + ?4, run_count = run_count + ?5
          WHERE id = ?1",
    )?;
    for delta in signatures.values() {
        let known: i64 = if recount {
            bump_run.execute(rusqlite::params![run_id, delta.id, delta.count, delta.last_at])? as i64
        } else {
            has_run.query_row(rusqlite::params![run_id, delta.id], |r| r.get(0))?
        };
        // run_count counts distinct runs, so it moves only when the pair is new.
        if known == 0 {
            new_run.execute(rusqlite::params![
                run_id,
                delta.id,
                delta.count,
                delta.first_at,
                delta.last_at
            ])?;
        }
        if recount || known == 0 {
            bump_fp.execute(rusqlite::params![
                delta.id,
                delta.first_at,
                delta.last_at,
                delta.count,
                (known == 0) as i64
            ])?;
        }
    }

    if let Some(c) = counters {
        tx.prepare_cached(
            "INSERT INTO run_counters (run_id, n_entries, n_error, n_warn, n_info,
                                       n_debug, n_trace, n_unknown)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(run_id) DO UPDATE SET
               n_entries = excluded.n_entries, n_error = excluded.n_error,
               n_warn = excluded.n_warn, n_info = excluded.n_info,
               n_debug = excluded.n_debug, n_trace = excluded.n_trace,
               n_unknown = excluded.n_unknown",
        )?
        .execute(rusqlite::params![
            run_id,
            c.n_entries,
            c.n_error,
            c.n_warn,
            c.n_info,
            c.n_debug,
            c.n_trace,
            c.n_unknown,
        ])?;
    }

    Ok(ids)
}

/// Groups consecutive runs of the same command into one session, so that repeatedly
/// restarting a server produces many runs but a single session.
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
        let err = entries.iter().find(|e| e.entry.is_error).unwrap();
        assert!(err.text.contains("at a (/app/x.js:1:1)"), "{}", err.text);
    }

    #[test]
    fn the_first_sighting_of_a_signature_is_flagged_new() {
        let h = harness();
        let mut w = Writer::start(&h.db, &h.index, &stamp(), Arc::new(Redactor::disabled())).unwrap();
        w.push(b"Error: connection refused\n", 1).unwrap();
        // A single line has not settled the delimiter mode yet; the idle tick is what
        // makes a quiet run's first entry visible.
        w.tick(crate::now_micros()).unwrap();
        let all = read::entries(&h.db, &Default::default()).unwrap();
        assert!(all[0].is_new_signature);

        // The same error in a second run is known, not new.
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
    fn a_quiet_run_flushes_at_once_and_a_busy_one_in_large_batches() {
        let h = harness();
        let mut w = Writer::start(&h.db, &h.index, &stamp(), Arc::new(Redactor::disabled())).unwrap();

        // Slow enough that every line finds the interval already past: nothing is held back
        // for a batch that is not coming, which is what keeps crash-to-hook short.
        w.last_flush -= FLUSH_MICROS;
        assert!(w.push(b"[info] one\n", crate::now_micros()).unwrap().is_some());
        w.last_flush -= FLUSH_MICROS;
        assert!(w.push(b"[info] two\n", crate::now_micros()).unwrap().is_some());

        // A burst inside one interval holds until the byte threshold.
        let line = format!("[info] {}\n", "x".repeat(1024));
        let mut flushes = 0;
        for _ in 0..(3 * FLUSH_BYTES / line.len()) {
            w.last_flush = crate::now_micros();
            if w.push(line.as_bytes(), crate::now_micros()).unwrap().is_some() {
                flushes += 1;
            }
        }
        assert!((2..=3).contains(&flushes), "3 MB took {flushes} flushes");
    }

    /// Fails every write that needs a new page, as a full disk would.
    fn fill_the_disk(db: &Db) {
        db.with(|c| {
            let pages: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            c.execute_batch(&format!("PRAGMA max_page_count = {pages};"))?;
            Ok(())
        })
        .unwrap();
    }

    fn free_the_disk(db: &Db) {
        db.with(|c| {
            c.execute_batch("PRAGMA max_page_count = 1073741823;")?;
            Ok(())
        })
        .unwrap();
    }

    fn bulk(from: usize, to: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for i in from..to {
            out.extend_from_slice(format!("[info] request {i} {}\n", "x".repeat(200)).as_bytes());
        }
        out
    }

    #[test]
    fn a_full_disk_loses_nothing_once_it_clears() {
        let h = harness();
        let mut w = Writer::start(&h.db, &h.index, &stamp(), Arc::new(Redactor::disabled())).unwrap();
        w.push(b"[info] before\n", crate::now_micros()).unwrap();
        w.flush().unwrap();

        fill_the_disk(&h.db);
        w.push(&bulk(0, 2000), crate::now_micros()).ok();
        let err = w.flush().expect_err("nothing fits");
        assert!(err.is_disk_full(), "{err}");

        // Still capturing, and the run's lines are all still in hand.
        w.push(&bulk(2000, 2100), crate::now_micros()).ok();

        free_the_disk(&h.db);
        w.finish(&h.index, Some(0), None).unwrap();
        let run = read::get_run(&h.db, w.run_id()).unwrap();
        assert_eq!(run.counters.n_entries, 2101);
        assert_eq!(run.exit_code, Some(0));
        let lines = read::line_count(&h.db, w.run_id()).unwrap();
        assert_eq!(lines, 2101, "a line stored out of place would leave a gap");
        let kept = h.db.with(|c| crate::store::read_lines(c, w.run_id(), 0, lines)).unwrap();
        assert_eq!(kept.len(), 2101);
    }

    #[test]
    fn a_finish_refused_by_a_full_disk_can_be_tried_again() {
        let h = harness();
        let mut w = Writer::start(&h.db, &h.index, &stamp(), Arc::new(Redactor::disabled())).unwrap();
        w.push(&bulk(0, 2000), crate::now_micros()).ok();
        fill_the_disk(&h.db);
        assert!(w.finish(&h.index, Some(3), None).is_err());
        let first_end = w.finished_at;

        free_the_disk(&h.db);
        w.retry_at = 0;
        w.finish(&h.index, Some(3), None).unwrap();
        assert_eq!(w.finished_at, first_end, "a retry must not move the end of the run");
        let run = read::get_run(&h.db, w.run_id()).unwrap();
        assert_eq!(run.exit_code, Some(3));
        assert_eq!(run.ended_at, first_end);
        assert_eq!(run.counters.n_entries, 2000);
    }

    #[test]
    fn a_long_outage_gives_up_the_oldest_output_and_keeps_the_newest() {
        let h = harness();
        let mut w = Writer::start(&h.db, &h.index, &stamp(), Arc::new(Redactor::disabled())).unwrap();
        w.max_buffered = 64 * 1024;
        fill_the_disk(&h.db);
        for start in (0..3000).step_by(100) {
            w.push(&bulk(start, start + 100), crate::now_micros()).ok();
        }
        assert!(w.buffered_bytes <= w.max_buffered, "held {} bytes", w.buffered_bytes);
        assert!(w.lines_lost > 0);

        free_the_disk(&h.db);
        w.finish(&h.index, Some(0), None).unwrap();
        let id = w.run_id();
        let newest = read::entries(&h.db, &crate::proto::EntryFilter { run_id: Some(id), limit: 1, ..Default::default() })
            .unwrap();
        assert!(newest[0].text.contains("request 2999"), "{}", newest[0].text);
        assert!(w.lines_lost == 0, "reported once storage came back");

        // The gap where lines went missing must not trip anything that reads the run later.
        assert!(crate::store::retention::squash_run(&h.db, id).is_ok());
        assert!(crate::store::retention::reparse_run(&h.db, id).is_ok());
    }

    #[test]
    fn a_panic_with_the_writer_locked_does_not_take_the_project_down() {
        let h = harness();
        let db = h.db.clone();
        let _ = std::thread::spawn(move || {
            let _ = db.with(|_| -> Result<()> { panic!("boom") });
        })
        .join();
        let mut w = Writer::start(&h.db, &h.index, &stamp(), Arc::new(Redactor::disabled())).unwrap();
        w.push(b"[info] still here\n", crate::now_micros()).unwrap();
        w.finish(&h.index, Some(0), None).unwrap();
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

/// Closes runs that were still live when the daemon last stopped.
///
/// The daemon owns every writer in memory, so at startup a run with no `ended_at` belongs
/// to a previous daemon that was killed or upgraded and would otherwise report as running
/// forever.
///
/// `ended_at` is set to the last byte captured rather than the current clock, so a run
/// reconciled hours later does not report an inflated duration.
pub fn close_orphaned_runs(db: &Db) -> Result<usize> {
    Ok(close_orphans(db)?.len())
}

/// [`close_orphaned_runs`], and the same on the central index, whose summaries would
/// otherwise call those runs live for good and whose open runs say which projects need
/// looking at on startup.
pub fn reconcile_orphans(db: &Db, index: &IndexDb) -> Result<usize> {
    let orphans = close_orphans(db)?;
    for (run_id, ended_at) in &orphans {
        let n_error = read::get_run(db, *run_id).map_or(0, |r| r.counters.n_error);
        index.record_run_end(db.project_id(), *run_id, *ended_at, None, n_error)?;
    }
    Ok(orphans.len())
}

fn close_orphans(db: &Db) -> Result<Vec<(RunId, i64)>> {
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
        return Ok(orphans);
    }
    db.with_mut(|conn| {
        let tx = conn.transaction()?;
        for (run_id, ended_at) in &orphans {
            // Left with no exit code and no signal, which `run_failed` already classifies
            // as an unexpected death.
            tx.execute(
                "UPDATE runs SET ended_at = ?2 WHERE id = ?1 AND ended_at IS NULL",
                rusqlite::params![run_id, ended_at],
            )?;
        }
        tx.commit()?;
        Ok(())
    })?;
    Ok(orphans)
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::store::index::IndexDb;

    #[test]
    fn a_run_abandoned_by_a_dead_daemon_is_closed_on_the_next_start() {
        let tmp = tempfile::tempdir().unwrap();
        let index = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let p = index.register(&root, "proj", &tmp.path().join("p.db")).unwrap();
        let db = Db::open(&tmp.path().join("p.db"), p.id).unwrap();

        // A run that starts, produces output, and is never finished.
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
        // Ends when output stopped, not at reconciliation time.
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
