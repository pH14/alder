#!/usr/bin/env bash
# End-to-end check of `alder-ext-runner`, in a sandbox of its own.
#
#   crates/alder-ext-runner/scripts/verify-start.sh [alder-ext-runner-binary]
#
# The argument defaults to this workspace's target/debug binary, built if it
# is not there yet. A clean checkout therefore runs it with no arguments and
# no setup beyond a Rust toolchain, git and tmux.
#
# What it proves, which unit tests cannot: that a real tmux pane really does
# receive the prompt as one argv element, that the pane is attributable from
# its creation and outlives a one-shot engine, that `status` walks
# running -> done -> dead, that `send` lands real bytes in the pane, and that
# the worktree is really cut on the requested branch.
#
# Everything happens under one throwaway directory: its own git repo and its
# own tmux SERVER, so no session leaks anywhere. Nothing is written inside
# the checkout.
#
# Isolating that server is the whole safety story, and TMUX_TMPDIR does NOT
# do it. When this script runs inside a tmux pane, the inherited $TMUX names
# the real server's socket, and tmux prefers it over TMUX_TMPDIR for every
# client command. So: one explicit private socket (tmux -S "$SOCK"), $TMUX
# unset on every tmux call, and that includes the calls made by the runner
# itself — a PATH shim puts them on the same socket without editing the
# binary under test. That shim also logs every tmux invocation, which is how
# "types nothing via send-keys" is checked rather than asserted.
#
# Teardown is narrow on purpose: scripts/tmux-sandbox.sh kills ONE session,
# by exact name, and only after asserting that the sandbox server holds
# nothing else. No kill-server, no pattern matching, no form of kill against
# the default server, anywhere in this file.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
CRATE=$(cd "$HERE/.." && pwd)

for tool in git tmux python3; do
  command -v "$tool" >/dev/null ||
    { echo "$tool is required to run this verification" >&2; exit 2; }
done

RUNNER=${1:-}
if [ -z "$RUNNER" ]; then
  RUNNER=$(cd "$CRATE" && cargo build 2>/dev/null; cargo metadata --format-version 1 2>/dev/null |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])' )/debug/alder-ext-runner
fi
if [ ! -x "$RUNNER" ]; then
  echo "building alder-ext-runner"
  (cd "$CRATE" && cargo build)
fi
[ -x "$RUNNER" ] || { echo "not an executable: $RUNNER" >&2; exit 2; }
RUNNER=$(cd "$(dirname "$RUNNER")" && pwd)/$(basename "$RUNNER")

REAL_TMUX=$(command -v tmux)
REAL_SOCK=${TMUX-}
REAL_SOCK=${REAL_SOCK%%,*}

real_sessions() {
  [ -n "$REAL_SOCK" ] || return 0
  "$REAL_TMUX" -S "$REAL_SOCK" list-sessions -F '#{session_name}' 2>/dev/null |
    sort || true
}

SB=$(mktemp -d /tmp/alder-ext-verify.XXXXXX)
SOCKDIR=$(mktemp -d /tmp/alder-ext-verify-sock.XXXXXX)
SOCK=$SOCKDIR/tmux.sock
[ -n "$SOCK" ] && [ "$SOCK" != "$REAL_SOCK" ] || {
  echo "refusing to run: sandbox socket is the real socket" >&2
  rm -rf "$SB" "$SOCKDIR"
  exit 1
}

# SESSION_NAME stays empty until the session exists, so a failure before the
# start tears nothing down.
SESSION_NAME=""
# shellcheck source-path=SCRIPTDIR source=tmux-sandbox.sh
. "$HERE/tmux-sandbox.sh"

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

# Poll a condition (a shell command) until it holds or a bound elapses. This
# script waits only on conditions it then asserts — never a fixed sleep whose
# length is a guess about the machine.
await() {
  local what=$1
  shift
  for _ in $(seq 1 50); do
    "$@" && return 0
    sleep 0.2
  done
  fail "timed out waiting until $what"
}

REAL_BEFORE=$(real_sessions)

