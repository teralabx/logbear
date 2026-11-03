# Agent Hooks: Change → Verify Loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** After a change, when a live dev server has logged something the agent has not seen, tell the agent to go look — and teach it to start servers under `logbear` in the first place.

**Architecture:** The hook gains a second session watermark so a "go look" nudge can advance without disturbing the error cursor the Stop gate depends on. A pure intent classifier reads Bash commands to spot uncaptured servers and misdirected log-hunting. All new rules live in `decide.rs`, which stays a pure function of state.

**Tech Stack:** Rust 2021, rusqlite (SQLite), serde_json. No new dependencies.

## Global Constraints

- **Silence is the default.** Every rule is gated so the common case emits nothing. Never add a rule that fires on an unremarkable tool call.
- **Every path fails open.** Daemon down, malformed payload, timeout, unreadable field: exit 0 with `allow()`. Never let a hook failure become an agent failure.
- **Stdout is JSON only.** Codex treats unparseable stdout on `Stop` as a hard hook failure. Diagnostics go to stderr.
- **Do not touch the Codex `Stop` workaround.** `render()`'s `Harness::Codex && event.is_stop()` branch is deliberate and stays as-is.
- **`decide.rs` stays I/O-free.** It is a pure function of event and state so the loop-safety rules are testable without a socket. `intent.rs` follows the same rule.
- Run `cargo clippy --workspace --all-targets` and `cargo test --workspace` before every commit. Both must be clean.
- Comments explain *why*, matching the surrounding style. No comment restates what the code says.

## File Structure

| File | Responsibility |
|---|---|
| `crates/core/src/proto.rs` | `AckKind`, `UnreadState` fields, `Settings.change_block_budget` |
| `crates/core/src/store/schema.rs` | Two additive columns on `session_cursors` |
| `crates/core/src/store/read.rs` | Cursor reads/writes per kind; `unseen_lines`, `server_running` |
| `crates/core/src/client.rs` | `ack` signature carries `AckKind` |
| `crates/daemon/src/conn.rs` | Charges the right budget per ack kind |
| `crates/hook/src/intent.rs` | **New.** Pure `classify(&str) -> Intent` |
| `crates/hook/src/payload.rs` | `Event::SessionStart`, `Payload.command` |
| `crates/hook/src/decide.rs` | All decision rules and their copy |
| `crates/hook/src/lib.rs` | Maps decisions onto ack kinds |
| `crates/hook/src/config.rs` | Registers `SessionStart` |

---

### Task 1: A second watermark on the session cursor

**Files:**
- Modify: `crates/core/src/proto.rs` (add `AckKind` near `Request`)
- Modify: `crates/core/src/store/schema.rs:137-144`
- Modify: `crates/core/src/store/read.rs:376-434`
- Test: `crates/core/src/store/read.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces: `proto::AckKind` enum; `read::ensure_cursor(db, session_id, harness) -> Result<Cursor>`; `read::ack(db, session_id, harness, entry_id, kind: AckKind) -> Result<Cursor>`; `read::Cursor { last_entry_id, nudge_entry_id, blocks_issued, change_blocks_issued }`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/core/src/store/read.rs`:

```rust
#[test]
fn the_nudge_cursor_moves_without_disturbing_the_error_cursor() {
    let h = harness();
    let before = ensure_cursor(&h.db, "s1", "claude_code").unwrap();

    ack(&h.db, "s1", "claude_code", Some(500), AckKind::Nudge).unwrap();
    let after = ensure_cursor(&h.db, "s1", "claude_code").unwrap();

    assert_eq!(after.nudge_entry_id, 500, "the nudge cursor must advance");
    assert_eq!(
        after.last_entry_id, before.last_entry_id,
        "a nudge must not advance the error cursor, or the Stop hook loses its errors"
    );
    assert_eq!(after.blocks_issued, 0, "a nudge spends no block budget");
}

#[test]
fn each_block_kind_charges_only_its_own_budget() {
    let h = harness();

    ack(&h.db, "s1", "claude_code", Some(10), AckKind::StopBlock).unwrap();
    let c = ensure_cursor(&h.db, "s1", "claude_code").unwrap();
    assert_eq!((c.blocks_issued, c.change_blocks_issued), (1, 0));

    ack(&h.db, "s1", "claude_code", Some(20), AckKind::ChangeBlock).unwrap();
    let c = ensure_cursor(&h.db, "s1", "claude_code").unwrap();
    assert_eq!(
        (c.blocks_issued, c.change_blocks_issued),
        (1, 1),
        "a change block must not spend the Stop gate's budget"
    );
    assert_eq!(c.last_entry_id, 20, "both block kinds advance the error cursor");
}

#[test]
fn a_plain_ack_advances_the_error_cursor_and_charges_nothing() {
    let h = harness();
    ack(&h.db, "s1", "claude_code", Some(7), AckKind::Seen).unwrap();
    let c = ensure_cursor(&h.db, "s1", "claude_code").unwrap();
    assert_eq!(c.last_entry_id, 7);
    assert_eq!((c.blocks_issued, c.change_blocks_issued), (0, 0));
}
```

If no `harness()` helper exists in that test module, use whatever fixture the neighbouring tests use to build a `Db` (see the existing tests around `read.rs:560`).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p logbear-core --lib store::read`
Expected: FAIL — `AckKind` not found, `Cursor` not found.

- [ ] **Step 3: Add `AckKind` to the protocol**

In `crates/core/src/proto.rs`, above the `Request` enum:

```rust
/// What an `Ack` is acknowledging.
///
/// The error cursor and the nudge cursor answer different questions — "which errors are
/// new?" and "which lines has the agent not looked at?" — and advance on different events,
/// so one watermark cannot serve both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AckKind {
    /// Advance the error cursor. Charges nothing.
    #[default]
    Seen,
    /// A Stop block: advance the error cursor and charge the Stop budget.
    StopBlock,
    /// A PostToolUse block: advance the error cursor and charge the change budget.
    ChangeBlock,
    /// Advance the nudge cursor only.
    Nudge,
}

impl AckKind {
    /// Whether this kind reports errors to the agent, and so must advance the error cursor.
    /// A block whose watermark did not move re-fires on the same errors forever.
    pub fn advances_errors(self) -> bool {
        !matches!(self, AckKind::Nudge)
    }

