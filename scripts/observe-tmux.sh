#!/usr/bin/env bash
# Normalized tmux observation for alder: one JSON object per worker session.
#
# Lists only `alder-work-*` sessions — the leader's own session is a pass
# handle, not an attempt handle, and would read as noise. The attempt stamp
# is the ALDER_ATTEMPT session environment variable set by `alderd spawn`.
# Session names and attempt IDs are alder-generated slugs, so no JSON
# escaping is needed.
set -euo pipefail

valid_codex_session() {
  [[ "$1" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]]
}

codex_sessions_for() {
  local session=$1 path marker session_id codex_home sessions
  path=$(tmux display-message -p -t "$session" '#{session_path}' 2>/dev/null || true)
  marker="$path/.alder/codex-session"
  if [ -r "$marker" ]; then
    IFS= read -r session_id <"$marker" || true
    if valid_codex_session "$session_id"; then
      # A launcher sidecar saw this rollout before the worker could run a
      # consult, so its marker is the unambiguous answer.
      printf '%s\n' "$session_id"
      return 0
    fi
  fi

  # The marker is a recovery aid, not the only source of truth. An early
  # sidecar failure still leaves the Codex rollout, whose first JSON line is
  # session_meta and names the worktree cwd. The fallback only needs rollouts
  # from the last day: its purpose is to recover a fresh launch before the
  # next leader pass, while a full historic scan would make every refresh pay
  # for years of transcripts. Return every matching UUID: a caller must report
  # ambiguity, never choose a later consult by accident.
  codex_home=${CODEX_HOME:-"$HOME/.codex"}
  sessions="$codex_home/sessions"
  [ -d "$sessions" ] || return 0
  while IFS= read -r session_id; do
    if valid_codex_session "$session_id"; then
      printf '%s\n' "$session_id"
    fi
  done < <(
    # Filter the large transcript inventory with one lightweight pass first;
    # jq only has to parse session_meta records whose raw line mentions this
    # worktree. It still verifies cwd exactly below.
    find "$sessions" -type f -name '*.jsonl' -mmin -1440 -exec awk -v cwd="$path" \
      'FNR == 1 && index($0, cwd) { print }' {} + 2>/dev/null |
      jq -r --arg cwd "$path" '
      select(.type == "session_meta" and .payload.cwd == $cwd)
      | (.payload.session_id // .payload.id)
      | select(type == "string")
    ' 2>/dev/null
  )
}

codex_metadata_for() {
  local session=$1 session_ids session_id count first
  session_ids=$(codex_sessions_for "$session" | sort -u)
  [ -n "$session_ids" ] || return 0
  count=$(printf '%s\n' "$session_ids" | grep -c . || true)
  if [ "$count" -eq 1 ]; then
    printf '{"codex_session":"%s"}' "$session_ids"
    return 0
  fi

  # IDs have already passed the UUID check, so hand-built JSON is safe here.
  printf '{"codex_sessions":['
  first=1
  while IFS= read -r session_id; do
    [ -n "$session_id" ] || continue
    [ "$first" -eq 1 ] || printf ','
    first=0
    printf '"%s"' "$session_id"
  done <<<"$session_ids"
  printf ']}'
}

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
  codex_metadata=$(codex_metadata_for "$session")
  [ "$first" -eq 1 ] || printf ','
  first=0
  if [ -n "$attempt" ] && [ -n "$codex_metadata" ]; then
    printf '{"value":"%s","attempt_id":"%s","metadata":%s}' \
      "$session" "$attempt" "$codex_metadata"
  elif [ -n "$attempt" ]; then
    printf '{"value":"%s","attempt_id":"%s"}' "$session" "$attempt"
  else
    printf '{"value":"%s"}' "$session"
  fi
done < <(tmux list-sessions -F '#{session_name}' 2>/dev/null || true)
printf ']\n'
