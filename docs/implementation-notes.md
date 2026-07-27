# Implementation notes

Where the build diverged from `superpowers/specs/2026-07-26-runlog-design.md`, and why.
The spec is the design; this is what happened when it met a compiler.

---

## 1. Entries reference **line ranges**, not chunk ranges

The spec has `entries.start_chunk` / `end_chunk`. That is too coarse to be correct: an
entry boundary almost never coincides with a socket read boundary, so rendering a chunk
range would return the neighbouring entries' text as well.

Chunks are now **line-aligned batches** — a chunk holds whole lines and records
`first_line` and `n_lines` — and entries reference `start_line` / `end_line`. Concatenating
a run's chunks in `seq` order still reproduces the stream, and truth is still append-only,
so the two-layer property the spec cared about is intact. What changed is that the index
now points at something that survives re-batching, which is what makes reparse safe in
practice rather than only in principle.

## 2. Hand-rolled MCP instead of `rmcp`

The spec named `rmcp` 0.16. That version no longer exists; the crate is at `3.0.0-beta`
with a moving API. The surface we need — `initialize`, `tools/list`, `tools/call`, `roots`,
plus JSON-RPC framing — is a few hundred lines of well-understood protocol, and owning it
removes a churning pre-release dependency from the path between an agent and its errors.

## 3. `drain-rs` deferred; normalised-hash fingerprinting shipped alone

The spec flagged this ("confirm maintenance status before committing"). Normalised hashing
already produces a **readable template**, which means there is no second human-summary
string to keep in sync with the identity — the template *is* the summary. Drain would be an
upgrade to clustering quality, not a dependency of the core loop, and it can be added later
without touching the schema. It is not in this build.

Per-format identifying-line rules **are** implemented, because the prototype's Python
collision (`RuntimeError` and `ValueError` sharing a fingerprint because both tracebacks
open with the same line) was a correctness bug, not a quality one.

## 4. Vectors and `fastembed` are not in this build

Spec §7.4 marks them v1.1 and they stayed there. `runlog_embed()` is therefore absent from
the SQL function list; FTS5 covers text search today. Nothing in the schema blocks adding
`fingerprint_vec` later — it is a separate virtual table over `fingerprints`.

## 5. FTS5 needs `contentless_delete=1`

A contentless FTS5 table refuses `DELETE`, which retention and reparse both require. Found
by the eviction tests, which is the correct way to find it. Without the flag, evicting a
run would leave search hits whose text can no longer be produced.

## 6. SQL functions are `INNOCUOUS`, not `DIRECTONLY`

`SQLITE_DIRECTONLY` forbids use inside a view, and `runlog_render()` exists specifically to
be used inside `v_entries`. Under SQLite's default `trusted_schema = OFF`, a non-innocuous
function in a view is rejected outright. These functions read the same database file and
nothing else, so `SQLITE_INNOCUOUS | SQLITE_DETERMINISTIC` is the accurate classification.

## 7. Notifications: the daemon decides, the app usually posts

The spec has the daemon deciding so that policy works with the app closed. It still does.
But posting is split: when the app is running it posts through
`tauri-plugin-notification`, because that path has an application identity and an icon; when
nothing is subscribed the daemon falls back to `terminal-notifier` / `osascript` /
`notify-send`. Linking a notification framework into a headless daemon would buy a
bundle-identity requirement on macOS and a dbus connection on Linux in exchange for a banner
the user normally sees from the app anyway.

Storm detection was also reordered: the per-fingerprint cooldown is checked **before** the
rate calculation. A hundred repeats of one signature is one banner with a rising count, not
a storm — storm mode is for when the *variety* of distinct failures is the problem. The
original ordering made a single looping error trip storm mode, which is louder, not quieter.

## 8. Diff reports error and warn signatures only

Comparing every signature meant a run that logged a new info line showed up as "new". The
screen answers "what broke", so info, debug and trace deltas are excluded. Caught by a test
whose expectation was right and whose implementation was not.

## 9. Signal precedence in run-level failure detection

`SIGINT` is how you stop a dev server, and it exits 130. Checking the exit code before the
signal reported every deliberate Ctrl-C as a crash. The signal is now checked first.

## 10. Codex `Stop` cannot deliver advisory context — only a block reaches the model

Spec §10.1 treated the two harnesses as symmetric: "one adapter with two config writers".
Verified against Codex's generated schemas and source, that is true for `PostToolUse` and
**false for `Stop`**.

Codex's `Stop` output schema is `additionalProperties: false` and permits only `continue`,
`decision`, `reason`, `stopReason`, `suppressOutput`, `systemMessage`. There is no
`hookSpecificOutput`, so there is no `additionalContext`. And `systemMessage` reaches the
human UI but **never the model** — it lands in the UI entries list as a warning, on both
events.