    pub fn is_block(self) -> bool {
        matches!(self, AckKind::StopBlock | AckKind::ChangeBlock)
    }
}
```

Replace `record_block: bool` in `Request::Ack` (proto.rs:206-214) with:

```rust
    Ack {
        project: ProjectRef,
        session_id: String,
        harness: String,
        entry_id: Option<EntryId>,
        /// Which cursor to move and which budget, if any, to charge.
        #[serde(default)]
        kind: AckKind,
    },
```

- [ ] **Step 4: Add the columns**

In `crates/core/src/store/schema.rs`, change the `session_cursors` table (line 137) to:

```sql
CREATE TABLE IF NOT EXISTS session_cursors (
  session_id           TEXT    NOT NULL,
  harness              TEXT    NOT NULL,              -- claude_code | codex | mcp
  last_entry_id        INTEGER NOT NULL DEFAULT 0,
  last_seen_at         INTEGER NOT NULL,
  blocks_issued        INTEGER NOT NULL DEFAULT 0,
  -- Advanced when a verify nudge fires, never by the error path. Separate from
  -- last_entry_id so a nudge cannot consume errors the Stop hook has yet to report.
  nudge_entry_id       INTEGER NOT NULL DEFAULT 0,
  change_blocks_issued INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (session_id)
) WITHOUT ROWID;
```

Existing databases need the columns added rather than recreated. Find where `schema.rs` applies its DDL and add, after the `CREATE TABLE` statements run:

```rust
/// Columns added after v1 shipped. `ALTER TABLE ADD COLUMN` on an existing column is an
/// error rather than a no-op, so each is attempted and its "duplicate column" rejection
/// ignored — cheaper and less brittle than reading back `PRAGMA table_info`.
const ADDITIVE_COLUMNS: &[&str] = &[
    "ALTER TABLE session_cursors ADD COLUMN nudge_entry_id INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE session_cursors ADD COLUMN change_blocks_issued INTEGER NOT NULL DEFAULT 0",
];

pub fn apply_additive_columns(conn: &rusqlite::Connection) {
    for stmt in ADDITIVE_COLUMNS {
        let _ = conn.execute(stmt, []);
    }
}
```

Call `apply_additive_columns` immediately after the main schema DDL is executed. Do **not** bump `SCHEMA_VERSION`: these are additive with defaults, and a version bump implies a migration this does not need.

- [ ] **Step 5: Teach the cursor about both watermarks**

In `crates/core/src/store/read.rs`, replace `ensure_cursor` and `ack` (lines 372-434):

```rust
/// Everything a session's cursor row holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub last_entry_id: EntryId,
    pub nudge_entry_id: EntryId,
    pub blocks_issued: i64,
    pub change_blocks_issued: i64,
}

/// Reads the session's cursor, creating it if this session is new.
///
/// A new cursor starts at the current high-water mark rather than zero, so a fresh session
/// is not handed the project's entire error history.
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

/// Advances one watermark, charging the budget that matches `kind`.
///
/// The cursor advances when a block is issued rather than when the agent acknowledges it,
/// so the same errors cannot block twice even if the agent ignores them.
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
        // `max(…)` on every watermark: an out-of-order ack must never rewind a cursor and
        // re-show the agent what it has already been told.
        conn.execute(
            "UPDATE session_cursors
                SET last_entry_id  = CASE WHEN ?5 THEN max(last_entry_id, ?2)
                                          ELSE last_entry_id END,
                    nudge_entry_id = CASE WHEN ?5 THEN nudge_entry_id
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
                kind.advances_errors(),
                (kind == AckKind::ChangeBlock) as i64,
            ],
        )?;
        Ok(())
    })?;
    ensure_cursor(db, session_id, harness)
}
```

Add `use crate::proto::AckKind;` to the imports at the top of `read.rs`.

- [ ] **Step 6: Fix the call sites this breaks**

`read::unread` (read.rs:329) now receives a `Cursor` rather than a tuple. Update its three uses:

```rust
    let cursor = ensure_cursor(db, session_id, harness)?;
```
then replace `cursor.0` with `cursor.last_entry_id` (three occurrences: the `EntryFilter`, the count query, the signature query) and `cursor.1` with `cursor.blocks_issued` (two occurrences in the returned `UnreadState`).

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p logbear-core --lib store::read`
Expected: PASS — all three new tests plus the existing ones.

- [ ] **Step 8: Commit**

```bash
git add crates/core/src/proto.rs crates/core/src/store/schema.rs crates/core/src/store/read.rs
git commit -m "Give each session a second watermark for verify nudges

The error cursor answers which errors are new and may only advance when
they are reported; a nudge that advanced it would consume errors the Stop
hook exists to surface. They are different questions, so they get
different columns and different budgets."
```

---

### Task 2: `unseen_lines` and `server_running`

**Files:**
- Modify: `crates/core/src/proto.rs` (`UnreadState`, `Settings`)
- Modify: `crates/core/src/store/read.rs:322-370`
- Test: `crates/core/src/store/read.rs`

**Interfaces:**
- Consumes: `read::Cursor` from Task 1
- Produces: `UnreadState.unseen_lines: i64`, `UnreadState.server_running: bool`, `Settings.change_block_budget: i64`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn unseen_lines_counts_every_level_not_just_errors() {
    let h = harness();
    // Reset both cursors to the bottom so everything written counts as unseen.
    ack(&h.db, "s1", "claude_code", Some(0), AckKind::Seen).unwrap();

    write_entry(&h, Level::Info, "compiled successfully");
    write_entry(&h, Level::Error, "boom");

    let state = unread(&h.db, "s1", "claude_code", 3, 2, 10).unwrap();
    assert_eq!(state.new_errors, 1, "only the error counts as an error");
    assert_eq!(state.unseen_lines, 2, "both lines are unseen");
}

#[test]
fn a_nudge_clears_unseen_lines_without_clearing_errors() {
    let h = harness();
    ack(&h.db, "s1", "claude_code", Some(0), AckKind::Seen).unwrap();
    write_entry(&h, Level::Info, "compiled successfully");

    ack(&h.db, "s1", "claude_code", None, AckKind::Nudge).unwrap();

    let state = unread(&h.db, "s1", "claude_code", 3, 2, 10).unwrap();
    assert_eq!(state.unseen_lines, 0, "the nudge covered those lines");
}

