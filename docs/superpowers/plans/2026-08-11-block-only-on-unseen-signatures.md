# Block Only on Unseen Error Signatures — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Stop hook blocks only when an unread error carries a signature this session has not seen; signatures already surfaced (by a block or a verified read) degrade to a one-line advisory.

**Architecture:** Three layers change. (1) `decide.rs` gains a rule: `new_signatures == 0` → advisory Context, never Block. (2) `read.rs` fixes the `new_signatures` SQL for NULL fingerprints, refreshes `last_seen_at` on every unread, and grows receipt helpers (`active_hook_sessions`, `record_seen_for_active`, `query_receipt_fps`). (3) `conn.rs` records read receipts after serving `Entries`/`Wait`/`Diff`/`Fingerprints`/`Query`, attributing them to recently-active hook sessions.

**Tech Stack:** Rust workspace, rusqlite, existing test harnesses (`crates/hook/tests/hooks.rs` FakeDaemon + golden payloads; `read.rs` `mod tests` with `h()`/`run()`).

**Spec:** `docs/superpowers/specs/2026-08-11-block-only-on-unseen-signatures-design.md`

## Global Constraints

- Receipts are best-effort: a receipt failure is logged to stderr and never fails the read that produced it.
- Receipt attribution window: 30 minutes, a constant (`RECEIPT_WINDOW_MICROS`), no settings knob.
- Receipts apply only to cursors with `harness != 'mcp'`.
- `session_blocks` keeps its name and schema; its meaning widens to "signatures this session has seen".
- The PostToolUse change-block rule in `decide.rs` is untouched.
- Every failure path in the hook still fails open.

---

### Task 1: decide.rs — recurring-only unread degrades the Stop to advisory

**Files:**
- Modify: `crates/hook/src/decide.rs` (Stop arm ~line 81–94, `block_reason` ~line 210)
- Test: `crates/hook/tests/hooks.rs`

**Interfaces:**
- Consumes: `UnreadState.new_signatures` (already populated by the daemon).
- Produces: a new advisory decision path; later tasks make `new_signatures` accurate, they do not change this rule.

- [ ] **Step 1: Write the failing tests** in `crates/hook/tests/hooks.rs` (after the existing block tests; reuse the `unread()` / `entry()` / `run()` helpers already in the file):

```rust
#[test]
fn recurring_only_errors_never_block_a_stop() {
    // new_errors > 0 but every signature was already seen this session.
    let mut d = FakeDaemon::with(UnreadState {
        new_signatures: 0,
        ..unread()
    });
    let v = run("claude_stop", None, &mut d);
    assert!(v.get("decision").is_none(), "recurring-only must not block: {v}");
    let text = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(text.contains("already seen"), "{text}");
    assert!(d.acks.is_empty(), "an advisory must spend no budget");
}

#[test]
fn a_new_signature_still_blocks_a_stop() {
    let mut d = FakeDaemon::with(unread()); // new_signatures: 1
    let v = run("claude_stop", None, &mut d);
    assert_eq!(v["decision"], "block");
}

#[test]
fn the_block_reason_lists_new_signatures_first() {
    // Recurring entry first in the state; the reason must reorder it behind the new one.
    let mut state = unread();
    state.entries = vec![
        entry(1001, false, "[error] old friend", "[error] old friend"),
        entry(1002, true, "Error: brand new", "Error: brand new"),
    ];
    let mut d = FakeDaemon::with(state);
    let v = run("claude_stop", None, &mut d);
    let reason = v["reason"].as_str().unwrap();
    let new_at = reason.find("NEW SIGNATURE").expect("new entry shown");
    let old_at = reason.find("recurring").expect("recurring entry shown");
    assert!(new_at < old_at, "new signatures must precede recurring ones:\n{reason}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p logbear-hook --test hooks recurring_only -- --nocapture` and the other two names.
Expected: FAIL — today `recurring_only_errors_never_block_a_stop` gets `"decision": "block"`.

- [ ] **Step 3: Implement in `crates/hook/src/decide.rs`**

