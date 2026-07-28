#!/usr/bin/env bash
# Normalized tmux observation for alder: one JSON object per worker session.
#
# Lists only `alder-work-*` sessions — the leader's own session is a pass
# handle, not an attempt handle, and would read as noise. The attempt stamp
# is the ALDER_ATTEMPT session environment variable set by worker-spawn.sh.
# Session names and attempt IDs are alder-generated slugs, so no JSON
# escaping is needed.
set -euo pipefail

first=1
printf '['
# No tmux server means no workers, which is an empty inventory, not an error.
while IFS= read -r session; do
  case "$session" in
  alder-work-*) ;;
  *) continue ;;
  esac
  attempt=$(tmux show-environment -t "$session" ALDER_ATTEMPT 2>/dev/null |
    { grep '^ALDER_ATTEMPT=' || true; } | cut -d= -f2-)
  [ "$first" -eq 1 ] || printf ','
  first=0
  if [ -n "$attempt" ]; then
    printf '{"value":"%s","attempt_id":"%s"}' "$session" "$attempt"
  else
    printf '{"value":"%s"}' "$session"
  fi
done < <(tmux list-sessions -F '#{session_name}' 2>/dev/null || true)
printf ']\n'
