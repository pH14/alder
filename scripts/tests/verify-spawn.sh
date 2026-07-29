#!/usr/bin/env bash
# End-to-end check of `alderd spawn`, in a sandbox of its own.
#
#   scripts/tests/verify-spawn.sh [repo-under-test] [alder-binary] [alderd-binary]
#
# All three arguments default to this checkout: the repository this script
# lives in and its target/debug binaries, built if they are not there yet. A
# clean checkout therefore runs it with no arguments and no setup beyond a Rust
# toolchain, git, tmux and jq.
#
# What it proves, which unit tests cannot: that a real tmux pane really does
# receive the goal as one argv element, that the pane outlives a one-shot
# engine, that the worktree is really cut and really carries `alder`, that the
# attempt is really bound with its tier stamped — and that the whole dispatch
# types nothing and waits for nothing.
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
# call, and that includes the calls made by alderd itself — a PATH shim puts
# them on the same socket without editing the binary under test. That shim also
# logs every tmux invocation, which is how "injects nothing via send-keys" is
# checked rather than asserted.
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
ALDERD=${3:-$REPO_UNDER_TEST/target/debug/alderd}
for binary in "$ALDER" "$ALDERD"; do
  [ -x "$binary" ] && continue
  if [ -n "${2:-}" ]; then
    echo "not an executable: $binary" >&2
    exit 2
  fi
  echo "building $binary"
  (cd "$REPO_UNDER_TEST" && cargo build --bin "$(basename "$binary")")
done
# Absolute, because the run works from inside the sandbox.
ALDER=$(cd "$(dirname "$ALDER")" && pwd)/$(basename "$ALDER")
ALDERD=$(cd "$(dirname "$ALDERD")" && pwd)/$(basename "$ALDERD")

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

fail() {
  echo "FAIL: $1" >&2
  exit 1
}

# What the real server looked like before we touched anything. Read-only, and
# compared again at the end: the regression this run exists to disprove is
# "the sandbox reached the real server".
REAL_BEFORE=$(real_sessions)

mkdir -p "$SB/bin"

# Every tmux call alderd makes lands on the sandbox server, and every one of
# them is logged: what alderd does to a terminal is an assertion here, not a
# claim.
TMUX_LOG=$SB/tmux-calls.log
cat >"$SB/bin/tmux" <<SHIM
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$TMUX_LOG"
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
cp "$REPO_UNDER_TEST/WORKER.md" "$SB/repo/"
git -C "$SB/repo" add -A
git -C "$SB/repo" -c user.email=v@x -c user.name=v commit -qm "sandbox"
git -C "$SB/repo" remote add scratch "$SB/store.git"

cd "$SB/repo"
"$ALDER" init --prefix wv --remote scratch >/dev/null

WORK=$("$ALDER" work add --title "Sandbox item for spawn verification" \
  --spec "docs/SANDBOX.md" \
  --check "tests:the sandbox check description reaches the worker" \
  --check "report:the second check reaches the worker too" \
  --json | jq -r '.work_id')
echo "sandbox work=$WORK"
echo "sandbox tree=$SB"
echo "sandbox tmux socket=$SOCK (real server socket=$REAL_SOCK)"

# The stub engine records the argv it was handed — one line per argument, so
# a goal that arrived as several arguments is visible as several lines — and
# then EXITS. What happens after it exits is the point: the pane must survive.
cat >"$SB/stub.sh" <<STUB
#!/usr/bin/env bash
printf '%s\n' "\$#" >"$SB/argc.txt"
printf '%s\n' "\$@" >"$SB/argv.txt"
STUB
chmod +x "$SB/stub.sh"

# An unknown tier must be refused before anything exists. No attempt, no
# worktree, no session — this is the check that a typo cannot silently launch
# a worker at whatever the CLI defaults to.
if ALDER_BIN=$ALDER "$ALDERD" spawn "$WORK" gpt-5.6-luna >"$SB/bogus.out" 2>&1; then
  fail "an unknown tier was accepted: $(cat "$SB/bogus.out")"
fi
grep -q "unknown tier" "$SB/bogus.out" ||
  fail "the unknown-tier error does not say so: $(cat "$SB/bogus.out")"
for rung in luna terra sol sonnet opus fable; do
  grep -q "$rung" "$SB/bogus.out" || fail "the unknown-tier error omits $rung"
done
[ "$("$ALDER" status --section in_flight --json | jq '.in_flight | length')" -eq 0 ] ||
  fail "a rejected tier still recorded an attempt"
[ -d "$SB/alder-work-$WORK" ] && fail "a rejected tier still cut a worktree"
[ -s "$TMUX_LOG" ] && fail "a rejected tier still touched tmux"