Insert a new arm between the `stop_hook_active` arm and the `blocks_remaining` arm of `decide()`:

```rust
        // Every unread signature has already been surfaced to this session — by an
        // earlier block or a read the daemon verified. Re-blocking on them cannot teach
        // the agent anything new; only an unseen signature is worth a turn.
        Event::Stop | Event::SubagentStop if unread.new_signatures == 0 => Decision::Context {
            text: recurring_summary(unread),
            ack_nudge: false,
        },
```

Add next to `summary()`:

```rust
/// The degraded stop: errors exist, but this session has seen every signature.
fn recurring_summary(u: &UnreadState) -> String {
    format!(
        "logbear: {} unread error{}, but every signature is one you have already seen \
         this session — nothing new to account for. Recheck with `logbear logs --level \
         error` if that seems wrong.",
        u.new_errors,
        plural(u.new_errors),
    )
}
```

In `block_reason`, sort new signatures ahead of recurring ones (stable, so chronology holds within each group) and iterate the sorted list:

```rust
    let mut entries: Vec<&EntryView> = u.entries.iter().collect();
    // The new signatures are why this block fired; MAX_ENTRIES must never truncate
    // them away in favour of recurring ones.
    entries.sort_by_key(|e| !e.is_new_signature);

    for (i, e) in entries.iter().take(MAX_ENTRIES).enumerate() {
```

(the loop body is unchanged; also change the two trailing uses of `u.entries.len()` to `entries.len()`).

- [ ] **Step 4: Run the whole hook suite**

Run: `cargo test -p logbear-hook`
Expected: the three new tests PASS. Existing tests that built an `UnreadState` with `new_errors > 0` but defaulted `new_signatures: 0` while expecting a Stop block will now fail — for each, set `new_signatures` to the number of distinct unseen signatures the fixture intends (usually `1`), keeping each test's original point intact. Do not weaken assertions.

- [ ] **Step 5: Commit**

```bash
git add crates/hook/src/decide.rs crates/hook/tests/hooks.rs
git commit -m "hook: degrade recurring-only stops to advisory context"
```

---

### Task 2: read.rs — NULL fingerprints count as new; unread refreshes last_seen_at

**Files:**
- Modify: `crates/core/src/store/read.rs` (`unread()` ~line 322, its `new_signatures` query ~line 350)
- Test: `crates/core/src/store/read.rs` `mod tests`

**Interfaces:**
- Consumes: existing `unread()`, `ensure_cursor()`, test helpers `h()` / `run()`.
- Produces: `unread()` whose `new_signatures` is NULL-safe, and `session_cursors.last_seen_at` that Task 3 can trust as an activity signal.

- [ ] **Step 1: Write the failing tests** in `read.rs` `mod tests`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p logbear-core an_error_without_a_fingerprint unread_refreshes`
Expected: FAIL — `new_signatures` is 0 (NULL `NOT IN` is falsy), and `last_seen_at` stays 1.

- [ ] **Step 3: Implement in `unread()`**

Replace the `new_signatures` query:

```rust
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
```

Directly after the `ensure_cursor` call at the top of `unread()`, add:

```rust
    // `last_seen_at` is how read receipts find the sessions that are actually present:
    // the hook calls unread on every event, so a live turn keeps its cursor fresh.
    db.with(|conn| {
        conn.execute(
            "UPDATE session_cursors SET last_seen_at = ?2 WHERE session_id = ?1",
            rusqlite::params![session_id, crate::now_micros()],
        )?;
        Ok(())
    })?;
```

- [ ] **Step 4: Run the core suite**

Run: `cargo test -p logbear-core`
Expected: PASS (the two new tests plus all existing cursor tests).

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/store/read.rs
git commit -m "store: NULL-fingerprint errors count as new; unread refreshes last_seen_at"
```

---

### Task 3: read.rs — receipt primitives: active sessions and record_seen_for_active

**Files:**
- Modify: `crates/core/src/store/read.rs` (add functions next to `record_block`, ~line 507)
- Test: `crates/core/src/store/read.rs` `mod tests`

