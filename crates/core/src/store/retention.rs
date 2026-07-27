//! Retention, eviction and rebuild.
//!
//! History degrades in stages rather than vanishing. Chunk bytes are dropped first, since
//! they hold nearly all the disk, then the derived index for those runs. Run rows, their
//! signatures and their counts cost a few KB each and are kept indefinitely, so a run's
//! git SHA and error signatures remain queryable after its output is gone.

use crate::error::Result;
use crate::model::RunId;
use crate::parse::RawLine;
use crate::proto::RetentionState;
use crate::store::{read, split_chunk, Db};

/// Pages reclaimed per `incremental_vacuum` step. Bounded because a full `VACUUM` would
/// stall the daemon.
const VACUUM_PAGES: i64 = 512;

pub fn state(db: &Db, cap: i64) -> Result<RetentionState> {
    let (runs_total, runs_evicted) = db.with(|c| read::count_runs(c))?;
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
        oldest_run_at: oldest,
    })
}

/// Brings a project back under its cap, returning how many runs were evicted.
///
/// Runs that have not ended are never evicted, however old.
pub fn enforce(db: &Db, cap: i64) -> Result<usize> {
    if cap <= 0 || db.size_bytes() <= cap {
        return Ok(0);
    }
    let mut evicted = 0usize;

    loop {
        let candidate: Option<RunId> = db.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT r.id FROM runs r
                      WHERE r.ended_at IS NOT NULL AND r.chunks_evicted = 0
                        AND EXISTS (SELECT 1 FROM chunks c WHERE c.run_id = r.id)
                      ORDER BY r.started_at ASC LIMIT 1",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .ok())
        })?;

        let Some(run_id) = candidate else { break };
        evict_run(db, run_id)?;
        evicted += 1;
        reclaim(db)?;

        if db.size_bytes() <= cap {
            break;
        }
    }

    Ok(evicted)
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
        conn.execute_batch(&format!("PRAGMA incremental_vacuum({VACUUM_PAGES});"))?;
        Ok(())
    })
}

/// Rebuilds the derived index for a run from its chunks.
///
/// Safe to run at any time: entries hold no text and chunks are never rewritten, so
/// regenerating the index cannot lose captured output.
pub fn reparse_run(db: &Db, run_id: RunId) -> Result<usize> {
    let lines = load_lines(db, run_id)?;
    if lines.is_empty() {
        return Ok(0);
    }
    let (mode, entries) = crate::parse::reparse(&lines);

    db.with_mut(|conn| {
        let tx = conn.transaction()?;
        // Roll back this run's contribution first, or the counts double.
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
        tx.execute("DELETE FROM entries WHERE run_id = ?1", [run_id])?;
        tx.execute("DELETE FROM run_fingerprints WHERE run_id = ?1", [run_id])?;

        let mut counters = crate::model::Counters::default();
        for e in &entries {
            let fp_id: i64 = {
                let existing: Option<i64> = tx
                    .query_row("SELECT id FROM fingerprints WHERE hash = ?1", [&e.hash], |r| {
                        r.get(0)
                    })
                    .ok();
                match existing {
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

            let changed = tx.execute(
                "UPDATE run_fingerprints SET count = count + 1, last_at = max(last_at, ?3)
                  WHERE run_id = ?1 AND fingerprint_id = ?2",
                rusqlite::params![run_id, fp_id, e.captured_at],
            )?;
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
        let evicted = enforce(&h.db, 1).unwrap();
        assert!(evicted >= 1);
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