# From here on there is a session teardown may kill — and only this one.
SESSION_NAME=alder-work-$WORK

STARTED=$(date +%s)
ALDER_BIN=$ALDER ALDER_WORKER_CMD="$SB/stub.sh" \
  "$ALDERD" spawn "$WORK" luna
ELAPSED=$(( $(date +%s) - STARTED ))

# The stub exits immediately, so its argv file appears at once. Waiting for it
# is not a sleep in the spawn path; it is this script waiting for a process
# alderd deliberately does not wait for.
for _ in $(seq 1 20); do
  [ -s "$SB/argv.txt" ] && break
  sleep 0.2
done

ATTEMPT=$("$ALDER" status --section in_flight --json | jq -r '.in_flight[0].id')

echo "=== session list (sandbox tmux server) ==="
tmux list-sessions -F '#{session_name}'
echo "=== argv the engine actually received ==="
cat "$SB/argv.txt"
echo "=== every tmux call alderd made ==="
cat "$TMUX_LOG"
echo "=== attempt after spawn ==="
"$ALDER" show "$ATTEMPT" --json | jq -c '.current | {handle, metadata}'

# The goal is one argument, and it is the whole brief.
[ -s "$SB/argv.txt" ] || fail "the engine received no goal"
[ "$(cat "$SB/argc.txt")" = "1" ] ||
  fail "the goal arrived as $(cat "$SB/argc.txt") arguments, not one"
[ "$(wc -l <"$SB/argv.txt")" -eq 1 ] || fail "the goal was not a single line"
received=$(cat "$SB/argv.txt")
for part in "$WORK" "$ATTEMPT" "Sandbox item for spawn verification" \
  "docs/SANDBOX.md" "the sandbox check description reaches the worker" \
  "the second check reaches the worker too" "cargo clippy --workspace --all-targets" \
  "Read WORKER.md"; do
  case $received in
  *"$part"*) ;;
  *) fail "goal omits: $part" ;;
  esac
done

# Nothing was typed at the session, and nothing waited for it to boot.
if grep -q "send-keys" "$TMUX_LOG"; then
  fail "the spawn used send-keys: $(grep send-keys "$TMUX_LOG")"
fi
[ "$ELAPSED" -lt 5 ] ||
  fail "the spawn took ${ELAPSED}s: something on the path is sleeping"

# The pane outlives the engine: the stub has exited, and the session is still
# there for the observer to see and for a ruling to be relayed into. Half a
# second of settling is what makes this an assertion rather than a race — tmux
# is not slow about destroying a session whose last pane exited.
[ ! -s "$SB/argc.txt" ] && fail "the stub never ran"
sleep 0.5
tmux has-session -t "=$SESSION_NAME" 2>/dev/null ||
  fail "the session died with the engine; the pane does not end '; exec bash'"

# The worktree, the branch, and what the worker was given to reach the log.
[ -d "$SB/alder-work-$WORK" ] || fail "worktree missing"
[ "$(git -C "$SB/repo" rev-parse --abbrev-ref "work/$WORK")" = "work/$WORK" ] ||
  fail "branch missing"
[ -x "$SB/alder-work-$WORK/.alder/bin/alder" ] || fail "the worker has no alder"
[ -f "$SB/alder-work-$WORK/.alder/config.json" ] || fail "the worker has no config"
[ -e "$SB/alder-work-$WORK/.alder/bin/alderd" ] &&
  fail "the worker was given alderd: workers cannot dispatch"

# The attempt carries the handle and the whole tier, model and effort both.
"$ALDER" show "$ATTEMPT" --json |
  jq -e '.current.handle == "tmux:alder-work-'"$WORK"'"' >/dev/null ||
  fail "handle not bound"
"$ALDER" show "$ATTEMPT" --json |
  jq -e '.current.metadata | .engine == "gpt-5.6-luna" and .effort == "high" and .tier == "luna"' \
    >/dev/null ||
  fail "the attempt does not carry model, effort and tier"

# A second spawn at a live worker is refused, and changes nothing.
if ALDER_BIN=$ALDER ALDER_WORKER_CMD="$SB/stub.sh" \
  "$ALDERD" spawn "$WORK" luna >"$SB/second.out" 2>&1; then
  fail "a second worker was spawned onto a live one"
fi
[ "$("$ALDER" status --section in_flight --json | jq '.in_flight | length')" -eq 1 ] ||
  fail "the refused second spawn left an extra attempt"

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
echo "PASS: alderd spawn delivered the goal as argv in ${ELAPSED}s, typed nothing," \
  "left a live pane behind, and stamped luna/gpt-5.6-luna/high on the attempt"