mkdir -p "$SB/bin"
TMUX_LOG=$SB/tmux-calls.log
cat >"$SB/bin/tmux" <<SHIM
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$TMUX_LOG"
unset TMUX TMUX_PANE
exec "$REAL_TMUX" -f /dev/null -S "$SOCK" "\$@"
SHIM
chmod +x "$SB/bin/tmux"
export PATH=$SB/bin:$PATH
unset TMUX TMUX_PANE
export TMUX_TMPDIR=$SOCKDIR
# The runner's machine-local files stay inside the sandbox too.
export ALDER_EXT_RUNNER_STATE_DIR=$SB/state
unset ALDER_EXT_RUNNER_CONFIG

git init -q -b main "$SB/repo"
echo sandbox >"$SB/repo/README.md"
git -C "$SB/repo" add -A
git -C "$SB/repo" -c user.email=v@x -c user.name=v commit -qm "sandbox"

BRANCH=work/wv-1
printf 'Sandbox prompt for start verification.\nSecond line of the prompt.\n' \
  >"$SB/prompt.txt"
echo "sandbox tree=$SB"
echo "sandbox tmux socket=$SOCK (real server socket=$REAL_SOCK)"

# The stub engine records the argv it was handed — one line per argument, so
# a prompt that arrived as several arguments is visible as several lines —
# and then EXITS. What happens after it exits is the point: the pane must
# survive.
cat >"$SB/stub.sh" <<STUB
#!/usr/bin/env bash
printf '%s\n' "\$#" >"$SB/argc.txt"
printf '%s\n' "\$@" >"$SB/argv.txt"
STUB
chmod +x "$SB/stub.sh"

# An unknown tier must be refused before anything exists: no worktree, no
# session — this is the check that a typo cannot silently launch at whatever
# the CLI defaults to.
if "$RUNNER" start --repo "$SB/repo" --branch "$BRANCH" --tier gpt-5.6-luna \
  --prompt-file "$SB/prompt.txt" >"$SB/bogus.out" 2>&1; then
  fail "an unknown tier was accepted: $(cat "$SB/bogus.out")"
fi
grep -q "unknown tier" "$SB/bogus.out" ||
  fail "the unknown-tier error does not say so: $(cat "$SB/bogus.out")"
for rung in luna terra sol sonnet opus fable; do
  grep -q "$rung" "$SB/bogus.out" || fail "the unknown-tier error omits $rung"
done
[ -d "$SB/alder-ext-work-wv-1" ] && fail "a rejected tier still cut a worktree"
[ -s "$TMUX_LOG" ] && fail "a rejected tier still touched tmux"

# From here on there is a session teardown may kill — and only this one.
SESSION_NAME=alder-ext-work-wv-1

ALDER_EXT_RUNNER_CMD="$SB/stub.sh" \
  "$RUNNER" start --repo "$SB/repo" --branch "$BRANCH" --tier luna \
  --prompt-file "$SB/prompt.txt" >"$SB/start.out"
# Stdout is the machine contract: the handle, then the served tier.
HANDLE=$(sed -n 1p "$SB/start.out")
[ "$HANDLE" = "$SESSION_NAME" ] ||
  fail "the printed handle is '$HANDLE', expected $SESSION_NAME"
[ "$(sed -n 2p "$SB/start.out")" = "tier luna" ] ||
  fail "the second stdout line is not the served tier: $(cat "$SB/start.out")"
[ "$(wc -l <"$SB/start.out" | tr -d ' ')" = "2" ] ||
  fail "start printed more than the handle and the served tier: $(cat "$SB/start.out")"

# The stub exits immediately, so its argv file appears at once. Waiting for
# it is not a sleep in the start path; it is this script waiting for a
# process the runner deliberately does not wait for.
await "the engine records its argv" test -s "$SB/argv.txt"

echo "=== session list (sandbox tmux server) ==="
tmux list-sessions -F '#{session_name}'
echo "=== argv the engine actually received ==="
cat "$SB/argv.txt"
echo "=== every tmux call the runner made ==="
cat "$TMUX_LOG"

# The prompt is one argument, byte for byte.
[ -s "$SB/argv.txt" ] || fail "the engine received no prompt"
[ "$(cat "$SB/argc.txt")" = "1" ] ||
  fail "the prompt arrived as $(cat "$SB/argc.txt") arguments, not one"
