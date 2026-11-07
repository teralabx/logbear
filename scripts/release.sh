#!/usr/bin/env bash
# Prepare a release: set the version, roll the changelog, commit and tag.
#
#   scripts/release.sh 0.2.0
#
# Pushes nothing. Pushing the tag is what publishes, and that stays a separate decision.
# LOGBEAR_RELEASE_BRANCH overrides the branch a release must be cut from.
set -euo pipefail

cd "$(dirname "$0")/.."

die() { echo "release: $*" >&2; exit 1; }

version="${1:-}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] ||
  die "usage: scripts/release.sh <version>, like 0.2.0 or 0.2.0-rc.1"

branch="${LOGBEAR_RELEASE_BRANCH:-master}"
[ "$(git branch --show-current)" = "$branch" ] || die "not on $branch"
[ -z "$(git status --porcelain)" ] || die "the working tree is not clean"
git rev-parse -q --verify "refs/tags/v$version" >/dev/null && die "v$version already exists"

previous="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

# Every place the version is written down. The two path dependencies carry it so the crates
# can be published. Each pattern is anchored: other dependencies may share these numbers.
perl -pi -e 's/^version = ".*"/version = "'"$version"'"/ if !$done && /^version = / && ($done = 1)' Cargo.toml
perl -pi -e 's/^(logbear-(?:core|setup) = \{.*version = ")[^"]*/${1}'"$version"'/' Cargo.toml
perl -pi -e 's/^(  "version": ")[^"]*/${1}'"$version"'/' \
  apps/desktop/src-tauri/tauri.conf.json apps/desktop/package.json
cargo metadata --format-version 1 >/dev/null # rewrites Cargo.lock's own entries

# Skipped for a pre-release, whose notes stay under Unreleased for the real one.
if [[ "$version" != *-* ]]; then
  grep -q '^## \[Unreleased\]' CHANGELOG.md || die "CHANGELOG.md has no Unreleased section"
  repo="https://github.com/teralabx/logbear"
  today="$(date +%Y-%m-%d)"
  V="$version" D="$today" R="$repo" perl -0pi -e '
    s/^## \[Unreleased\]\n/## [Unreleased]\n\n## [$ENV{V}] — $ENV{D}\n/m;
    s/^\[Unreleased\]: .*\n/[Unreleased]: $ENV{R}\/compare\/v$ENV{V}...HEAD\n[$ENV{V}]: $ENV{R}\/releases\/tag\/v$ENV{V}\n/m;
  ' CHANGELOG.md
fi

git add -A
git commit -m "Release v$version"
git tag -a "v$version" -m "logbear $version"

echo
echo "Tagged v$version (was $previous). Nothing has been pushed. To publish:"
echo "  git push origin $branch v$version"