The consequence for the block budget: on Claude Code, exhausting the budget degrades to
advisory context the agent still reads. On Codex, exhausting it degrades to *nothing the
agent can see*, because the only channel to the model at `Stop` is a block reason. This is
a real capability difference and it is documented rather than papered over.

Two other Codex details that changed the implementation:

- Non-empty stdout that fails to parse is a **hard hook failure** on Codex `Stop` (unlike
  `PostToolUse`, which tolerates it). Every byte the hook writes to stdout is serialiser
  output; every diagnostic goes to stderr.
- A Codex matcher made only of `[A-Za-z0-9_|]` is **exact alternation, not a regex**, and
  an invalid regex silently matches nothing. `--emit-config codex` therefore omits the
  matcher rather than guessing tool names into a hook that would never fire.
- Codex refuses to run non-managed command hooks until they are trusted via `/hooks`, and
  the failure mode is silence. `--emit-config` and `runlog doctor` both say so.

## 11. Severity is read, never inferred

The spec described error detection as a union of independent signals, biased to over-catch,
on the reasoning that a false positive is cheaper than a miss. Measured against a real
Phoenix server, that bias produced **25 false errors out of 27**. Every false one was a
`[debug]` request log whose indented `Parameters:` / `Pipelines:` lines were counted as
stack frames. The two real errors were the only two the process had *declared*.

Over-catching did not trade precision for recall. It drowned the recall it bought, and the
notifications built on top of it were worthless.

Severity now comes from one ladder, stopping at the first rung that answers:

| rung | source |
|---|---|
| `declared` | a severity token — JSON field, logfmt field, or a keyword in the line's prefix region |
| `structural` | no token, but a runtime printed an exception banner anchored at column zero |
| `unknown` | the producer said nothing |

A declared level wins outright: `[debug]` stays debug whatever follows it. `unknown` is a
value, not a missing one, and is never an error and never notified on.

This matches where the industry landed. OpenTelemetry's log data model makes
`SEVERITY_NUMBER_UNSPECIFIED` first-class and its spec community
[rejected defaulting it to INFO](https://github.com/open-telemetry/opentelemetry-specification/issues/4478).
Grafana Loki, which ingests stdout from every language there is, detects levels with a
nine-word keyword scan and returns `unknown` when none match — no stack-trace rule, no
exception-shape rule, no indentation heuristic anywhere in the path. Sentry and AppSignal
get exact errors by running *inside* the process, hooking each language's own uncaught
exception mechanism; that is ruled out here by the opacity constraint.

Two deliberate deviations from Loki's detector:

- **A prefix window.** A level token is a prefix field, so the scan stops after 80 bytes.
- **Declaration, not just word boundary.** Loki's open issue #18060 is `productinformation­service` reading as info; word boundaries fix that but not `loaded 12 error handlers`, which is bounded on both sides and still prose. A match counts only if it is at position zero, opened by `[ ( { =`, closed by `:`, or capitalised. Quotes are boundaries but not declarations, which keeps `%{status: "error"}` — an `IO.inspect` of a map — out of the error list.

Output that declares nothing is still captured, indexed and searchable. It surfaces as
**novelty** instead: a signature's first appearance in a project is a fact, which is what
makes freshly added `print` / `IO.inspect` debugging findable when it has no level to
filter on. `v_novel`, `EntryFilter::novel_only` and `runlog logs --new` expose it.
Novelty never raises a notification on its own.

Drain-style template mining (see §3) would improve the *identity* half of this and remains
deferred; it is not a substitute for the severity ladder.

## 12. Windows is not built or tested

Spec §18 left this open. `portable-pty` supports ConPTY and the code compiles for it in
principle, but the socket layer is `std::os::unix`, the service installer writes launchd
plists and systemd units, and none of it has been run on Windows. macOS and Linux only.

---

## Bugs the tests caught that review did not

Worth recording, because each one names a class of mistake rather than a typo.

- **`EntryFilter::default()` gave `limit: 0`.** Derived `Default` on a struct whose serde
  default is 100. Every call site using `..Default::default()` silently returned one row.
  The lesson: a derived `Default` on a field with a meaningful non-zero default is a
  silent truncation waiting to happen.
- **Redaction ran after classification.** Secrets were masked in the stored bytes but had
  already been read into the entry text and the fingerprint template — so the mask was
  cosmetic on the paths that actually leave the machine. Now the parser masks each line the
  instant it is split, before anything derived exists.
- **A traceback opener started no entry in `Prefix` mode.** `Traceback (most recent call
  last):` matches no line-start rule, so it merged into whatever info line preceded it.
  Block openers now start an entry in every mode.
- **`\bERR!\b` never matched.** `!` is not a word character, so the trailing boundary
  required one after it. npm's errors were invisible.
- **Daemon tests raced on `RUNLOG_HOME`.** Process-global state set by three parallel
  tests; they passed or failed on scheduling. Serialised on a lock.
