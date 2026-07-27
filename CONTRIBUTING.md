# Contributing to runlog

Thanks for your interest. Issues and pull requests are both welcome.

## Two things that get a PR declined

**Total opacity is not negotiable.** runlog never touches the process it wraps. No injected
logger handlers, no `ERL_FLAGS` / `NODE_OPTIONS` / `RUBYOPT` / `PYTHONPATH`, no
`FORCE_COLOR`, no dependency added to the user's app. We observe the output stream and
nothing else. A change that violates this will be declined however well it works — a
sidecar Elixir logger handler was built, verified to work perfectly, and rejected on this
principle.

**The shim can never degrade the wrapped process.** Bytes reach the terminal before they
reach us. No blocking, no added latency, no altered exit code, no swallowed signal. If the
daemon is down the shim spools to disk and carries on.

## Getting set up

Requires Rust 1.82+ and [Bun](https://bun.sh).

```sh
git clone https://github.com/teralabx/runlog
cd runlog
cargo test --workspace
```

`RUNLOG_HOME` redirects every piece of state runlog writes. The test suite sets it, which
is how tests avoid touching your real history — use it when experimenting too:

```sh
RUNLOG_HOME=/tmp/runlog-scratch cargo run -p runlog -- npm run dev
```

## Before opening a pull request

```sh
cargo test --workspace
cargo test --workspace -- --ignored   # spawns real processes under a PTY; slower
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cd apps/desktop && bun run check
```

CI runs all of these.

## House style

**Comments** are standard rustdoc: `///` on items, `//` for the non-obvious inside a body.
Explain *why*, not *what* — if the code says it, the comment should not repeat it. No
product narration.

**Commit subjects** are imperative and sentence case, with no type prefix and no trailing
full stop: `Signal the query timer instead of polling it`, not `fix(db): polling`. The body
states the problem, the change, and the evidence it works. One concern per commit.

**Tests** are named as sentences describing the property under test —
`a_command_that_cannot_start_is_not_recorded_as_a_run` — and assert behaviour rather than
implementation. Timing assertions must be robust to a loaded machine: measure a floor
across repeated runs, never a single wall-clock sample.

## Where things live

| | |
|---|---|
| `crates/core` | capture, parsing, storage, query, diff, notification policy |
| `crates/shim` | the `runlog` binary: PTY wrapper and CLI |
| `crates/daemon` | `runlogd` |
| `crates/mcp` | `runlog-mcp`, the stdio MCP server |
| `crates/hook` | `runlog-hook`, one binary for both harnesses |
| `apps/desktop` | Tauri v2 + SvelteKit |

`runlog-core` has zero coupling to the CLI, MCP or UI. New logic belongs there; everything
else should stay a thin adapter over it.

## Licensing of contributions

By submitting a pull request you agree that your contribution is licensed under the terms
in [LICENSE.md](LICENSE.md) (FSL-1.1-MIT, converting to MIT two years after each release).

## Code of Conduct

Participation is governed by the [Code of Conduct](CODE_OF_CONDUCT.md).