#[test]
fn server_running_tracks_whether_a_run_is_open() {
    let h = harness();
    let state = unread(&h.db, "s1", "claude_code", 3, 2, 10).unwrap();
    assert!(!state.server_running, "no run has been started");
    // `harness()` fixtures start a run; if yours does, assert the true case instead by
    // opening one with the same helper the neighbouring tests use.
}
```

Use whatever helper the existing tests in this module use to insert entries; name it `write_entry` here for readability and adapt to the real fixture.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p logbear-core --lib store::read`
Expected: FAIL — `unread` takes 5 arguments, `unseen_lines` does not exist.

- [ ] **Step 3: Add the fields**

In `crates/core/src/proto.rs`, add to `UnreadState` (after `push_enabled`):

```rust
    /// Entries of any level after the nudge cursor. The verify nudge fires on this, not on
    /// `new_errors`: a change that produced only ordinary output still needs looking at.
    pub unseen_lines: i64,
    /// Whether a run is open for this project. Nothing is worth checking if nothing is
    /// running to produce it.
    pub server_running: bool,
```

Add to `Settings` (after `block_budget`):

```rust
    /// PostToolUse blocks per session. Deliberately smaller than `block_budget` and charged
    /// separately, so a burst of new signatures mid-turn cannot starve the Stop gate.
    pub change_block_budget: i64,
```

and to its `Default` impl: `change_block_budget: 2,`.

Verify `UnreadState` derives `Default`; the new fields default correctly (`0`, `false`).

- [ ] **Step 4: Compute them**

In `read.rs`, change `unread`'s signature and body:

```rust
pub fn unread(
    db: &Db,
    session_id: &str,
    harness: &str,
    block_budget: i64,
    change_block_budget: i64,
    max_entries: usize,
) -> Result<UnreadState> {
    let cursor = ensure_cursor(db, session_id, harness)?;
```

Keep the existing `entries`, count and signature queries, then before the `Ok(UnreadState { … })`:

```rust
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
```

and extend the returned struct:

```rust
        blocks_issued: cursor.blocks_issued,
        blocks_remaining: (block_budget - cursor.blocks_issued).max(0),
        change_blocks_remaining: (change_block_budget - cursor.change_blocks_issued).max(0),
        push_enabled: true,
        unseen_lines,
        server_running,
        entries,
```

Add `change_blocks_remaining: i64` to `UnreadState` alongside the other fields in proto.rs.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p logbear-core --lib store::read`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/proto.rs crates/core/src/store/read.rs
git commit -m "Report unseen lines and whether a server is running

The verify nudge fires on ordinary output, not only on errors: a change
that logged nothing needs no attention, and one that logged a clean
reload still deserves a look."
```

---

### Task 3: The daemon charges the right budget

**Files:**
- Modify: `crates/daemon/src/conn.rs:281-317`
- Modify: `crates/core/src/client.rs:172-187`
- Modify: `crates/mcp/src/backend.rs:145`
- Modify: `crates/hook/src/lib.rs:33-67, 96-113`

**Interfaces:**
- Consumes: `AckKind`, the six-argument `read::unread` from Tasks 1–2
- Produces: `Client::ack(project, session_id, harness, entry_id, kind: AckKind)`; `hook::Daemon::ack` with the same trailing parameter

- [ ] **Step 1: Update the client**

In `crates/core/src/client.rs`, change `ack`:

```rust
    pub fn ack(
        &mut self,
        project: ProjectRef,
        session_id: &str,
        harness: &str,
        entry_id: Option<EntryId>,
        kind: AckKind,
    ) -> Result<UnreadState> {
        self.call(&Request::Ack {
            project,
            session_id: session_id.to_string(),
            harness: harness.to_string(),
            entry_id,
            kind,
        })
    }
```

Add `AckKind` to the `use crate::proto::*;` coverage (it is already glob-imported).

- [ ] **Step 2: Update the daemon handler**

In `crates/daemon/src/conn.rs`, replace the `Request::Unread` and `Request::Ack` arms:

```rust
        Request::Unread {
            project,
            session_id,
            harness,
        } => {
            let h = daemon.resolve(&project)?;
            let s = daemon.settings();
            let mut state =
                read::unread(&h.db, &session_id, &harness, s.block_budget, s.change_block_budget, 20)?;
            state.push_enabled = h.project.push_enabled && s.push_enabled;
            to_value(state)?
        }

        Request::Ack {
            project,
            session_id,
            harness,
            entry_id,
            kind,
        } => {
            let h = daemon.resolve(&project)?;
            let s = daemon.settings();
            if kind.is_block() {
                // Record the reported signatures before advancing, so a repeat of the same
                // one can never block this session twice.
                let reported =
                    read::unread(&h.db, &session_id, &harness, s.block_budget, s.change_block_budget, 50)?;
                let fps: Vec<FingerprintId> = reported
                    .entries
                    .iter()
                    .filter_map(|e| e.entry.fingerprint_id)
                    .collect();
                read::record_block(&h.db, &session_id, &fps)?;
            }
            read::ack(&h.db, &session_id, &harness, entry_id, kind)?;
            let mut state =
                read::unread(&h.db, &session_id, &harness, s.block_budget, s.change_block_budget, 20)?;
            state.push_enabled = h.project.push_enabled && s.push_enabled;
            to_value(state)?
        }
```

- [ ] **Step 3: Update the two remaining callers**

`crates/mcp/src/backend.rs:145` — the MCP ack records that the agent read entries, which is `Seen`:

```rust
        self.with_client(|c| c.ack(project.clone(), session_id, HARNESS, entry_id, AckKind::Seen))
```

`crates/hook/src/lib.rs` — change the trait and its `Client` impl to take `kind: AckKind` instead of `record_block: bool`, passing it straight through. In `respond()`, replace the `/* record_block */ true` argument with `AckKind::StopBlock` for now; Task 8 makes it depend on the decision.

- [ ] **Step 4: Update the fake daemon in the hook tests**

