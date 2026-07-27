## What this changes

<!-- The problem, then the change. Link the issue if there is one. -->

## How it was verified

<!-- The command you ran and what it showed. For a performance change, the before and after
     numbers. For a fix, the test that fails without it. -->

## Checklist

- [ ] `cargo test --workspace` passes
- [ ] `cargo test --workspace -- --ignored` passes, if capture or the CLI changed
- [ ] `cargo fmt --check` and `cargo clippy --workspace --all-targets` are clean
- [ ] `bun run check` passes, if the desktop app changed
- [ ] Nothing is injected into the wrapped process, and the shim still cannot block or
      degrade it
- [ ] `CHANGELOG.md` updated under `Unreleased`, if this is user-visible
