# runlog

**It worked two days ago. What changed?**

runlog wraps a development process, records everything it emits, and tells your coding
agent the moment something breaks.

```
runlog mix phx.server
```

That is the whole interface. Your terminal behaves exactly as it did before — colours,
`IEx.pry`, Ctrl-C, resize — and everything the process prints is now queryable, diffable,
and visible to an agent that would otherwise have no idea it happened.

---

## The problem it solves

You are working with a coding agent. It edits code. Your dev server, running in another
terminal, throws an exception. The agent has no idea, and reports "done, fixed it". You
have to notice the error, copy it, paste it back, and restart the conversation.

Nothing else closes that loop:

| | Sees | Doesn't see |
|---|---|---|
| Framework log viewers | logs, when asked | never *tells* the agent something broke |
| Claude Code's background bash | processes it started itself | anything else, and no history |
| Sentry / Datadog | production exceptions | your local dev loop |
| Loki / Splunk | log streams at fleet scale | run boundaries, git context |
| Atuin | command, exit code, duration | **the output** |
| pm2 / overmind | supervises and tails | stores nothing queryable |

runlog sits at the point of invocation, which is the only place where the command and the
context that produced it are visible together, for free, at the same instant.

---

## What you get

**A blocking hook.** When your agent says it is finished, runlog checks whether anything
broke since it last looked. If so, the agent is interrupted and handed the actual
exception, its history, and which run it came from. If not, nothing happens at all.

**A queryable history.** Every run is stamped with its git SHA, branch, a hash of the dirty
working tree, and the hash of every lockfile. `runlog diff` answers "what changed" with
the commits in between, the lockfiles that moved, and the error signatures that appeared.

**A read-only SQL endpoint for agents**, rather than a fixed menu of tools. An agent can
ask "which error signatures appear in every failed run but no successful one" — a question
no canned tool would have anticipated.

**A desktop app** for the human: live tail, run history, error signatures with sparklines,
and the diff view.

---

## Install

Download the app and open it. On first launch it offers, with confirmation for each step:

1. `runlog` on your PATH
2. `runlogd` as a background service that survives reboots
3. the MCP server registered with Claude Code and Codex
4. the hook installed in both

Every config file is backed up before it is touched, every write **merges** rather than
overwrites, and each step can be reverted individually. Nothing is done without asking.

`runlog doctor` reports the state of all four and tells you how to fix whatever is broken.

---

## Using it

```sh
runlog mix phx.server          # or npm run dev, cargo watch, pytest, anything
runlog runs                    # history for this project
runlog logs --errors -n 20     # recent errors, whole entries
runlog logs --new              # output never seen in this project before
runlog diff                    # last known good vs now
runlog cat 42                  # replay a run byte for byte, ANSI intact
runlog query "SELECT template, count(*) FROM v_errors GROUP BY 1"
runlog status                  # daemon health, project, retention
runlog doctor                  # check every integration
```

`runlog diff` with no arguments compares the **last known good** run — the most recent one
that exited cleanly *and* logged no errors — against the latest. That is what people mean
by "it worked yesterday", and it is almost never the previous run.

### What counts as an error

Severity is read from the output, never guessed at from its shape. An entry is an error
only when the process **declared** one — `[error]`, `level=error`, `{"level":"error"}` —
or when a runtime printed an exception banner it only prints when something threw:
`** (`, `Traceback (most recent call last):`, `panic:`. Anything else is `unknown`, which
is an answer rather than a missing value. `unknown` is never an error and never raises a
notification, however alarming the text reads.

That leaves a gap, and `--new` fills it. A bare `print` / `IO.inspect` / `console.log` has
no severity by construction — no language attaches one. But a line whose signature has
never appeared in this project before is a *fact*, so debug output you added five minutes
ago is findable without anything having to guess whether it looked bad.

---

## How it works

```
    your terminal
         │
         ▼
   ┌───────────┐   PTY    ┌────────────────┐
   │  runlog   │◄────────►│ mix phx.server │
   │  (shim)   │          └────────────────┘
   └─────┬─────┘
         │ unix socket, fire and forget
         ▼
   ┌───────────────────────────────────────┐
   │            runlogd (daemon)           │
   │  parse · group · fingerprint · store  │
   │  retention · fanout · notifications   │
   └───┬───────────────┬───────────────┬───┘
       ▼               ▼               ▼
  ┌─────────┐    ┌───────────┐   ┌──────────┐
  │  Tauri  │    │ runlog-mcp│   │  runlog- │
  │   app   │    │  (stdio)  │   │   hook   │
  └─────────┘    └───────────┘   └──────────┘
```

### Four constraints everything defers to

**Total opacity.** runlog never touches the wrapped process. No injected logger handlers,
no `ERL_FLAGS` / `NODE_OPTIONS` / `RUBYOPT` / `PYTHONPATH`, no `FORCE_COLOR`, no dependency
added to your app. We observe the output stream and nothing else. During design we verified
that a sidecar Elixir logger handler gives perfect structured entries while leaving the
console untouched — and rejected it anyway, on this principle.

