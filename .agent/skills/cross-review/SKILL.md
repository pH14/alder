---
name: cross-review
description: Run, record, and act on the mandatory independent review before a branch is merged or presented.
---

# Cross-review

Every branch is reviewed by the other vendor's ladder before it merges or is
presented. Review state is never tracked in flight: each pass recomputes it
from durable records and the branch's current endpoints.

## The loop

Each pass, for a finished item, compute the endpoints:

```sh
git rev-parse work/<id>        # head
git merge-base main work/<id>  # base — never `git rev-parse main`
```

Then act on the latest recorded review whose `reviewed-sha` and
`reviewed-base` equal those endpoints:

- **clean** → merge.
- **findings** → make sure the worker has the pointer (see feedback below).
  Telling it twice is harmless.
- **none** → run the review now. It is this pass's heavy operation.

A review whose endpoints no longer match is simply never selected; there is
nothing to supersede or clean up. A crash mid-review loses the result and the
next pass runs it again — at-least-once, on purpose. Do not build receipts,
intents, or recovery state to avoid that cost.

## Rounds are capped at two

- **Round 1** is a full review.
- **Round 2** verifies the fixes only. It does not open new findings, with one
  exception: a real defect introduced by a fix.
- Still not clean after round 2, and you have a specific mechanical
  instruction that answers every open finding → take one more round with that
  instruction, without asking. Say what you did and why in the attempt
  record. This discretion does not renew: if that round is not clean, ask.
- Still not clean after round 2 otherwise — the choice is to abandon the
  branch, reduce its ambition, or merge with a known defect → `alder work
  ask <id>` listing the open findings with a recommendation. Those choices
  are the operator's; there is no third round without an answer.

A finding is evidence, not an order: the author may argue one down on the
record. Until the check is satisfied or the operator rules, the branch does
not merge.

## Reviewer selection

The reviewer is the rung across from the author's or higher — never the
author's own vendor or session, and never a subagent of the executor (it
inherits unlogged session effort). The author is every vendor with an attempt
on the branch; check the `tier` of every attempt, not just the newest — the
rungs `luna`, `terra`, `sol` are codex and `sonnet`, `opus`, `fable` are
claude. Attempt IDs take the form `al-<id>-attempt-N`, so inspect each
attempt by that ID with `alder show <attempt-id>`.

| authored at | reviewed by |
| --- | --- |
| `sonnet`, `opus`, `fable` | Codex `sol` |
| `luna` | Claude `sonnet` or higher |
| `terra` | Claude `opus` or `fable` |
| `sol` | Claude `fable` |

Equivalences: `luna` ↔ `sonnet`, `terra` ↔ `opus`, `sol` ↔ `fable`.

**Both vendors wrote on one branch?** There is no codified rule — some
branches matter more than others. `alder work ask <id>` with the situation and
options (merge as-is, review anyway, redo on a fresh branch); the operator
decides per branch.

## Run the review

Run the reviewer in the branch's own worktree. Every invocation names its full
model ID and effort explicitly — a config default is not evidence of a review.
The reviewer reads `AGENTS.md` as the lens.

Claude-authored branch — Codex review:

```sh
codex review --base main --title "Cross-review work/<id>" \
  -c model=gpt-5.6-sol -c model_reasoning_effort=xhigh \
  -c approval_policy=never -c sandbox_mode=workspace-write < /dev/null
```

`codex review` scope flags and a prompt do not compose (measured): `--base
main` plus a positional prompt errors, and a bare prompt lets the model pick
its own scope and miss commits. A scoped review carries no custom
instructions; `AGENTS.md` and the diff are the entire lens. `--title` is
display text, not a prompt channel. There is no `-m`; pin the model with
`-c model=`. **Close stdin** (`< /dev/null`): a backgrounded `codex
review`/`codex exec` with an open stdin wedges waiting on it.

