#!/bin/sh
# Install logbear: the CLI, the daemon, the MCP server and the agent hook.
#
#   curl -fsSL https://raw.githubusercontent.com/teralabx/logbear/master/install.sh | sh
#
# Downloads the release archive for this machine, checks it against the SHA-256 published
# beside it, and copies four binaries into ~/.local/bin. Nothing else changes: no shell
# profile, no agent config, no background service. `logbear setup` does those afterwards,
# and shows you what it will change before it changes it.
#
#   LOGBEAR_VERSION=0.2.0       a specific release instead of the latest
#   LOGBEAR_INSTALL_DIR=<dir>   somewhere other than ~/.local/bin

set -eu

repo="teralabx/logbear"
dir="${LOGBEAR_INSTALL_DIR:-$HOME/.local/bin}"
base="${LOGBEAR_DOWNLOAD_BASE:-https://github.com/$repo/releases}"
version="${LOGBEAR_VERSION:-}"

say() { printf '%s\n' "$*"; }
die() { printf 'logbear install: %s\n' "$*" >&2; exit 1; }

case "$(uname -s)/$(uname -m)" in
  Darwin/arm64) target="aarch64-apple-darwin" ;;
  Darwin/x86_64) target="x86_64-apple-darwin" ;;
  *) die "release binaries are macOS-only for now. On $(uname -s), build from source: cargo install logbear logbeard logbear-mcp logbear-hook" ;;
esac

command -v curl >/dev/null || die "curl is required"
command -v shasum >/dev/null || die "shasum is required"

if [ -z "$version" ]; then
  # The /latest page redirects to /tag/vX.Y.Z; the version is the last path segment.
  latest="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$base/latest")" ||
    die "could not reach $base/latest"
  version="${latest##*/v}"
  [ -n "$version" ] && [ "$version" != "$latest" ] || die "could not find the latest release at $base"
fi
version="${version#v}"

name="logbear-$version-$target"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading logbear $version for $target"
curl -fsSL "$base/download/v$version/$name.tar.gz" -o "$tmp/$name.tar.gz" ||
  die "no release archive at $base/download/v$version/$name.tar.gz"
curl -fsSL "$base/download/v$version/$name.tar.gz.sha256" -o "$tmp/$name.tar.gz.sha256" ||
  die "no checksum published for $name.tar.gz"

(cd "$tmp" && shasum -a 256 -c "$name.tar.gz.sha256" >/dev/null) ||
  die "checksum mismatch for $name.tar.gz; nothing was installed"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$dir"
for bin in logbear logbeard logbear-mcp logbear-hook; do
  [ -f "$tmp/$name/$bin" ] || die "$bin is missing from the archive"
  # Replace rather than overwrite in place: a running daemon keeps its old inode.
  rm -f "$dir/$bin"
  cp "$tmp/$name/$bin" "$dir/$bin"
  chmod 755 "$dir/$bin"
done
say "Installed logbear, logbeard, logbear-mcp and logbear-hook into $dir"

# Two copies on PATH means debugging a fix that was never running.
found="$(command -v logbear 2>/dev/null || true)"
case ":$PATH:" in
  *":$dir:"*)
    if [ -n "$found" ] && [ "$found" != "$dir/logbear" ]; then
      say ""
      say "Note: $found comes first on your PATH, so \`logbear\` still runs that copy."
    fi
    ;;
  *)
    say ""
    say "$dir is not on your PATH. Add this to your shell profile:"
    say "  export PATH=\"$dir:\$PATH\""
    ;;
esac

# `logbear upgrade` runs this script, and that user has done the next step already.
[ -z "${LOGBEAR_UPGRADING:-}" ] || exit 0

say ""
say "Next, connect logbear to your coding agents and start the daemon:"
say "  $dir/logbear setup"
