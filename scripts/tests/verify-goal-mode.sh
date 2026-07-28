#!/usr/bin/env bash
# End-to-end check of goal-mode spawn, in a sandbox of its own.
#
#   scripts/tests/verify-goal-mode.sh [repo-under-test] [alder-binary]
#
# Both arguments default to this checkout: the repository this script lives
# in, and its target/debug/alder, built if it is not there yet. A clean
# checkout therefore runs it with no arguments and no setup beyond a Rust
# toolchain, git, tmux and jq.
#
# Everything happens under one throwaway directory: its own git repo, its own
# bare "remote" holding the alder log, its own work item, and its own tmux
# SERVER, so no session leaks into the project's observer and nothing is
# appended to the project's ledger. Nothing is written inside the checkout.
#
# Isolating that server is the whole safety story, and TMUX_TMPDIR does NOT do
# it. When this script runs inside a tmux pane — which it does, because workers
# live in tmux sessions — the inherited $TMUX names the real server's socket,
# and tmux prefers it over TMUX_TMPDIR for every client command. A previous run
# learned that the hard way: its "tmux kill-server" cleanup reached the real
# server and killed every session on the machine.
#
# So: one explicit private socket (tmux -S "$SOCK"), $TMUX unset on every tmux
# call, and that includes the calls made by worker-spawn.sh itself — a PATH
# shim puts them on the same socket without editing the script under test.
#
# Teardown is narrow on purpose. It kills ONE session, by exact name, and only
# after asserting that the sandbox server holds nothing but that session — if
# it holds anything else, this is not the socket we think it is, so the run
# aborts and kills nothing. No kill-server, no pattern matching, no form of
# kill against the default server, anywhere in this file.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)

for tool in git jq tmux; do
  command -v "$tool" >/dev/null ||
    { echo "$tool is required to run this verification" >&2; exit 2; }
done

REPO_UNDER_TEST=$(cd "${1:-$ROOT}" && pwd)
ALDER=${2:-$REPO_UNDER_TEST/target/debug/alder}
if [ ! -x "$ALDER" ]; then
  if [ -n "${2:-}" ]; then
    echo "not an executable: $ALDER" >&2
    exit 2
  fi
  echo "building $ALDER"
  (cd "$REPO_UNDER_TEST" && cargo build --bin alder)
fi
# Absolute, because the run works from inside the sandbox.
ALDER=$(cd "$(dirname "$ALDER")" && pwd)/$(basename "$ALDER")

REAL_TMUX=$(command -v tmux)
# The real server's socket, so the run can prove it left it alone. Empty when
# this is run outside tmux, in which case there is no outer server to compare.
REAL_SOCK=${TMUX-}
REAL_SOCK=${REAL_SOCK%%,*}

# Read the real server's session list, read-only; empty if there is none.
real_sessions() {
  [ -n "$REAL_SOCK" ] || return 0
  "$REAL_TMUX" -S "$REAL_SOCK" list-sessions -F '#{session_name}' 2>/dev/null |
    sort || true
}

# Two throwaway directories, because they have different lifetimes. The socket
# dir is teardown's: it removes it once the session is gone, and leaves it when
# it declines to kill. The sandbox tree is this run's, and outlives a failure
# so there is something to read afterwards. Both sit in /tmp: a unix socket
# path is capped at ~104 bytes, so the socket cannot live under an arbitrarily
# deep path.
SB=$(mktemp -d /tmp/alder-verify.XXXXXX)
SOCKDIR=$(mktemp -d /tmp/alder-verify-sock.XXXXXX)
SOCK=$SOCKDIR/tmux.sock
[ -n "$SOCK" ] && [ "$SOCK" != "$REAL_SOCK" ] || {
  echo "refusing to run: sandbox socket is the real socket" >&2
  rm -rf "$SB" "$SOCKDIR"
  exit 1
}

# The teardown lives in its own sourced file so test-teardown-guard.sh can
# exercise the real thing — both the kill path and the abort path — rather
# than a copy of it. SESSION_NAME stays empty until the session exists, so a
# failure before the spawn tears nothing down.
SESSION_NAME=""
# shellcheck source-path=SCRIPTDIR source=tmux-sandbox.sh
. "$HERE/tmux-sandbox.sh"

# The sandbox tree is removed only by a run that passed and tore down cleanly.
# Anything else — a failed assertion, or a teardown that found a session it
# does not own and killed nothing — leaves it where it can be inspected.
RUN_STATUS=fail
cleanup() {
  sandbox_teardown
  if [ "$RUN_STATUS" = pass ] && [ "$SANDBOX_TEARDOWN_STATUS" != aborted ]; then
    rm -rf "$SB"
  else
    echo "sandbox left for inspection at $SB" >&2
  fi
}
trap cleanup EXIT

# What the real server looked like before we touched anything. Read-only, and
# compared again at the end: the regression this run exists to disprove is
# "the sandbox reached the real server".
REAL_BEFORE=$(real_sessions)

mkdir -p "$SB/bin"

# Every tmux call worker-spawn.sh makes lands on the sandbox server. The shim
# is what lets the script under test run unmodified.
cat >"$SB/bin/tmux" <<SHIM
#!/usr/bin/env bash
unset TMUX TMUX_PANE
exec "$REAL_TMUX" -S "$SOCK" "\$@"
SHIM
chmod +x "$SB/bin/tmux"
export PATH=$SB/bin:$PATH
# Belt and braces: with $TMUX gone, even a call that slipped past the shim
# would fall back to TMUX_TMPDIR rather than to the real server.
unset TMUX TMUX_PANE
export TMUX_TMPDIR=$SOCKDIR