In `crates/hook/tests/hooks.rs`, change the `FakeDaemon::ack` signature to take `kind: AckKind` and record it:

```rust
    acks: Vec<(Option<EntryId>, AckKind)>,
```

Update the existing assertions that match on `record_block` booleans to match on `AckKind::StopBlock` instead.

- [ ] **Step 5: Verify the whole workspace builds and passes**

Run: `cargo clippy --workspace --all-targets && cargo test --workspace`
Expected: clean clippy, all tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/daemon/src/conn.rs crates/core/src/client.rs crates/mcp/src/backend.rs crates/hook/src/lib.rs crates/hook/tests/hooks.rs
git commit -m "Charge each block kind against its own budget

A boolean could say a block happened but not which gate issued it, so
both spent the same allowance and a noisy turn could leave the Stop hook
unable to block when it mattered most."
```

---

### Task 4: The intent classifier

**Files:**
- Create: `crates/hook/src/intent.rs`
- Modify: `crates/hook/src/lib.rs` (add `pub mod intent;`)

**Interfaces:**
- Produces: `intent::Intent { StartsServer { suggested: String }, ChasesBug, Neither }`; `intent::classify(command: &str) -> Intent`

- [ ] **Step 1: Write the failing test**

Create `crates/hook/src/intent.rs` with only the test module, so the test fails to compile against a missing `classify`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn starts(cmd: &str) -> String {
        match classify(cmd) {
            Intent::StartsServer { suggested } => suggested,
            other => panic!("{cmd:?} should start a server, got {other:?}"),
        }
    }

    #[test]
    fn the_common_dev_servers_are_recognised() {
        assert_eq!(starts("npm run dev"), "logbear npm run dev");
        assert_eq!(starts("pnpm dev"), "logbear pnpm dev");
        assert_eq!(starts("bun run start"), "logbear bun run start");
        assert_eq!(starts("mix phx.server"), "logbear mix phx.server");
        assert_eq!(starts("rails server"), "logbear rails server");
        assert_eq!(starts("python manage.py runserver"), "logbear python manage.py runserver");
        assert_eq!(starts("vite"), "logbear vite");
        assert_eq!(starts("next dev"), "logbear next dev");
        assert_eq!(starts("cargo watch -x run"), "logbear cargo watch -x run");
    }

    #[test]
    fn a_command_already_wrapped_is_left_alone() {
        assert_eq!(classify("logbear npm run dev"), Intent::Neither);
        assert_eq!(classify("  logbear   mix phx.server"), Intent::Neither);
    }

    #[test]
    fn one_off_commands_are_never_server_starts() {
        // These terminate and hand their output straight back to the agent, so logbear adds
        // nothing and a nudge would be pure noise.
        for cmd in [
            "npm run build",
            "npm test",
            "cargo test",
            "cargo build --release",
            "tsc --noEmit",
            "git status",
            "ls -la",
            "docker compose up -d",
        ] {
            assert_eq!(classify(cmd), Intent::Neither, "{cmd:?} must stay silent");
        }
    }

    #[test]
    fn hunting_through_log_files_is_recognised() {
        for cmd in [
            "tail -f /tmp/dev.log",
            "tail -100 server.log",
            "grep -i error build.log",
            "cat nohup.out",
            "docker logs web",
            "journalctl -u myapp",
        ] {
            assert_eq!(classify(cmd), Intent::ChasesBug, "{cmd:?} should redirect");
        }
    }

    #[test]
    fn grepping_source_is_not_hunting_logs() {
        // Only log files count; searching code is ordinary work.
        assert_eq!(classify("grep -rn TODO src/"), Intent::Neither);
        assert_eq!(classify("cat src/main.rs"), Intent::Neither);
    }

    #[test]
    fn a_compound_command_is_judged_on_any_of_its_parts() {
        assert_eq!(
            starts("cd frontend && npm run dev"),
            "logbear npm run dev",
            "the suggestion names the server, not the cd"
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p logbear-hook --lib intent`
Expected: FAIL — `classify` and `Intent` not found.

- [ ] **Step 3: Implement the classifier**

Prepend to `crates/hook/src/intent.rs`:

