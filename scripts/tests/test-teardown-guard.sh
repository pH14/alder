#!/usr/bin/env bash
# Exercise the real teardown from tmux-sandbox.sh — both paths.
#
#   A. server holds only the owned session  -> it is killed, socket cleaned
#   B. server holds anything else           -> abort, nothing is killed
#
# Throughout, the default tmux server must be untouched. That is the property
# the incident was about, so it is asserted after every case.
set -euo pipefail

command -v tmux >/dev/null ||
  { echo "tmux is required to run this test" >&2; exit 2; }

REAL_TMUX=$(command -v tmux)
REAL_SOCK=${TMUX-}
REAL_SOCK=${REAL_SOCK%%,*}
HERE=$(cd "$(dirname "$0")" && pwd)

real_sessions() {
  [ -n "$REAL_SOCK" ] || return 0
  "$REAL_TMUX" -S "$REAL_SOCK" list-sessions -F '#{session_name}' 2>/dev/null |
    sort || true
}
REAL_BEFORE=$(real_sessions)

fail() { echo "FAIL: $1" >&2; exit 1; }
assert_real_intact() {
  [ "$(real_sessions)" = "$REAL_BEFORE" ] ||
    fail "$1: the default tmux server changed"
}

# --- Case A: the owned session, alone ---------------------------------------
SOCKDIR=$(mktemp -d /tmp/aldv-t.XXXXXX)
SOCK=$SOCKDIR/tmux.sock
SESSION_NAME=owned
# shellcheck source-path=SCRIPTDIR source=tmux-sandbox.sh
. "$HERE/tmux-sandbox.sh"

sbtmux new-session -d -s owned 'sleep 300'
[ "$(sandbox_sessions)" = "owned" ] || fail "A: setup did not produce one session"
sandbox_teardown
[ -z "$(sandbox_sessions)" ] || fail "A: owned session survived teardown"
[ ! -d "$SOCKDIR" ] || fail "A: socket dir not cleaned up"
[ "$SANDBOX_TEARDOWN_STATUS" = killed ] ||
  fail "A: teardown reported '$SANDBOX_TEARDOWN_STATUS', expected killed"
assert_real_intact A
echo "PASS A: teardown killed the owned session by exact name and cleaned up"

# --- Case B: a session this run does not own --------------------------------
SOCKDIR=$(mktemp -d /tmp/aldv-t.XXXXXX)
SOCK=$SOCKDIR/tmux.sock
SESSION_NAME=owned
SANDBOX_TEARDOWN_STATUS=

sbtmux new-session -d -s owned 'sleep 300'
sbtmux new-session -d -s a-stranger 'sleep 300'
[ "$(sandbox_sessions)" = "$(printf 'a-stranger\nowned')" ] ||
  fail "B: setup did not produce two sessions"

# Through a file rather than $(...), so teardown runs in this shell and the
# status it records is visible here — the way a caller's cleanup reads it.
ERR=$(mktemp /tmp/aldv-t-err.XXXXXX)
set +e
sandbox_teardown 2>"$ERR"
rc=$?
set -e
out=$(cat "$ERR")
rm -f "$ERR"
[ $rc -eq 0 ] || fail "B: teardown returned $rc instead of declining cleanly"
case $out in *"ABORT"*) ;; *) fail "B: teardown did not report an abort" ;; esac
[ "$SANDBOX_TEARDOWN_STATUS" = aborted ] ||
  fail "B: teardown reported '$SANDBOX_TEARDOWN_STATUS', expected aborted"
[ "$(sandbox_sessions)" = "$(printf 'a-stranger\nowned')" ] ||
  fail "B: teardown killed something after aborting"
[ -d "$SOCKDIR" ] || fail "B: socket dir removed despite the abort"
assert_real_intact B
echo "PASS B: teardown aborted and killed nothing"
printf '%s\n' "$out" | sed 's/^/       | /'

# Clean up case B the way the guard requires: one exact name at a time.
sbtmux kill-session -t "=a-stranger"
sbtmux kill-session -t "=owned"
rm -rf "$SOCKDIR"
assert_real_intact "final"

echo
echo "default tmux server, unchanged across both cases:"
printf '%s\n' "$REAL_BEFORE" | sed 's/^/       | /'
echo "PASS: teardown guard holds"