**Interfaces:**
- Consumes: `record_block(db, session_id, fps)` (existing), `session_cursors.last_seen_at` (fresh per Task 2).
- Produces: `pub fn active_hook_sessions(db: &Db, within_micros: i64) -> Result<Vec<String>>` and `pub fn record_seen_for_active(db: &Db, fps: &[FingerprintId], within_micros: i64) -> Result<()>` — Task 5 calls the latter from `conn.rs`.

- [ ] **Step 1: Write the failing tests** in `read.rs` `mod tests`:

```rust
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
        assert_eq!(active_hook_sessions(&h.db, WINDOW).unwrap(), vec!["live-hook".to_string()]);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p logbear-core a_verified_read receipts_skip`
Expected: FAIL to compile — `record_seen_for_active` / `active_hook_sessions` do not exist.

- [ ] **Step 3: Implement**, directly below `record_block`:

```rust
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
/// concurrent session; the cost of no attribution is today's bug — re-blocking on errors
/// the agent already read.
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
```

- [ ] **Step 4: Run the core suite**

Run: `cargo test -p logbear-core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/store/read.rs
git commit -m "store: read receipts mark signatures seen for active hook sessions"
```

---

### Task 4: read.rs — query_receipt_fps: which fingerprints a SQL result proved

**Files:**
- Modify: `crates/core/src/store/read.rs` (below `record_seen_for_active`)
- Test: `crates/core/src/store/read.rs` `mod tests`

**Interfaces:**
- Consumes: `QueryResult`-shaped data (`columns: &[String]`, `rows: &[Vec<serde_json::Value>]`).
- Produces: `pub fn query_receipt_fps(db: &Db, columns: &[String], rows: &[Vec<serde_json::Value>]) -> Result<Vec<FingerprintId>>` — Task 5 calls it for `Request::Query`.

- [ ] **Step 1: Write the failing tests** in `read.rs` `mod tests`:

```rust
    #[test]
    fn query_receipts_resolve_entry_ids_fingerprint_ids_and_templates() {
        let h = h();
        run(&h, b"Error: boom\n", 1);
        let view = entries(&h.db, &EntryFilter { errors_only: true, ..Default::default() })
            .unwrap()
            .remove(0);
        let (id, fp) = (view.entry.id, view.entry.fingerprint_id.unwrap());
        let template = view.template.unwrap();

        let by_id = query_receipt_fps(
            &h.db,
            &["id".to_string()],
            &[vec![serde_json::json!(id)]],
        )
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
        let fps = query_receipt_fps(
            &h.db,
            &["n".to_string()],
            &[vec![serde_json::json!(7)]],
        )
        .unwrap();
        assert!(fps.is_empty(), "a count column must not mark anything seen");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p logbear-core query_receipts an_aggregate`
Expected: FAIL to compile — `query_receipt_fps` does not exist.

- [ ] **Step 3: Implement:**

```rust
/// Receipt-list cap: bounds the IN-list work a giant result can cause.
const RECEIPT_MAX_KEYS: usize = 500;

/// Which fingerprints a SQL result demonstrably showed the caller.
///
/// Sniffs the result's columns rather than its SQL: `id`/`entry_id` resolve through the
/// entries table (error entries only), `fingerprint_id` is taken as-is, `template`
/// resolves by exact text against error-level fingerprints. A result carrying none of
/// these — an aggregate, an info-only read — proves nothing and marks nothing. An `id`
/// column from a non-entries table can resolve to unrelated fingerprints; the cost is a
/// rare missed block, accepted by the spec's attribution trade-off.
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
            let marks = std::iter::repeat("?").take(ids.len()).collect::<Vec<_>>().join(",");
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
            let marks = std::iter::repeat("?")
                .take(templates.len())
                .collect::<Vec<_>>()
                .join(",");
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
```

- [ ] **Step 4: Run the core suite**

