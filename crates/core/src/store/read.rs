//! Typed reads.
//!
//! Every query shape the rest of the workspace needs lives here, so they can be reviewed
//! in one place instead of being spread across the binaries and the UI.

use crate::error::{Error, Result};
use crate::model::*;
use crate::proto::{AckKind, EntryFilter, RunRef, UnreadState};
use crate::store::{render_range, Db};
use rusqlite::{Connection, OptionalExtension};

const RUN_COLUMNS: &str = "r.id, r.project_id, r.command, r.cwd, r.session_group, r.started_at,
                           r.ended_at, r.exit_code, r.signal, r.git_sha, r.git_branch,
                           r.git_dirty_hash, r.lockfile_hashes, r.env_keys, r.term_cols,
                           r.delimiter_mode, r.chunks_evicted,
                           COALESCE(c.n_entries,0), COALESCE(c.n_error,0), COALESCE(c.n_warn,0),
                           COALESCE(c.n_info,0), COALESCE(c.n_debug,0), COALESCE(c.n_trace,0),
                           COALESCE(c.n_unknown,0)";

const RUN_FROM: &str = "FROM runs r LEFT JOIN run_counters c ON c.run_id = r.id";

fn map_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: r.get(0)?,
        project_id: r.get(1)?,
        command: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
        cwd: r.get(3)?,
        session_group: r.get(4)?,
        started_at: r.get(5)?,
        ended_at: r.get(6)?,
        exit_code: r.get(7)?,
        signal: r.get(8)?,
        git_sha: r.get(9)?,
        git_branch: r.get(10)?,
        git_dirty_hash: r.get(11)?,
        lockfile_hashes: serde_json::from_str(&r.get::<_, String>(12)?).unwrap_or_default(),
        env_keys: serde_json::from_str(&r.get::<_, String>(13)?).unwrap_or_default(),
        term_cols: r.get::<_, i64>(14)? as u16,
        delimiter_mode: DelimiterMode::parse(&r.get::<_, String>(15)?),
        chunks_evicted: r.get::<_, i64>(16)? != 0,
        counters: Counters {
            n_entries: r.get(17)?,
            n_error: r.get(18)?,
            n_warn: r.get(19)?,
            n_info: r.get(20)?,
            n_debug: r.get(21)?,
            n_trace: r.get(22)?,
            n_unknown: r.get(23)?,
        },
    })
}

pub fn get_run(db: &Db, run_id: RunId) -> Result<Run> {
    db.with(|conn| {
        conn.query_row(
            &format!("SELECT {RUN_COLUMNS} {RUN_FROM} WHERE r.id = ?1"),
            [run_id],
            map_run,
        )
        .optional()?
        .ok_or(Error::NoSuchRun(run_id))
    })
}

pub fn list_runs(db: &Db, limit: usize, offset: usize) -> Result<Vec<Run>> {
    db.with(|conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} {RUN_FROM} ORDER BY r.started_at DESC LIMIT ?1 OFFSET ?2"
        ))?;
        let rows = stmt.query_map([limit as i64, offset as i64], map_run)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })
}

pub fn active_runs(db: &Db) -> Result<Vec<Run>> {
    db.with(|conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} {RUN_FROM} WHERE r.ended_at IS NULL ORDER BY r.started_at DESC"
        ))?;
        let rows = stmt.query_map([], map_run)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })
}

/// Resolves a symbolic run reference.
///
/// `LastGood` requires both a zero exit code and no logged errors, so a run that exited
/// cleanly while reporting failures does not qualify.
pub fn resolve_run(db: &Db, r: &RunRef) -> Result<Option<Run>> {
    let sql = match r {
        RunRef::Id(id) => {
            return get_run(db, *id).map(Some).or_else(|e| match e {
                Error::NoSuchRun(_) => Ok(None),
                other => Err(other),
            })
        }
        RunRef::Latest => format!("SELECT {RUN_COLUMNS} {RUN_FROM} ORDER BY r.started_at DESC LIMIT 1"),
        RunRef::LastGood => format!(
            "SELECT {RUN_COLUMNS} {RUN_FROM}
              WHERE r.ended_at IS NOT NULL AND r.exit_code = 0 AND COALESCE(c.n_error,0) = 0
              ORDER BY r.started_at DESC LIMIT 1"
        ),
        RunRef::LastBad => format!(
            "SELECT {RUN_COLUMNS} {RUN_FROM}
              WHERE r.ended_at IS NOT NULL AND (r.exit_code != 0 OR COALESCE(c.n_error,0) > 0)
              ORDER BY r.started_at DESC LIMIT 1"
        ),
    };
    db.with(|conn| Ok(conn.query_row(&sql, [], map_run).optional()?))
}

