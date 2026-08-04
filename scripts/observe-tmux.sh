#!/usr/bin/env bash
# Per-handle liveness probe for alder's tmux runner.
#
# alder invokes the configured probe once per relevant handle with the handle
# as `$1` and reads back exactly one word:
#
#   present  the execution this handle names is running
#   absent   the handle is one of this runner's names and nothing runs under it
#   unknown  not a name this runner recognizes; alder writes nothing
#
# The handle stays opaque to alder — it is matched by equality and never
# parsed there. Recognition of `tmux:<session>` names lives here, on the
# runner's side, which is what keeps alder free of any handle grammar.
#
# A missing tmux server or tmux binary means no session can be running, so a
# recognized `tmux:*` name answers `absent` rather than failing the probe.
set -euo pipefail

case "${1-}" in
tmux:*)
  session="${1#tmux:}"
  # `=` pins the exact session name; without it tmux prefix-matches.
  if tmux has-session -t "=$session" 2>/dev/null; then
    echo present
  else
    echo absent
  fi
  ;;
*)
  echo unknown
  ;;
esac
