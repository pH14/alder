#!/usr/bin/env bash
# Normalized tmux observation for alder: one current liveness level per
# worker session.
#
# Lists only `alder-work-*` sessions — the leader's own session is loop
# machinery, not an attempt handle, and would read as noise. Each subject is
# the opaque handle exactly as the runner bound it (`tmux:<session>`); alder
# matches it against attempt records by equality and never parses it. The
# script reports nothing else: no attempt stamp, no session metadata — the
# runner stores nothing of alder's, and alder reads nothing of the runner's
# beyond the handles it was given.
#
# Session names are runner-generated slugs, so no JSON escaping is needed.
set -euo pipefail

first=1
printf '['
# No tmux server means no workers, which is an empty inventory, not an error.
while IFS= read -r session; do
  case "$session" in
  alder-work-*) ;;
  *) continue ;;
  esac
  [ "$first" -eq 1 ] || printf ','
  first=0
  printf '{"subject":"tmux:%s","field":"liveness","level":"present"}' "$session"
done < <(tmux list-sessions -F '#{session_name}' 2>/dev/null || true)
printf ']\n'
