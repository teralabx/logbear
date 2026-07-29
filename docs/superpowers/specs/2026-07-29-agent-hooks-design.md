# Agent hooks: closing the change → verify loop

**Date:** 2026-07-29
**Status:** approved, not yet implemented

## Problem

The hook surfaces errors well but only reactively, and only once they have been
classified as errors. Two gaps follow from that.

**The agent does not know its change worked.** Today the only signal is an error the
daemon happened to recognise. A change that broke behaviour without producing a
recognisable error, or that crashed the reloader outright, reads to the agent as
success. Absence of a detected error is not evidence the change landed — reading the
log is.

**The agent does not know to use logbear at all.** Nothing tells it to start a dev
server under `logbear`, so the server it spawns is uncaptured and its output is lost
to everyone, including the hook. Nothing tells it that logbear already holds the
output it is about to go grep out of a file.

## Principle: silence by default

The hook speaks when it has something the agent cannot already see, and is otherwise
silent. Every rule below is gated so that the common case emits nothing. A hook that
chatters gets ignored, which costs the errors it exists to surface.

## Behaviour

Dispatch is on the tool that fired `PostToolUse`. Changes take the verify path; `Bash`
takes the intent path.

| Event | Condition | Response |
|---|---|---|
| `SessionStart` | always | Standing rule: start long-lived processes under `logbear` |
| `Edit`/`Write`/`MultiEdit`/`NotebookEdit` | server live + new errors | Full error detail; blocks if the signature is new |
| same | server live + non-error output unseen | "Server logged N lines since your change — check `logbear logs`" |
| same | server live, nothing unseen | silent |
| same | no server running | silent |
| `Bash` | `StartsServer`, not already wrapped | "Running uncaptured — restart as `logbear <cmd>`" |
| `Bash` | `ChasesBug` and project has history | "logbear already has this — use `logbear logs`" |
| `Bash` | otherwise | silent |
| `Stop` | unread errors | unchanged from today |

Error responses take precedence over nudges. The agent never receives two messages
from one event.

## Intent classification

New module `crates/hook/src/intent.rs`, a pure function of the command string:

```rust
pub enum Intent {
    StartsServer { suggested: String },  // the logbear-wrapped form to advise
    ChasesBug,                           // reaching for output logbear already holds
    Neither,
}

pub fn classify(command: &str) -> Intent
```

Pure and I/O-free, matching how `decide.rs` is kept testable without a socket. The
pattern table lives here so there is one file to read and extend.

The distinction is **process lifetime**, not a fixed command list:

- `StartsServer` — runs until killed, so its output would otherwise be lost:
  `npm|pnpm|yarn|bun run dev|start|serve|watch`, `vite`, `next dev`, `astro dev`,
  `rails s`, `mix phx.server`, `python manage.py runserver`, `cargo watch`,
  `docker compose up` without `-d`. Skipped when already prefixed with `logbear`.
- `ChasesBug` — reaching for output logbear already has: `tail`/`cat`/`grep` against a
  `.log` file, `journalctl`, `docker logs`.
- `Neither` — everything else, including `cargo test`, `npm run build`, `tsc`, git.
  These terminate and hand their output straight back to the agent, so logbear adds
  nothing. This is the overwhelmingly common case and is silent.

False positives cost a few tokens of misdirected advice. Nothing is ever blocked on
the strength of a classification, so the failure mode stays cheap.

## The two-cursor problem

`decide.rs` documents today that `PostToolUse` must never acknowledge, because
advancing the error cursor "would rob the Stop hook of the errors it exists to
surface". The verify nudge needs the opposite: if nothing advances, every later edit
re-reports the same lines and the nudge fires forever.

These requirements are irreconcilable on one watermark, so the session carries two:

| Cursor | Advanced by | Governs |
|---|---|---|
| `last_entry_id` (existing) | any block, Stop or `PostToolUse` | Which errors are "new" |
| `nudge_entry_id` (new) | each verify nudge | Which lines are "unseen" |

The rule the existing comment states — never acknowledge on `PostToolUse` — applies to
advisory context, which is what `PostToolUse` could emit until now. A block is
different: it has put the errors in front of the agent, so it must advance the error
cursor exactly as a Stop block does, or it re-fires on them forever.

Conflating them breaks the Stop gate. This is the highest-risk part of the change and
should be implemented and tested before anything else depends on it.

## Protocol and schema

`UnreadState` gains two fields, both computed on the round trip the hook already makes:

- `server_running: bool` — whether the project has an active run
- `unseen_lines: i64` — entries of any level after `nudge_entry_id`

`session_cursors` gains two columns:

- `nudge_entry_id INTEGER NOT NULL DEFAULT 0`
- `post_blocks_issued INTEGER NOT NULL DEFAULT 0`

Both are additive with defaults, so they go in via `ALTER TABLE ADD COLUMN` guarded on
existing columns rather than a `SCHEMA_VERSION` bump. `Ack` gains a cursor-kind
parameter so the hook can advance the nudge cursor without touching the error cursor.

## Block budgets stay separate

`PostToolUse` blocks spend `post_blocks_issued` against a small allowance (default 2),
not the existing `block_budget` that funds Stop blocks.

Sharing one budget lets a mid-turn burst of new signatures exhaust it and leave the
Stop hook unable to block when the agent tries to finish. The Stop gate is the more
important of the two and must not be starved by the advisory one.

Every `PostToolUse` block must advance the error cursor for the entries it reports, for
the same reason the Stop path does: a block whose watermark did not move re-fires on
the same errors forever. A failed ack degrades the block to advisory context, matching
the existing `respond()` behaviour.

## Harness dialects

Verified against Codex hook documentation on 2026-07-29:

- Codex supports `SessionStart`, with `additionalContext`. The standing rule works on
  both harnesses.
- Codex `PostToolUse` supports both `decision: "block"` and `additionalContext`, which
  the new escalation and nudges need.

The existing Codex `Stop` workaround — `additionalProperties: false`, no
`hookSpecificOutput`, degrade to `systemMessage` — is untouched. `Stop` behaviour does
not change.

## Failure behaviour

Unchanged: every path fails open. Daemon down, malformed payload, timeout or
unreadable field all exit 0 with an allow response. A hook that cannot reach the daemon
must not become an agent that cannot work.

## Testing

`crates/hook/tests/hooks.rs` already covers the decision rules against a fake `Daemon`,
which is the pattern to extend.

- `intent.rs` unit tests: each `StartsServer` pattern, `logbear`-prefixed forms
  classified `Neither`, `cargo test`/`npm run build` classified `Neither`
- Verify nudge fires only when a server is live and lines are unseen; silent otherwise
- The nudge cursor advances on nudge; the error cursor does not
- A second edit with no new output is silent — the regression the two-cursor split exists to prevent
- `PostToolUse` block exhausts `post_blocks_issued` and degrades to context, leaving
  `blocks_remaining` untouched for Stop
- Both harness dialects render each new decision correctly

## Out of scope

- `PreToolUse` interception. Advise-only was chosen deliberately: a denied command
  costs a round trip on every false positive, and the nudge recovers the same ground
  by telling the agent to restart the server under `logbear`.
- Per-project configurable intent patterns. The built-in table ships first; config
  follows only if the table proves insufficient in practice.
