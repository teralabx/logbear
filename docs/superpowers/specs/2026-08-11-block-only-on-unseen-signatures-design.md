# Block only on unseen error signatures

**Date:** 2026-08-11
**Status:** Approved

## Problem

The hook re-blocks the agent on errors it has already dealt with, which slows the
agent down without teaching it anything. Two mechanisms cause this:

1. **Split-brain session identity.** Unread state is per session cursor. The hook
   uses the harness's `session_id`; the MCP server invents its own random session id
   per process (`derive_session_id()` in `crates/mcp/src/server.rs`). Reading errors
   through `logbear_query`/`logbear_ack` advances only the MCP session's cursor, so
   the Stop hook still sees those errors as unread and blocks on them.
2. **The Stop gate ignores signatures.** `decide.rs` blocks whenever
   `new_errors > 0` and budget remains, even when every unread error is a recurring
   fingerprint the session was already shown (e.g. a restarted server re-emitting
   the same errors as new entries).

## Decision summary

- **Seen scope:** per session. A signature surfaced to this session (by a block or
  a verified read) never blocks this session again; fresh sessions start clean.
- **Seen trigger:** verified reads. A signature counts as seen when a hook block
  showed it, or when a daemon-served read demonstrably returned that error entry
  or signature.
- **Degraded stop:** when unread errors exist but all signatures were already
  seen, emit a non-blocking advisory line, never silence.

## Design

### 1. Policy change (`crates/hook/src/decide.rs`)

New rule in the Stop/SubagentStop arm, between the `stop_hook_active` guard and
the budget check:

- `unread.is_empty()` → Allow (unchanged)
- `stop_hook_active` → advisory Context (unchanged)
- **new:** `unread.new_signatures == 0` → advisory Context with recurring-specific
  wording ("N unread errors, but every signature is one you've already seen this
  session"). Never blocks, spends no budget.
- otherwise → Block / degrade as today.

`block_reason` orders new-signature entries before recurring ones so the entries
that justify the block are never the ones truncated by `MAX_ENTRIES`. The
PostToolUse change-block already requires `new_signatures > 0` and is unchanged.

### 2. Seen-set: widen `session_blocks`, add read receipts

`session_blocks (session_id, fingerprint_id, at)` already backs the
`new_signatures` exclusion. Its meaning widens from "signatures blocked on" to
"signatures this session has seen — via a block or a verified read". No schema
change; receipts reuse the `record_block`-style `INSERT OR IGNORE`.

**Read receipts** (daemon, `crates/daemon/src/conn.rs`): after serving a read,
record the fingerprints of error entries/signatures the response actually
contained:

- `Entries` (CLI `logbear logs`) and `Wait`: responses are `EntryView`s — take
  fingerprints of error-level entries directly.
- `Diff`: mark the fingerprints in the returned signature lists.
- `Fingerprints`: the response is signatures — mark the error ones.
- `Query` (arbitrary sandboxed SQL): sniff result columns. `id`/`entry_id`
  resolve to fingerprints via the entries table; `fingerprint_id` is direct;
  `template` resolves by exact template text. Results with none of these columns
  (aggregates, info-only reads) mark nothing.

Receipts are best-effort: failures are logged to stderr and never fail the read.

### 3. Attribution: recently-active harness sessions

Reads carry no harness session id, so a receipt applies to every hook session
cursor in the project DB (`harness != "mcp"`) whose `last_seen_at` is within 30
minutes. `Request::Unread` starts refreshing `last_seen_at`, making it a live
activity signal (the hook calls unread on every event). Trade-off, accepted: a
concurrent second session on the same project inherits the reads and misses a
block; the errors still reach it through advisory summaries and MCP banners.

### 4. Edge cases

- **Null fingerprints:** today's `new_signatures` SQL treats a NULL
  `fingerprint_id` as not-new (`NULL NOT IN (…)` is falsy), which under the new
  gate would mean it never blocks. Change the query to count NULL-fingerprint
  errors as new: they block once per occurrence batch (the cursor advance covers
  the batch), and a later recurrence blocks again — the safe direction for
  something that cannot be identified.
- **Reads of pre-cursor history** may mark old fingerprints seen — harmless,
  those entries were already behind the cursor.
- **Fresh sessions** are unaffected: cursors and seen-sets are per session, so a
  new conversation is still blocked once on standing errors.

### 5. Testing

- `decide.rs` unit tests: recurring-only unread yields Context, not Block;
  new-signature entries sort first in the block reason.
- Store tests (`crates/core/src/store/read.rs`): receipts insert into
  `session_blocks`; NULL-fingerprint errors count as new; unread refreshes
  `last_seen_at`.
- Protocol-level tests: an `Entries`/`Wait` read containing an error followed by
  a Stop for an active hook session yields advisory, not block; a query whose
  result has no entry/fingerprint/template column still blocks.

## Out of scope

- No new settings knob for the activity window (constant, 30 minutes).
- No MCP protocol changes and no attempt to pass the harness session id into the
  MCP server.
- No change to the verify nudge, budgets, or banner wording.
