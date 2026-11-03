//! Retention, eviction and rebuild.
//!
//! History degrades in stages rather than vanishing. Old runs are squashed first: their
//! stored output is rewritten to the first occurrence of each distinct entry, which keeps
//! everything that was ever said while dropping the thousandth identical request line.
//! Only if that is not enough are chunk bytes dropped, then the derived index for those
//! runs. Run rows, their signatures and their counts cost a few KB each and are kept
//! indefinitely, so a run's git SHA and error signatures remain queryable after its output
//! is gone, and its counts still describe the run as captured after it is squashed.

use crate::error::Result;
use crate::model::RunId;
use crate::parse::RawLine;
use crate::proto::{RetentionState, SquashReport, Sweep};
use crate::store::{pack_chunk, read, split_chunk, Db};
use std::collections::{HashMap, HashSet};

/// Pages reclaimed per `incremental_vacuum` step. Bounded because a full `VACUUM` would
/// stall the daemon.
const VACUUM_PAGES: i64 = 512;

pub fn state(db: &Db, cap: i64) -> Result<RetentionState> {
    let (runs_total, runs_evicted) = db.with(read::count_runs)?;
    let runs_squashed = db.with(|c| {
        Ok(c.query_row("SELECT count(*) FROM runs WHERE squashed_at IS NOT NULL", [], |r| r.get(0))?)
    })?;
    let oldest = db.with(|c| {
        Ok(c.query_row("SELECT min(started_at) FROM runs", [], |r| {
            r.get::<_, Option<i64>>(0)
        })?)
    })?;
    Ok(RetentionState {
        project_id: db.project_id(),
        bytes_used: db.size_bytes(),
        bytes_cap: cap,
        runs_total,
        runs_evicted,
        runs_squashed,
        oldest_run_at: oldest,
    })
}

/// Brings a project back under its cap: squashes the oldest finished runs first, and
/// evicts whole runs' bytes only if squashing everything was not enough.
///
/// Runs that have not ended are never touched, however old.
pub fn enforce(db: &Db, cap: i64) -> Result<Sweep> {
    let mut sweep = Sweep::default();
    if cap <= 0 || db.size_bytes() <= cap {
        return Ok(sweep);
    }

    while let Some(run_id) = next_candidate(db, "AND r.squashed_at IS NULL")? {
        squash_run(db, run_id)?;
        sweep.squashed += 1;
        reclaim_all(db)?;
        if db.size_bytes() <= cap {
            return Ok(sweep);
        }
    }

    while let Some(run_id) = next_candidate(db, "")? {
        evict_run(db, run_id)?;
        sweep.evicted += 1;
        reclaim(db)?;
        if db.size_bytes() <= cap {
            break;
        }
    }

    Ok(sweep)
}

/// The oldest finished run that still has bytes, narrowed by `extra`.
fn next_candidate(db: &Db, extra: &str) -> Result<Option<RunId>> {
    db.with(|conn| {
        Ok(conn
            .query_row(
                &format!(
                    "SELECT r.id FROM runs r
                      WHERE r.ended_at IS NOT NULL AND r.chunks_evicted = 0 {extra}
                        AND EXISTS (SELECT 1 FROM chunks c WHERE c.run_id = r.id)
                      ORDER BY r.started_at ASC LIMIT 1"
                ),
                [],
                |r| r.get::<_, i64>(0),
            )
            .ok())
    })
}