Run: `cargo test -p logbear-core`
Expected: PASS. If the `template` assertion fails because the stored fingerprint level is not `'error'` for the harness's `Error: boom` line, check the actual level with a debug query in the test and relax the SQL filter to `level IN ('error','fatal')` only if the fingerprints table genuinely stores a different error-class label — do not drop the level filter entirely.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/store/read.rs
git commit -m "store: resolve which fingerprints a SQL result proved were read"
```

---

### Task 5: conn.rs — record receipts after serving reads

**Files:**
- Modify: `crates/daemon/src/conn.rs` (`handle()`, arms `Request::Entries` ~199, `Fingerprints` ~204, `Query` ~213, `Diff` ~249, `Wait` ~254)
- Test: `crates/core/src/store/read.rs` `mod tests` (end-to-end at the store layer; `handle()` has no socketless test harness)

**Interfaces:**
- Consumes: `read::record_seen_for_active`, `read::query_receipt_fps` (Tasks 3–4), `daemon.wait(...) -> Vec<EntryView>`, `diff::diff(...) -> RunDiff`.
- Produces: receipts on every daemon-served read; no protocol change, responses byte-identical.

- [ ] **Step 1: Write the failing end-to-end test** in `read.rs` `mod tests` — the exact sequence the bug report describes, at the store layer:

```rust
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
```

- [ ] **Step 2: Run it**

Run: `cargo test -p logbear-core reading_errors_then_stopping`
Expected: PASS immediately (it composes Task 3's pieces) — it pins the contract `conn.rs` is about to be wired to. If it fails, fix Task 3 before touching `conn.rs`.

- [ ] **Step 3: Wire receipts into `handle()`** in `crates/daemon/src/conn.rs`. Add near the top of the file:

```rust
/// How recently a hook session must have been seen for a read receipt to apply to it.
const RECEIPT_WINDOW_MICROS: i64 = 30 * 60 * 1_000_000;

/// Best-effort by design: a receipt failure must never fail the read that produced it.
fn receipt(h: &ProjectHandle, fps: Vec<FingerprintId>) {
    if fps.is_empty() {
        return;
    }
    if let Err(e) = read::record_seen_for_active(&h.db, &fps, RECEIPT_WINDOW_MICROS) {
        eprintln!("logbeard: read receipt failed ({e})");
    }
}

fn error_fps(views: &[EntryView]) -> Vec<FingerprintId> {
    views
        .iter()
        .filter(|v| v.entry.is_error)
        .filter_map(|v| v.entry.fingerprint_id)
        .collect()
}
```

Rewrite the five read arms (only the shown lines change):

```rust
        Request::Entries { project, filter } => {
            let h = daemon.resolve(&project)?;
            let views = read::entries(&h.db, &filter)?;
            receipt(&h, error_fps(&views));
            to_value(views)?
        }

        Request::Fingerprints { project, limit, errors_only } => {
            let h = daemon.resolve(&project)?;
            let fps = read::fingerprints(&h.db, limit, errors_only)?;
            let ids: Vec<FingerprintId> = fps
                .iter()
                .filter(|f| matches!(f.level, Level::Error))
                .map(|f| f.id)
                .collect();
            receipt(&h, ids);
            to_value(fps)?
        }

        Request::Query { project, sql, params, max_rows } => {
            let h = daemon.resolve(&project)?;
            let result = sandbox::query(h.db.path(), &sql, &params, max_rows)?;
            match read::query_receipt_fps(&h.db, &result.columns, &result.rows) {
                Ok(fps) => receipt(&h, fps),
                Err(e) => eprintln!("logbeard: query receipt failed ({e})"),
            }
            to_value(result)?
        }

        Request::Diff { project, a, b } => {
            let h = daemon.resolve(&project)?;
            let d = diff::diff(&h.db, a, b)?;
            let fps: Vec<FingerprintId> = d
                .new_signatures
                .iter()
                .chain(d.resolved_signatures.iter())
                .chain(d.changed_counts.iter())
                .filter(|s| matches!(s.level, Level::Error))
                .map(|s| s.fingerprint_id)
                .collect();
            receipt(&h, fps);
            to_value(d)?
        }

        Request::Wait { project, timeout_ms, level, after_entry } => {
            let h = daemon.resolve(&project)?;
            let already = read::entries(
                &h.db,
                &EntryFilter {
                    level,
                    after_entry,
                    errors_only: level.is_none(),
                    ascending: true,
                    limit: 50,
                    ..Default::default()
                },
            )?;
            let served = if !already.is_empty() && after_entry.is_some() {
                already
            } else {
                daemon.wait(h.project.id, timeout_ms.min(600_000), level, after_entry)
            };
            receipt(&h, error_fps(&served));
            to_value(served)?
        }
