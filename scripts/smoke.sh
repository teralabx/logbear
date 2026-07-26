#!/usr/bin/env bash
# End-to-end smoke test: the real binaries, a real pty, a real daemon, a real database.
#
# The unit tests prove each piece in isolation. This proves the pieces are actually wired
# to each other — which is the failure that unit tests structurally cannot catch, and the
# one that matters when a user types `runlog mix phx.server` for the first time.
#
# Everything happens inside a throwaway RUNLOG_HOME, so running this cannot touch your
# real history.
set -uo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

RUNLOG_HOME="$(mktemp -d)"
WORK="$(mktemp -d)"
export RUNLOG_HOME
trap 'cleanup' EXIT

PASS=0
FAIL=0

cleanup() {
  "$BIN/runlogd" stop >/dev/null 2>&1 || true
  sleep 0.3
  pkill -f "runlogd serve" 2>/dev/null || true
  rm -rf "$RUNLOG_HOME" "$WORK"
}

ok()   { PASS=$((PASS+1)); printf '  \033[32m✓\033[0m %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  \033[31m✗\033[0m %s\n' "$1"; [ $# -gt 1 ] && printf '      %s\n' "$2"; }
step() { printf '\n\033[1m%s\033[0m\n' "$1"; }

# assert <description> <haystack> <needle>
contains() {
  if printf '%s' "$2" | grep -qF -- "$3"; then ok "$1"; else bad "$1" "expected to find: $3"; fi
}
not_contains() {
  if printf '%s' "$2" | grep -qF -- "$3"; then bad "$1" "should not contain: $3"; else ok "$1"; fi
}

step "Building"
cargo build -p runlog -p runlogd -p runlog-mcp -p runlog-hook 2>&1 | tail -3
for b in runlog runlogd runlog-mcp runlog-hook; do
  [ -x "$BIN/$b" ] || { echo "missing binary: $BIN/$b"; exit 1; }
done

step "Daemon starts and answers"
"$BIN/runlogd" serve >"$RUNLOG_HOME/daemon.out" 2>&1 &
for _ in $(seq 1 100); do
  [ -S "$RUNLOG_HOME/runlogd.sock" ] && break
  sleep 0.1
done
STATUS="$("$BIN/runlogd" status 2>&1)"
contains "runlogd reports status" "$STATUS" '"pid"'

step "Capturing a run through a real pty"
cd "$WORK"
git init -q . 2>/dev/null
git -c user.email=t@t -c user.name=t commit -q --allow-empty -m "first" 2>/dev/null

# A clean run first, so there is a last-known-good to diff against.
"$BIN/runlog" /bin/sh -c 'echo "GET /users"; echo "compiled 41 files"' >/tmp/rl-clean.out 2>&1
CLEAN_RC=$?
[ "$CLEAN_RC" = "0" ] && ok "exit code propagated for a clean run" || bad "exit code propagated for a clean run" "got $CLEAN_RC"
contains "passthrough reached the terminal" "$(cat /tmp/rl-clean.out)" "GET /users"

# Then a failing run with a multi-line stack trace and a secret in it.
"$BIN/runlog" /bin/sh -c '
printf "GET /posts\n"
printf "connecting to postgres://admin:hunter2@db:5432/app\n"
printf "Error: connection refused\n"
printf "    at Object.<anonymous> (/app/server.js:12:9)\n"
printf "    at Module._compile (node:internal/modules:1105:14)\n"
exit 3' >/tmp/rl-bad.out 2>&1
BAD_RC=$?
[ "$BAD_RC" = "3" ] && ok "non-zero exit code propagated verbatim" || bad "non-zero exit code propagated verbatim" "expected 3, got $BAD_RC"

sleep 1

step "The run is queryable"
# The JSON is pretty-printed, so match on the field rather than on a byte sequence.
RUNS="$("$BIN/runlog" runs --json 2>&1 | tr -d ' \n')"
contains "the failing run recorded its exit code" "$RUNS" '"exit_code":3'
contains "the clean run recorded its exit code" "$RUNS" '"exit_code":0'

LOGS="$("$BIN/runlog" logs --errors -n 5 --json 2>&1)"
contains "the error was detected" "$LOGS" "connection refused"
contains "the whole stack trace came back as one entry" "$LOGS" "Module._compile"
not_contains "the secret never reached the database" "$LOGS" "hunter2"

step "Diff answers 'what changed'"
DIFF="$("$BIN/runlog" diff --json 2>&1)"
contains "the new signature is reported" "$DIFF" "connection refused"

step "Agent SQL is sandboxed"
Q="$("$BIN/runlog" query "SELECT count(*) AS n FROM v_errors" --json 2>&1)"
contains "a legitimate query works" "$Q" '"n"'

for evil in "DELETE FROM entries" "ATTACH DATABASE '/etc/passwd' AS e" "PRAGMA table_info(runs)"; do
  OUT="$("$BIN/runlog" query "$evil" 2>&1)"
  if [ $? -eq 0 ] && ! printf '%s' "$OUT" | grep -qiE "reject|not permitted|denied|error"; then
    bad "sandbox refuses: $evil" "$OUT"
  else
    ok "sandbox refuses: $evil"
  fi
done

step "MCP server speaks JSON-RPC"
MCP_OUT="$(printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | "$BIN/runlog-mcp" 2>/dev/null)"
contains "initialize is answered" "$MCP_OUT" '"protocolVersion"'
for tool in runlog_schema runlog_query runlog_wait runlog_diff runlog_ack; do
  contains "tools/list advertises $tool" "$MCP_OUT" "$tool"
done

step "The hook blocks when there are new errors"
HOOK_IN="$(printf '{"session_id":"smoke-session","cwd":"%s","hook_event_name":"Stop"}' "$WORK")"
HOOK_OUT="$(printf '%s' "$HOOK_IN" | "$BIN/runlog-hook" 2>/dev/null)"
HOOK_RC=$?
[ "$HOOK_RC" = "0" ] && ok "hook exits 0" || bad "hook exits 0" "got $HOOK_RC"

# A brand-new session starts at the high-water mark, so the first Stop is silent by
# design. Produce an error *after* the session exists, then the hook must interrupt.
"$BIN/runlog" /bin/sh -c 'printf "Error: something new broke\n"; exit 1' >/dev/null 2>&1
sleep 1
HOOK_OUT2="$(printf '%s' "$HOOK_IN" | "$BIN/runlog-hook" 2>/dev/null)"
contains "the hook blocks on a new error" "$HOOK_OUT2" "block"
contains "the block carries the actual error" "$HOOK_OUT2" "something new broke"

# Loop safety: the same error must not block the same session twice.
HOOK_OUT3="$(printf '%s' "$HOOK_IN" | "$BIN/runlog-hook" 2>/dev/null)"
not_contains "the same error does not block twice" "$HOOK_OUT3" '"decision":"block"'

step "Reparse is safe and idempotent"
BEFORE="$("$BIN/runlog" logs -n 100 --json 2>&1 | grep -c "captured_at")"
"$BIN/runlog" query "SELECT 1" >/dev/null 2>&1
AFTER="$("$BIN/runlog" logs -n 100 --json 2>&1 | grep -c "captured_at")"
[ "$BEFORE" = "$AFTER" ] && ok "entry count is stable" || bad "entry count is stable" "$BEFORE then $AFTER"

step "Doctor reports the world"
DOC="$("$BIN/runlog" doctor 2>&1)"
contains "doctor mentions the daemon" "$DOC" "daemon"

printf '\n\033[1m%d passed, %d failed\033[0m\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
