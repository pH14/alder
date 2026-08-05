#!/usr/bin/env bash
# Per-handle liveness probe for executions launched through alder-ext-runner.
#
# alder invokes the configured probe once per relevant handle with the handle
# as `$1` and reads back exactly one word:
#
#   present  the execution this handle names is running
#   absent   the handle is one of this runner's names and nothing runs under it
#   unknown  not a name this probe recognizes; alder writes nothing
#
# The runner's own status words map so:
#
#   running -> present
#   done    -> absent    a finished execution is no longer a live execution;
#                        the result lives on the branch, and an attempt still
#                        claiming the handle is reconcile's to judge, not a
#                        live reading
#   dead    -> absent
#
# The handle stays opaque to alder — it is matched by equality and never
# parsed there. Recognition of the runner's `alder-ext-*` names lives here,
# on the runner's side, which is what keeps alder free of any handle grammar.
# A recognized name that cannot be asked about (no runner binary, a failing
# status) fails the probe loudly rather than guessing: alder retries and the
# sweep fails whole, which is the honest answer for a broken machine.
set -euo pipefail

case "${1-}" in
alder-ext-*) ;;
*)
  echo unknown
  exit 0
  ;;
esac

runner=${ALDER_EXT_RUNNER_BIN:-}
if [ -z "$runner" ]; then
  script_dir=$(cd "$(dirname "$0")" && pwd -P)
  root=$(dirname "$(git -C "$script_dir" rev-parse --path-format=absolute --git-common-dir)")
  for candidate in "$root/target/release/alder-ext-runner" "$root/target/debug/alder-ext-runner"; do
    if [ -x "$candidate" ]; then
      runner=$candidate
      break
    fi
  done
fi
runner=${runner:-alder-ext-runner}
if ! command -v "$runner" >/dev/null 2>&1; then
  echo "observe-runner: alder-ext-runner not found; set ALDER_EXT_RUNNER_BIN" >&2
  exit 1
fi

if ! status=$("$runner" status "$1"); then
  echo "observe-runner: status failed for \`$1\`" >&2
  exit 1
fi
case "$(printf '%s\n' "$status" | head -n 1)" in
running)
  echo present
  ;;
done | dead)
  echo absent
  ;;
*)
  echo "observe-runner: unrecognized status for \`$1\`: $status" >&2
  exit 1
  ;;
esac
