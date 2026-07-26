//! Typed reads. Every query shape the product needs lives here so they can be reviewed in
//! one place, rather than being spread across four binaries and a UI.

use crate::error::{Error, Result};
use crate::model::*;
use crate::proto::{EntryFilter, RunRef, UnreadState};
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

/// Resolve a symbolic run reference.
///
/// `LastGood` is the one that matters: the most recent run that exited cleanly **and**
/// logged no errors. That is what people mean by "it worked yesterday" — not "the previous
/// run", which is usually the one that just failed the same way.
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

/// Fetch entries with their rendered text.
///
/// Text is produced by re-rendering the line range, so a returned entry is always the
/// whole group. A caller asking for five entries can never get the bottom half of a stack
/// trace with the header missing.
pub fn entries(db: &Db, f: &EntryFilter) -> Result<Vec<EntryView>> {
    let mut sql = String::from(
        "SELECT e.id, e.run_id, e.start_line, e.end_line, e.captured_at, e.level, e.is_error,
                e.error_reason, e.fingerprint_id, e.boundaries, fp.template,
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
                    is_error: r.get::<_, i64>(6)? != 0,
                    error_reason: r.get(7)?,
                    fingerprint_id: r.get(8)?,
                    boundaries: match r.get::<_, String>(9)?.as_str() {
                        "exact" => Boundaries::Exact,
                        _ => Boundaries::Inferred,
                    },
                },
                r.get::<_, Option<String>>(10)?,
                r.get::<_, Option<i64>>(11)?.unwrap_or(0) != 0,
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

/// Occurrences of one signature across recent runs. Feeds the sparkline on the errors
/// screen, which is what turns a list of templates into a story about a regression.
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

/// A representative rendered entry for a signature, for the diff view's sample line.
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
/// Per session, never global: two Claude Code windows on the same project must not steal
/// each other's notifications, and a fresh session must not be handed yesterday's errors.
pub fn unread(
    db: &Db,
    session_id: &str,
    harness: &str,
    block_budget: i64,
    max_entries: usize,
) -> Result<UnreadState> {
    let cursor = ensure_cursor(db, session_id, harness)?;
    let filter = EntryFilter {
        errors_only: true,
        after_entry: Some(cursor.0),
        limit: max_entries,
        ascending: true,
        ..Default::default()
    };
    let entries = entries(db, &filter)?;

    let (count, oldest, newest) = db.with(|conn| {
        let row: (i64, Option<i64>, Option<i64>) = conn.query_row(
            "SELECT count(*), min(captured_at), max(id) FROM entries
              WHERE is_error = 1 AND id > ?1",
            [cursor.0],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(row)
    })?;

    let new_signatures = db.with(|conn| {
        Ok(conn.query_row(
            "SELECT count(DISTINCT e.fingerprint_id) FROM entries e
              WHERE e.is_error = 1 AND e.id > ?1
                AND e.fingerprint_id NOT IN (SELECT fingerprint_id FROM session_blocks
                                              WHERE session_id = ?2)",
            rusqlite::params![cursor.0, session_id],
            |r| r.get::<_, i64>(0),
        )?)
    })?;

    Ok(UnreadState {
        new_errors: count,
        new_signatures,
        oldest_at: oldest,
        newest_entry_id: newest,
        blocks_issued: cursor.1,
        blocks_remaining: (block_budget - cursor.1).max(0),
        push_enabled: true,
        entries,
    })
}

/// Returns `(last_entry_id, blocks_issued)`, creating the cursor if this session is new.
///
/// A new session starts at the current high-water mark, not at zero. Otherwise the first
/// thing every fresh agent session would see is the entire error history of the project,
/// which is noise, not signal.
pub fn ensure_cursor(db: &Db, session_id: &str, harness: &str) -> Result<(EntryId, i64)> {
    db.with(|conn| {
        let existing: Option<(i64, i64)> = conn
            .query_row(
                "SELECT last_entry_id, blocks_issued FROM session_cursors WHERE session_id = ?1",
                [session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some(v) = existing {
            return Ok(v);
        }
        let high: i64 = conn
            .query_row("SELECT COALESCE(max(id), 0) FROM entries", [], |r| r.get(0))
            .unwrap_or(0);
        conn.execute(
            "INSERT INTO session_cursors (session_id, harness, last_entry_id, last_seen_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![session_id, harness, high, crate::now_micros()],
        )?;
        Ok((high, 0))
    })
}

/// Advance the watermark, optionally spending a block from the budget.
///
/// The cursor advances **when a block is issued**, not when the agent acknowledges. That
/// is what stops the same errors blocking twice even if the agent ignores them entirely.
pub fn ack(
    db: &Db,
    session_id: &str,
    harness: &str,
    entry_id: Option<EntryId>,
    record_block: bool,
) -> Result<(EntryId, i64)> {
    ensure_cursor(db, session_id, harness)?;
    db.with(|conn| {
        let target: i64 = match entry_id {
            Some(id) => id,
            None => conn
                .query_row("SELECT COALESCE(max(id), 0) FROM entries", [], |r| r.get(0))
                .unwrap_or(0),
        };
        conn.execute(
            "UPDATE session_cursors
                SET last_entry_id = max(last_entry_id, ?2),
                    last_seen_at = ?3,
                    blocks_issued = blocks_issued + ?4
              WHERE session_id = ?1",
            rusqlite::params![session_id, target, crate::now_micros(), record_block as i64],
        )?;
        conn.query_row(
            "SELECT last_entry_id, blocks_issued FROM session_cursors WHERE session_id = ?1",
            [session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(Into::into)
    })
}

/// Has this signature already blocked this session? A fingerprint blocks at most once,
/// however many times it recurs — otherwise a server throwing the same exception in a loop
/// would trap the agent forever.
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

    #[test]
    fn a_new_session_starts_at_the_high_water_mark_not_at_zero() {
        let h = h();
        run(&h, b"Error: old news\n", 1);
        // A session created now must not be handed the project's back catalogue.
        let state = unread(&h.db, "session-a", "mcp", 3, 10).unwrap();
        assert_eq!(state.new_errors, 0, "a fresh session inherited history");

        run(&h, b"Error: brand new\n", 1);
        let state = unread(&h.db, "session-a", "mcp", 3, 10).unwrap();
        assert_eq!(state.new_errors, 1);
        assert!(state.entries[0].text.contains("brand new"));
    }

    #[test]
    fn two_sessions_do_not_steal_each_others_notifications() {
        let h = h();
        unread(&h.db, "a", "mcp", 3, 10).unwrap();
        unread(&h.db, "b", "mcp", 3, 10).unwrap();
        run(&h, b"Error: shared\n", 1);

        assert_eq!(unread(&h.db, "a", "mcp", 3, 10).unwrap().new_errors, 1);
        ack(&h.db, "a", "mcp", None, false).unwrap();
        assert_eq!(unread(&h.db, "a", "mcp", 3, 10).unwrap().new_errors, 0);
        assert_eq!(
            unread(&h.db, "b", "mcp", 3, 10).unwrap().new_errors,
            1,
            "session b lost a notification session a consumed"
        );
    }

    #[test]
    fn issuing_a_block_spends_budget_and_advances_the_cursor() {
        let h = h();
        unread(&h.db, "s", "claude_code", 3, 10).unwrap();
        run(&h, b"Error: boom\n", 1);

        let before = unread(&h.db, "s", "claude_code", 3, 10).unwrap();
        assert_eq!(before.blocks_remaining, 3);

        ack(&h.db, "s", "claude_code", before.newest_entry_id, true).unwrap();
        let after = unread(&h.db, "s", "claude_code", 3, 10).unwrap();
        assert_eq!(after.blocks_issued, 1);
        assert_eq!(after.blocks_remaining, 2);
        assert_eq!(after.new_errors, 0, "blocking must consume the errors it reported");
    }

    #[test]
    fn a_fingerprint_blocks_a_session_at_most_once() {
        let h = h();
        unread(&h.db, "s", "codex", 3, 10).unwrap();
        run(&h, b"Error: boom\n", 1);
        let fp = entries(&h.db, &EntryFilter { errors_only: true, ..Default::default() })
            .unwrap()[0]
            .entry
            .fingerprint_id
            .unwrap();

        assert!(!already_blocked(&h.db, "s", fp).unwrap());
        record_block(&h.db, "s", &[fp]).unwrap();
        assert!(already_blocked(&h.db, "s", fp).unwrap());
        // Idempotent — recording twice must not error or double count.
        record_block(&h.db, "s", &[fp]).unwrap();
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
