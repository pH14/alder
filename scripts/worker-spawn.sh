#!/usr/bin/env bash
# Spawn one worker for one alder work item.
#
#   scripts/worker-spawn.sh <work-id> <attempt-id> [model]
#
# Creates a git worktree at ../alder-work-<work-id> on branch work/<work-id>,
# starts a tmux session alder-work-<work-id> running the engine there, stamps
# the session with the attempt ID, binds the attempt to the session handle,
# and injects the worker's goal.
#
# Goal mode: the injected message is the goal itself — the item's title and
# spec, its acceptance checks, and the gates — composed from the log rather
# than a pointer telling the worker to go read something. A worker that knows
# what "done" means from its first token spends no turns discovering it.
#
# Model tiers: the default claude-sonnet-5 suits narrow, well-specified items;
# pass claude-opus-5 for ordinary work and reserve claude-fable-5 for the
# genuinely hard. The tier is recorded on the attempt as engine metadata, so
# an item resent up the ladder carries its own history. Workers never push and
# never touch remotes; they commit on their branch only, and the leader merges
# after review.
#
# ALDER_BIN overrides the alder binary; ALDER_WORKER_CMD replaces the whole
# engine invocation (used by verification to spawn a stub instead of an LLM).
set -euo pipefail

usage() {
  echo "usage: $0 <work-id> <attempt-id> [model]" >&2
  exit 2
}
[ $# -ge 2 ] || usage

WORK_ID=$1
ATTEMPT_ID=$2
MODEL=${3:-claude-sonnet-5}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORKTREE=$ROOT/../alder-work-$WORK_ID
BRANCH=work/$WORK_ID
SESSION=alder-work-$WORK_ID
ALDER=${ALDER_BIN:-$ROOT/target/debug/alder}

# The gates every worker is held to. Keep in step with WORKER.md, which
# states the same list for a worker that needs it after its launch turn.
GATES='cargo fmt --check, cargo clippy --workspace --all-targets with zero warnings, cargo test --workspace green'

if tmux has-session -t "$SESSION" 2>/dev/null; then
  echo "session $SESSION already exists" >&2
  exit 1
fi
if [ -e "$WORKTREE" ]; then
  echo "worktree $WORKTREE already exists" >&2
  exit 1
fi

# Compose the goal before creating anything: an unknown item or an
# unreachable store should fail with no worktree and no session to clean up.
# Collapsing whitespace keeps the goal a single line, so no part of it reads
# as a newline when it is typed into the session.
GOAL=$("$ALDER" show "$WORK_ID" --json |
  jq -er --arg attempt "$ATTEMPT_ID" --arg gates "$GATES" '
    .current as $w
    | [
        "You are the worker for \($w.id), attempt \($attempt).",
        "Goal: \($w.title).",
        (if ($w.spec // "") == "" then empty else "Spec: \($w.spec)." end),
        (if ($w.checks | length) == 0
         then "No acceptance checks are recorded; the spec is the whole bar."
         else "Done when every check is satisfied: "
              + ([$w.checks[] | "\(.key) — \(.description)"] | join("; "))
              + "."
         end),
        "Gates: \($gates).",
        "Read WORKER.md for the protocol, then begin."
      ]
    | join(" ")
    | gsub("\\s+"; " ")
  ')

git -C "$ROOT" worktree add "$WORKTREE" -b "$BRANCH" main

# The machine-local alder config and binary are gitignored, so they do not
# travel with the checkout; the worker needs both to reach the shared log.
mkdir -p "$WORKTREE/.alder/bin"
cp "$ROOT/.alder/config.json" "$WORKTREE/.alder/config.json"
cp "$ALDER" "$WORKTREE/.alder/bin/alder"

ENGINE=${ALDER_WORKER_CMD:-claude --model $MODEL --permission-mode auto}
tmux new-session -d -s "$SESSION" -c "$WORKTREE" "caffeinate -i $ENGINE"
tmux set-environment -t "$SESSION" ALDER_ATTEMPT "$ATTEMPT_ID"

# Bind the attempt to the session so observation and reconcile connect them,
# and record the tier this attempt was launched at.
(cd "$ROOT" && "$ALDER" attempt edit "$ATTEMPT_ID" \
  --handle "tmux:$SESSION" --meta "engine=$MODEL" >/dev/null)

# Hand the worker its goal once the engine has had a moment to boot. The
# literal text and the Enter are separate sends so no part reads as a key
# name.
sleep 5
tmux send-keys -t "$SESSION" -l -- "$GOAL"
tmux send-keys -t "$SESSION" Enter

echo "spawned $SESSION on $BRANCH at $WORKTREE (model $MODEL)"