/// Squashes every finished run in the project that has not been squashed yet.
pub fn squash_all(db: &Db) -> Result<SquashReport> {
    let bytes_before = db.size_bytes();
    let ids: Vec<RunId> = db.with(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id FROM runs
              WHERE ended_at IS NOT NULL AND chunks_evicted = 0 AND squashed_at IS NULL
              ORDER BY started_at",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })?;
    let mut report = SquashReport {
        bytes_before,
        ..Default::default()
    };
    for id in ids {
        let dropped = squash_run(db, id)?;
        report.runs += 1;
        report.entries_dropped += dropped;
    }
    reclaim_all(db)?;
    report.bytes_after = db.size_bytes();
    Ok(report)
}

/// Squashes one run and reclaims the space it freed.
pub fn squash_one(db: &Db, run_id: RunId) -> Result<SquashReport> {
    let bytes_before = db.size_bytes();
    let squashable: bool = db.with(|conn| {
        Ok(conn
            .query_row(
                "SELECT ended_at IS NOT NULL AND chunks_evicted = 0 AND squashed_at IS NULL
                   FROM runs WHERE id = ?1",
                [run_id],
                |r| r.get(0),
            )
            .unwrap_or(false))
    })?;
    if !squashable {
        return Ok(SquashReport {
            bytes_before,
            bytes_after: bytes_before,
            ..Default::default()
        });
    }
    let entries_dropped = squash_run(db, run_id)?;
    reclaim_all(db)?;
    Ok(SquashReport {
        runs: 1,
        entries_dropped,
        bytes_before,
        bytes_after: db.size_bytes(),
    })
}

/// Rewrites a finished run's stored output down to the first occurrence of each distinct
/// entry, returning how many entries were dropped.
///
/// "Distinct" is the entry's signature and level. Signatures are templates with the
/// volatile parts — timestamps, ids, numbers — already stripped, so the thousandth
/// `GET /users/42 200 in 13ms` repeats the first and goes, while a line that differs in
/// substance stays. Lines that belong to no entry are kept.
///
/// The run's counters and its per-signature counts are left as they were: they describe
/// what the run produced, and a squashed run still produced it.
pub fn squash_run(db: &Db, run_id: RunId) -> Result<usize> {
    // Each chunk's lines, in order, so the rewrite can keep each line's capture time.
    let chunks: Vec<(i64, i64, Vec<Vec<u8>>)> = db.with(|conn| {
        let mut stmt = conn.prepare(
            "SELECT captured_at, first_line, n_lines, bytes FROM chunks
              WHERE run_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (at, first_line, n_lines, bytes) = row?;
            out.push((at, first_line, split_chunk(&bytes, n_lines as usize)));
        }
        Ok(out)
    })?;

    let entries: Vec<(i64, i64, i64, String, Option<i64>)> = db.with(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, start_line, end_line, level, fingerprint_id FROM entries
              WHERE run_id = ?1 ORDER BY start_line, id",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })?;

    let mut seen: HashSet<(i64, String)> = HashSet::new();
    let mut dropped_ids: Vec<i64> = Vec::new();
    let mut kept: Vec<(i64, i64, i64)> = Vec::new();
    let mut dropped_lines: HashSet<i64> = HashSet::new();
    for (id, start, end, level, fp) in entries {
        let repeat = match fp {
            Some(fp) => !seen.insert((fp, level)),
            None => false,
        };
        if repeat {
            dropped_ids.push(id);
            dropped_lines.extend(start..=end);
        } else {
            kept.push((id, start, end));
        }
    }

    // Renumber the surviving lines densely, chunk by chunk.
    let mut remap: HashMap<i64, i64> = HashMap::new();
    let mut rewritten: Vec<(i64, i64, Vec<u8>, i64)> = Vec::new();
    let mut next_line = 0i64;
    for (at, first_line, lines) in chunks {
        let survivors: Vec<crate::parse::RawLine> = lines
            .into_iter()
            .enumerate()
            .map(|(i, bytes)| (first_line + i as i64, bytes))
            .filter(|(index, _)| !dropped_lines.contains(index))
            .map(|(index, bytes)| {
                remap.insert(index, next_line);
                next_line += 1;
                crate::parse::RawLine {
                    index,
                    captured_at: at,
                    bytes,
                }
            })
            .collect();
        if let Some(first) = survivors.first() {
            let new_first = remap[&first.index];
            rewritten.push((
                at,
                new_first,
                pack_chunk(&survivors),
                survivors.len() as i64,
            ));
        }
    }

    let now = crate::now_micros();
    db.with_mut(|conn| {
        let tx = conn.transaction()?;
        // Another squash may have got here first; doing it twice would be harmless but
        // pointless, and the flag is what the UI reads.
        let already: bool = tx.query_row(
            "SELECT squashed_at IS NOT NULL FROM runs WHERE id = ?1",
            [run_id],
            |r| r.get(0),
        )?;
        if already {
            return Ok(());
        }
        if !dropped_ids.is_empty() {
            tx.execute("DELETE FROM chunks WHERE run_id = ?1", [run_id])?;
            {
                let mut ins = tx.prepare(
                    "INSERT INTO chunks (run_id, seq, captured_at, first_line, n_lines, bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?;
                for (seq, (at, first_line, bytes, n_lines)) in rewritten.iter().enumerate() {
                    ins.execute(rusqlite::params![
                        run_id, seq as i64, at, first_line, n_lines, bytes
                    ])?;
                }
                let mut del_fts = tx.prepare("DELETE FROM entries_fts WHERE rowid = ?1")?;
                let mut del = tx.prepare("DELETE FROM entries WHERE id = ?1")?;
                for id in &dropped_ids {
                    del_fts.execute([id])?;
                    del.execute([id])?;
                }
                let mut upd =
                    tx.prepare("UPDATE entries SET start_line = ?2, end_line = ?3 WHERE id = ?1")?;
                for (id, start, end) in &kept {
                    // Every line of a kept entry survives, so both ends are mapped.
                    upd.execute(rusqlite::params![id, remap[start], remap[end]])?;
                }
            }
        }
        tx.execute(
            "UPDATE runs SET squashed_at = ?2 WHERE id = ?1",
            rusqlite::params![run_id, now],
        )?;
        tx.commit()?;
        Ok(())
    })?;

    Ok(dropped_ids.len())
}

/// Drops one run's bytes and its derived index, keeping the summary rows.
pub fn evict_run(db: &Db, run_id: RunId) -> Result<()> {
    db.with_mut(|conn| {
        let tx = conn.transaction()?;
        // FTS rows must go with the entries they describe, or search returns hits whose
        // text can no longer be produced.
        tx.execute(
            "DELETE FROM entries_fts WHERE rowid IN (SELECT id FROM entries WHERE run_id = ?1)",
            [run_id],
        )?;
        tx.execute("DELETE FROM chunks WHERE run_id = ?1", [run_id])?;
        tx.execute("DELETE FROM entries WHERE run_id = ?1", [run_id])?;
        tx.execute(
            "UPDATE runs SET chunks_evicted = 1 WHERE id = ?1",
            [run_id],
        )?;
        tx.commit()?;
        Ok(())
    })
}

/// Removes a run entirely, including its contribution to signature counts.
pub fn delete_run(db: &Db, run_id: RunId) -> Result<()> {
    db.with_mut(|conn| {
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE fingerprints SET
                total_count = max(0, total_count - COALESCE(
                  (SELECT count FROM run_fingerprints
                    WHERE run_id = ?1 AND fingerprint_id = fingerprints.id), 0)),
                run_count = max(0, run_count - COALESCE(
                  (SELECT 1 FROM run_fingerprints
                    WHERE run_id = ?1 AND fingerprint_id = fingerprints.id), 0))
              WHERE id IN (SELECT fingerprint_id FROM run_fingerprints WHERE run_id = ?1)",
            [run_id],
        )?;
        tx.execute(
            "DELETE FROM entries_fts WHERE rowid IN (SELECT id FROM entries WHERE run_id = ?1)",
            [run_id],
        )?;
        tx.execute("DELETE FROM chunks WHERE run_id = ?1", [run_id])?;
        tx.execute("DELETE FROM entries WHERE run_id = ?1", [run_id])?;
        tx.execute("DELETE FROM run_fingerprints WHERE run_id = ?1", [run_id])?;
        tx.execute("DELETE FROM run_counters WHERE run_id = ?1", [run_id])?;
        tx.execute("DELETE FROM runs WHERE id = ?1", [run_id])?;
        // Drop signatures no run references any more.
        tx.execute(
            "DELETE FROM fingerprints WHERE id NOT IN (SELECT fingerprint_id FROM run_fingerprints)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    })?;
    reclaim(db)
}

pub fn reclaim(db: &Db) -> Result<()> {
    db.with(|conn| {
        // `incremental_vacuum` frees one page per step of the statement, and
        // `execute_batch` steps it only once — which is how this used to free a single
        // page per call and leave evicted runs' space in the file. Drain it.
        let mut stmt = conn.prepare(&format!("PRAGMA incremental_vacuum({VACUUM_PAGES})"))?;
        let mut rows = stmt.query([])?;
        while rows.next()?.is_some() {}
        Ok(())
    })
}

/// Returns every free page to the filesystem, a bounded step at a time so other writers
/// interleave. Squashing frees pages spread across the whole file, and until they are
/// returned the file is no smaller.
pub fn reclaim_all(db: &Db) -> Result<()> {
    for _ in 0..4096 {
        let free: i64 =
            db.with(|conn| Ok(conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?))?;
        if free == 0 {
            break;
        }
        reclaim(db)?;
    }
    db.with(|conn| {
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    })
}

/// Rebuilds the derived index for a run from its chunks.
///
/// Safe to run at any time: entries hold no text and chunks are only ever rewritten by
/// squashing, which keeps one copy of everything, so regenerating the index cannot lose
/// captured output.
///
/// A squashed run's bytes no longer hold its repeats, so its counts are left as recorded
/// rather than recounted from what survives: only the entries themselves are rebuilt.
pub fn reparse_run(db: &Db, run_id: RunId) -> Result<usize> {
    let lines = load_lines(db, run_id)?;
    if lines.is_empty() {
        return Ok(0);
    }
    let (mode, entries) = crate::parse::reparse(&lines);
    let squashed: bool = db.with(|conn| {
        Ok(conn.query_row("SELECT squashed_at IS NOT NULL FROM runs WHERE id = ?1", [run_id], |r| {
            r.get(0)
        })?)
    })?;

    db.with_mut(|conn| {
        let tx = conn.transaction()?;
        // Roll back this run's contribution first, or the counts double. A squashed run
        // keeps its recorded counts, so there is nothing to roll back.
        if !squashed {
            tx.execute(
                "UPDATE fingerprints SET
                    total_count = max(0, total_count - COALESCE(
                      (SELECT count FROM run_fingerprints
                        WHERE run_id = ?1 AND fingerprint_id = fingerprints.id), 0)),
                    run_count = max(0, run_count - COALESCE(
                      (SELECT 1 FROM run_fingerprints
                        WHERE run_id = ?1 AND fingerprint_id = fingerprints.id), 0))
                  WHERE id IN (SELECT fingerprint_id FROM run_fingerprints WHERE run_id = ?1)",
                [run_id],
            )?;
        }
        tx.execute(
            "DELETE FROM entries_fts WHERE rowid IN (SELECT id FROM entries WHERE run_id = ?1)",
            [run_id],
        )?;
        tx.execute("DELETE FROM entries WHERE run_id = ?1", [run_id])?;
        if !squashed {
            tx.execute("DELETE FROM run_fingerprints WHERE run_id = ?1", [run_id])?;
        }

        let mut counters = crate::model::Counters::default();
        for e in &entries {
            let fp_id: i64 = {
                let existing: Option<i64> = tx
                    .query_row("SELECT id FROM fingerprints WHERE hash = ?1", [&e.hash], |r| {
                        r.get(0)
                    })
                    .ok();
                match existing {
                    Some(id) if squashed => id,
                    Some(id) => {
                        tx.execute(
                            "UPDATE fingerprints SET total_count = total_count + 1,
                                    last_seen = max(last_seen, ?2), first_seen = min(first_seen, ?2)
                              WHERE id = ?1",
                            rusqlite::params![id, e.captured_at],
                        )?;
                        id
                    }
                    None => {
                        tx.execute(
                            "INSERT INTO fingerprints (template, hash, level, first_seen,
                                                       last_seen, total_count, run_count)
                             VALUES (?1, ?2, ?3, ?4, ?4, 1, 0)",
                            rusqlite::params![e.template, e.hash, e.level.as_str(), e.captured_at],
                        )?;
                        tx.last_insert_rowid()
                    }
                }
            };

            tx.execute(
                // Must stay column-for-column identical to the insert in `store::write`.
                // A column present there and missing here silently degrades on reparse,
                // which is the one path that is supposed to be lossless.
                "INSERT INTO entries (run_id, start_line, end_line, captured_at, level,
                                      evidence, is_error, error_reason, fingerprint_id,
                                      boundaries)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
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
                ],
            )?;
            let entry_id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO entries_fts (rowid, text) VALUES (?1, ?2)",
                rusqlite::params![entry_id, e.text],
            )?;

            // A squashed run already has its row for every signature it ever produced.
            let changed = if squashed {
                tx.query_row(
                    "SELECT count(*) FROM run_fingerprints WHERE run_id = ?1 AND fingerprint_id = ?2",
                    rusqlite::params![run_id, fp_id],
                    |r| r.get::<_, i64>(0),
                )? as usize
            } else {
                tx.execute(
                    "UPDATE run_fingerprints SET count = count + 1, last_at = max(last_at, ?3)
                      WHERE run_id = ?1 AND fingerprint_id = ?2",
                    rusqlite::params![run_id, fp_id, e.captured_at],
                )?
            };
            if changed == 0 {
                tx.execute(
                    "INSERT INTO run_fingerprints (run_id, fingerprint_id, count, first_at, last_at)
                     VALUES (?1, ?2, 1, ?3, ?3)",
                    rusqlite::params![run_id, fp_id, e.captured_at],
                )?;
                tx.execute(
                    "UPDATE fingerprints SET run_count = run_count + 1 WHERE id = ?1",
                    [fp_id],
                )?;
            }
            counters.bump(e.level);
        }

        if !squashed {
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
        }
        tx.execute(
            "UPDATE runs SET delimiter_mode = ?2 WHERE id = ?1",
            rusqlite::params![run_id, mode.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    })?;

    Ok(entries.len())
}

pub fn reparse_all(db: &Db) -> Result<usize> {
    let ids: Vec<RunId> = db.with(|conn| {
        let mut stmt = conn.prepare("SELECT id FROM runs WHERE chunks_evicted = 0 ORDER BY id")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })?;
    let mut total = 0;
    for id in ids {
        total += reparse_run(db, id)?;
    }
    Ok(total)
}

/// Rebuilds the derived layer after [`Db::derived_stale`] reports a version bump.
///
/// Reparses every run whose chunks survive, then parks each agent session's watermark at
/// the newest entry. Entry ids do not survive a rebuild, so a stale watermark would either
/// replay the whole history as unread or skip past genuinely new errors.
///
/// [`Db::derived_stale`]: crate::store::Db::derived_stale
pub fn rebuild_derived(db: &Db) -> Result<usize> {
    let entries = reparse_all(db)?;
    db.with(|conn| {
        conn.execute(
            "UPDATE session_cursors
                SET last_entry_id = COALESCE((SELECT max(id) FROM entries), 0)",
            [],
        )?;
        Ok(())
    })?;
    Ok(entries)
}

fn load_lines(db: &Db, run_id: RunId) -> Result<Vec<RawLine>> {
    db.with(|conn| {
        let mut stmt = conn.prepare(
            "SELECT first_line, n_lines, captured_at, bytes FROM chunks
              WHERE run_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (first_line, n_lines, at, bytes) = row?;
            for (i, line) in split_chunk(&bytes, n_lines as usize).into_iter().enumerate() {
                out.push(RawLine {
                    index: first_line + i as i64,
                    captured_at: at,
                    bytes: line,
                });
            }
        }
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RunStamp;
    use crate::proto::EntryFilter;
    use crate::redact::Redactor;
    use crate::store::{index::IndexDb, Writer};
    use std::sync::Arc;

    struct H {
        _tmp: tempfile::TempDir,
        db: Db,
        index: IndexDb,
    }

    fn h() -> H {
        let tmp = tempfile::tempdir().unwrap();
        let index = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let p = index.register(&root, "proj", &tmp.path().join("p.db")).unwrap();
        let db = Db::open(&tmp.path().join("p.db"), p.id).unwrap();
        H { _tmp: tmp, db, index }
    }

    fn run(h: &H, output: &[u8]) -> RunId {
        let mut w = Writer::start(
            &h.db,
            &h.index,
            &RunStamp {
                command: vec!["mix".into(), "phx.server".into()],
                git_sha: Some("abc123".into()),
                ..Default::default()
            },
            Arc::new(Redactor::disabled()),
        )
        .unwrap();
        w.push(output, crate::now_micros()).unwrap();
        w.finish(&h.index, Some(1), None).unwrap();
        w.run_id()
    }

    #[test]
    fn a_derived_version_bump_rebuilds_the_index_from_chunks() {
        // `derived_version` used to be written on every open and read on none, so bumping
        // it did nothing at all: `CREATE TABLE IF NOT EXISTS` left the old columns in
        // place and the daemon carried on serving entries built by the previous rules.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("p.db");
        let index = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let p = index.register(&root, "proj", &path).unwrap();

        let h = H {
            _tmp: tempfile::tempdir().unwrap(),
            db: Db::open(&path, p.id).unwrap(),
            index,
        };
        assert!(!h.db.derived_stale(), "a fresh database is already current");
        let id = run(&h, b"[error] boom\n    at a (/app/x.js:1:1)\nplain output\n");
        let before = read::entries(&h.db, &EntryFilter { run_id: Some(id), ..Default::default() })
            .unwrap()
            .len();
        assert!(before > 0);
        drop(h);

        // Pretend the database was written by an older build.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            crate::store::set_meta(&conn, "derived_version", "0").unwrap();
        }

        let reopened = Db::open(&path, p.id).unwrap();
        assert!(reopened.derived_stale(), "an older derived layer must be noticed");
        let rebuilt = rebuild_derived(&reopened).unwrap();
        assert_eq!(rebuilt, before, "the rebuild reproduces the same entries");

        // The dropped table came back at the current shape, which is the whole point.
        let n: i64 = reopened
            .with(|conn| {
                Ok(conn
                    .query_row("SELECT count(*) FROM entries WHERE evidence = 'declared'", [], |r| {
                        r.get(0)
                    })?)
            })
            .unwrap();
        assert!(n > 0, "the new column exists and is populated");

        assert!(!Db::open(&path, p.id).unwrap().derived_stale(), "and it settles");
    }

    #[test]
    fn a_current_version_stamp_over_a_stale_table_still_rebuilds() {
        // How this failed in the field: a build wrote the new `derived_version` without
        // dropping the table, so the stamp said 2 while `entries` still had the version-1
        // columns. A later correct build then saw a matching version and rebuilt nothing.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("p.db");
        let index = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let p = index.register(&root, "proj", &path).unwrap();

        let h = H {
            _tmp: tempfile::tempdir().unwrap(),
            db: Db::open(&path, p.id).unwrap(),
            index,
        };
        run(&h, b"[error] boom\n");
        drop(h);

        // Stamp stays current; the column disappears. The views are dropped first because
        // they call `logbear_render`, which a bare connection has not registered — `Db::open`
        // recreates them.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP VIEW IF EXISTS v_novel;
                 DROP VIEW IF EXISTS v_errors;
                 DROP VIEW IF EXISTS v_entries;
                 ALTER TABLE entries DROP COLUMN evidence;",
            )
            .unwrap();
        }

        let reopened = Db::open(&path, p.id).unwrap();
        assert!(
            reopened.derived_stale(),
            "the shape is authoritative, not the version stamp"
        );
        rebuild_derived(&reopened).unwrap();
        let n: i64 = reopened
            .with(|conn| Ok(conn.query_row("SELECT count(*) FROM entries", [], |r| r.get(0))?))
            .unwrap();
        assert!(n > 0, "and the rebuild repopulated it");
    }

    #[test]
    fn eviction_keeps_the_run_and_its_signatures_after_the_bytes_are_gone() {
        let h = h();
        let id = run(&h, b"Error: connection refused\n    at a (/app/x.js:1:1)\n");
        evict_run(&h.db, id).unwrap();

        let r = read::get_run(&h.db, id).unwrap();
        assert!(r.chunks_evicted, "the UI needs to know the log is gone");
        assert_eq!(r.git_sha.as_deref(), Some("abc123"), "context must survive");

        let sigs = read::signatures_for_run(&h.db, id).unwrap();
        assert_eq!(sigs.len(), 1, "signatures outlive the bytes — that is the point");
        assert_eq!(sigs[0].3, 1);

        // The entries themselves are gone, so nothing dangles.
        let entries = read::entries(&h.db, &EntryFilter { run_id: Some(id), ..Default::default() }).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn eviction_takes_the_oldest_run_first_and_leaves_live_runs_alone() {
        let h = h();
        let old = run(&h, &vec![b'x'; 200_000]);
        let new = run(&h, &vec![b'y'; 200_000]);

        // A cap below current usage, forcing at least one eviction.
        let sweep = enforce(&h.db, 1).unwrap();
        assert!(sweep.evicted >= 1);
        assert!(read::get_run(&h.db, old).unwrap().chunks_evicted);
        let _ = new;
    }

    #[test]
    fn a_live_run_is_never_evicted() {
        let h = h();
        let mut w = Writer::start(
            &h.db,
            &h.index,
            &RunStamp::default(),
            Arc::new(Redactor::disabled()),
        )
        .unwrap();
        w.push(&vec![b'z'; 100_000], crate::now_micros()).unwrap();
        w.flush().unwrap();

        enforce(&h.db, 1).unwrap();
        assert!(
            !read::get_run(&h.db, w.run_id()).unwrap().chunks_evicted,
            "the run the user is watching was evicted out from under them"
        );
    }

    #[test]
    fn reparsing_is_idempotent() {
        let h = h();
        let id = run(&h, b"GET /users\nError: boom\n    at a (/app/x.js:1:1)\nGET /posts\n");
        let before = read::get_run(&h.db, id).unwrap().counters;
        let fps_before = read::fingerprints(&h.db, 100, false).unwrap();

        reparse_run(&h.db, id).unwrap();
        reparse_run(&h.db, id).unwrap();

        let after = read::get_run(&h.db, id).unwrap().counters;
        assert_eq!(before.n_entries, after.n_entries);
        assert_eq!(before.n_error, after.n_error);

        let fps_after = read::fingerprints(&h.db, 100, false).unwrap();
        assert_eq!(fps_before.len(), fps_after.len());
        for (a, b) in fps_before.iter().zip(fps_after.iter()) {
            assert_eq!(a.hash, b.hash);
            assert_eq!(a.total_count, b.total_count, "counts drifted on reparse: {}", a.template);
            assert_eq!(a.run_count, b.run_count);
        }
    }

    #[test]
    fn deleting_a_run_removes_its_contribution_to_signature_counts() {
        let h = h();
        let a = run(&h, b"Error: boom\n");
        run(&h, b"Error: boom\n");
        assert_eq!(read::fingerprints(&h.db, 10, true).unwrap()[0].total_count, 2);

        delete_run(&h.db, a).unwrap();
        let fps = read::fingerprints(&h.db, 10, true).unwrap();
        assert_eq!(fps[0].total_count, 1);
        assert_eq!(fps[0].run_count, 1);
    }

    /// Output where most lines are repeats that differ only in volatile tokens.
    fn noisy() -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..200 {
            out.extend_from_slice(
                format!("[info] GET /users/{i} 200 in {}ms\n", i % 17).as_bytes(),
            );
            if i == 50 {
                out.extend_from_slice(b"[error] payment gateway timed out\n");
            }
            if i % 40 == 0 {
                out.extend_from_slice(
                    format!("[warn] pool at {}% capacity\n", 60 + i / 10).as_bytes(),
                );
            }
        }
        out.extend_from_slice(b"[info] shutting down\n");
        out
    }

    fn texts(h: &H, id: RunId) -> Vec<String> {
        read::entries(
            &h.db,
            &EntryFilter {
                run_id: Some(id),
                ascending: true,
                limit: 10_000,
                ..Default::default()
            },
        )
        .unwrap()
        .into_iter()
        .map(|e| e.text)
        .collect()
    }

    #[test]
    fn squashing_keeps_the_first_of_each_distinct_entry_and_drops_the_repeats() {
        let h = h();
        let id = run(&h, &noisy());
        let before = read::get_run(&h.db, id).unwrap().counters;
        assert!(texts(&h, id).len() > 200);

        let dropped = squash_run(&h.db, id).unwrap();
        assert!(dropped > 190, "only {dropped} repeats went");

        assert_eq!(
            texts(&h, id),
            vec![
                "[info] GET /users/0 200 in 0ms",
                "[warn] pool at 60% capacity",
                "[error] payment gateway timed out",
                "[info] shutting down",
            ],
            "each distinct entry survives once, in order, with its own text"
        );

        let r = read::get_run(&h.db, id).unwrap();
        assert!(r.squashed_at.is_some());
        assert_eq!(
            r.counters.n_entries, before.n_entries,
            "counts describe the run as captured"
        );
        assert_eq!(r.counters.n_error, before.n_error);
    }

    #[test]
    fn a_squashed_run_replays_only_what_survived() {
        let h = h();
        let id = run(&h, &noisy());
        squash_run(&h.db, id).unwrap();
        let n = read::line_count(&h.db, id).unwrap();
        let replay =
            h.db.with(|c| crate::store::read_lines(c, id, 0, n - 1))
                .unwrap();
        let replay: Vec<String> = replay
            .into_iter()
            .map(|l| String::from_utf8(l).unwrap())
            .collect();
        assert_eq!(replay.len(), 4, "{replay:?}");
        assert_eq!(replay[3], "[info] shutting down");
    }

    #[test]
    fn squashing_twice_changes_nothing_the_second_time() {
        let h = h();
        let id = run(&h, &noisy());
        squash_run(&h.db, id).unwrap();
        let once = texts(&h, id);
        assert_eq!(squash_run(&h.db, id).unwrap(), 0);
        assert_eq!(
            squash_one(&h.db, id).unwrap().runs,
            0,
            "an already squashed run is skipped"
        );
        assert_eq!(texts(&h, id), once);
    }

    #[test]
    fn reparsing_a_squashed_run_keeps_its_recorded_counts() {
        let h = h();
        let id = run(&h, &noisy());
        let counters = read::get_run(&h.db, id).unwrap().counters;
        let fps = read::fingerprints(&h.db, 100, false).unwrap();
        squash_run(&h.db, id).unwrap();

        reparse_run(&h.db, id).unwrap();

        assert_eq!(
            read::get_run(&h.db, id).unwrap().counters.n_entries,
            counters.n_entries
        );
        assert_eq!(
            texts(&h, id).len(),
            4,
            "the entries are rebuilt from what survived"
        );
        let after = read::fingerprints(&h.db, 100, false).unwrap();
        for (a, b) in fps.iter().zip(after.iter()) {
            assert_eq!(a.total_count, b.total_count, "{}", a.template);
            assert_eq!(a.run_count, b.run_count);
        }
    }

    #[test]
    fn retention_squashes_old_runs_before_evicting_any() {
        let h = h();
        let old = run(&h, &noisy().repeat(40));
        let new = run(&h, &noisy().repeat(40));
        reclaim_all(&h.db).unwrap();
        let full = h.db.size_bytes();

        // Squashing the oldest run alone frees well over a few pages.
        let sweep = enforce(&h.db, full - 16 * 1024).unwrap();
        assert_eq!(
            sweep,
            Sweep {
                squashed: 1,
                evicted: 0
            }
        );
        assert!(read::get_run(&h.db, old).unwrap().squashed_at.is_some());
        assert!(!read::get_run(&h.db, old).unwrap().chunks_evicted);
        assert!(
            read::get_run(&h.db, new).unwrap().squashed_at.is_none(),
            "newer runs wait their turn"
        );
    }

    #[test]
    fn a_database_created_without_incremental_vacuum_is_converted_on_open() {
        // Every database before this fix: WAL first, auto_vacuum after, so it never took
        // and evicted space was never returned.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("old.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL; CREATE TABLE t(x); PRAGMA auto_vacuum = INCREMENTAL;",
            )
            .unwrap();
            let mode: i64 = conn
                .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                mode, 0,
                "the setting silently did not apply, as in the field"
            );
        }
        let db = Db::open(&path, 1).unwrap();
        let mode: i64 = db
            .with(|c| Ok(c.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(mode, 2);
    }

    #[test]
    fn a_live_run_is_never_squashed() {
        let h = h();
        let mut w = Writer::start(
            &h.db,
            &h.index,
            &RunStamp::default(),
            Arc::new(Redactor::disabled()),
        )
        .unwrap();
        w.push(&noisy(), crate::now_micros()).unwrap();
        w.flush().unwrap();

        enforce(&h.db, 1).unwrap();
        assert_eq!(squash_all(&h.db).unwrap().runs, 0);
        assert!(read::get_run(&h.db, w.run_id())
            .unwrap()
            .squashed_at
            .is_none());
    }

    #[test]
    fn full_text_search_does_not_return_hits_for_evicted_runs() {
        let h = h();
        let id = run(&h, b"Error: connection pool timeout\n");
        evict_run(&h.db, id).unwrap();
        let found = read::entries(
            &h.db,
            &EntryFilter { search: Some("timeout".into()), ..Default::default() },
        )
        .unwrap();
        assert!(found.is_empty(), "search returned an entry whose text is gone");
    }
}