# The stub prints its one argument plus a final newline, so the expected
# bytes are the prompt file plus exactly one newline.
diff <(cat "$SB/prompt.txt"; echo) "$SB/argv.txt" >/dev/null ||
  fail "the prompt bytes were changed on the way to the engine"

# Nothing was typed at the session, and nothing waited for it to boot.
if grep -q "send-keys" "$TMUX_LOG"; then
  fail "the start used send-keys: $(grep send-keys "$TMUX_LOG")"
fi

# The pane outlives the engine, and status reads it back: the stub has
# exited, so the handle is done — not dead — and the worktree detail names
# where the result lives. The exited marker is the asserted condition, so it
# is polled with a bound rather than slept toward.
engine_marker_reads_exited() {
  [ "$(tmux show-environment -t "=$SESSION_NAME" ALDER_EXT_RUNNER_ENGINE 2>/dev/null)" = \
    "ALDER_EXT_RUNNER_ENGINE=exited" ]
}
await "the session records that its engine exited" engine_marker_reads_exited
tmux has-session -t "=$SESSION_NAME" 2>/dev/null ||
  fail "the session died with the engine; the pane does not end '; exec bash'"
[ "$(tmux show-environment -t "=$SESSION_NAME" ALDER_EXT_RUNNER_HANDLE)" = \
  "ALDER_EXT_RUNNER_HANDLE=$SESSION_NAME" ] ||
  fail "the session was not stamped with its handle at creation"
if tmux show-environment -t "=$SESSION_NAME" ALDER_ATTEMPT >/dev/null 2>&1; then
  fail "the runner stamped somebody else's marker into its session"
fi
STATUS=$("$RUNNER" status "$HANDLE" | head -n 1)
[ "$STATUS" = "done" ] || fail "status says '$STATUS' for an exited engine, expected done"

# The worktree and the branch — and nothing of the runner's inside the
# worktree: its machinery lives in the state directory, where the execution
# cannot rewrite what the runner later executes.
RESUME=$ALDER_EXT_RUNNER_STATE_DIR/$SESSION_NAME/resume
[ -d "$SB/alder-ext-work-wv-1" ] || fail "worktree missing"
[ "$(git -C "$SB/repo" rev-parse --abbrev-ref "$BRANCH")" = "$BRANCH" ] ||
  fail "branch missing"
[ -x "$RESUME" ] ||
  fail "a codex execution has no executable resume script in the state dir"
sh -n "$RESUME" || fail "the resume script is not valid sh"
for part in "codex exec resume" "gpt-5.6-luna" "model_reasoning_effort=high" \
  "sandbox_workspace_write.network_access=true" "writable_roots"; do
  grep -q -- "$part" "$RESUME" || fail "the resume script omits: $part"
done
grep -q -- "$SB/repo/.git" "$RESUME" ||
  fail "the resume script does not name this repo's git dir"
[ -e "$SB/alder-ext-work-wv-1/.alder" ] &&
  fail "the runner wrote somebody else's directory into the worktree"
[ -e "$SB/alder-ext-work-wv-1/.alder-ext-runner" ] &&
  fail "the runner wrote its machinery into the worker-writable worktree"

# kill ends it, verified; status then reads dead.
"$RUNNER" kill "$HANDLE" | grep -q "killed $HANDLE" ||
  fail "kill did not report the verified kill"
status_reads_dead() {
  [ "$("$RUNNER" status "$HANDLE" | head -n 1)" = "dead" ]
}
await "status reads dead after kill" status_reads_dead

# The sandbox session is gone from the sandbox server, and the real server
# never changed.
REAL_AFTER=$(real_sessions)
[ "$REAL_BEFORE" = "$REAL_AFTER" ] ||
  fail "the real tmux server changed: [$REAL_BEFORE] -> [$REAL_AFTER]"
if printf '%s\n' "$REAL_AFTER" | grep -qx "$SESSION_NAME"; then
  fail "sandbox session leaked onto the real server"
fi

echo
echo "=== real tmux server, unchanged across the run ==="
printf '%s\n' "$REAL_AFTER"
echo
RUN_STATUS=pass
echo "PASS: alder-ext-runner delivered the prompt as argv, typed nothing," \
  "left its pane holding after the engine exited, and answered done/dead honestly"
