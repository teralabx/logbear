#!/bin/sh
# Print the Homebrew formula for a release.
#
#   scripts/render-formula.sh <version> <arm64-sha256> <x86_64-sha256>
#
# LOGBEAR_DOWNLOAD_BASE points the archive URLs somewhere other than the GitHub release,
# which is how a formula is tried before anything is published.
set -eu

[ $# -eq 3 ] || { echo "usage: $0 <version> <arm64-sha256> <x86_64-sha256>" >&2; exit 2; }

for sha in "$2" "$3"; do
  case "$sha" in
    *[!0-9a-f]*) echo "not a SHA-256: $sha" >&2; exit 1 ;;
  esac
  [ "${#sha}" -eq 64 ] || { echo "not a SHA-256: $sha" >&2; exit 1; }
done

base="${LOGBEAR_DOWNLOAD_BASE:-https://github.com/teralabx/logbear/releases/download}"
template="$(dirname "$0")/../packaging/homebrew/logbear.rb.in"

sed -e "s|@VERSION@|$1|g" -e "s|@BASE@|$base|g" \
    -e "s|@SHA_ARM@|$2|g" -e "s|@SHA_INTEL@|$3|g" "$template"
