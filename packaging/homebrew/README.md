# Homebrew packaging

`runlog.rb` is the formula. It lives here so it is versioned alongside the code it
builds; Homebrew itself reads it from a separate tap repository.

## Why a tap and not homebrew-core

`homebrew-core` only accepts formulae under an OSI-approved licence. runlog is
source-available under FSL-1.1-MIT, so it is not eligible, and `brew install runlog`
will never resolve. A tap has no licence requirement, which makes
`brew install teralabx/tap/runlog` the supported route.

## One-time tap setup

```sh
gh repo create teralabx/homebrew-tap --public \
  --description "Homebrew formulae for teralabx tools"
git clone https://github.com/teralabx/homebrew-tap
mkdir -p homebrew-tap/Formula
cp packaging/homebrew/runlog.rb homebrew-tap/Formula/
```

The repository **must** be named `homebrew-tap`; `brew` derives the tap name by
stripping that prefix.

## Publishing to crates.io

Order matters. Four crates depend on `runlog-core`, and `cargo publish` resolves against
the real index, so the dependency has to exist there first:

```sh
cargo publish -p runlog-core
cargo publish -p runlogd -p runlog-mcp -p runlog-hook -p runlog
```

`runlog-desktop` is marked `publish = false` — it ships as an app bundle.

## Cutting a release

The formula pins a tarball by hash, so both change together.

```sh
git tag v0.1.0 && git push origin v0.1.0
curl -sL https://github.com/teralabx/runlog/archive/refs/tags/v0.1.0.tar.gz | shasum -a 256
```

Update `url` and `sha256` in `runlog.rb`, copy it into the tap, and verify before
pushing:

```sh
brew install --build-from-source ./Formula/runlog.rb
brew test runlog
brew audit --strict --online runlog
```

`brew audit` will flag `license :cannot_represent`. That is expected and correct —
it is the documented value for a licence SPDX cannot express.

## What the formula does not cover

The desktop app. Shipping it through Homebrew means a cask pointing at a signed,
notarised `.dmg`, which needs an Apple Developer ID and a release pipeline that does
not exist yet. Until then the app is a direct download and the formula covers the
CLI, daemon, MCP server and hook.