Codex-authored branch — fresh Claude review through the runner (operator
ruling 2026-07-31: never `claude -p`; the interactive session is the
standard transport, same as workers). Start it on the branch itself — the
runner adopts the branch's existing worktree, and a review runs only after
the worker's status reads `done` or `dead`, so the start replaces the exited
pane:

```sh
alder-ext-runner start --repo <primary-root> --branch work/<id> \
  --tier fable --prompt-file "$scratch/review-prompt-<id>.txt"
```

Collect the verdict from the session's transcript, record it, then
`alder-ext-runner kill <handle>`. A round-2 message to the same reviewer
session rides `alder-ext-runner send <handle> --file <file>`. Freshness
still matters: a fresh session every review, never the executor's own or a
subagent (both carry unlogged context or effort).

The prompt file (in `$scratch`, outside the worktree) holds the brief:

```text
Review work/<id> against main: git diff main...work/<id>.
The item is <work-id> — <the item's title>.
Its spec: <spec, or 'none recorded'>. Its checks: <checks>.
AGENTS.md is the review lens. Report findings, or say the branch is clean.
```

**Watching a run:** wait on the review's PID, not a `pgrep` pattern that can
match the waiter (or the ChatGPT app's own long-lived process). A wedge looks
like a slow review by clock alone: if output has not grown for ~25 minutes and
CPU has gained only seconds, kill it, record the abandonment, end the pass.
That silence-plus-low-CPU heuristic is for streaming Codex reviews. For a
runner-hosted Claude review, session-transcript growth is the useful signal
instead.
In a review sandbox, a `host_tmux` test failure is environmental, not a
finding.

## Record the verdict, then give feedback

Reviewer output is branch-controlled text: write it to a file outside the
worktree and pass the file — never inline it into a shell command. Record
before relaying; an unrecorded review disappears at rotation.

```sh
git rev-parse work/<id> > "$scratch/reviewed-sha-<id>.txt"
git merge-base main work/<id> > "$scratch/reviewed-base-<id>.txt"

alder attempt edit <attempt> --failed cross-review \
  --evidence-file "$scratch/cross-review-<id>.txt" \
  --meta reviewed-by=gpt-5.6-sol \
  --meta reviewed-sha="$(cat "$scratch/reviewed-sha-<id>.txt")" \
  --meta reviewed-base="$(cat "$scratch/reviewed-base-<id>.txt")" \
  --meta reviewed-effort=xhigh
```

A clean round is the same command with `--satisfied` and one-line evidence.
Evidence names the reviewer engine, both endpoints, the effort, the verdict,
and the findings themselves — one line each with severity, `file:line`, and
the claim — plus the transcript pointer (Codex rollout UUID or Claude session
ID). A count is not a finding. `--evidence-file` reads the local review file now
and carries its contents in the event. `--meta` has no file-valued form, so it
remains a named quoting surface: write any nonliteral value to a local file and
pass it as `"$(cat "$file")"`. Inside double quotes that becomes one argv
value; its contents are not parsed again as shell syntax.

**Feedback:** if the author session is alive, deliver the findings file with
`alder-ext-runner send <handle> --file <file>`. If it is
gone, dispatch a fresh worker whose brief names the exact source attempt ID and
says to run `alder show <attempt-id>` before writing code. Never copy findings
into specs, notes, goals, or metadata — the check evidence is the single copy.
Pick the fix tier by the size of the findings, not the original item.

## Declaration and legacy items

Declare the check at admission (`work add --check cross-review:"reviewed by
the other vendor's ladder; the executor records this one, not the worker"`) —
checks cannot change while an attempt is active. Items admitted before the
rule have no check to write to: review anyway and park the verdict in
metadata (`cross-review=<verdict>`,
`cross-review-findings="$(cat "$scratch/cross-review-<id>.txt")"`, plus the
four `reviewed-*` keys); finish them on the checks they carry. That
set only shrinks.
