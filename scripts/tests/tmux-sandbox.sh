# shellcheck shell=bash
# SANDBOX_TEARDOWN_STATUS is written here and read by the caller, which is
# what SC2034 cannot see.
# shellcheck disable=SC2034
#
# Sandbox tmux teardown, shared by the goal-mode verification and its own test.
#
# Sourced, never executed. Callers must set, before sourcing:
#   REAL_TMUX     absolute path to tmux, resolved before any PATH shim
#   SOCK          private socket path (must not be the default server's)
#   SOCKDIR       directory holding SOCK, removed on a clean teardown
# and must set SESSION_NAME to the one session this run owns, once it exists.
#
# Isolation comes from -S plus an unset $TMUX. Inside a tmux pane $TMUX names
# the real server and takes precedence over TMUX_TMPDIR, so TMUX_TMPDIR alone
# isolates nothing — that mistake once turned a cleanup into a machine-wide
# kill of every worker session.
SESSION_NAME=${SESSION_NAME:-}

# What the last teardown did, for a caller that cleans up around it:
#   empty    the server held nothing, so there was nothing to kill
#   killed   the one owned session was killed and the socket dir removed
#   aborted  the server held something else; nothing was killed
SANDBOX_TEARDOWN_STATUS=

sbtmux() {
  env -u TMUX -u TMUX_PANE "$REAL_TMUX" -S "$SOCK" "$@"
}

sandbox_sessions() {
  sbtmux list-sessions -F '#{session_name}' 2>/dev/null | sort || true
}

# Kill exactly one session, by exact name, on the private socket — and only
# after proving the server holds nothing else. If it does, we are not on the
# socket we think we are: kill nothing and leave it reachable for inspection.
# No kill-server, no patterns, nothing aimed at the default server, ever.
sandbox_teardown() {
  local held
  held=$(sandbox_sessions)
  if [ -z "$held" ]; then
    SANDBOX_TEARDOWN_STATUS=empty
    rm -rf "$SOCKDIR"
    return 0
  fi
  if [ "$held" != "$SESSION_NAME" ]; then
    SANDBOX_TEARDOWN_STATUS=aborted
    echo "ABORT: sandbox server at $SOCK holds sessions this run does not own" >&2
    echo "  expected: ${SESSION_NAME:-<none>}" >&2
    echo "  found:    $(echo "$held" | tr '\n' ' ')" >&2
    echo "  killing nothing; socket left at $SOCK" >&2
    return 0
  fi
  SANDBOX_TEARDOWN_STATUS=killed
  sbtmux kill-session -t "=$SESSION_NAME" 2>/dev/null || true
  rm -rf "$SOCKDIR"
}
