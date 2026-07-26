#!/usr/bin/env bash
# Build the four binaries and stage them as Tauri sidecars.
#
# Tauri requires each external binary to carry its target triple in the filename, so the
# bundle can pick the right one per platform. This staging step is what makes the app the
# installer: one download that already contains the daemon, the shim, the MCP server and
# the hook.
set -euo pipefail

cd "$(dirname "$0")/.."
PROFILE="${1:-release}"
TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
OUT="apps/desktop/src-tauri/binaries"

if [ "$PROFILE" = "release" ]; then
  cargo build --release -p runlog -p runlogd -p runlog-mcp -p runlog-hook
  SRC="target/release"
else
  cargo build -p runlog -p runlogd -p runlog-mcp -p runlog-hook
  SRC="target/debug"
fi

mkdir -p "$OUT"
for bin in runlog runlogd runlog-mcp runlog-hook; do
  cp "$SRC/$bin" "$OUT/$bin-$TRIPLE"
  chmod +x "$OUT/$bin-$TRIPLE"
  echo "staged $OUT/$bin-$TRIPLE"
done