**The shim can never degrade your process.** Bytes reach your terminal *before* they reach
us. If the daemon is down, wedged, or upgrading, the wrapped process is unaffected: the
shim spools to disk and carries on. No blocking, no added latency, no altered exit code, no
swallowed signal.

**Raw bytes are truth.** Grouping, levels, fingerprints, rendered text — all of it is a
regenerable projection over the captured bytes. A parser bug is a rebuild, never data loss.

**No bad UX.** Colours work. `IEx.pry` works. Ctrl-C works. Resize works. Progress bars
don't explode into ten thousand rows. Install does not clobber your config.

### The capture pipeline

```
pty bytes → vt emulation → physical lines → entry grouping → classification → fingerprint
            deterministic   deterministic     inferred         union of signals   normalised
```

Exactly one stage is inferential, and it records that it is. The run stores which
delimiter mode was detected, and every entry carries `boundaries: exact | inferred`, so an
agent knows how far to trust the grouping instead of assuming.

Grouping is tried in order of certainty: one JSON object per line, then per-entry colour
bracketing (Elixir's Logger does this), then line-start timestamp and level prefixes, and
only then indentation with per-runtime continuation markers. When nothing matches, the
default is to **start a new entry** — over-splitting is cosmetic, over-merging destroys the
boundary that fingerprinting depends on.

Errors are detected by a union of independent signals — level tags, exception markers,
runs of stack frames, non-zero exit, unexpected death — so detection does not depend on
grouping being right. It is deliberately biased to over-catch.

### Storage

One SQLite database per project. Two layers:

```sql
chunks   -- truth: append-only, never rewritten, the captured bytes
entries  -- derived index: a line range, a level, a fingerprint. NO text.
```

Text lives once. Rendered text is produced on demand by `runlog_render()` over a line
range. That is what makes `DELETE FROM entries` plus a rebuild always safe — if the row
*were* the logical entry, regenerating would split and merge rows, and every reference to
it would break.

Retention degrades gracefully rather than vanishing. The bytes of the oldest runs go first,
then their derived index; the run rows, their git stamps and their error signatures are
kept effectively forever at a few KB each. So "it worked three weeks ago, what changed?"
still answers correctly long after the raw log is gone.

### The MCP surface

Five tools, not fifteen: `runlog_schema`, `runlog_query`, `runlog_wait`, `runlog_diff`,
`runlog_ack`. Every tool's JSON schema sits in the agent's context in every session, so a
large fixed menu is a permanent tax that still cannot answer the interesting questions.
Instead the agent gets the schema and a read-only SQL endpoint.

Every response is prefixed with the caller's unread state:

```
⚠️ 6 new errors since your last check (2 new signatures). Oldest 40s ago.
```

That costs nothing per-harness and works in any MCP client, which is why Cursor, Zed and
anything else get most of the value without a dedicated integration.

Agent SQL runs behind five independent layers: a read-only connection with
`PRAGMA query_only`, an authorizer allowlist that denies `ATTACH`, `PRAGMA` and extension
loading, a five-second interrupt timer, row and byte caps that report truncation, and one
project database per connection so a query cannot reach anything else on disk.

### Loop safety

A `Stop` hook that blocks unconditionally traps an agent forever. Three guards:

1. a fingerprint blocks a given session **at most once**, however often it recurs;
2. blocks per session are capped, after which errors degrade to advisory context;
3. the cursor advances **when the block is issued**, so the same errors cannot block twice
   even if the agent ignores them entirely.

If the daemon is unreachable the hook exits cleanly and allows. A hook that breaks your
agent because our daemon is down is far worse than one that misses a notification.

### Privacy

- **Environment variables: names only, never values.** No flag, no exception.
- **Redaction on the way in, not on the way out.** Token shapes, connection strings with
  credentials, and PEM blocks are masked before anything reaches disk — and before the
  entry text or the fingerprint template is computed. Masking is length-preserving so
  replay stays aligned.
- **Interactive programs are not captured.** `psql`, `ssh`, `vim` and friends are sessions
  rather than runs; they are denylisted by default and run untouched.
- **Nothing leaves your machine.** There is no network code in this repository.

---

## Repository layout

```
crates/core      capture, parsing, storage, query, diff, notification policy
crates/shim      the `runlog` binary: PTY wrapper and CLI
crates/daemon    `runlogd`
crates/mcp       `runlog-mcp`, the stdio MCP server
crates/hook      `runlog-hook`, one binary for both harnesses
apps/desktop     Tauri v2 + SvelteKit
brand/           the Delta mark and the Signal palette
docs/            the design spec and product docs
```

`runlog-core` has zero coupling to the CLI, MCP or UI. Everything else is a thin adapter
over it.

## Building

```sh
cargo test --workspace          # Rust
cd apps/desktop && bun install && bun run check && bun run build
./scripts/sidecars.sh           # stage the binaries as Tauri sidecars
cd apps/desktop && bun run tauri build
```

`RUNLOG_HOME` redirects all state, which is how the test suite avoids touching your real
history.

## Licence

MIT.