```rust
//! What a Bash command is trying to do, as far as logbear cares.
//!
//! Pure and I/O-free, like [`crate::decide`]: the rules are worth testing on their own, and
//! a classifier that needed a daemon could not be.
//!
//! The distinction that matters is process lifetime. A command that runs until killed loses
//! its output unless logbear is holding it; a command that terminates hands its output
//! straight back to the agent, so saying anything about it is noise.

/// The wrapper the agent should have used.
const WRAPPER: &str = "logbear";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Runs until killed. Its output is being lost.
    StartsServer { suggested: String },
    /// Reaching for output logbear already holds.
    ChasesBug,
    /// Anything else. The overwhelmingly common case, and always silent.
    Neither,
}

/// Commands that run until killed, as `(program, first-argument-or-empty)` prefixes.
///
/// Matched on the leading words rather than by substring, so `npm run dev` matches but
/// `echo npm run dev` does not.
const SERVER_HEADS: &[&[&str]] = &[
    &["vite"],
    &["next", "dev"],
    &["nuxt", "dev"],
    &["astro", "dev"],
    &["remix", "dev"],
    &["rails", "server"],
    &["rails", "s"],
    &["mix", "phx.server"],
    &["cargo", "watch"],
    &["flask", "run"],
    &["hugo", "server"],
    &["jekyll", "serve"],
];

/// Package-manager scripts whose conventional names mean "run until killed".
const RUNNERS: &[&str] = &["npm", "pnpm", "yarn", "bun", "deno"];
const LONG_LIVED_SCRIPTS: &[&str] = &["dev", "start", "serve", "watch"];

/// Programs that read back output logbear already has.
const LOG_READERS: &[&str] = &["tail", "less", "journalctl"];

pub fn classify(command: &str) -> Intent {
    // A compound command is judged on its parts: `cd frontend && npm run dev` starts a
    // server, and the advice has to name the server rather than the whole line.
    for segment in split_segments(command) {
        let words = words(segment);
        if words.is_empty() {
            continue;
        }
        // Already captured. Nothing to say about it, and re-wrapping would be wrong.
        if words[0] == WRAPPER {
            return Intent::Neither;
        }
        if is_server(&words) {
            return Intent::StartsServer {
                suggested: format!("{WRAPPER} {}", words.join(" ")),
            };
        }
    }
    for segment in split_segments(command) {
        if is_log_hunt(&words(segment)) {
            return Intent::ChasesBug;
        }
    }
    Intent::Neither
}

fn is_server(words: &[&str]) -> bool {
    if SERVER_HEADS
        .iter()
        .any(|head| words.len() >= head.len() && words[..head.len()] == **head)
    {
        return true;
    }
    // `python manage.py runserver`, and the `./manage.py` form.
    if words.iter().any(|w| w.ends_with("manage.py")) && words.contains(&"runserver") {
        return true;
    }
    // `docker compose up` holds the terminal; `-d` detaches and returns immediately.
    if words.len() >= 3
        && words[0] == "docker"
        && words[1] == "compose"
        && words[2] == "up"
        && !words.iter().any(|w| *w == "-d" || *w == "--detach")
    {
        return true;
    }
    if RUNNERS.contains(&words[0]) {
        // `npm run dev` and the `npm dev` shorthand both reach the same script.
        let script = match words.get(1) {
            Some(&"run") => words.get(2),
            other => other,
        };
        return script.is_some_and(|s| LONG_LIVED_SCRIPTS.contains(s));
    }
    false
}

fn is_log_hunt(words: &[&str]) -> bool {
    let Some(program) = words.first() else {
        return false;
    };
    if LOG_READERS.contains(program) {
        return true;
    }
    if *program == "docker" && words.get(1) == Some(&"logs") {
        return true;
    }
    // `cat` and `grep` are ordinary tools; only a log file makes them a log hunt.
    if matches!(*program, "cat" | "grep" | "rg" | "head") {
        return words.iter().skip(1).any(is_log_path);
    }
    false
}

fn is_log_path(word: &&str) -> bool {
    let w = word.trim_end_matches(['"', '\'']);
    w.ends_with(".log") || w.ends_with("nohup.out") || w.ends_with(".out")
}

/// Splits on the shell operators that separate whole commands. Deliberately not a shell
/// parser: a wrong split costs at most a missed or misdirected nudge, never a wrong action.
fn split_segments(command: &str) -> Vec<&str> {
    command
        .split(&["&&", "||", ";", "|"][..])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

fn words(segment: &str) -> Vec<&str> {
    segment.split_whitespace().collect()
}
```

Add `pub mod intent;` to `crates/hook/src/lib.rs` beside the other module declarations.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p logbear-hook --lib intent`
Expected: PASS — all six tests.

- [ ] **Step 5: Commit**

```bash
git add crates/hook/src/intent.rs crates/hook/src/lib.rs
git commit -m "Classify what a Bash command is trying to do

Process lifetime is the distinction that matters: a command that runs
until killed loses its output unless logbear holds it, and one that
terminates hands its output straight back, so saying anything is noise."
```

---

### Task 5: The payload carries the command and SessionStart

**Files:**
- Modify: `crates/hook/src/payload.rs:32-65, 102-116`
- Create: `crates/hook/tests/payloads/claude_session_start.json`
- Create: `crates/hook/tests/payloads/claude_bash_dev_server.json`
- Test: `crates/hook/tests/hooks.rs`

**Interfaces:**
- Produces: `Event::SessionStart`; `Payload.command: Option<String>`

- [ ] **Step 1: Write the failing test**

Add to `crates/hook/tests/hooks.rs`:

```rust
#[test]
fn a_bash_payload_carries_the_command_it_ran() {
    let p = payload::parse(&golden("claude_bash_dev_server"), Some(Harness::ClaudeCode)).unwrap();
    assert_eq!(p.tool_name.as_deref(), Some("Bash"));
    assert_eq!(p.command.as_deref(), Some("npm run dev"));
}

#[test]
fn a_session_start_payload_parses() {
    let p = payload::parse(&golden("claude_session_start"), Some(Harness::ClaudeCode)).unwrap();
    assert_eq!(p.event, Event::SessionStart);
}

