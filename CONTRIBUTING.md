# Contributing to LogBear

Thanks for your interest. Issues and pull requests are both welcome.

## Two things that get a PR declined

**Total opacity is not negotiable.** LogBear never touches the process it wraps. No injected
logger handlers, no `ERL_FLAGS` / `NODE_OPTIONS` / `RUBYOPT` / `PYTHONPATH`, no
`FORCE_COLOR`, no dependency added to the user's app. We observe the output stream and
nothing else. A change that violates this will be declined however well it works — a
sidecar Elixir logger handler was built, verified to work perfectly, and rejected on this
principle.

**The shim stays out of the wrapped process's way.** Output goes to the terminal first,
capture can't block the wrapped command, and the daemon is contacted in the background
after the child starts. The exit code and signals pass through unchanged. If the daemon is
down the shim spools to disk and carries on.

## Getting set up

Requires Rust 1.90+ and [Bun](https://bun.sh).

```sh
git clone https://github.com/teralabx/logbear
cd logbear
cargo test --workspace --exclude logbear-desktop
```

A plain `cargo test --workspace` fails on a fresh clone: `tauri-build` won't compile
`logbear-desktop` until the four sidecar binaries exist, and those are build outputs that
are never committed. To include the desktop crate, stage them first:

```sh
./scripts/sidecars.sh debug
cargo test --workspace
```

`LOGBEAR_HOME` redirects every piece of state LogBear writes. The test suite sets it, which
is how tests avoid touching your real history — use it when experimenting too:

```sh
LOGBEAR_HOME=/tmp/logbear-scratch cargo run -p logbear -- npm run dev
```

## Before opening a pull request

```sh
cargo test --workspace --exclude logbear-desktop
cargo test -p logbear -- --ignored   # spawns real processes under a PTY; slower
cargo clippy --workspace --exclude logbear-desktop --all-targets -- -D warnings
cd apps/desktop && bun run check
```

CI runs all of these, with the desktop crate included, and checks that the workspace still
builds on the Rust version `Cargo.toml` declares. It does not run `cargo fmt --check`: the
tree is hand-formatted wider than rustfmt's defaults, so match the file you are editing.

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
| `crates/shim` | the `logbear` binary: PTY wrapper and CLI |
| `crates/daemon` | `logbeard` |
| `crates/mcp` | `logbear-mcp`, the stdio MCP server |
| `crates/hook` | `logbear-hook`, one binary for every harness |
| `crates/setup` | the installer, shared by `logbear setup` and the desktop app |
| `apps/desktop` | Tauri v2 + SvelteKit |

`logbear-core` has zero coupling to the CLI, MCP or UI. New logic belongs there; everything
else should stay a thin adapter over it.

## Licensing of contributions

By submitting a pull request you agree that your contribution is licensed under the terms
in [LICENSE.md](LICENSE.md) (MIT).

## Code of Conduct

Participation is governed by the [Code of Conduct](CODE_OF_CONDUCT.md).
