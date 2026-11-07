//! Retention, eviction and rebuild.
//!
//! History degrades in stages rather than vanishing. Old runs are squashed first: their
//! stored output is rewritten to the first occurrence of each distinct entry, which keeps
//! everything that was ever said while dropping the thousandth identical request line.
//! Only if that is not enough are chunk bytes dropped, then the derived index for those
//! runs. Run rows and their counts cost a few KB each and are kept, so a run's git SHA
//! remains queryable after its output is gone, and its counts still describe the run as
//! captured after it is squashed. Signatures outlive the bytes too, up to a budget tied to
//! the cap; past it the least useful go first, with errors last of all.
//!
//! A run still being written is not squashed or evicted, but it does not get to spend the
//! whole cap either. One noisy server would otherwise push every other run out of history
//! while it held the database over the limit by itself, so past [`LIVE_RUN_SHARE`] of the
//! cap a live run pays for its own growth by losing its oldest output.

use crate::error::Result;
use crate::model::{Counters, RunId};
use crate::parse::{Parser, RawLine};
use crate::proto::{RetentionState, SquashReport, Sweep};
use crate::store::schema::{REBUILD_AFTER, REBUILD_UPTO};
use crate::store::write::{write_batch, Batch};
use crate::store::{get_meta, pack_chunk, read, set_meta, split_chunk, Db};
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashSet;

/// Pages reclaimed per `incremental_vacuum` step. Bounded because a full `VACUUM` would
/// stall the daemon: the writer's lock is released between steps.
const VACUUM_PAGES: i64 = 2048;

/// The share of the cap a live run may fill before it starts losing its own oldest output.
const LIVE_RUN_SHARE: i64 = 2;
/// Chunks a single trim removes, so capture gets the lock back between steps.
const TRIM_CHUNKS: i64 = 32;
/// Evicting stops this far under the cap (as a fraction of it), so the next run does not
/// start another pass straight away.
const EVICT_HEADROOM: i64 = 20;
/// Rounds of "free some, then measure what really came back".
const MAX_ROUNDS: usize = 8;

/// The share of the cap signature tables may fill once their runs' bytes are gone.
const SIGNATURE_SHARE: i64 = 8;
/// Rough cost of a signature with its indexes, in bytes.
const SIGNATURE_BYTES: i64 = 512;
const MIN_SIGNATURES: i64 = 1024;
/// How many run-to-signature rows are kept per signature, on average.
const PAIRS_PER_SIGNATURE: i64 = 4;
/// Rows deleted in one transaction while pruning.
const PRUNE_BATCH: usize = 2048;

/// Runs reparsed from memory at a time, in bytes of stored output.
const REPARSE_BATCH_BYTES: usize = 8 * 1024 * 1024;

