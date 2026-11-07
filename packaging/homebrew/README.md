# Homebrew packaging

`logbear.rb.in` is the formula's template. It lives here so it is versioned alongside the
code; Homebrew reads the rendered copy from the
[`teralabx/homebrew-tap`](https://github.com/teralabx/homebrew-tap) repository. Nobody edits
that copy: each release overwrites it.

The formula installs the prebuilt archives from the GitHub release, the same files
[`install.sh`](../../install.sh) downloads, so an install takes seconds and needs no Rust.
`homebrew-core` requires building from source, so moving there later means a source formula
rather than this one.

## One-time tap setup

The repository **must** be named `homebrew-tap`; `brew` derives the tap name by stripping
that prefix. Releases push to it with a deploy key, which can write to that repository and
nothing else, and does not expire the way a personal token does:

```sh
gh repo create teralabx/homebrew-tap --public \
  --description "Homebrew formulae for teralabx tools"
ssh-keygen -t ed25519 -N "" -C "logbear release" -f tap_key
gh repo deploy-key add tap_key.pub --repo teralabx/homebrew-tap \
  --title "logbear release" --allow-write
gh secret set HOMEBREW_TAP_DEPLOY_KEY --repo teralabx/logbear < tap_key
rm tap_key tap_key.pub
```

## Publishing to crates.io

Order matters. `cargo publish` resolves against the real index, so each crate's
dependencies have to exist there first: everything depends on `logbear-core`, and
`logbear` (the CLI) and the desktop app also depend on `logbear-setup`:

```sh
cargo publish -p logbear-core
cargo publish -p logbear-setup
cargo publish -p logbeard -p logbear-mcp -p logbear-hook -p logbear
```

`logbear-desktop` is marked `publish = false` — it ships as an app bundle.

## Cutting a release

From a clean, up-to-date `master`:

```sh
scripts/release.sh 0.2.0
git push origin master v0.2.0
```

`release.sh` sets the version everywhere it is recorded, turns the changelog's Unreleased
section into this release, commits, and tags. It pushes nothing, so the last line above is
the point of no return.

The tag runs [`release.yml`](../../.github/workflows/release.yml), which has three stages:

1. **Build:** checks the tag against the workspace version, runs the tests, and builds the
   four binaries for both macOS architectures.
2. **Publish:** creates the GitHub release with a `.sha256` beside each archive, with notes
   from the changelog.
3. **Homebrew:** renders the formula with those hashes, pushes it to the tap, then installs
   it from the tap on a clean runner and runs `brew test`.

A tag with a suffix, like `v0.2.0-rc.1`, is published as a pre-release and stops before the
Homebrew stage. Neither the tap nor `logbear upgrade` will offer it.

If the Homebrew stage fails, the release is already public and only `brew` users are behind.
Fix the cause and use **Re-run failed jobs**; it reads the hashes back from the release.

To try a formula without releasing, render it against any archive:

```sh
scripts/render-formula.sh 0.2.0 <arm-sha256> <intel-sha256> > logbear.rb
```

## Upgrades

`logbear upgrade` runs `brew update` and `brew upgrade teralabx/tap/logbear`; a plain
`brew upgrade` works just as well. Either way the running daemon is the old version until
it notices its binary was replaced. It waits for live runs to finish, then exits with code
75, and launchd restarts it on the new one. `post_install` cannot do this, because Homebrew
sandboxes it away from the daemon. See
[`crates/daemon/src/retire.rs`](../../crates/daemon/src/retire.rs).

## The desktop app and `brew link`

The desktop app can link its own `logbear` into `/opt/homebrew/bin`. Homebrew will not
replace a file it does not own, so on such a machine the install finishes but leaves the
formula unlinked. To make the Homebrew copy the one on PATH:

```sh
brew link --overwrite logbear
```

## What the formula does not cover

The desktop app. Shipping it through Homebrew means a cask pointing at a signed,
notarised `.dmg`, which needs an Apple Developer ID and a release pipeline that does
not exist yet. Until then the app is a direct download and the formula covers the
CLI, daemon, MCP server and hook.
