# Homebrew packaging

`logbear.rb` is the formula. It lives here so it is versioned alongside the code it
builds; Homebrew itself reads it from a separate tap repository.

## Why a tap first

logbear is MIT-licensed, so it is eligible for `homebrew-core`. But core also asks for a
tagged stable release and evidence of use before it accepts a new formula, so the tap,
`brew install teralabx/tap/logbear`, is the route until then.

## One-time tap setup

```sh
gh repo create teralabx/homebrew-tap --public \
  --description "Homebrew formulae for teralabx tools"
git clone https://github.com/teralabx/homebrew-tap
mkdir -p homebrew-tap/Formula
cp packaging/homebrew/logbear.rb homebrew-tap/Formula/
```

The repository **must** be named `homebrew-tap`; `brew` derives the tap name by
stripping that prefix.

## Publishing to crates.io

Order matters. Four crates depend on `logbear-core`, and `cargo publish` resolves against
the real index, so the dependency has to exist there first:

```sh
cargo publish -p logbear-core
cargo publish -p logbeard -p logbear-mcp -p logbear-hook -p logbear
```

`logbear-desktop` is marked `publish = false` — it ships as an app bundle.

## Cutting a release

The formula pins a tarball by hash, so both change together.

```sh
git tag v0.1.0 && git push origin v0.1.0
curl -sL https://github.com/teralabx/logbear/archive/refs/tags/v0.1.0.tar.gz | shasum -a 256
```

Update `url` and `sha256` in `logbear.rb`, copy it into the tap, and verify before
pushing:

```sh
brew install --build-from-source ./Formula/logbear.rb
brew test logbear
brew audit --strict --online logbear
```

## What the formula does not cover

The desktop app. Shipping it through Homebrew means a cask pointing at a signed,
notarised `.dmg`, which needs an Apple Developer ID and a release pipeline that does
not exist yet. Until then the app is a direct download and the formula covers the
CLI, daemon, MCP server and hook.