```

Keep the existing comments on the `Query` sandbox line and the `Wait` fast-path — move them with the code, do not delete them. `daemon.resolve` returns `Arc<ProjectHandle>`; `receipt(&h, …)` derefs fine.

- [ ] **Step 4: Build and run the whole workspace suite**

Run: `cargo test --workspace`
Expected: PASS, everything compiles. Fix any import needed (`Level` is already in scope via `logbear_core::model::*`).

- [ ] **Step 5: Commit**

```bash
git add crates/daemon/src/conn.rs crates/core/src/store/read.rs
git commit -m "daemon: reads leave receipts so the hook stops re-blocking read errors"
```

---

### Task 6: Deploy — update the installed app and reset the daemon/MCP

**Files:**
- No source changes. Touches `/Applications/logbear.app/Contents/MacOS/` and the running LaunchAgent.

**Interfaces:**
- Consumes: release binaries `logbear`, `logbeard`, `logbear-hook`, `logbear-mcp` from `target/release/`.
- Produces: the user's installed app running this branch's behavior.

- [ ] **Step 1: Build release binaries**

```bash
cargo build --release -p logbear -p logbeard -p logbear-hook -p logbear-mcp
```

- [ ] **Step 2: Copy into the app bundle and re-sign**

```bash
for b in logbear logbeard logbear-hook logbear-mcp; do
  cp "target/release/$b" "/Applications/logbear.app/Contents/MacOS/$b"
  codesign --force --sign - "/Applications/logbear.app/Contents/MacOS/$b"
done
```

(Ad-hoc signing: replacing a binary inside the bundle invalidates any existing signature, and launchd will not start an unsigned binary on modern macOS.)

- [ ] **Step 3: Restart the daemon**

```bash
launchctl kickstart -k "gui/$(id -u)/sh.logbear.daemon"
```

If the label is rejected, find it with `launchctl list | grep -i logbear` and kickstart that label. Fallback: `pkill -x logbeard` (the LaunchAgent restarts it on failure).

- [ ] **Step 4: Verify**

```bash
logbear status
```

Expected: daemon up, `version` matching `crates/daemon/Cargo.toml`, fresh pid. The hook binary is exec'd per event, so it is live immediately; verify the registration still points at `/Applications/logbear.app/Contents/MacOS/logbear-hook` in `~/.claude/settings.json`.

- [ ] **Step 5: Reset the MCP servers**

Long-lived `logbear-mcp` processes started by open agent sessions still run the old code (they are thin clients; the changed behavior lives in the daemon and hook, so this is hygiene, not correctness):

```bash
pkill -x logbear-mcp || true
```

Tell the user: any open Claude Code session should run `/mcp` → reconnect logbear if its logbear tools error on next use.

---

## Self-review notes

- Spec coverage: policy change (Task 1), seen-set + receipts (Tasks 3–5), attribution (Tasks 2–3), NULL-fingerprint edge (Task 2), advisory wording (Task 1), tests (each task), deploy/reset (Task 6). The spec's "receipts are best-effort" lives in `receipt()`; "no settings knob" holds — the window is a constant.
- The `entries.sort_by_key` in Task 1 uses project-scoped `is_new_signature` for ordering only; the gate itself uses session-scoped `new_signatures`. Ordering by an approximation is acceptable; gating by one is not, and the gate does not.
- Type consistency: `record_seen_for_active(&Db, &[FingerprintId], i64)` is defined in Task 3 and consumed with that exact signature in Task 5; `query_receipt_fps(&Db, &[String], &[Vec<serde_json::Value>])` defined in Task 4, consumed in Task 5 via `&result.columns, &result.rows`.