pub fn state(db: &Db, cap: i64) -> Result<RetentionState> {
    let (runs_total, runs_evicted, runs_squashed, oldest) = db.read(|c| {
        let (total, evicted) = read::count_runs(c)?;
        let squashed = c.query_row(
            "SELECT count(*) FROM runs WHERE squashed_at IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        let oldest = c.query_row("SELECT min(started_at) FROM runs", [], |r| {
            r.get::<_, Option<i64>>(0)
        })?;
        Ok((total, evicted, squashed, oldest))
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

/// What the project will take up on disk once reclaimed: every page in use, and the shm
/// file, which stays for as long as the database is open. Free pages and the WAL are left
/// out, since a reclaim hands those back.
///
/// This is what a pass steers by. The size on disk does not move until the reclaim at the
/// end, and steering by it is how a pass used to evict far too much.
fn used_bytes(db: &Db) -> Result<i64> {
    let shm = std::fs::metadata(format!("{}-shm", db.path().display())).map_or(0, |m| m.len() as i64);
    db.read(|c| {
        let pages: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let free: i64 = c.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        let size: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok((pages - free) * size + shm)
    })
}

/// Bytes in the FTS index, and how many entries they cover.
///
/// Deleting from a contentless index writes a tombstone and frees nothing until segments
/// merge, so a pass has to allow for what its deletions will free or it evicts on behalf of
/// space that is already as good as gone.
fn fts_weight(db: &Db) -> Result<(i64, i64)> {
    db.read(|c| {
        let bytes: i64 = c.query_row(
            "SELECT COALESCE(sum(pgsize), 0) FROM dbstat WHERE name = 'entries_fts_data'",
            [],
            |r| r.get(0),
        )?;
        let entries: i64 = c.query_row("SELECT count(*) FROM entries", [], |r| r.get(0))?;
        Ok((bytes, entries))
    })
}

/// Merges the FTS index until it has nothing left to merge, which is what turns tombstones
/// into free pages. In steps, with the writer's lock released between them.
fn compact_fts(db: &Db) -> Result<()> {
    for _ in 0..16_384 {
        let changed = db.with(|c| {
            let before = c.total_changes();
            c.execute("INSERT INTO entries_fts(entries_fts, rank) VALUES ('merge', -256)", [])?;
            Ok(c.total_changes() - before)
        })?;
        // The documented signal that a merge had nothing to do.
        if changed < 2 {
            break;
        }
    }
    Ok(())
}

/// Sheds runs oldest first until the database needs no more than `target`.
///
/// `shed_one` removes one run's worth and says how many entries that took, or `None` when
/// nothing is left to remove. Returns how many runs went.
fn shed(db: &Db, target: i64, mut shed_one: impl FnMut() -> Result<Option<i64>>) -> Result<usize> {
    let mut shed_runs = 0;
    for _ in 0..MAX_ROUNDS {
        let (fts_bytes, fts_entries) = fts_weight(db)?;
        let per_entry = fts_bytes.checked_div(fts_entries).unwrap_or(0);
        let mut entries_gone = 0;
        let mut exhausted = false;
        while used_bytes(db)? - entries_gone * per_entry > target {
            match shed_one()? {
                Some(n) => {
                    shed_runs += 1;
                    entries_gone += n;
                }
                None => {
                    exhausted = true;
                    break;
                }
            }
        }
        if entries_gone == 0 {
            break;
        }
        compact_fts(db)?;
        if exhausted || used_bytes(db)? <= target {
            break;
        }
    }
    Ok(shed_runs)
}

/// Brings a project back under its cap, on disk, WAL included.
///
/// Costs are taken in order of how little they lose: free pages and the WAL, then what a
/// noisy live run holds beyond its share, then squashing finished runs, then evicting them.
/// Only when nothing finished is left does a live run give up more than its share.
pub fn enforce(db: &Db, cap: i64) -> Result<Sweep> {
    let mut sweep = Sweep::default();
    if cap <= 0 || db.size_bytes() <= cap {
        return Ok(sweep);
    }
    // A pass already running will finish the job.
    let Some(_pass) = db.try_retention() else {
        return Ok(sweep);
    };

    reclaim_all(db)?;
    if db.size_bytes() <= cap {
        return Ok(sweep);
    }

    let share = cap / LIVE_RUN_SHARE;
    if trim_live_runs(db, share)? > 0 {
        prune_signatures(db, cap)?;
        compact_fts(db)?;
    }

    let squash_target = cap;
    if used_bytes(db)? > squash_target {
        sweep.squashed = shed(db, squash_target, || {
            let Some(run_id) = next_candidate(db, "AND r.squashed_at IS NULL")? else {
                return Ok(None);
            };
            squash_run(db, run_id).map(|n| Some(n as i64))
        })?;
    }

    let evict_target = cap - cap / EVICT_HEADROOM;
    if used_bytes(db)? > cap {
        sweep.evicted = shed(db, evict_target, || {
            let Some(run_id) = next_candidate(db, "")? else {
                return Ok(None);
            };
            let entries = evict_run(db, run_id)?;
            // As it goes: with a template per line, signatures can outweigh the output,
            // and eviction would go on through history for space they are holding.
            prune_after_eviction(db, run_id, cap)?;
            Ok(Some(entries))
        })?;
        prune_signatures(db, cap)?;
    }

    if used_bytes(db)? > cap {
        trim_live_runs(db, 0)?;
        prune_signatures(db, cap)?;
        compact_fts(db)?;
    }

    reclaim_all(db)?;
    db.release_memory();
    Ok(sweep)
}

/// The oldest finished run that still has bytes, narrowed by `extra`.
fn next_candidate(db: &Db, extra: &str) -> Result<Option<RunId>> {
    db.read(|conn| {
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
            .optional()?)
    })
}

/// Stored output of each live run, in bytes, with the database's overall ratio applied so
/// the figure is comparable to the cap.
fn live_run_weights(db: &Db) -> Result<Vec<(RunId, i64, f64)>> {
    let used = used_bytes(db)?;
    db.read(|c| {
        let total: i64 =
            c.query_row("SELECT COALESCE(sum(length(bytes)), 0) FROM chunks", [], |r| r.get(0))?;
        let ratio = if total > 0 { used as f64 / total as f64 } else { 1.0 };
        let mut stmt = c.prepare(
            "SELECT r.id, COALESCE(sum(length(ch.bytes)), 0)
               FROM runs r JOIN chunks ch ON ch.run_id = r.id
              WHERE r.ended_at IS NULL GROUP BY r.id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, ratio)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })
}

/// Holds every live run to `share` of the database, measured on disk, returning how many
/// stored bytes went.
fn trim_live_runs(db: &Db, share: i64) -> Result<i64> {
    let mut total = 0;
    for (run_id, stored, ratio) in live_run_weights(db)? {
        let keep = (share as f64 / ratio) as i64;
        if stored > keep {
            let dropped = trim_run(db, run_id, keep)?;
            if dropped > 0 {
                eprintln!(
                    "logbeard: run {run_id} is larger than its retention share; \
                     dropped its oldest {dropped} bytes"
                );
            }
            total += dropped;
        }
    }
    Ok(total)
}

/// Drops a live run's oldest chunks until `keep` bytes are left, returning how many went.
///
/// Chunks holding an error go last, and the newest chunk never goes: the run the user is
/// watching keeps its tail and whatever went wrong in it for as long as there is any choice.
/// A live run's writer only ever appends, so deleting from the front does not disturb it.
pub fn trim_run(db: &Db, run_id: RunId, keep: i64) -> Result<i64> {
    let mut dropped = 0;
    for keep_errors in [true, false] {
        loop {
            let step = trim_step(db, run_id, keep, keep_errors)?;
            dropped += step;
            if step == 0 {
                break;
            }
        }
    }
    Ok(dropped)
}

fn trim_step(db: &Db, run_id: RunId, keep: i64, keep_errors: bool) -> Result<i64> {
    db.with_mut(|conn| {
        let tx = conn.transaction()?;
        let (total, newest): (i64, Option<i64>) = tx.query_row(
            "SELECT COALESCE(sum(length(bytes)), 0), max(seq) FROM chunks WHERE run_id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut excess = total - keep;
        let Some(newest) = newest.filter(|_| excess > 0) else {
            return Ok(0);
        };

        // Oldest first, and only as many as one step should hold the lock for.
        let candidates: Vec<(i64, i64, i64, i64, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT c.seq, c.first_line, c.n_lines, length(c.bytes), c.captured_at
                   FROM chunks c
                  WHERE c.run_id = ?1 AND c.seq < ?2
                    AND NOT (?4 AND EXISTS (
                          SELECT 1 FROM entries e
                           WHERE e.run_id = c.run_id AND e.is_error = 1
                             AND e.start_line < c.first_line + c.n_lines
                             AND e.end_line >= c.first_line))
                  ORDER BY c.seq LIMIT ?3",
            )?;
            let rows = stmt.query_map(rusqlite::params![run_id, newest, TRIM_CHUNKS, keep_errors], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        let mut dropped = 0;
        for (seq, first_line, n_lines, bytes, captured_at) in candidates {
            if excess <= 0 {
                break;
            }
            let end = first_line + n_lines - 1;
            // Only entries wholly inside the chunk: one that continues into the next
            // chunk keeps what is left of itself. The time bound keeps this to the front of
            // the run's index rather than a walk over all of it.
            let in_chunk = "SELECT id FROM entries
                             WHERE run_id = ?1 AND captured_at <= ?2
                               AND start_line >= ?3 AND end_line <= ?4";
            let params = rusqlite::params![run_id, captured_at + TRIM_TIME_SLACK_MICROS, first_line, end];
            tx.execute(&format!("DELETE FROM entries_fts WHERE rowid IN ({in_chunk})"), params)?;
            tx.execute(&format!("DELETE FROM entries WHERE id IN ({in_chunk})"), params)?;
            tx.execute(
                "DELETE FROM chunks WHERE run_id = ?1 AND seq = ?2",
                rusqlite::params![run_id, seq],
            )?;
            excess -= bytes;
            dropped += bytes;
        }
        tx.commit()?;
        Ok(dropped)
    })
}

/// An entry's time is that of its first line, a chunk's that of its first line, and a batch
/// can sit in the writer for this long, so entries can be stamped this far after their
/// chunk.
const TRIM_TIME_SLACK_MICROS: i64 = 60_000_000;

/// How many signatures a project keeps, by cap.
fn signature_budget(cap: i64) -> i64 {
    (cap / SIGNATURE_SHARE / SIGNATURE_BYTES).max(MIN_SIGNATURES)
}

/// Holds the signature tables to their budget.
///
/// Only signatures with no surviving entries are candidates: those on live entries already
/// count against the cap and go when the entries do. Of the rest, anything that is not an
/// error goes first, least recently seen first. A signature that returns after being pruned
/// reads as new, which is the honest cost of the bound.
pub fn prune_signatures(db: &Db, cap: i64) -> Result<()> {
    let budget = signature_budget(cap);
    let doomed: Vec<i64> = db.read(|c| {
        let excess = signature_count(c)? - budget;
        if excess <= 0 {
            return Ok(Vec::new());
        }
        let mut stmt = c.prepare(
            "SELECT f.id FROM fingerprints f
              WHERE NOT EXISTS (SELECT 1 FROM entries e WHERE e.fingerprint_id = f.id)
              ORDER BY f.level = 'error', f.last_seen LIMIT ?1",
        )?;
        let rows = stmt.query_map([excess], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })?;
    delete_signatures(db, &doomed)?;
    prune_run_pairs(db, budget * PAIRS_PER_SIGNATURE)
}

/// The cheap form of [`prune_signatures`] for one run just evicted: if the budget is spent,
/// what only that run ever said goes with it, unless it was an error. This is for a pass
/// that evicts run after run, where looking over every signature each time would be the
/// slowest part of it.
fn prune_after_eviction(db: &Db, run_id: RunId, cap: i64) -> Result<()> {
    let budget = signature_budget(cap);
    let doomed: Vec<i64> = db.read(|c| {
        if signature_count(c)? <= budget {
            return Ok(Vec::new());
        }
        let mut stmt = c.prepare(
            "SELECT f.id FROM run_fingerprints rf JOIN fingerprints f ON f.id = rf.fingerprint_id
              WHERE rf.run_id = ?1 AND f.level != 'error' AND f.run_count <= 1",
        )?;
        let rows = stmt.query_map([run_id], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })?;
    delete_signatures(db, &doomed)
}

fn signature_count(c: &Connection) -> Result<i64> {
    Ok(c.query_row("SELECT count(*) FROM fingerprints", [], |r| r.get(0))?)
}

/// Deletes signatures and everything that refers to them, a batch at a time.
///
/// Each is checked again under the writer's lock: the list came from a read, and a live run
/// may have repeated one since, which would leave its entry pointing at nothing.
fn delete_signatures(db: &Db, ids: &[i64]) -> Result<()> {
    for batch in ids.chunks(PRUNE_BATCH) {
        db.with_mut(|conn| {
            let tx = conn.transaction()?;
            for id in batch {
                let in_use: bool = tx
                    .prepare_cached("SELECT EXISTS (SELECT 1 FROM entries WHERE fingerprint_id = ?1)")?
                    .query_row([id], |r| r.get(0))?;
                if in_use {
                    continue;
                }
                tx.prepare_cached("DELETE FROM run_fingerprints WHERE fingerprint_id = ?1")?
                    .execute([id])?;
                tx.prepare_cached("DELETE FROM session_blocks WHERE fingerprint_id = ?1")?
                    .execute([id])?;
                tx.prepare_cached("DELETE FROM notice_state WHERE fingerprint_id = ?1")?
                    .execute([id])?;
                tx.prepare_cached("DELETE FROM fingerprints WHERE id = ?1")?.execute([id])?;
            }
            tx.commit()?;
            Ok(())
        })?;
    }
    Ok(())
}

/// Run-to-signature rows grow with signatures times runs, so a steady server with a stable
/// set of templates reaches any count eventually. The oldest evicted runs give up theirs,
/// errors last.
fn prune_run_pairs(db: &Db, budget: i64) -> Result<()> {
    for scope in [
        "AND fingerprint_id NOT IN (SELECT id FROM fingerprints WHERE level = 'error')",
        "",
    ] {
        loop {
            let pairs: i64 = db.read(|c| {
                Ok(c.query_row("SELECT count(*) FROM run_fingerprints", [], |r| r.get(0))?)
            })?;
            if pairs <= budget {
                return Ok(());
            }
            let oldest: Option<RunId> = db.read(|c| {
                Ok(c.query_row(
                    &format!(
                        "SELECT r.id FROM runs r
                          WHERE r.chunks_evicted = 1 AND EXISTS (
                                SELECT 1 FROM run_fingerprints WHERE run_id = r.id {scope})
                          ORDER BY r.started_at LIMIT 1"
                    ),
                    [],
                    |r| r.get(0),
                )
                .optional()?)
            })?;
            let Some(run_id) = oldest else {
                break;
            };
            db.with(|c| {
                c.execute(
                    &format!("DELETE FROM run_fingerprints WHERE run_id = ?1 {scope}"),
                    [run_id],
                )?;
                Ok(())
            })?;
        }
    }
    Ok(())
}

/// Squashes every finished run in the project that has not been squashed yet.
pub fn squash_all(db: &Db) -> Result<SquashReport> {
    let bytes_before = db.size_bytes();
    let ids: Vec<RunId> = db.read(|conn| {
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
    compact_fts(db)?;
    reclaim_all(db)?;
    report.bytes_after = db.size_bytes();
    Ok(report)
}

/// Squashes one run and reclaims the space it freed.
pub fn squash_one(db: &Db, run_id: RunId) -> Result<SquashReport> {
    let bytes_before = db.size_bytes();
    let squashable: bool = db.read(|conn| {
        Ok(conn
            .query_row(
                "SELECT ended_at IS NOT NULL AND chunks_evicted = 0 AND squashed_at IS NULL
                   FROM runs WHERE id = ?1",
                [run_id],
                |r| r.get(0),
            )
            .optional()?
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
    compact_fts(db)?;
    reclaim_all(db)?;
    Ok(SquashReport {
        runs: 1,
        entries_dropped,
        bytes_before,
        bytes_after: db.size_bytes(),
    })
}

/// Lines dropped by a squash, as sorted disjoint ranges with a running total, so a line's
/// new number is arithmetic rather than a map holding every line of the run.
#[derive(Default)]
struct DroppedLines {
    /// `(first, last, dropped before first)`.
    ranges: Vec<(i64, i64, i64)>,
    total: i64,
}

impl DroppedLines {
    fn add(&mut self, first: i64, last: i64) {
        // Entries arrive in line order and do not overlap, but a range must not count a line
        // twice whatever the stored entries say.
        let first = self.ranges.last().map_or(first, |r| first.max(r.1 + 1));
        if last < first {
            return;
        }
        let n = last - first + 1;
        match self.ranges.last_mut() {
            Some(r) if r.1 + 1 == first => r.1 = last,
            _ => self.ranges.push((first, last, self.total)),
        }
        self.total += n;
    }

    fn contains(&self, line: i64) -> bool {
        let i = self.ranges.partition_point(|r| r.1 < line);
        self.ranges.get(i).is_some_and(|r| r.0 <= line)
    }

    /// Lines dropped before `line`, which is the amount it moves up by.
    fn before(&self, line: i64) -> i64 {
        let i = self.ranges.partition_point(|r| r.1 < line);
        match i.checked_sub(1).and_then(|i| self.ranges.get(i)) {
            Some(r) => r.2 + (r.1 - r.0 + 1),
            None => 0,
        }
    }
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
///
/// Reads and rewrites one chunk at a time: a run can be far larger than memory should be
/// allowed to get. Line numbers may have gaps, from output lost to a failed disk or trimmed
/// from a live run, so nothing here assumes a line it looks up is present.
pub fn squash_run(db: &Db, run_id: RunId) -> Result<usize> {
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
            return Ok(0);
        }

        let mut seen: HashSet<(i64, crate::model::Level)> = HashSet::new();
        let mut dropped_lines = DroppedLines::default();
        let mut dropped_ids: Vec<i64> = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT id, start_line, end_line, level, fingerprint_id FROM entries
                  WHERE run_id = ?1 ORDER BY start_line, id",
            )?;
            let mut rows = stmt.query([run_id])?;
            while let Some(r) = rows.next()? {
                let (id, start, end): (i64, i64, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
                let level = crate::model::Level::parse(&r.get::<_, String>(3)?);
                let fp: Option<i64> = r.get(4)?;
                if fp.is_some_and(|fp| !seen.insert((fp, level))) {
                    dropped_ids.push(id);
                    dropped_lines.add(start, end);
                }
            }
        }
        drop(seen);

        if !dropped_ids.is_empty() {
            for id in &dropped_ids {
                tx.prepare_cached("DELETE FROM entries_fts WHERE rowid = ?1")?.execute([id])?;
                tx.prepare_cached("DELETE FROM entries WHERE id = ?1")?.execute([id])?;
            }
            renumber_entries(&tx, run_id, &dropped_lines)?;
            rewrite_chunks(&tx, run_id, &dropped_lines)?;
        }
        tx.execute(
            "UPDATE runs SET squashed_at = ?2 WHERE id = ?1",
            rusqlite::params![run_id, now],
        )?;
        tx.commit()?;
        Ok(dropped_ids.len())
    })
}

/// Moves the surviving entries up over the dropped lines. Every line of a kept entry
/// survives, so both of its ends are in range.
fn renumber_entries(tx: &Connection, run_id: RunId, dropped: &DroppedLines) -> Result<()> {
    let kept: Vec<(i64, i64, i64)> = {
        let mut stmt = tx.prepare("SELECT id, start_line, end_line FROM entries WHERE run_id = ?1")?;
        let rows = stmt.query_map([run_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut update =
        tx.prepare("UPDATE entries SET start_line = ?2, end_line = ?3 WHERE id = ?1")?;
    for (id, start, end) in kept {
        if dropped.before(end) > 0 {
            update.execute(rusqlite::params![id, start - dropped.before(start), end - dropped.before(end)])?;
        }
    }
    Ok(())
}

/// Rewrites a run's chunks without the dropped lines, one chunk in memory at a time.
///
/// Sequence numbers only move down, so the slot a chunk moves to is always already empty.
fn rewrite_chunks(tx: &Connection, run_id: RunId, dropped: &DroppedLines) -> Result<()> {
    let seqs: Vec<i64> = {
        let mut stmt = tx.prepare("SELECT seq FROM chunks WHERE run_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map([run_id], |r| r.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut next_seq = 0i64;
    for seq in seqs {
        let (at, first_line, n_lines, bytes): (i64, i64, i64, Vec<u8>) = tx
            .prepare_cached(
                "SELECT captured_at, first_line, n_lines, bytes FROM chunks
                  WHERE run_id = ?1 AND seq = ?2",
            )?
            .query_row(rusqlite::params![run_id, seq], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
        let survivors: Vec<RawLine> = split_chunk(&bytes, n_lines as usize)
            .into_iter()
            .enumerate()
            .map(|(i, bytes)| (first_line + i as i64, bytes))
            .filter(|(index, _)| !dropped.contains(*index))
            .map(|(index, bytes)| RawLine {
                index,
                captured_at: at,
                bytes,
            })
            .collect();

        tx.prepare_cached("DELETE FROM chunks WHERE run_id = ?1 AND seq = ?2")?
            .execute(rusqlite::params![run_id, seq])?;
        let Some(first) = survivors.first() else {
            continue;
        };
        tx.prepare_cached(
            "INSERT INTO chunks (run_id, seq, captured_at, first_line, n_lines, bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?
        .execute(rusqlite::params![
            run_id,
            next_seq,
            at,
            first.index - dropped.before(first.index),
            survivors.len() as i64,
            pack_chunk(&survivors),
        ])?;
        next_seq += 1;
    }
    Ok(())
}

/// Drops one run's bytes and its derived index, keeping the summary rows. Returns how many
/// entries went.
pub fn evict_run(db: &Db, run_id: RunId) -> Result<i64> {
    db.with_mut(|conn| {
        let tx = conn.transaction()?;
        // FTS rows must go with the entries they describe, or search returns hits whose
        // text can no longer be produced.
        tx.execute(
            "DELETE FROM entries_fts WHERE rowid IN (SELECT id FROM entries WHERE run_id = ?1)",
            [run_id],
        )?;
        tx.execute("DELETE FROM chunks WHERE run_id = ?1", [run_id])?;
        let entries = tx.execute("DELETE FROM entries WHERE run_id = ?1", [run_id])?;
        tx.execute(
            "UPDATE runs SET chunks_evicted = 1 WHERE id = ?1",
            [run_id],
        )?;
        tx.commit()?;
        Ok(entries as i64)
    })
}

/// Removes a run entirely, including its contribution to signature counts.
pub fn delete_run(db: &Db, run_id: RunId) -> Result<()> {
    db.with_mut(|conn| {
        let tx = conn.transaction()?;
        tx.execute(ROLL_BACK_SIGNATURES, [run_id])?;
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

/// Takes one run's counts back out of its signatures.
const ROLL_BACK_SIGNATURES: &str = "UPDATE fingerprints SET
    total_count = max(0, total_count - COALESCE(
      (SELECT count FROM run_fingerprints
        WHERE run_id = ?1 AND fingerprint_id = fingerprints.id), 0)),
    run_count = max(0, run_count - COALESCE(
      (SELECT 1 FROM run_fingerprints
        WHERE run_id = ?1 AND fingerprint_id = fingerprints.id), 0))
  WHERE id IN (SELECT fingerprint_id FROM run_fingerprints WHERE run_id = ?1)";

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
/// interleave, then empties the WAL: until both are done the files are no smaller. The pages
/// moved here pass through the WAL, so the order matters.
pub fn reclaim_all(db: &Db) -> Result<()> {
    for _ in 0..65_536 {
        let free: i64 =
            db.with(|conn| Ok(conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?))?;
        if free == 0 {
            break;
        }
        reclaim(db)?;
    }
    db.checkpoint_truncate()
}

/// Rebuilds the derived index for a run from its chunks.
///
/// Safe to run at any time: entries hold no text and chunks are only ever rewritten by
/// squashing, which keeps one copy of everything, so regenerating the index cannot lose
/// captured output.
///
/// A squashed run's bytes no longer hold its repeats, so its counts are left as recorded
/// rather than recounted from what survives: only the entries themselves are rebuilt. The
/// exception is a run with no counters at all, which an older rebuild could leave behind.
/// What survives is then the only evidence, and it is what keeps a run that logged errors
/// from reading as clean.
///
/// Works through the run in batches with the writer's lock released between them, so
/// capture carries on and a large run costs a batch of memory rather than its own size. If
/// that is interrupted the run is left marked, and the next open picks it up again.
pub fn reparse_run(db: &Db, run_id: RunId) -> Result<usize> {
    // With no bytes there is nothing to rebuild from, and what the run has recorded is all
    // that is left of it. Output with holes gets the same care as a squashed run: what
    // survives is not what the run produced.
    let (lines, last_line): (i64, i64) = db.read(|conn| {
        Ok(conn.query_row(
            "SELECT COALESCE(sum(n_lines), 0), COALESCE(max(first_line + n_lines), 0)
               FROM chunks WHERE run_id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    })?;
    if lines == 0 {
        return Ok(0);
    }

    let (squashed, keep_counters, marked) = db.with_mut(|conn| {
        let tx = conn.transaction()?;
        let squashed: bool = lines < last_line
            || tx.query_row(
                "SELECT squashed_at IS NOT NULL FROM runs WHERE id = ?1",
                [run_id],
                |r| r.get(0),
            )?;
        let has_counters: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM run_counters WHERE run_id = ?1)",
            [run_id],
            |r| r.get(0),
        )?;
        // Inside a full rebuild the rebuild's own marks already cover this run.
        let mark = get_meta(&tx, REBUILD_UPTO).is_none();
        if mark {
            set_meta(&tx, REBUILD_UPTO, &run_id.to_string())?;
            set_meta(&tx, REBUILD_AFTER, &(run_id - 1).to_string())?;
        }
        // Roll back this run's contribution first, or the counts double. A squashed run
        // keeps its recorded counts, so there is nothing to roll back.
        if !squashed {
            tx.execute(ROLL_BACK_SIGNATURES, [run_id])?;
            tx.execute("DELETE FROM run_fingerprints WHERE run_id = ?1", [run_id])?;
        }
        tx.execute(
            "DELETE FROM entries_fts WHERE rowid IN (SELECT id FROM entries WHERE run_id = ?1)",
            [run_id],
        )?;
        tx.execute("DELETE FROM entries WHERE run_id = ?1", [run_id])?;
        tx.commit()?;
        Ok((squashed, squashed && has_counters, mark))
    })?;

    let seqs: Vec<i64> = db.read(|conn| {
        let mut stmt = conn.prepare("SELECT seq FROM chunks WHERE run_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map([run_id], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })?;

    let mut parser = Parser::new();
    let mut counters = Counters::default();
    let mut total = 0;
    let mut pending = Vec::new();
    let mut pending_bytes = 0usize;
    let mut last_at = 0;
    let mut next_line = 0;
    for seq in seqs {
        let (at, first_line, n_lines, bytes): (i64, i64, i64, Vec<u8>) = db.read(|conn| {
            Ok(conn.query_row(
                "SELECT captured_at, first_line, n_lines, bytes FROM chunks
                  WHERE run_id = ?1 AND seq = ?2",
                rusqlite::params![run_id, seq],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?)
        })?;
        // A gap is output that was lost or trimmed; the numbering has to carry on after it.
        if first_line != next_line {
            parser.skip_to(first_line);
        }
        next_line = first_line + n_lines;
        last_at = at;
        pending_bytes += bytes.len();
        for mut line in split_chunk(&bytes, n_lines as usize) {
            line.push(b'\n');
            pending.extend(parser.push(&line, at).entries);
        }
        if pending_bytes >= REPARSE_BATCH_BYTES {
            total += write_reparsed(db, run_id, &pending, !squashed, keep_counters, &mut counters)?;
            pending.clear();
            pending_bytes = 0;
        }
    }
    pending.extend(parser.finish(last_at).entries);
    total += write_reparsed(db, run_id, &pending, !squashed, keep_counters, &mut counters)?;

    let mode = parser.mode();
    db.with_mut(|conn| {
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE runs SET delimiter_mode = ?2 WHERE id = ?1",
            rusqlite::params![run_id, mode.as_str()],
        )?;
        if marked {
            tx.execute(
                "DELETE FROM meta WHERE key IN (?1, ?2)",
                [REBUILD_UPTO, REBUILD_AFTER],
            )?;
        }
        tx.commit()?;
        Ok(())
    })?;
    Ok(total)
}

fn write_reparsed(
    db: &Db,
    run_id: RunId,
    entries: &[crate::parse::ParsedEntry],
    recount: bool,
    keep_counters: bool,
    counters: &mut Counters,
) -> Result<usize> {
    db.with_mut(|conn| {
        let tx = conn.transaction()?;
        write_batch(
            &tx,
            Batch {
                run_id,
                chunk: None,
                mode: None,
                entries,
                counters: if keep_counters { None } else { Some(counters) },
                recount,
            },
        )?;
        tx.commit()?;
        Ok(entries.len())
    })
}

pub fn reparse_all(db: &Db) -> Result<usize> {
    let ids: Vec<RunId> = db.read(|conn| {
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

/// Rebuilds the derived layer after [`Db::derived_stale`] reports it is needed.
///
/// Reparses every run the rebuild covers, recording each as it finishes, so a crash resumes
/// where it stopped rather than leaving the rest empty. A rebuild that is finished, or was
/// never needed, does nothing.
///
/// Then parks each agent session's watermark. Entry ids do not survive a rebuild, so a stale
/// watermark would either replay the whole history as unread or skip past genuinely new
/// errors. Runs newer than the rebuild have been writing alongside it, so the watermark goes
/// just below their first entry rather than above their errors.
///
/// [`Db::derived_stale`]: crate::store::Db::derived_stale
pub fn rebuild_derived(db: &Db) -> Result<usize> {
    let marks = db.read(|conn| {
        let mark = |key| get_meta(conn, key).and_then(|v| v.parse::<i64>().ok());
        Ok(mark(REBUILD_UPTO).map(|upto| (upto, mark(REBUILD_AFTER).unwrap_or(0))))
    })?;
    let Some((upto, after)) = marks else {
        return Ok(0);
    };

    let ids: Vec<RunId> = db.read(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id FROM runs WHERE chunks_evicted = 0 AND id > ?1 AND id <= ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map([after, upto], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })?;
    let mut entries = 0;
    for id in ids {
        entries += reparse_run(db, id)?;
        db.with(|conn| set_meta(conn, REBUILD_AFTER, &id.to_string()))?;
    }

    db.with_mut(|conn| {
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE session_cursors
                SET last_entry_id = COALESCE((SELECT min(id) - 1 FROM entries WHERE run_id > ?1),
                                             (SELECT max(id) FROM entries), 0),
                    nudge_entry_id = COALESCE((SELECT min(id) - 1 FROM entries WHERE run_id > ?1),
                                              (SELECT max(id) FROM entries), 0)",
            [upto],
        )?;
        // Signatures whose hash the new parser no longer produces. Anything still counted
        // belongs to a run whose bytes are gone, and stays.
        tx.execute(
            "DELETE FROM fingerprints
              WHERE total_count = 0 AND run_count = 0
                AND id NOT IN (SELECT fingerprint_id FROM run_fingerprints)
                AND id NOT IN (SELECT fingerprint_id FROM entries WHERE fingerprint_id IS NOT NULL)",
            [],
        )?;
        tx.execute(
            "DELETE FROM meta WHERE key IN (?1, ?2)",
            [REBUILD_UPTO, REBUILD_AFTER],
        )?;
        tx.commit()?;
        Ok(())
    })?;
    Ok(entries)
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

    /// Enough distinct text that nothing squashes away.
    fn distinct(tag: &str, lines: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..lines {
            out.extend_from_slice(
                format!("[info] {tag} {} {}\n", words(i), "padding ".repeat(12)).as_bytes(),
            );
        }
        out
    }

    /// Text a template cannot collapse: numbers are normalised away, words are not.
    fn words(mut n: usize) -> String {
        let mut out = String::new();
        for _ in 0..6 {
            out.push((b'a' + (n % 26) as u8) as char);
            n /= 26;
        }
        out
    }

    fn settled_size(h: &H) -> i64 {
        reclaim_all(&h.db).unwrap();
        h.db.size_bytes()
    }

    #[test]
    fn enforcing_a_cap_ends_near_it_with_the_wal_included() {
        let h = h();
        let mut runs = Vec::new();
        for r in 0..10 {
            runs.push(run(&h, &distinct(&format!("run{r}"), 4000)));
        }
        let full = settled_size(&h);
        let cap = full / 2;

        enforce(&h.db, cap).unwrap();

        let size = h.db.size_bytes();
        assert!(size <= cap, "{size} on disk against a cap of {cap}");
        assert!(size >= cap / 2, "over-evicted: {size} left of a cap of {cap}");
        let wal = std::fs::metadata(format!("{}-wal", h.db.path().display())).map_or(0, |m| m.len());
        assert!(wal <= 4096, "the WAL was left at {wal} bytes");

        let evicted = runs
            .iter()
            .filter(|id| read::get_run(&h.db, **id).unwrap().chunks_evicted)
            .count();
        assert!((1..=7).contains(&evicted), "evicted {evicted} of 10 to halve the size");
        assert!(
            !read::get_run(&h.db, *runs.last().unwrap()).unwrap().chunks_evicted,
            "the newest run went while older ones stayed"
        );
    }

    #[test]
    fn free_space_is_reclaimed_before_anything_is_given_up() {
        let h = h();
        let old = run(&h, &distinct("old", 4000));
        let new = run(&h, &distinct("new", 4000));
        let keeps = run(&h, b"[info] small\n");
        reclaim_all(&h.db).unwrap();
        evict_run(&h.db, old).unwrap();
        let cap = h.db.size_bytes() * 3 / 4;

        // The eviction's pages are free already, so the cap is met without touching a run.
        assert_eq!(enforce(&h.db, cap).unwrap(), Sweep::default());
        assert!(!read::get_run(&h.db, new).unwrap().chunks_evicted);
        assert!(!read::get_run(&h.db, keeps).unwrap().chunks_evicted);
    }

    #[test]
    fn a_noisy_live_run_pays_for_itself_before_history_does() {
        let h = h();
        let history = run(&h, &distinct("history", 2000));
        reclaim_all(&h.db).unwrap();
        let cap = h.db.size_bytes() * 3;

        let mut w = Writer::start(&h.db, &h.index, &RunStamp::default(), Arc::new(Redactor::disabled()))
            .unwrap();
        for batch in 0..40 {
            w.push(&distinct(&format!("noise{batch}"), 400), crate::now_micros()).unwrap();
            w.flush().unwrap();
        }
        assert!(h.db.size_bytes() > cap, "setup: the live run has blown the cap");

        enforce(&h.db, cap).unwrap();

        assert!(h.db.size_bytes() <= cap, "{} against {cap}", h.db.size_bytes());
        let r = read::get_run(&h.db, history).unwrap();
        assert!(!r.chunks_evicted && r.squashed_at.is_none(), "history paid for a live run");

        // The live run lost its head and kept its tail, and carries on writing.
        let live = w.run_id();
        let first: i64 = h.db.with(|c| {
            Ok(c.query_row("SELECT min(first_line) FROM chunks WHERE run_id = ?1", [live], |r| r.get(0))?)
        })
        .unwrap();
        assert!(first > 0, "nothing was trimmed");
        w.push(&distinct("tail", 400), crate::now_micros()).unwrap();
        w.finish(&h.index, Some(0), None).unwrap();
        assert!(texts(&h, live).last().unwrap().contains("tail"));
        assert!(!read::get_run(&h.db, live).unwrap().chunks_evicted);
    }

    #[test]
    fn trimming_keeps_a_live_runs_errors_while_there_is_any_choice() {
        let h = h();
        let mut w = Writer::start(&h.db, &h.index, &RunStamp::default(), Arc::new(Redactor::disabled()))
            .unwrap();
        w.push(&distinct("early", 100), crate::now_micros()).unwrap();
        w.flush().unwrap();
        w.push(b"[error] the one that matters\n", crate::now_micros()).unwrap();
        w.flush().unwrap();
        for batch in 0..4 {
            w.push(&distinct(&format!("later{batch}"), 100), crate::now_micros()).unwrap();
            w.flush().unwrap();
        }

        trim_run(&h.db, w.run_id(), 30_000).unwrap();

        let id = w.run_id();
        let errors = read::entries(
            &h.db,
            &EntryFilter { run_id: Some(id), errors_only: true, ..Default::default() },
        )
        .unwrap();
        assert_eq!(errors.len(), 1, "the error went with the noise around it");
        assert_eq!(errors[0].text, "[error] the one that matters");
        let all = texts(&h, id);
        assert!(!all.iter().any(|t| t.contains("early")), "the oldest noise should have gone");
        let hits = read::entries(
            &h.db,
            &EntryFilter { search: Some("early".into()), ..Default::default() },
        )
        .unwrap();
        assert!(hits.is_empty(), "search must not outlive the text");
    }

    #[test]
    fn signatures_of_evicted_runs_are_bounded_and_errors_go_last() {
        let h = h();
        let error = run(&h, b"[error] keep me\n");
        let mut output = distinct("sig", 3000);
        output.extend_from_slice(b"[info] done\n");
        let noisy = run(&h, &output);
        evict_run(&h.db, error).unwrap();
        evict_run(&h.db, noisy).unwrap();
        let count = |h: &H, table: &str| -> i64 {
            h.db.read(|c| Ok(c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?))
                .unwrap()
        };
        assert!(count(&h, "fingerprints") > 3000, "setup: templates survive eviction");

        // A cap whose budget sits at the floor.
        prune_signatures(&h.db, 1).unwrap();

        assert_eq!(count(&h, "fingerprints"), MIN_SIGNATURES);
        let kept = read::fingerprints(&h.db, 5000, true).unwrap();
        assert_eq!(kept.len(), 1, "an error's signature was pruned ahead of noise");
        assert_eq!(read::signatures_for_run(&h.db, error).unwrap().len(), 1);
    }

    #[test]
    fn a_signature_a_live_run_has_just_repeated_is_not_pruned_from_under_it() {
        // The list of candidates comes from a read, so it can be stale by the time the delete
        // runs. Under load that failed the whole pass with a foreign key error.
        let h = h();
        let id = run(&h, b"[info] said once\n");
        let fp = read::signatures_for_run(&h.db, id).unwrap()[0].0;
        delete_signatures(&h.db, &[fp]).unwrap();
        assert_eq!(texts(&h, id), ["[info] said once"]);
        assert_eq!(read::signatures_for_run(&h.db, id).unwrap().len(), 1);
    }

    #[test]
    fn signatures_on_surviving_entries_are_never_pruned() {
        let h = h();
        let id = run(&h, &distinct("sig", 2000));
        prune_signatures(&h.db, 1).unwrap();
        let found: i64 = h.db.read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM entries WHERE fingerprint_id IS NOT NULL
                  AND fingerprint_id NOT IN (SELECT id FROM fingerprints)",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
        assert_eq!(found, 0);
        assert_eq!(texts(&h, id).len(), 2000);
    }

    #[test]
    fn a_squash_with_a_gap_in_the_lines_does_not_panic_and_stays_consistent() {
        // How the panic got there: a failed flush lost lines but not the entries after them,
        // and the rewrite looked every line up in a map of those it had seen. Run outside any
        // test, that also poisoned the writer's mutex for the life of the daemon.
        let h = h();
        let id = run(&h, &noisy());
        h.db.with(|c| {
            let (seq, first): (i64, i64) = c.query_row(
                "SELECT seq, first_line FROM chunks WHERE run_id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            // Drop the chunk and put back only its tail, a chunk's worth of lines lost.
            let bytes: Vec<u8> = c.query_row(
                "SELECT bytes FROM chunks WHERE run_id = ?1 AND seq = ?2", (id, seq), |r| r.get(0),
            )?;
            let lines = split_chunk(&bytes, 0);
            let tail: Vec<RawLine> = lines[100..]
                .iter()
                .enumerate()
                .map(|(i, l)| RawLine { index: first + 100 + i as i64, captured_at: 1, bytes: l.clone() })
                .collect();
            c.execute("DELETE FROM chunks WHERE run_id = ?1", [id])?;
            c.execute(
                "INSERT INTO chunks VALUES (?1, 0, 1, ?2, ?3, ?4)",
                rusqlite::params![id, first + 100, tail.len() as i64, pack_chunk(&tail)],
            )?;
            Ok(())
        })
        .unwrap();

        squash_run(&h.db, id).unwrap();

        let shown = texts(&h, id);
        assert!(shown.iter().any(|t| t.contains("shutting down")), "{shown:?}");
        reparse_run(&h.db, id).unwrap();
        assert!(texts(&h, id).iter().any(|t| t.contains("shutting down")));
    }

    #[test]
    fn reparsing_output_with_a_gap_keeps_entries_on_their_own_lines() {
        let h = h();
        let mut w = Writer::start(&h.db, &h.index, &RunStamp::default(), Arc::new(Redactor::disabled()))
            .unwrap();
        for i in 0..6 {
            w.push(format!("[info] line {i}\n").as_bytes(), crate::now_micros()).unwrap();
            w.tick(crate::now_micros() + 10_000_000).unwrap();
            w.flush().unwrap();
        }
        w.finish(&h.index, Some(0), None).unwrap();
        let id = w.run_id();
        trim_run(&h.db, id, 40).unwrap();
        reparse_run(&h.db, id).unwrap();
        for e in read::entries(&h.db, &EntryFilter { run_id: Some(id), ..Default::default() }).unwrap() {
            let line: i64 = e.text.rsplit(' ').next().unwrap().parse().unwrap();
            assert_eq!(e.entry.start_line, line, "{}", e.text);
        }
    }

    #[test]
    fn reparsing_a_run_with_no_bytes_leaves_what_it_recorded() {
        let h = h();
        let id = run(&h, b"[error] boom\n");
        evict_run(&h.db, id).unwrap();
        assert_eq!(reparse_run(&h.db, id).unwrap(), 0);
        assert_eq!(read::get_run(&h.db, id).unwrap().counters.n_error, 1);
        assert_eq!(read::signatures_for_run(&h.db, id).unwrap().len(), 1);
    }

    #[test]
    fn reparsing_a_trimmed_run_keeps_the_counts_of_what_it_lost() {
        let h = h();
        let mut w = Writer::start(&h.db, &h.index, &RunStamp::default(), Arc::new(Redactor::disabled()))
            .unwrap();
        for i in 0..6 {
            w.push(format!("[error] number {}\n", words(i)).as_bytes(), crate::now_micros()).unwrap();
            w.tick(crate::now_micros() + 10_000_000).unwrap();
            w.flush().unwrap();
        }
        w.finish(&h.index, Some(1), None).unwrap();
        let id = w.run_id();
        trim_run(&h.db, id, 40).unwrap();
        reparse_run(&h.db, id).unwrap();
        assert_eq!(read::get_run(&h.db, id).unwrap().counters.n_error, 6);
    }

    #[test]
    fn a_large_run_reparses_with_the_same_result_as_a_small_one() {
        // Batches are a memory bound and must not show in the outcome.
        let h = h();
        let id = run(&h, &noisy().repeat(30));
        let before = read::get_run(&h.db, id).unwrap().counters;
        assert_eq!(reparse_run(&h.db, id).unwrap() as i64, before.n_entries);
        let after = read::get_run(&h.db, id).unwrap().counters;
        assert_eq!(after.n_entries, before.n_entries);
        assert_eq!(after.n_error, before.n_error);
    }

    #[test]
    fn an_interrupted_reparse_of_one_run_is_finished_by_the_next_open() {
        let h = h();
        let id = run(&h, b"[error] boom\n");
        // What a crash inside `reparse_run` leaves: the entries gone, the marks set.
        h.db.with(|c| {
            crate::store::set_meta(c, REBUILD_UPTO, &id.to_string())?;
            crate::store::set_meta(c, REBUILD_AFTER, &(id - 1).to_string())?;
            c.execute("DELETE FROM entries_fts WHERE rowid IN (SELECT id FROM entries)", [])?;
            c.execute("DELETE FROM entries", [])?;
            Ok(())
        })
        .unwrap();

        let reopened = Db::open(h.db.path(), h.db.project_id()).unwrap();
        assert!(reopened.derived_stale());
        assert_eq!(rebuild_derived(&reopened).unwrap(), 1);
        assert_eq!(read::errors_since(&reopened, 0).unwrap(), 1);
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