# A bare repo stands in for the store remote; nothing here touches the real one.
git init -q --bare "$SB/store.git"

git init -q -b main "$SB/repo"
mkdir -p "$SB/repo/scripts"
cp "$REPO_UNDER_TEST/scripts/worker-spawn.sh" "$SB/repo/scripts/"
cp "$REPO_UNDER_TEST/WORKER.md" "$SB/repo/"
git -C "$SB/repo" add -A
git -C "$SB/repo" -c user.email=v@x -c user.name=v commit -qm "sandbox"
git -C "$SB/repo" remote add scratch "$SB/store.git"

cd "$SB/repo"
"$ALDER" init --prefix wv --remote scratch >/dev/null

WORK=$("$ALDER" work add --title "Sandbox item for goal-mode verification" \
  --spec "docs/SANDBOX.md" \
  --check "tests:the sandbox check description reaches the worker" \
  --check "report:the second check reaches the worker too" \
  --json | jq -r '.work_id')
ATTEMPT=$("$ALDER" work start "$WORK" --json | jq -r '.attempt_id')
echo "sandbox work=$WORK attempt=$ATTEMPT"
echo "sandbox tree=$SB"
echo "sandbox tmux socket=$SOCK (real server socket=$REAL_SOCK)"

# The stub engine reads exactly the line that is typed at it and records it,
# then stays alive so the session is observable like any live worker.
cat >"$SB/stub.sh" <<STUB
#!/usr/bin/env bash
IFS= read -r line
printf '%s\n' "\$line" >"$SB/received.txt"
sleep 300
STUB
chmod +x "$SB/stub.sh"

# From here on there is a session teardown may kill — and only this one.
SESSION_NAME=alder-work-$WORK

ALDER_BIN=$ALDER ALDER_WORKER_CMD="$SB/stub.sh" \
  "$SB/repo/scripts/worker-spawn.sh" "$WORK" "$ATTEMPT" claude-opus-5

for _ in $(seq 1 20); do
  [ -s "$SB/received.txt" ] && break
  sleep 1
done

echo "=== session list (sandbox tmux server) ==="
tmux list-sessions -F '#{session_name}'
echo "=== goal the session actually received ==="
cat "$SB/received.txt"
echo "=== pane ==="
tmux capture-pane -pt "alder-work-$WORK" | head -5
echo "=== attempt after spawn ==="
"$ALDER" show "$ATTEMPT" --json | jq -c '.current | {handle, metadata}'

fail() {
  echo "FAIL: $1" >&2
  exit 1
}
[ -s "$SB/received.txt" ] || fail "the session received no goal"
received=$(cat "$SB/received.txt")
[ "$(wc -l <"$SB/received.txt")" -eq 1 ] || fail "goal was not a single line"
case $received in
*"$WORK"*) ;; *) fail "goal omits the work id" ;;
esac
case $received in
*"$ATTEMPT"*) ;; *) fail "goal omits the attempt id" ;;
esac
case $received in
*"Sandbox item for goal-mode verification"*) ;; *) fail "goal omits the title" ;;
esac
case $received in
*"docs/SANDBOX.md"*) ;; *) fail "goal omits the spec" ;;
esac
case $received in
*"the sandbox check description reaches the worker"*) ;; *) fail "goal omits check tests" ;;
esac
case $received in
*"the second check reaches the worker too"*) ;; *) fail "goal omits check report" ;;
esac
case $received in
*"cargo clippy --workspace --all-targets"*) ;; *) fail "goal omits the gates" ;;
esac
[ -d "$SB/alder-work-$WORK" ] || fail "worktree missing"
[ "$(git -C "$SB/repo" rev-parse --abbrev-ref "work/$WORK")" = "work/$WORK" ] ||
  fail "branch missing"
"$ALDER" show "$ATTEMPT" --json |
  jq -e '.current.handle == "tmux:alder-work-'"$WORK"'"' >/dev/null ||
  fail "handle not bound"
"$ALDER" show "$ATTEMPT" --json |
  jq -e '.current.metadata.engine == "claude-opus-5"' >/dev/null ||
  fail "engine metadata not stamped"

# The sandbox session exists on the sandbox server, alone, and nowhere else.
# This is the same assertion teardown makes before it kills anything.
[ "$(sandbox_sessions)" = "$SESSION_NAME" ] ||
  fail "sandbox server holds [$(sandbox_sessions | tr '\n' ' ')], expected only $SESSION_NAME"
REAL_AFTER=$(real_sessions)
[ "$REAL_BEFORE" = "$REAL_AFTER" ] ||
  fail "the real tmux server changed: [$REAL_BEFORE] -> [$REAL_AFTER]"
if printf '%s\n' "$REAL_AFTER" | grep -qx "alder-work-$WORK"; then
  fail "sandbox session leaked onto the real server"
fi

echo
echo "=== real tmux server, unchanged across the run ==="
printf '%s\n' "$REAL_AFTER"
echo
RUN_STATUS=pass
echo "PASS: goal-mode spawn delivered spec, checks and gates to a live session"