#[test]
fn an_edit_payload_has_no_command() {
    let p = payload::parse(&golden("claude_post_tool_use"), Some(Harness::ClaudeCode)).unwrap();
    assert_eq!(p.command, None, "only Bash carries a command");
}
```

- [ ] **Step 2: Create the golden payloads**

`crates/hook/tests/payloads/claude_bash_dev_server.json`:

```json
{
  "session_id": "cc-sess-0001",
  "prompt_id": "550e8400-e29b-41d4-a716-446655440002",
  "transcript_path": "/home/dev/.claude/projects/demo/transcript.jsonl",
  "cwd": "/home/dev/demo",
  "permission_mode": "default",
  "hook_event_name": "PostToolUse",
  "tool_name": "Bash",
  "tool_input": {
    "command": "npm run dev",
    "description": "Start the dev server"
  },
  "tool_use_id": "toolu_01ABC456",
  "tool_result": "",
  "tool_result_is_error": false
}
```

`crates/hook/tests/payloads/claude_session_start.json`:

```json
{
  "session_id": "cc-sess-0001",
  "transcript_path": "/home/dev/.claude/projects/demo/transcript.jsonl",
  "cwd": "/home/dev/demo",
  "permission_mode": "default",
  "hook_event_name": "SessionStart",
  "source": "startup"
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p logbear-hook --test hooks`
Expected: FAIL — no field `command` on `Payload`, no variant `SessionStart`.

- [ ] **Step 4: Extend the payload**

In `crates/hook/src/payload.rs`, add the variant to `Event`:

```rust
pub enum Event {
    Stop,
    SubagentStop,
    PostToolUse,
    SessionStart,
    /// A lifecycle event logbear does not register for. Always allowed.
    Other(String),
}
```

and to both `parse` (`"SessionStart" => Event::SessionStart,`) and `name` (`Event::SessionStart => "SessionStart",`). Leave `is_stop` alone — `SessionStart` is not a stop.

Add the field to `Payload`:

```rust
    /// The shell command, for `Bash`-family tools only. Other tools carry no command, and a
    /// file path is not one.
    pub command: Option<String>,
```

and populate it in `parse`, after `tool_name`:

```rust
        command: obj
            .get("tool_input")
            .and_then(|v| v.get("command"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
```

Reading `tool_input.command` rather than gating on the tool name keeps this correct across both harnesses, which name the shell tool differently (`Bash` and `shell`) but agree on the field.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p logbear-hook --test hooks`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/hook/src/payload.rs crates/hook/tests/payloads/ crates/hook/tests/hooks.rs
git commit -m "Parse the command a Bash tool call ran, and SessionStart

The command was being thrown away, so nothing downstream could tell a dev
server from a build."
```

---

### Task 6: The verify nudge

**Files:**
- Modify: `crates/hook/src/decide.rs:17-50` and add the copy
- Test: `crates/hook/tests/hooks.rs`

**Interfaces:**
- Consumes: `UnreadState.unseen_lines`, `.server_running` (Task 2); `Payload.command` (Task 5)
- Produces: `decide::Input<'a>`; `Decision::Context { text, ack_nudge }`; `Decision::Block { reason, budget: Budget }`

- [ ] **Step 1: Write the failing test**

```rust
/// A change while a server is live and has said something: the whole point of the feature.
#[test]
fn a_change_with_unseen_server_output_asks_the_agent_to_look() {
    let mut state = UnreadState { server_running: true, unseen_lines: 4, ..Default::default() };
    state.push_enabled = true;
    let d = decide::decide(
        &decide::Input { event: &Event::PostToolUse, tool: Some("Edit"), command: None },
        &state,
    );
    match d {
        decide::Decision::Context { text, ack_nudge } => {
            assert!(text.contains("4"), "the count belongs in the message: {text}");
            assert!(text.contains("logbear logs"), "it must say how to look: {text}");
            assert!(ack_nudge, "the nudge cursor must advance or this repeats forever");
        }
        other => panic!("expected a nudge, got {other:?}"),
    }
}

#[test]
fn a_change_with_a_silent_server_says_nothing() {
    let mut state = UnreadState { server_running: true, unseen_lines: 0, ..Default::default() };
    state.push_enabled = true;
    assert_eq!(
        decide::decide(
            &decide::Input { event: &Event::PostToolUse, tool: Some("Edit"), command: None },
            &state
        ),
        decide::Decision::Allow
    );
}

#[test]
fn a_change_with_no_server_running_says_nothing() {
    let mut state = UnreadState { server_running: false, unseen_lines: 12, ..Default::default() };
    state.push_enabled = true;
    assert_eq!(
        decide::decide(
            &decide::Input { event: &Event::PostToolUse, tool: Some("Edit"), command: None },
            &state
        ),
        decide::Decision::Allow,
        "nothing is running, so there is nothing to verify"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p logbear-hook --test hooks`
Expected: FAIL — `decide::Input` does not exist.

- [ ] **Step 3: Restructure `Decision` and `decide`**

In `crates/hook/src/decide.rs`:

```rust
/// Which allowance a block spends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// The Stop gate. The important one; never spent by an advisory rule.
    Stop,
    /// PostToolUse escalation, on its own smaller allowance.
    Change,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Say nothing. The common case.
    Allow,
    /// The agent is forced to continue with `reason` in front of it.
    Block { reason: String, budget: Budget },
    /// Advisory text. Never interrupts the turn.
    ///
    /// `ack_nudge` advances the nudge cursor. Set only by the verify nudge: every other
    /// advisory reports errors the Stop hook still needs to see.
    Context { text: String, ack_nudge: bool },
}

/// Everything the rules read, so the signature does not grow a parameter per rule.
pub struct Input<'a> {
    pub event: &'a Event,
    pub tool: Option<&'a str>,
    pub command: Option<&'a str>,
}

impl Input<'_> {
    /// Tools that change code. Both harnesses' names, since one binary serves both.
    fn is_change(&self) -> bool {
        matches!(
            self.tool,
            Some("Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "apply_patch" | "edit_file")
        )
    }
}
```

Replace `decide` with:

```rust
pub fn decide(input: &Input, unread: &UnreadState) -> Decision {
    // Push is off for this project; nothing else applies.
    if !unread.push_enabled {
        return Decision::Allow;
    }

    match input.event {
        Event::SessionStart => Decision::Allow, // Task 9 fills this in.

        // Past the block budget, errors degrade to advisory context: blocking without a cap
        // traps the agent forever.
        Event::Stop | Event::SubagentStop if unread.is_empty() => Decision::Allow,
        Event::Stop | Event::SubagentStop if unread.blocks_remaining > 0 => Decision::Block {
            reason: block_reason(unread),
            budget: Budget::Stop,
        },
        Event::Stop | Event::SubagentStop => Decision::Context {
            text: summary(unread),
            ack_nudge: false,
        },

        Event::PostToolUse => post_tool_use(input, unread),
        Event::Other(_) => Decision::Allow,
    }
}

/// Errors outrank nudges: the agent gets one message per event, and an error is always the
/// more useful one.
fn post_tool_use(input: &Input, unread: &UnreadState) -> Decision {
    if !unread.is_empty() {
        return Decision::Context {
            text: summary(unread),
            ack_nudge: false,
        };
    }
    if input.is_change() && unread.server_running && unread.unseen_lines > 0 {
        return Decision::Context {
            text: verify_nudge(unread.unseen_lines),
            ack_nudge: true,
        };
    }
    Decision::Allow
}

/// The change landed and the server reacted. Whether it reacted *well* is what the agent
/// cannot know without looking.
fn verify_nudge(lines: i64) -> String {
    format!(
        "logbear: the running process logged {lines} new line{} after your change. \
         Check them with `logbear logs` or the logbear MCP tools to confirm it did what \
         you intended — a change that compiles can still be wrong.",
        plural(lines),
    )
}
```

Update `render` to destructure the new field shapes (`Decision::Block { reason, .. }` and `Decision::Context { text, .. }`).

- [ ] **Step 4: Update `respond` and the existing tests**

In `crates/hook/src/lib.rs`, build the `Input` from the payload and map the decision onto an ack:

```rust
    let mut decision = decide::decide(
        &decide::Input {
            event: &payload.event,
            tool: payload.tool_name.as_deref(),
            command: payload.command.as_deref(),
        },
        &unread,
    );

    match &decision {
        Decision::Block { budget, .. } => {
            let kind = match budget {
                decide::Budget::Stop => AckKind::StopBlock,
                decide::Budget::Change => AckKind::ChangeBlock,
            };
            if let Err(e) = daemon.ack(project, &payload.session_id, harness, decide::watermark(&unread), kind) {
                eprintln!("logbear-hook: could not spend the block budget ({e}); degrading");
                decision = Decision::Context {
                    text: match &decision {
                        Decision::Block { reason, .. } => reason.clone(),
                        _ => unreachable!("checked immediately above"),
                    },
                    ack_nudge: false,
                };
            }
        }
        // A nudge that does not advance its cursor repeats on every later change.
        Decision::Context { ack_nudge: true, .. } => {
            if let Err(e) = daemon.ack(project, &payload.session_id, harness, None, AckKind::Nudge) {
                // Advisory only: the agent still gets told, it may just be told twice.
                eprintln!("logbear-hook: could not advance the nudge cursor ({e})");
            }
        }
        _ => {}
    }
```

Existing tests in `hooks.rs` that construct or match `Decision::Block { reason }` need the `budget` field; those matching `Decision::Context { text }` need `ack_nudge`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p logbear-hook`
Expected: PASS — new tests and all existing ones.

- [ ] **Step 6: Commit**

```bash
git add crates/hook/src/decide.rs crates/hook/src/lib.rs crates/hook/tests/hooks.rs
git commit -m "Ask the agent to check the logs after a change

An error the daemon recognised was the only signal a change had gone
wrong, so a change that broke behaviour quietly read as success."
```

---

### Task 7: The Bash intent nudges

**Files:**
- Modify: `crates/hook/src/decide.rs` (`post_tool_use`)
- Test: `crates/hook/tests/hooks.rs`

**Interfaces:**
- Consumes: `intent::classify` (Task 4), `Input.command` (Task 5)

- [ ] **Step 1: Write the failing test**

```rust
fn quiet_state() -> UnreadState {
    UnreadState { push_enabled: true, ..Default::default() }
}

#[test]
fn an_uncaptured_dev_server_is_named_with_the_wrapped_form() {
    let d = decide::decide(
        &decide::Input { event: &Event::PostToolUse, tool: Some("Bash"), command: Some("npm run dev") },
        &quiet_state(),
    );
    match d {
        decide::Decision::Context { text, ack_nudge } => {
            assert!(text.contains("logbear npm run dev"), "must show the exact fix: {text}");
            assert!(!ack_nudge, "an intent nudge reads no entries, so it moves no cursor");
        }
        other => panic!("expected a nudge, got {other:?}"),
    }
}

#[test]
fn a_wrapped_dev_server_is_left_alone() {
    assert_eq!(
        decide::decide(
            &decide::Input { event: &Event::PostToolUse, tool: Some("Bash"), command: Some("logbear npm run dev") },
            &quiet_state()
        ),
        decide::Decision::Allow
    );
}

#[test]
fn log_hunting_is_redirected_only_when_logbear_has_something() {
    let hunting = decide::Input {
        event: &Event::PostToolUse,
        tool: Some("Bash"),
        command: Some("tail -f /tmp/dev.log"),
    };
    // Nothing captured yet: the advice would send the agent to an empty database.
    assert_eq!(decide::decide(&hunting, &quiet_state()), decide::Decision::Allow);

    let with_history = UnreadState { server_running: true, ..quiet_state() };
    match decide::decide(&hunting, &with_history) {
        decide::Decision::Context { text, .. } => assert!(text.contains("logbear logs"), "{text}"),
        other => panic!("expected a redirect, got {other:?}"),
    }
}

#[test]
fn an_ordinary_command_says_nothing() {
    for cmd in ["cargo test", "git status", "npm run build"] {
        assert_eq!(
            decide::decide(
                &decide::Input { event: &Event::PostToolUse, tool: Some("Bash"), command: Some(cmd) },
                &quiet_state()
            ),
            decide::Decision::Allow,
            "{cmd} must stay silent"
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p logbear-hook --test hooks`
Expected: FAIL — the dev-server case returns `Allow`.

- [ ] **Step 3: Add the intent branch**

Extend `post_tool_use` in `decide.rs`, after the verify-nudge branch:

```rust
    if let Some(command) = input.command {
        return match intent::classify(command) {
            Intent::StartsServer { suggested } => Decision::Context {
                text: format!(
                    "logbear: that process is running uncaptured, so its output is not being \
                     recorded and its errors will not reach you. Stop it and start it again \
                     as `{suggested}`.",
                ),
                ack_nudge: false,
            },
            // Only worth saying when there is something to redirect to; on a project with
            // no history the advice points at an empty database.
            Intent::ChasesBug if unread.server_running => Decision::Context {
                text: "logbear: this project's output is already captured. \
                       `logbear logs --level error`, or the logbear MCP tools, cover the \
                       whole run rather than whatever happened to reach that file."
                    .to_string(),
                ack_nudge: false,
            },
            _ => Decision::Allow,
        };
    }
    Decision::Allow
```

Add `use crate::intent::{self, Intent};` at the top of `decide.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p logbear-hook`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/hook/src/decide.rs crates/hook/tests/hooks.rs
git commit -m "Nudge an uncaptured server, and redirect log hunts

Both cases are the agent working without logbear while logbear could
have helped: one loses the output, the other goes looking for output
already captured."
```

---

### Task 8: New-signature escalation on its own budget

**Files:**
- Modify: `crates/hook/src/decide.rs` (`post_tool_use`)
- Test: `crates/hook/tests/hooks.rs`

**Interfaces:**
- Consumes: `UnreadState.change_blocks_remaining` (Task 2), `Budget::Change` (Task 6)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_never_seen_signature_after_a_change_blocks_on_the_change_budget() {
    let mut state = unread();               // 2 errors, 1 new signature
    state.server_running = true;
    state.change_blocks_remaining = 2;
    match decide::decide(
        &decide::Input { event: &Event::PostToolUse, tool: Some("Edit"), command: None },
        &state,
    ) {
        decide::Decision::Block { reason, budget } => {
            assert_eq!(budget, decide::Budget::Change, "must not spend the Stop gate's budget");
            assert!(reason.contains("NEW SIGNATURE"), "the detail is the point: {reason}");
        }
        other => panic!("expected a block, got {other:?}"),
    }
}

#[test]
fn a_recurring_signature_stays_advisory() {
    let mut state = unread();
    state.new_signatures = 0;
    state.change_blocks_remaining = 2;
    match decide::decide(
        &decide::Input { event: &Event::PostToolUse, tool: Some("Edit"), command: None },
        &state,
    ) {
        decide::Decision::Context { .. } => {}
        other => panic!("a recurring error must not block, got {other:?}"),
    }
}

#[test]
fn an_exhausted_change_budget_degrades_to_context() {
    let mut state = unread();
    state.change_blocks_remaining = 0;
    state.blocks_remaining = 3;
    match decide::decide(
        &decide::Input { event: &Event::PostToolUse, tool: Some("Edit"), command: None },
        &state,
    ) {
        decide::Decision::Context { .. } => {}
        other => panic!("past its budget it must degrade, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p logbear-hook --test hooks`
Expected: FAIL — returns `Context`, not `Block`.

- [ ] **Step 3: Add the escalation**

Replace the first branch of `post_tool_use`:

```rust
    if !unread.is_empty() {
        // A signature never seen in this project is worth interrupting for: it is almost
        // certainly what the change just did, and it is cheapest to fix now. A recurring one
        // is not — it was already there.
        if input.is_change() && unread.new_signatures > 0 && unread.change_blocks_remaining > 0 {
            return Decision::Block {
                reason: block_reason(unread),
                budget: Budget::Change,
            };
        }
        return Decision::Context {
            text: summary(unread),
            ack_nudge: false,
        };
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p logbear-hook && cargo test --workspace`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/hook/src/decide.rs crates/hook/tests/hooks.rs
git commit -m "Interrupt on a signature this project has never seen

A brand-new error right after a change is almost certainly that change,
and is cheapest to fix while the agent still has the context for it."
```

---

### Task 9: The standing rule at SessionStart

**Files:**
- Modify: `crates/hook/src/decide.rs` (`decide`, `render`)
- Modify: `crates/hook/src/config.rs:47-72`
- Test: `crates/hook/tests/hooks.rs`, `crates/hook/src/config.rs`

**Interfaces:**
- Consumes: `Event::SessionStart` (Task 5)

- [ ] **Step 1: Write the failing test**

In `crates/hook/tests/hooks.rs`:

```rust
#[test]
fn session_start_teaches_the_agent_to_wrap_long_running_commands() {
    let mut daemon = FakeDaemon::with(UnreadState { push_enabled: true, ..Default::default() });
    let v = run("claude_session_start", Some(Harness::ClaudeCode), &mut daemon);
    let text = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or_default();
    assert!(text.contains("logbear"), "{text}");
    assert!(text.contains("dev server"), "it must say which commands: {text}");
}

/// Codex accepts `additionalContext` on SessionStart — unlike its Stop, which does not
/// define `hookSpecificOutput` at all.
#[test]
fn codex_session_start_uses_additional_context_too() {
    let mut daemon = FakeDaemon::with(UnreadState { push_enabled: true, ..Default::default() });
    let v = run("claude_session_start", Some(Harness::Codex), &mut daemon);
    assert!(v["hookSpecificOutput"]["additionalContext"].is_string());
}
```

In `crates/hook/src/config.rs` tests:

```rust
    #[test]
    fn both_harnesses_register_session_start() {
        for harness in [Harness::ClaudeCode, Harness::Codex] {
            let v = emit(harness, "/usr/local/bin/logbear-hook");
            assert!(
                v["hooks"]["SessionStart"][0]["hooks"][0]["command"].is_string(),
                "{harness:?} must register SessionStart"
            );
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p logbear-hook`
Expected: FAIL — `SessionStart` returns `Allow`, and the config has no `SessionStart` key.

- [ ] **Step 3: Emit the standing rule**

In `decide.rs`, replace the `Event::SessionStart => Decision::Allow` arm:

```rust
        // Stated once, up front, so the agent gets it right the first time rather than
        // being corrected after the output is already lost.
        Event::SessionStart => Decision::Context {
            text: STANDING_RULE.to_string(),
            ack_nudge: false,
        },
```

and add the copy near the other constants:

```rust
/// What the agent needs to know before it starts anything.
const STANDING_RULE: &str = "\
logbear is recording this project. Start long-running processes — dev servers, watchers, \
anything that runs until you kill it — with `logbear` in front, for example \
`logbear npm run dev` or `logbear mix phx.server`. Their output is then captured and any \
errors are surfaced to you automatically. One-off commands such as tests and builds do not \
need it, since you already see their output.";
```

`render` needs no change: `SessionStart` is not a stop, so it takes the
`hookSpecificOutput.additionalContext` path on both harnesses, which both accept.

- [ ] **Step 4: Register the event**

In `config.rs`, add to the `claude` block:

```rust
            "SessionStart": [{ "matcher": "", "hooks": [h] }],
```

and to the `codex` block:

```rust
            "SessionStart": [{ "hooks": [h] }],
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p logbear-hook`
Expected: PASS

- [ ] **Step 6: Full verification**

Run: `cargo clippy --workspace --all-targets && cargo test --workspace`
Expected: clippy clean, every test passing.

- [ ] **Step 7: Commit**

```bash
git add crates/hook/src/decide.rs crates/hook/src/config.rs crates/hook/tests/hooks.rs
git commit -m "Tell the agent up front to wrap long-running commands

Correcting it afterwards means the first server's output is already
lost; the rule is cheap to state once at the top of a session."
```

---

## Manual verification

After Task 9, confirm the loop end to end rather than trusting the unit tests alone:

1. `cargo install --path crates/hook --force` (or rebuild the app bundle via `scripts/sidecars.sh release`)
2. `logbear-hook --emit-config claude` and merge into `~/.claude/settings.json`
3. Start a real server: `logbear npm run dev` in a project
4. In an agent session, edit a file the server watches
5. Confirm the agent is told to check the logs, and that a **second** edit producing no new output says nothing — this is the two-cursor behaviour, and the regression most likely to slip through
