<p align="center">
  <img src="docs/assets/logbear.svg" alt="The LogBear mascot" width="96" height="96">
</p>

# LogBear

[![CI](https://github.com/teralabx/logbear/actions/workflows/ci.yml/badge.svg)](https://github.com/teralabx/logbear/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/logbear.svg)](https://crates.io/crates/logbear)
[![Licence: MIT](https://img.shields.io/badge/licence-MIT-blue.svg)](LICENSE.md)
[![Platform: macOS](https://img.shields.io/badge/platform-macOS-lightgrey.svg)](#platform-support)

**Your coding agent can't see your dev server crash.**

LogBear wraps a development process, records everything it emits, and tells your coding
agent the moment something breaks.

```
logbear mix phx.server
```

That is the whole interface. Your terminal behaves as it did before — colours,
`IEx.pry`, Ctrl-C, resize — and everything the process prints is now queryable, diffable,
and visible to an agent that would otherwise have no idea it happened.

## Contents

- [The problem it solves](#the-problem-it-solves)
- [What you get](#what-you-get)
- [Install](#install) · [Uninstall](#uninstall) · [Platform support](#platform-support)
- [Using it](#using-it)
- [How it works](#how-it-works) — [constraints](#four-constraints-everything-defers-to),
  [capture](#the-capture-pipeline), [storage](#storage), [MCP](#the-mcp-surface),
  [loop safety](#loop-safety), [privacy](#privacy)
- [Repository layout](#repository-layout)
- [Building from source](#building-from-source)
- [Contributing](#contributing) · [Security](#security) · [Licence](#licence)

---

## The problem it solves

You are working with a coding agent. It edits code. Your dev server, running in another
terminal, throws an exception. The agent has no idea, and reports "done, fixed it". You
have to notice the error, copy it, paste it back, and restart the conversation.

We couldn't find anything else that closes that loop:

| | Sees | Doesn't see |
|---|---|---|
| Framework log viewers | logs, when asked | never *tells* the agent something broke |
| Claude Code's background bash | processes it started itself | anything else, and no history |
| Sentry / Datadog | production exceptions | your local dev loop |
| Loki / Splunk | log streams at fleet scale | run boundaries, git context |
| Atuin | command, exit code, duration | **the output** |
| pm2 / overmind | supervises and tails | stores nothing queryable |

LogBear sits at the point of invocation, which is the only place where the command and the
context that produced it are visible together, for free, at the same instant.

---

## What you get

**A blocking hook.** When your agent says it is finished, LogBear checks whether anything
broke since it last looked. If so, the agent is interrupted and handed the actual
exception, its history, and which run it came from. If not, nothing happens at all.

**A queryable history.** Every run is stamped with its git SHA, branch, a hash of the
uncommitted changes, and the hash of every lockfile. `logbear diff` answers "what changed" with
the commits in between, the lockfiles that moved, and the error signatures that appeared.

**A read-only SQL endpoint for agents**, rather than a fixed menu of tools. An agent can
ask "which error signatures appear in every failed run but no successful one" — a question
no canned tool would have anticipated.

**A desktop app** for the human: live tail, run history, error signatures with sparklines,
and the diff view.

---

## Install

**Homebrew** — CLI, daemon, MCP server and hook:

```sh
brew install teralabx/tap/logbear
brew services start logbear
logbear setup             # connect your coding agents
```

It installs prebuilt binaries, so there is nothing to compile and no Rust to install. It
lives in its own tap until it qualifies for `homebrew-core`; Homebrew may ask you to
`brew trust` the tap the first time. See [`packaging/homebrew`](packaging/homebrew).

**Install script** — the same four binaries, prebuilt, into `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/teralabx/logbear/master/install.sh | sh
logbear setup             # connect your coding agents and start the daemon
```

The script checks each download against the SHA-256 published with the release and changes
nothing else on your machine. Read it first if you like: it is [`install.sh`](install.sh).

**Cargo** — the same four binaries, built from source:

```sh
cargo install logbear logbeard logbear-mcp logbear-hook
logbear setup
```

`logbear setup` finds Claude Code, Codex, pi and Hermes, shows exactly which files it would
change, and asks before touching anything.

**The desktop app** is the easiest path, because it is also the installer. On first launch,
without asking, it does the two local steps it cannot work without:

1. links `logbear` onto your PATH. If no writable directory is already on your PATH, it
   links into `~/.local/bin` and adds a marked block to your shell profiles, backing each
   one up first
2. registers `logbeard` as a background service that survives reboots, which on macOS
   means a LaunchAgent

Your agents are left alone until you ask. Settings → Install has a button for each of the
rest, or one for all of them:

3. the MCP server registered with each agent it finds: Claude Code, Codex, pi and Hermes
4. the hook installed in each of them

pi has no command hooks, so for pi the "hook" is a small extension written to
`~/.pi/agent/extensions/logbear.ts`. Hermes gets two shell hooks in `~/.hermes/config.yaml`,
added between marker comments so removing them leaves the rest of the file alone.

Every config file is backed up before it is touched, every write **merges** rather than
overwrites, and each step can be reverted individually. A config that won't parse is
refused with an error and left exactly as it was.

`logbear doctor` reports the state of every integration and tells you how to fix whatever is
broken. It checks pi and Hermes only where they are installed.

### Uninstall

`logbear setup --remove` takes LogBear back out of each agent's config, backing each file
up first, unregisters the daemon service, and removes the PATH link and the shell profile
block. Only LogBear's own entries go: a hook of yours in the same group stays.

Your captured data stays in `~/.logbear`, and the command prints where it is. To delete it
as well, which stops the daemon and can't be undone:

```sh
logbear setup --remove --purge
```

The binaries belong to whatever installed them. Homebrew keeps its own link and service, so
finish with `brew services stop logbear` and `brew uninstall logbear`; otherwise delete the
four binaries from `~/.local/bin` or `~/.cargo/bin`.

### Platform support

macOS is what LogBear is developed and tested on, and the desktop installer is macOS-only
— it registers `logbeard` with launchd. The CLI, daemon, MCP server and hook are ordinary
Unix programs (PTYs, unix sockets, SQLite) and should build on Linux, but that is
unverified. Windows is not supported.

---

## Using it

```sh
logbear mix phx.server          # or npm run dev, cargo watch, pytest, anything
logbear runs                    # history for this project
logbear logs --errors -n 20     # recent errors, whole entries
logbear logs --new              # output never seen in this project before
logbear diff                    # last known good vs now
logbear cat 42                  # replay a run byte for byte, ANSI intact
logbear query "SELECT template, count(*) FROM v_errors GROUP BY 1"
logbear status                  # daemon health, project, retention
logbear doctor                  # check every integration
logbear setup                   # connect agents: MCP server and hook for each one found
logbear setup --remove          # take it back out; --purge also deletes captured data
logbear updates on              # opt in to a daily check for new releases (off by default)
logbear upgrade                 # install the latest release (Homebrew or install script)
```

`logbear diff` with no arguments compares the **last known good** run against the latest.
That is the most recent run that logged no errors and either exited 0 or was stopped by you
with Ctrl-C or SIGTERM, which is how a dev server normally ends. It is what people mean by
"it worked yesterday", and it is almost never the previous run.

The diff also shows uncommitted changes between the two runs, naming the files while your
working tree still matches the latest run, and commits made while that run was up. Only a
hash of uncommitted changes is stored; file names are read from git when you ask.

### A program with the same name as a command

`cat`, `logs`, `runs`, `diff`, `status`, `doctor`, `setup`, `updates`, `query`, `sql-schema`
and `squash` are LogBear's own commands, and they take priority over programs with those
names. Put `--` first to wrap the program instead:

```sh
logbear cat 42                  # LogBear's: replay run 42
logbear -- cat notes.txt        # the real cat, wrapped
```

### Pipes and redirects

When stdin, stdout or stderr isn't a terminal, LogBear uses a pipe for that stream instead
of the pty, so programs see what they'd see without LogBear.

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
   │  logbear   │◄────────►│ mix phx.server │
   │  (shim)   │          └────────────────┘
   └─────┬─────┘
         │ unix socket, fire and forget
         ▼
   ┌───────────────────────────────────────┐
   │            logbeard (daemon)           │
   │  parse · group · fingerprint · store  │
   │  retention · fanout · notifications   │
   └───┬───────────────┬───────────────┬───┘
       ▼               ▼               ▼
  ┌─────────┐    ┌───────────┐   ┌──────────┐
  │  Tauri  │    │ logbear-mcp│   │  logbear- │
  │   app   │    │  (stdio)  │   │   hook   │
  └─────────┘    └───────────┘   └──────────┘
```

### Four constraints everything defers to

**Total opacity.** LogBear never touches the wrapped process. No injected logger handlers,
no `ERL_FLAGS` / `NODE_OPTIONS` / `RUBYOPT` / `PYTHONPATH`, no `FORCE_COLOR`, no dependency
added to your app. We observe the output stream and nothing else. During design we verified
that a sidecar Elixir logger handler gives perfect structured entries while leaving the
console untouched — and rejected it anyway, on this principle.

**The shim stays out of your process's way.** Output goes to your terminal first, and
capture can't block the wrapped command. The daemon is contacted in the background after
the child starts. If it is down, wedged, or upgrading, the shim spools to disk and carries
on, and your process's exit code comes back unchanged.

**Raw bytes are truth.** Grouping, levels, fingerprints, rendered text — all of it is a
regenerable projection over the captured bytes. A parser bug is a rebuild, never data loss.

**No bad UX.** Colours work. `IEx.pry` works. Ctrl-C works. Resize works. Progress bars
don't explode into ten thousand rows. Install does not clobber your config.

### The capture pipeline

```
raw bytes → vt emulation → physical lines → entry grouping → classification → fingerprint
            deterministic   deterministic     inferred         read, not guessed  normalised
```

Exactly one stage is inferential, and it records that it is. The run stores which
delimiter mode was detected, and every entry carries `boundaries: exact | inferred`, so an
agent knows how far to trust the grouping instead of assuming.

Grouping is tried in order of certainty: one JSON object per line, then per-entry colour
bracketing (Elixir's Logger does this), then line-start timestamp and level prefixes, and
only then indentation with per-runtime continuation markers. When nothing matches, the
default is to **start a new entry** — over-splitting is cosmetic, over-merging destroys the
boundary that fingerprinting depends on.

### Storage

One SQLite database per project. Two layers:

```sql
chunks   -- truth: append-only, never rewritten, the captured bytes
entries  -- derived index: a line range, a level, a fingerprint. NO text.
```

Text lives once. Rendered text is produced on demand by `logbear_render()` over a line
range. That is what makes `DELETE FROM entries` plus a rebuild always safe — if the row
*were* the logical entry, regenerating would split and merge rows, and every reference to
it would break.

Retention degrades gracefully rather than vanishing. The bytes of the oldest runs go first,
then their derived index; the run rows, their git stamps and their signatures outlive
them at a few KB each. Signatures are held to a share of the cap, with errors the last to
go. So "it worked three weeks ago, what changed?" still answers correctly long after the
raw log is gone.

### The MCP surface

Five tools, not fifteen: `logbear_schema`, `logbear_query`, `logbear_wait`, `logbear_diff`,
`logbear_ack`. Every tool's JSON schema sits in the agent's context in every session, so a
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
- **Common secret shapes are masked before storage; this is best effort, not a
  guarantee.** Masking happens on the way in, before the entry text or the fingerprint
  template is computed, and is length-preserving so replay stays aligned. Whatever the
  patterns don't recognise is stored as printed. Spool files can hold raw output briefly,
  at 0600, until they are ingested.
- **Interactive programs are not captured.** `psql`, `ssh`, `vim` and friends are sessions
  rather than runs; they are denylisted by default and run untouched.
- **LogBear sends what you capture nowhere; your agent sees what you connect it to.** Hooks
  and the MCP server hand log text to your coding agent, and a hosted agent sends what it
  reads to its model provider. LogBear itself sends what you capture nowhere. The only
  network requests it can make are about its own releases. The update check is **off unless
  you turn it on** with `logbear updates on`: then, at most once a day, one GET to GitHub's
  releases API carrying a `User-Agent` with the installed version, and nothing else.
  `logbear upgrade` makes the same request when you run it, then downloads the release. That
  is the whole of [`crates/shim/src/updates.rs`](crates/shim/src/updates.rs).

---

## Repository layout

```
crates/core      capture, parsing, storage, query, diff, notification policy
crates/shim      the `logbear` binary: PTY wrapper and CLI
crates/daemon    `logbeard`
crates/mcp       `logbear-mcp`, the stdio MCP server
crates/hook      `logbear-hook`, one binary for every harness (and the pi extension)
crates/setup     the installer, shared by `logbear setup` and the desktop app
apps/desktop     Tauri v2 + SvelteKit
packaging/       the Homebrew formula template
docs/            implementation notes
```

`logbear-core` has zero coupling to the CLI, MCP or UI. Everything else is a thin adapter
over it.

## Building from source

Requires Rust 1.90+ and [Bun](https://bun.sh).

```sh
cargo test --workspace --exclude logbear-desktop   # Rust, without the Tauri app
cargo test -p logbear -- --ignored   # integration tests that spawn real processes
(cd apps/desktop && bun install && bun run check && bun run build)
./scripts/sidecars.sh           # stage the binaries as Tauri sidecars
(cd apps/desktop && bun run tauri build)
```

A plain `cargo test --workspace` fails on a fresh clone: `tauri-build` won't compile
`logbear-desktop` until the four sidecar binaries exist, and those are build outputs that
are never committed. Exclude the crate as above, or stage them first with
`./scripts/sidecars.sh debug`, after which `cargo test --workspace` works.

`LOGBEAR_HOME` redirects all state, which is how the test suite avoids touching your real
history.

## Contributing

Issues and pull requests are welcome — start with [CONTRIBUTING.md](CONTRIBUTING.md) for
the workflow and the two constraints that get PRs declined. Participation is governed by
our [Code of Conduct](CODE_OF_CONDUCT.md). Released versions are listed in
[CHANGELOG.md](CHANGELOG.md).

## Security

LogBear stores your development output on disk, so a parsing or redaction bug is a privacy
bug. Please report vulnerabilities privately rather than in a public issue — see
[SECURITY.md](SECURITY.md).

## Licence

**MIT** — the full text is in [`LICENSE.md`](LICENSE.md).

Use it, modify it, redistribute it, sell it, build on it, fork it. Everything you install
from this repository runs entirely on your machine, with no account, and makes no network
calls unless you opt in to update checks.

The **LogBear** name and logo are not covered by the licence grant. Fork the
code freely; ship it under your own name.