/// Fetches entries with their rendered text.
///
/// Text is produced by re-rendering the stored line range, so a returned entry is always a
/// whole group rather than a slice of one.
pub fn entries(db: &Db, f: &EntryFilter) -> Result<Vec<EntryView>> {
    let mut sql = String::from(
        "SELECT e.id, e.run_id, e.start_line, e.end_line, e.captured_at, e.level, e.evidence,
                e.is_error, e.error_reason, e.fingerprint_id, e.boundaries, fp.template,
                (fp.first_seen = e.captured_at) AS is_new
           FROM entries e LEFT JOIN fingerprints fp ON fp.id = e.fingerprint_id",
    );
    let mut wheres: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(search) = f.search.as_ref().filter(|s| !s.trim().is_empty()) {
        sql.push_str(" JOIN entries_fts ON entries_fts.rowid = e.id");
        wheres.push("entries_fts MATCH ?".into());
        params.push(Box::new(search.clone()));
    }
    if let Some(run_id) = f.run_id {
        wheres.push("e.run_id = ?".into());
        params.push(Box::new(run_id));
    }
    if f.errors_only {
        wheres.push("e.is_error = 1".into());
    }
    if f.novel_only {
        // The signature's first sighting in this project is this entry. Independent of
        // severity, since ad-hoc print output carries no level to filter on.
        wheres.push("fp.first_seen = e.captured_at".into());
    }
    if let Some(level) = f.level {
        wheres.push("e.level = ?".into());
        params.push(Box::new(level.as_str().to_string()));
    }
    if let Some(since) = f.since {
        wheres.push("e.captured_at > ?".into());
        params.push(Box::new(since));
    }
    if let Some(after) = f.after_entry {
        wheres.push("e.id > ?".into());
        params.push(Box::new(after));
    }
    if !wheres.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&wheres.join(" AND "));
    }
    sql.push_str(if f.ascending {
        " ORDER BY e.id ASC"
    } else {
        " ORDER BY e.id DESC"
    });
    sql.push_str(" LIMIT ? OFFSET ?");
    params.push(Box::new(f.limit.clamp(1, 5000) as i64));
    params.push(Box::new(f.offset as i64));

    db.with(|conn| {
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| {
            Ok((
                Entry {
                    id: r.get(0)?,
                    run_id: r.get(1)?,
                    start_line: r.get(2)?,
                    end_line: r.get(3)?,
                    captured_at: r.get(4)?,
                    level: Level::parse(&r.get::<_, String>(5)?),
                    evidence: Evidence::parse(&r.get::<_, String>(6)?),
                    is_error: r.get::<_, i64>(7)? != 0,
                    error_reason: r.get(8)?,
                    fingerprint_id: r.get(9)?,
                    boundaries: match r.get::<_, String>(10)?.as_str() {
                        "exact" => Boundaries::Exact,
                        _ => Boundaries::Inferred,
                    },
                },
                r.get::<_, Option<String>>(11)?,
                r.get::<_, Option<i64>>(12)?.unwrap_or(0) != 0,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (entry, template, is_new) = row?;
            let text = render_range(conn, entry.run_id, entry.start_line, entry.end_line)?;
            out.push(EntryView {
                entry,
                text,
                template,
                is_new_signature: is_new,
            });
        }
        Ok(out)
    })
}

pub fn fingerprints(db: &Db, limit: usize, errors_only: bool) -> Result<Vec<Fingerprint>> {
    let filter = if errors_only { "WHERE level = 'error'" } else { "" };
    db.with(|conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT id, template, hash, level, first_seen, last_seen, total_count, run_count
               FROM fingerprints {filter}
              ORDER BY last_seen DESC, total_count DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(Fingerprint {
                id: r.get(0)?,
                template: r.get(1)?,
                hash: r.get(2)?,
                level: Level::parse(&r.get::<_, String>(3)?),
                first_seen: r.get(4)?,
                last_seen: r.get(5)?,
                total_count: r.get(6)?,
                run_count: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })
}

/// Occurrences of one signature across recent runs, oldest first.
///
/// Runs where the signature did not occur are included with a count of zero.
pub fn fingerprint_history(
    db: &Db,
    fingerprint_id: FingerprintId,
    limit: usize,
) -> Result<Vec<(RunId, i64, i64)>> {
    db.with(|conn| {
        let mut stmt = conn.prepare(
            "SELECT r.id, COALESCE(rf.count, 0), r.started_at
               FROM runs r
               LEFT JOIN run_fingerprints rf ON rf.run_id = r.id AND rf.fingerprint_id = ?1
              ORDER BY r.started_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map([fingerprint_id, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        let mut v = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        v.reverse();
        Ok(v)
    })
}

pub fn errors_since(db: &Db, since: i64) -> Result<i64> {
    db.with(|conn| {
        Ok(conn.query_row(
            "SELECT count(*) FROM entries WHERE is_error = 1 AND captured_at > ?1",
            [since],
            |r| r.get(0),
        )?)
    })
}

pub fn signatures_for_run(db: &Db, run_id: RunId) -> Result<Vec<(FingerprintId, String, Level, i64)>> {
    db.with(|conn| {
        let mut stmt = conn.prepare(
            "SELECT f.id, f.template, f.level, rf.count
               FROM run_fingerprints rf JOIN fingerprints f ON f.id = rf.fingerprint_id
              WHERE rf.run_id = ?1 ORDER BY rf.count DESC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                Level::parse(&r.get::<_, String>(2)?),
                r.get(3)?,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })
}

/// A representative rendered entry for a signature.
pub fn sample_for_fingerprint(db: &Db, fp: FingerprintId, run_id: Option<RunId>) -> Result<Option<String>> {
    db.with(|conn| {
        let row: Option<(RunId, i64, i64)> = match run_id {
            Some(run) => conn
                .query_row(
                    "SELECT run_id, start_line, end_line FROM entries
                      WHERE fingerprint_id = ?1 AND run_id = ?2 ORDER BY id DESC LIMIT 1",
                    rusqlite::params![fp, run],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?,
            None => conn
                .query_row(
                    "SELECT run_id, start_line, end_line FROM entries
                      WHERE fingerprint_id = ?1 ORDER BY id DESC LIMIT 1",
                    [fp],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?,
        };
        match row {
            Some((run, start, end)) => Ok(Some(render_range(conn, run, start, end)?)),
            None => Ok(None),
        }
    })
}

// ---------------------------------------------------------------------------
// Agent session state
// ---------------------------------------------------------------------------

/// What this session has not been told about yet.
///
/// State is per session, never global, so concurrent agent sessions on one project do not
/// consume each other's notifications.
pub fn unread(
    db: &Db,
    session_id: &str,
    harness: &str,
    block_budget: i64,
    change_block_budget: i64,
    max_entries: usize,
) -> Result<UnreadState> {
    let cursor = ensure_cursor(db, session_id, harness)?;
    // `last_seen_at` is how read receipts find the sessions that are actually present:
    // the hook calls unread on every event, so a live turn keeps its cursor fresh.
    db.with(|conn| {
        conn.execute(
            "UPDATE session_cursors SET last_seen_at = ?2 WHERE session_id = ?1",
            rusqlite::params![session_id, crate::now_micros()],
        )?;
        Ok(())
    })?;
    let filter = EntryFilter {
        errors_only: true,
        after_entry: Some(cursor.last_entry_id),
        limit: max_entries,
        ascending: true,
        ..Default::default()
    };
    let entries = entries(db, &filter)?;

    let (count, oldest, newest) = db.with(|conn| {
        let row: (i64, Option<i64>, Option<i64>) = conn.query_row(
            "SELECT count(*), min(captured_at), max(id) FROM entries
              WHERE is_error = 1 AND id > ?1",
            [cursor.last_entry_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(row)
    })?;

    let new_signatures = db.with(|conn| {
        Ok(conn.query_row(
            // NULL fingerprints count as new: an error that failed fingerprinting cannot
            // be matched against anything this session has seen, and the safe direction
            // for an unidentifiable error is to surface it. COALESCE onto the negated
            // entry id keeps each such entry distinct without colliding with real ids.
            "SELECT count(DISTINCT COALESCE(e.fingerprint_id, -e.id)) FROM entries e
              WHERE e.is_error = 1 AND e.id > ?1
                AND (e.fingerprint_id IS NULL
                     OR e.fingerprint_id NOT IN (SELECT fingerprint_id FROM session_blocks
                                                  WHERE session_id = ?2))",
            rusqlite::params![cursor.last_entry_id, session_id],
            |r| r.get::<_, i64>(0),
        )?)
    })?;

    // Every level, not just errors: the nudge exists because a change can go wrong without
    // logging anything the daemon would classify as an error.
    let unseen_lines = db.with(|conn| {
        Ok(conn.query_row(
            "SELECT count(*) FROM entries WHERE id > ?1",
            [cursor.nudge_entry_id],
            |r| r.get::<_, i64>(0),
        )?)
    })?;

    let server_running = db.with(|conn| {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE ended_at IS NULL)",
            [],
            |r| r.get::<_, bool>(0),
        )?)
    })?;

    Ok(UnreadState {
        new_errors: count,
        new_signatures,
        oldest_at: oldest,
        newest_entry_id: newest,
        blocks_issued: cursor.blocks_issued,
        blocks_remaining: (block_budget - cursor.blocks_issued).max(0),
        change_blocks_remaining: (change_block_budget - cursor.change_blocks_issued).max(0),
        push_enabled: true,
        unseen_lines,
        server_running,
        entries,
    })
}

/// Everything a session's cursor row holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub last_entry_id: EntryId,
    pub nudge_entry_id: EntryId,
    pub blocks_issued: i64,
    pub change_blocks_issued: i64,
}

/// Reads a session's cursor, creating it if this session is new.
///
/// Both watermarks start at the current high-water mark rather than zero, so a fresh session
/// is not handed the project's entire history as though it were new.
pub fn ensure_cursor(db: &Db, session_id: &str, harness: &str) -> Result<Cursor> {
    db.with(|conn| {
        let existing: Option<Cursor> = conn
            .query_row(
                "SELECT last_entry_id, nudge_entry_id, blocks_issued, change_blocks_issued
                   FROM session_cursors WHERE session_id = ?1",
                [session_id],
                |r| {
                    Ok(Cursor {
                        last_entry_id: r.get(0)?,
                        nudge_entry_id: r.get(1)?,
                        blocks_issued: r.get(2)?,
                        change_blocks_issued: r.get(3)?,
                    })
                },
            )
            .optional()?;
        if let Some(v) = existing {
            return Ok(v);
        }
        let high: i64 = conn
            .query_row("SELECT COALESCE(max(id), 0) FROM entries", [], |r| r.get(0))
            .unwrap_or(0);
        conn.execute(
            "INSERT INTO session_cursors
                 (session_id, harness, last_entry_id, nudge_entry_id, last_seen_at)
             VALUES (?1, ?2, ?3, ?3, ?4)",
            rusqlite::params![session_id, harness, high, crate::now_micros()],
        )?;
        Ok(Cursor {
            last_entry_id: high,
            nudge_entry_id: high,
            blocks_issued: 0,
            change_blocks_issued: 0,
        })
    })
}

/// Advances the watermark `kind` names, charging the budget that goes with it.
///
/// The error cursor advances when a block is issued rather than when the agent acknowledges
/// it, so the same errors cannot block twice even if the agent ignores them.
pub fn ack(
    db: &Db,
    session_id: &str,
    harness: &str,
    entry_id: Option<EntryId>,
    kind: AckKind,
) -> Result<Cursor> {
    ensure_cursor(db, session_id, harness)?;
    db.with(|conn| {
        let target: i64 = match entry_id {
            Some(id) => id,
            None => conn
                .query_row("SELECT COALESCE(max(id), 0) FROM entries", [], |r| r.get(0))
                .unwrap_or(0),
        };
        // `max(…)` on both watermarks: an ack that arrived out of order must never rewind a
        // cursor and re-show the agent what it has already been told.
        conn.execute(
            "UPDATE session_cursors
                SET last_entry_id  = CASE WHEN ?5 = 1 THEN max(last_entry_id, ?2)
                                          ELSE last_entry_id END,
                    nudge_entry_id = CASE WHEN ?5 = 1 THEN nudge_entry_id
                                          ELSE max(nudge_entry_id, ?2) END,
                    last_seen_at   = ?3,
                    blocks_issued        = blocks_issued + ?4,
                    change_blocks_issued = change_blocks_issued + ?6
              WHERE session_id = ?1",
            rusqlite::params![
                session_id,
                target,
                crate::now_micros(),
                (kind == AckKind::StopBlock) as i64,
                kind.advances_errors() as i64,
                (kind == AckKind::ChangeBlock) as i64,
            ],
        )?;
        Ok(())
    })?;
    ensure_cursor(db, session_id, harness)
}

/// Whether this signature has already blocked this session.
///
/// A fingerprint blocks at most once however often it recurs, so a process throwing the
/// same exception in a loop cannot trap the agent.
pub fn already_blocked(db: &Db, session_id: &str, fp: FingerprintId) -> Result<bool> {
    db.with(|conn| {
        Ok(conn
            .query_row(
                "SELECT 1 FROM session_blocks WHERE session_id = ?1 AND fingerprint_id = ?2",
                rusqlite::params![session_id, fp],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    })
}

pub fn record_block(db: &Db, session_id: &str, fps: &[FingerprintId]) -> Result<()> {
    db.with(|conn| {
        for fp in fps {
            conn.execute(
                "INSERT OR IGNORE INTO session_blocks (session_id, fingerprint_id, at)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![session_id, fp, crate::now_micros()],
            )?;
        }
        Ok(())
    })
}

/// Sessions a read receipt applies to: hook cursors touched within the window.
///
/// `mcp` cursors are excluded on purpose — the MCP server's own cursor drives its banner,
/// and the read that produced the receipt came from it in the first place.
pub fn active_hook_sessions(db: &Db, within_micros: i64) -> Result<Vec<String>> {
    let floor = crate::now_micros() - within_micros;
    db.with(|conn| {
        let mut stmt = conn.prepare(
            "SELECT session_id FROM session_cursors
              WHERE harness != 'mcp' AND last_seen_at >= ?1",
        )?;
        let rows = stmt.query_map([floor], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    })
}

/// The verified-read receipt: marks `fps` seen for every recently-active hook session.
///
/// Reads carry no harness session id (the MCP server invents its own), so attribution is
/// "whoever is here right now". The cost of over-attribution is a missed block in a rare
/// concurrent session; the cost of no attribution is re-blocking on errors the agent
/// already read.
pub fn record_seen_for_active(
    db: &Db,
    fps: &[FingerprintId],
    within_micros: i64,
) -> Result<()> {
    if fps.is_empty() {
        return Ok(());
    }
    for session in active_hook_sessions(db, within_micros)? {
        record_block(db, &session, fps)?;
    }
    Ok(())
}

/// Receipt-list cap: bounds the IN-list work a giant result can cause.
const RECEIPT_MAX_KEYS: usize = 500;

/// Which fingerprints a SQL result demonstrably showed the caller.
///
/// Sniffs the result's columns rather than its SQL: `id`/`entry_id` resolve through the
/// entries table (error entries only), `fingerprint_id` is taken as-is, `template`
/// resolves by exact text against error-level fingerprints. A result carrying none of
/// these — an aggregate, an info-only read — proves nothing and marks nothing. An `id`
/// column from a non-entries table can resolve to unrelated fingerprints; that costs a
/// rare missed block, the direction the attribution trade-off already accepts.
pub fn query_receipt_fps(
    db: &Db,
    columns: &[String],
    rows: &[Vec<serde_json::Value>],
) -> Result<Vec<FingerprintId>> {
    let col = |name: &str| columns.iter().position(|c| c == name);
    let mut fps: Vec<FingerprintId> = Vec::new();

    if let Some(i) = col("fingerprint_id") {
        fps.extend(rows.iter().filter_map(|r| r.get(i)?.as_i64()).take(RECEIPT_MAX_KEYS));
    }
    if let Some(i) = col("id").or_else(|| col("entry_id")) {
        let ids: Vec<i64> = rows
            .iter()
            .filter_map(|r| r.get(i)?.as_i64())
            .take(RECEIPT_MAX_KEYS)
            .collect();
        if !ids.is_empty() {
            let marks = vec!["?"; ids.len()].join(",");
            let sql = format!(
                "SELECT DISTINCT fingerprint_id FROM entries
                  WHERE id IN ({marks}) AND is_error = 1 AND fingerprint_id IS NOT NULL"
            );
            fps.extend(db.with(|conn| {
                let mut stmt = conn.prepare(&sql)?;
                let refs: Vec<&dyn rusqlite::ToSql> =
                    ids.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
                let rows = stmt.query_map(refs.as_slice(), |r| r.get::<_, i64>(0))?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })?);
        }
    }
    if let Some(i) = col("template") {
        let templates: Vec<String> = rows
            .iter()
            .filter_map(|r| r.get(i)?.as_str().map(str::to_string))
            .take(RECEIPT_MAX_KEYS)
            .collect();
        if !templates.is_empty() {
            let marks = vec!["?"; templates.len()].join(",");
            let sql = format!(
                "SELECT id FROM fingerprints WHERE level = 'error' AND template IN ({marks})"
            );
            fps.extend(db.with(|conn| {
                let mut stmt = conn.prepare(&sql)?;
                let refs: Vec<&dyn rusqlite::ToSql> =
                    templates.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
                let rows = stmt.query_map(refs.as_slice(), |r| r.get::<_, i64>(0))?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })?);
        }
    }

    fps.sort_unstable();
    fps.dedup();
    Ok(fps)
}

pub fn count_runs(conn: &Connection) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT count(*), COALESCE(sum(chunks_evicted), 0) FROM runs",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RunStamp;
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

    fn run(h: &H, output: &[u8], exit: i32) -> RunId {
        let mut w = Writer::start(
            &h.db,
            &h.index,
            &RunStamp {
                command: vec!["mix".into(), "phx.server".into()],
                ..Default::default()
            },
            Arc::new(Redactor::disabled()),
        )
        .unwrap();
        if !output.is_empty() {
            w.push(output, crate::now_micros()).unwrap();
        }
        w.finish(&h.index, Some(exit), None).unwrap();
        w.run_id()
    }

    #[test]
    fn last_good_skips_runs_that_logged_errors_even_when_they_exited_zero() {
        let h = h();
        let good = run(&h, b"all fine\n", 0);
        run(&h, b"Error: boom\n", 0); // exited cleanly but logged an error
        run(&h, b"nope\n", 1); // exited badly

        let found = resolve_run(&h.db, &RunRef::LastGood).unwrap().unwrap();
        assert_eq!(found.id, good, "a run that logged errors did not 'work'");
    }

    #[test]
    fn entries_come_back_as_whole_groups() {
        let h = h();
        run(
            &h,
            b"Error: boom\n    at a (/app/x.js:1:1)\n    at b (/app/y.js:2:2)\n",
            1,
        );
        let e = entries(&h.db, &EntryFilter { limit: 5, ..Default::default() }).unwrap();
        assert_eq!(e.len(), 1);
        assert!(e[0].text.contains("at a"), "{}", e[0].text);
        assert!(e[0].text.contains("at b"), "{}", e[0].text);
    }

    #[test]
    fn full_text_search_finds_an_entry_by_a_word_inside_it() {
        let h = h();
        run(&h, b"Error: connection pool timeout exceeded\n", 1);
        let found = entries(
            &h.db,
            &EntryFilter {
                search: Some("timeout".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(found.len(), 1);
    }

    /// The invariant the whole verify nudge rests on: telling the agent to go look must not
    /// consume errors the Stop hook has yet to report.
    #[test]
    fn a_nudge_moves_its_own_cursor_and_leaves_the_error_cursor_alone() {
        let h = h();
        let before = ensure_cursor(&h.db, "s1", "claude_code").unwrap();

        ack(&h.db, "s1", "claude_code", Some(500), AckKind::Nudge).unwrap();
        let after = ensure_cursor(&h.db, "s1", "claude_code").unwrap();

        assert_eq!(after.nudge_entry_id, 500, "the nudge cursor must advance");
        assert_eq!(
            after.last_entry_id, before.last_entry_id,
            "a nudge must never advance the error cursor"
        );
        assert_eq!(after.blocks_issued, 0, "a nudge spends no budget");
        assert_eq!(after.change_blocks_issued, 0);
    }

    #[test]
    fn each_block_kind_charges_only_its_own_budget() {
        let h = h();

        ack(&h.db, "s1", "claude_code", Some(10), AckKind::StopBlock).unwrap();
        let c = ensure_cursor(&h.db, "s1", "claude_code").unwrap();
        assert_eq!((c.blocks_issued, c.change_blocks_issued), (1, 0));

        ack(&h.db, "s1", "claude_code", Some(20), AckKind::ChangeBlock).unwrap();
        let c = ensure_cursor(&h.db, "s1", "claude_code").unwrap();
        assert_eq!(
            (c.blocks_issued, c.change_blocks_issued),
            (1, 1),
            "a mid-turn block must not spend the Stop gate's budget"
        );
        assert_eq!(c.last_entry_id, 20, "both block kinds report errors");
    }

    #[test]
    fn unseen_lines_counts_every_level_not_only_errors() {
        let h = h();
        // The cursor is created first, while the project is empty: a watermark never rewinds,
        // so everything written after this counts as unseen.
        unread(&h.db, "s1", "mcp", 3, 2, 10).unwrap();
        run(&h, b"compiled successfully\nError: boom\n", 0);

        let state = unread(&h.db, "s1", "mcp", 3, 2, 10).unwrap();
        assert_eq!(state.new_errors, 1, "only one line is an error");
        assert!(
            state.unseen_lines >= 2,
            "every line counts as unseen, got {}",
            state.unseen_lines
        );

        ack(&h.db, "s1", "mcp", None, AckKind::Nudge).unwrap();
        let after = unread(&h.db, "s1", "mcp", 3, 2, 10).unwrap();
        assert_eq!(after.unseen_lines, 0, "the nudge covered them");
        assert_eq!(
            after.new_errors, 1,
            "and left the error for the Stop hook to report"
        );
    }

    #[test]
    fn server_running_follows_whether_a_run_is_still_open() {
        let h = h();
        assert!(
            !unread(&h.db, "s1", "mcp", 3, 2, 10).unwrap().server_running,
            "nothing has been started"
        );

        run(&h, b"hello\n", 0);
        assert!(
            !unread(&h.db, "s1", "mcp", 3, 2, 10).unwrap().server_running,
            "a finished run is not a running server"
        );

        let _live = Writer::start(
            &h.db,
            &h.index,
            &RunStamp {
                command: vec!["mix".into(), "phx.server".into()],
                ..Default::default()
            },
            Arc::new(Redactor::disabled()),
        )
        .unwrap();
        assert!(
            unread(&h.db, "s1", "mcp", 3, 2, 10).unwrap().server_running,
            "an open run is a running server"
        );
    }

    #[test]
    fn a_new_session_starts_at_the_high_water_mark_not_at_zero() {
        let h = h();
        run(&h, b"Error: old news\n", 1);
        let state = unread(&h.db, "session-a", "mcp", 3, 2, 10).unwrap();
        assert_eq!(state.new_errors, 0, "a fresh session inherited history");

        run(&h, b"Error: brand new\n", 1);
        let state = unread(&h.db, "session-a", "mcp", 3, 2, 10).unwrap();
        assert_eq!(state.new_errors, 1);
        assert!(state.entries[0].text.contains("brand new"));
    }

    #[test]
    fn two_sessions_do_not_steal_each_others_notifications() {
        let h = h();
        unread(&h.db, "a", "mcp", 3, 2, 10).unwrap();
        unread(&h.db, "b", "mcp", 3, 2, 10).unwrap();
        run(&h, b"Error: shared\n", 1);

        assert_eq!(unread(&h.db, "a", "mcp", 3, 2, 10).unwrap().new_errors, 1);
        ack(&h.db, "a", "mcp", None, AckKind::Seen).unwrap();
        assert_eq!(unread(&h.db, "a", "mcp", 3, 2, 10).unwrap().new_errors, 0);
        assert_eq!(
            unread(&h.db, "b", "mcp", 3, 2, 10).unwrap().new_errors,
            1,
            "session b lost a notification session a consumed"
        );
    }

    #[test]
    fn issuing_a_block_spends_budget_and_advances_the_cursor() {
        let h = h();
        unread(&h.db, "s", "claude_code", 3, 2, 10).unwrap();
        run(&h, b"Error: boom\n", 1);

        let before = unread(&h.db, "s", "claude_code", 3, 2, 10).unwrap();
        assert_eq!(before.blocks_remaining, 3);

        ack(&h.db, "s", "claude_code", before.newest_entry_id, AckKind::StopBlock).unwrap();
        let after = unread(&h.db, "s", "claude_code", 3, 2, 10).unwrap();
        assert_eq!(after.blocks_issued, 1);
        assert_eq!(after.blocks_remaining, 2);
        assert_eq!(after.new_errors, 0, "blocking must consume the errors it reported");
    }

    #[test]
    fn a_fingerprint_blocks_a_session_at_most_once() {
        let h = h();
        unread(&h.db, "s", "codex", 3, 2, 10).unwrap();
        run(&h, b"Error: boom\n", 1);
        let fp = entries(&h.db, &EntryFilter { errors_only: true, ..Default::default() })
            .unwrap()[0]
            .entry
            .fingerprint_id
            .unwrap();

        assert!(!already_blocked(&h.db, "s", fp).unwrap());
        record_block(&h.db, "s", &[fp]).unwrap();
        assert!(already_blocked(&h.db, "s", fp).unwrap());
        // Recording twice must not error or double count.
        record_block(&h.db, "s", &[fp]).unwrap();
    }

    #[test]
    fn an_error_without_a_fingerprint_still_counts_as_a_new_signature() {
        let h = h();
        unread(&h.db, "s", "claude_code", 3, 2, 10).unwrap();
        run(&h, b"Error: boom\n", 1);
        h.db.with(|conn| {
            conn.execute("UPDATE entries SET fingerprint_id = NULL WHERE is_error = 1", [])?;
            Ok(())
        })
        .unwrap();
        let state = unread(&h.db, "s", "claude_code", 3, 2, 10).unwrap();
        assert_eq!(
            state.new_signatures, 1,
            "an unidentifiable error must still be able to block"
        );
    }

    const WINDOW: i64 = 30 * 60 * 1_000_000;

    #[test]
    fn a_verified_read_marks_signatures_seen_for_active_hook_sessions() {
        let h = h();
        unread(&h.db, "hook-session", "claude_code", 3, 2, 10).unwrap();
        run(&h, b"Error: boom\n", 1);

        let fp = entries(&h.db, &EntryFilter { errors_only: true, ..Default::default() })
            .unwrap()[0]
            .entry
            .fingerprint_id
            .unwrap();
        record_seen_for_active(&h.db, &[fp], WINDOW).unwrap();

        let state = unread(&h.db, "hook-session", "claude_code", 3, 2, 10).unwrap();
        assert_eq!(state.new_errors, 1, "a receipt must not consume the error count");
        assert_eq!(state.new_signatures, 0, "a receipt must mark the signature seen");
    }

    #[test]
    fn receipts_skip_mcp_and_stale_cursors() {
        let h = h();
        unread(&h.db, "mcp-session", "mcp", 3, 2, 10).unwrap();
        unread(&h.db, "stale-hook", "claude_code", 3, 2, 10).unwrap();
        h.db.with(|conn| {
            conn.execute(
                "UPDATE session_cursors SET last_seen_at = 1 WHERE session_id = 'stale-hook'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

        assert_eq!(
            active_hook_sessions(&h.db, WINDOW).unwrap(),
            Vec::<String>::new(),
            "neither an mcp cursor nor a stale hook cursor is active"
        );

        unread(&h.db, "live-hook", "claude_code", 3, 2, 10).unwrap();
        assert_eq!(
            active_hook_sessions(&h.db, WINDOW).unwrap(),
            vec!["live-hook".to_string()]
        );
    }

    #[test]
    fn query_receipts_resolve_entry_ids_fingerprint_ids_and_templates() {
        let h = h();
        run(&h, b"Error: boom\n", 1);
        let view = entries(&h.db, &EntryFilter { errors_only: true, ..Default::default() })
            .unwrap()
            .remove(0);
        let (id, fp) = (view.entry.id, view.entry.fingerprint_id.unwrap());
        let template = view.template.unwrap();

        let by_id =
            query_receipt_fps(&h.db, &["id".to_string()], &[vec![serde_json::json!(id)]])
                .unwrap();
        assert_eq!(by_id, vec![fp]);

        let by_fp = query_receipt_fps(
            &h.db,
            &["fingerprint_id".to_string()],
            &[vec![serde_json::json!(fp)]],
        )
        .unwrap();
        assert_eq!(by_fp, vec![fp]);

        let by_template = query_receipt_fps(
            &h.db,
            &["template".to_string()],
            &[vec![serde_json::json!(template)]],
        )
        .unwrap();
        assert_eq!(by_template, vec![fp]);
    }

    #[test]
    fn an_aggregate_result_proves_nothing() {
        let h = h();
        run(&h, b"Error: boom\n", 1);
        let fps =
            query_receipt_fps(&h.db, &["n".to_string()], &[vec![serde_json::json!(7)]])
                .unwrap();
        assert!(fps.is_empty(), "a count column must not mark anything seen");
    }

    /// The scenario this whole feature exists for: the agent reads the errors, then stops.
    #[test]
    fn reading_errors_then_stopping_yields_no_new_signatures() {
        let h = h();
        unread(&h.db, "claude-session", "claude_code", 3, 2, 10).unwrap();
        run(&h, b"Error: boom\n", 1);

        // What conn.rs does after serving Entries/Wait: extract error fps, record.
        let served = entries(
            &h.db,
            &EntryFilter { errors_only: true, ..Default::default() },
        )
        .unwrap();
        let fps: Vec<FingerprintId> = served
            .iter()
            .filter(|v| v.entry.is_error)
            .filter_map(|v| v.entry.fingerprint_id)
            .collect();
        record_seen_for_active(&h.db, &fps, WINDOW).unwrap();

        let state = unread(&h.db, "claude-session", "claude_code", 3, 2, 10).unwrap();
        assert_eq!(state.new_signatures, 0, "the stop after a read must not block");
        assert_eq!(state.new_errors, 1, "the advisory line still has its count");
    }

    #[test]
    fn unread_refreshes_the_activity_stamp() {
        let h = h();
        unread(&h.db, "s", "claude_code", 3, 2, 10).unwrap();
        h.db.with(|conn| {
            conn.execute("UPDATE session_cursors SET last_seen_at = 1", [])?;
            Ok(())
        })
        .unwrap();
        unread(&h.db, "s", "claude_code", 3, 2, 10).unwrap();
        let seen: i64 = h
            .db
            .with(|conn| {
                Ok(conn.query_row(
                    "SELECT last_seen_at FROM session_cursors WHERE session_id = 's'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert!(seen > 1, "unread must refresh last_seen_at, got {seen}");
    }

    #[test]
    fn history_pads_runs_where_the_signature_did_not_occur() {
        let h = h();
        run(&h, b"Error: boom\n", 1);
        run(&h, b"all fine\n", 0);
        run(&h, b"Error: boom\n", 1);
        let fp = fingerprints(&h.db, 10, true).unwrap()[0].id;
        let hist = fingerprint_history(&h.db, fp, 10).unwrap();
        assert_eq!(hist.len(), 3);
        assert_eq!(hist.iter().map(|(_, c, _)| *c).collect::<Vec<_>>(), vec![1, 0, 1]);
    }
}
