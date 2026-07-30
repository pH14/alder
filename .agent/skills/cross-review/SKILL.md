---
name: cross-review
description: Run, record, and act on the mandatory independent review before a branch is merged or presented.
---

# Cross-review

This manual is advisory craft for the `cross-review` check. The item's check
description is the binding criterion. Read this manual before satisfying or
verifying that check.

## The rule

Before a branch is merged—or, when it is staged for the operator rather than
merged, before it is presented and again before any re-presentation—it is
reviewed by an engine on the other vendor's ladder. This review is mandatory.
The leader's reading of a diff is useful, but it is not the independent review
because the leader dispatched the work.

The author is every vendor that has written on the branch, not merely the
latest attempt. `alderd spawn` can reuse a branch after a respawn, so inspect
the `engine` metadata of every attempt in `alder show <work>`. A reviewer may
not share the authoring vendor or session. The paired rungs are:

| authored at | reviewed by |
| --- | --- |
| `sonnet`, `opus`, `fable` | Codex `sol` |
| `luna` | Claude `sonnet` or higher |
| `terra` | Claude `opus` or `fable` |
| `sol` | Claude `fable` |

The standing equivalences are `luna` ↔ `sonnet`, `terra` ↔ `opus`, and
`sol` ↔ `fable`, as the counterpart column in
`crates/alderd/README.md` records. Thus the hardest Codex work is reviewed by
`fable`, not `opus`. Every engine invocation names its full model identifier
and its reasoning effort explicitly; no alias, config default, or inherited
effort is evidence of a review.

If both vendors have written on one branch, it needs one whole-diff review from
each ladder. Record both reviewers comma-joined in `reviewed-by`; the check is
satisfied only after every authoring vendor has been read by the other. That
requires two heavy operations and therefore two passes. The model has no durable
partial-verdict state between pending and satisfied, so avoid this case by
keeping a respawn on one vendor's fresh branch until product work
`al-q8qwhy` supplies that fact. When authorship cannot be attributed to a
commit, count that vendor as an author.

## The weight ladder

There is no opt-out. Once work is ready, choose one of these scheduling forms;
they determine when the review runs and which item owns it, never whether it
runs.

- **In-pass.** Run one bounded review command. It is that pass's heavy
  operation, whether or not the subsequent merge fits in the same pass.
- **Admitted verification item.** Create a bounded verification item and add
  the original item's `requires` edge in one atomic structured graph change.
  Use `alder work edit --from`, with a `$name` local reference. Never add the
  item and edit the dependency in separate commands: a crash between them
  leaves a graph that says neither what is required nor why.

For example, this document creates the review work and makes `al-x` wait for
it in one append. The verification item's spec identifies the reviewed branch
and says to record the verdict on the original authoring attempt.

```json
{
  "why": "Schedule the mandatory cross-review for al-x before it can finish",
  "add": [
    {
      "local": "review",
      "title": "Cross-review work/al-x before merge",
      "priority": 70,
      "spec": "Review work/al-x against main and record the verdict on al-x's authoring attempt."
    }
  ],
  "edit": [
    {
      "id": "al-x",
      "add_requires": ["$review"]
    }
  ]
}
```

Run it as `alder work edit --from <file>`. The graph document has an `edit`
operation, a single durable `why`, and no intermediate observable state. A
verification item schedules the review; it does not make a review of itself
recursive.

## Run the other ladder

Run the reviewer in the branch's own worktree, where `work/<id>` is checked
out. A review has two different mechanics according to the authoring vendor.
The reviewer reads `AGENTS.md` as the repository's review lens and the
relevant v0 contract document before treating something as a defect.

### Claude-authored branch: Codex review

Run a Codex `sol` review in the branch worktree:

```sh
codex review --base main --title "$(cat "$scratch/review-title-<id>.txt")" \
  -c model=gpt-5.6-sol -c model_reasoning_effort=xhigh \
  -c approval_policy=never -c sandbox_mode=workspace-write
```

Each explicit setting is deliberate:

- `--base main` makes the harness review the branch delta from its fork point.
- `--title` gives the summary the work ID and title; it is display text, not a
  prompt channel.
- `-c model=gpt-5.6-sol` pins the full review model. `codex review` rejects
  `-m`; that flag belongs to `codex exec`.
- `-c model_reasoning_effort=xhigh` records the actual review effort rather
  than a moving config default.
- `approval_policy=never` and `sandbox_mode=workspace-write` say the review
  runs unattended: nothing answers an approval request inside a review, and a
  review stopped waiting on one is indistinguishable from a slow one.

`--base`, `--uncommitted`, `--commit`, and a positional prompt are alternative
scope selectors in codex-cli 0.146.0-alpha.3.1; they do not compose. A scoped
review therefore carries **no custom instructions**. `AGENTS.md` is the entire
lens on this route, which is why it must be complete.

| form | measured result |
| --- | --- |
| `--base main "<prompt>"` | errors: `--base <BRANCH>` cannot be used with `[PROMPT]` |
| `--base main -`, prompt on stdin | the same error; `-` is still positional |
| `"<prompt>"`, no scope flag | runs, but chose `git diff HEAD^ HEAD` and missed a second change |
| `-c instructions="…<token>…"` | accepted, but the token never surfaced in the review |
| `--base main --title "<text>"` | runs |

The branch must therefore argue for itself through its diff, tests, and commit
messages. A prompt buys item context and gives up harness-computed scope, and
scope is the one thing a merge gate cannot trade: a review of the last commit
on a five-commit branch is worse than no review, because it reports clean. Do
not try to smuggle the item brief through `--title` or config.

### Codex-authored branch: fresh Claude review

Run a fresh, one-shot Claude review at the matching rung in the branch
worktree. This is a `claude` invocation, not a subagent, precisely because
a subagent inherits the leader's session effort, which is nowhere in the log;
otherwise `reviewed-by` could name a rung the review did not run at.

```sh
claude -p --model claude-fable-5 --effort xhigh --permission-mode auto \
  "$(cat "$scratch/review-prompt-<id>.txt")"
```

The prompt file, outside the worktree, contains the diff scope and the item
context. The title leads because it is the one requested-change description
that is always present; a v0 spec is optional.

```text
Review work/<id> against main: git diff main...work/<id>.
The item is <work-id> — <the item's title>.
Its spec: <spec, or 'none recorded'>. Its checks: <checks>.
AGENTS.md is the review lens. Report findings, or say the branch is clean.
```

Use the full model ID (`claude-fable-5`, not the moving `fable` alias), name
the effort, and use `--permission-mode auto` because no person is watching for
a prompt. Freshness is part of the review: a session carrying this pass's
context has already agreed with it.

## Observe a review without observing yourself

Do not wait with `until ! pgrep -f 'codex review'; do sleep n; done`. The
waiter's own command line matches that pattern, so it can wait on itself.
`pgrep -x codex` is also unsafe here because the ChatGPT application's
long-lived Codex process matches it. Capture the review process's PID and wait
on that PID (`while kill -0 <pid> 2>/dev/null; do sleep n; done`), or use a
pattern that cannot match the waiter.

A wedged review is indistinguishable from a slow review by elapsed time alone.
Watch both transcript/output growth and cumulative CPU. Ordinary repository
reviews take about 10–20 minutes; a measured `gpt-5.6-sol`/`xhigh` review ran
71 minutes, printed 6,846 lines, then sat for about 25 minutes while gaining
only 2.5 CPU seconds and never produced a verdict. If output has not grown for
25 minutes and the review has gained only a few CPU seconds, kill it, record
the abandonment, and end the pass. Do not keep a pass open waiting for a run
that cannot be distinguished from a wedge.

In a review sandbox, the real-tmux host test cannot create its private Unix
socket. A transcript failure shaped like `error: test failed … --test
host_tmux` is expected in that environment, not a finding about the branch.

## Record the verdict before feedback

Every verdict is durable on the authoring attempt before a word reaches the
author. Reviewer output is branch-controlled text: write the summary and
findings to a file outside the worktree, then pass that file to `attempt edit`
and to any relay. Do not inline it into a shell command.

Evidence for every round names the reviewer engine, both reviewed revisions,
the reviewer effort, the verdict, the finding count, and the findings
themselves. Each finding is one line with severity, `file:line`, and its claim;
the worst finding level is therefore explicit and auditable. Include the
review transcript pointer as well—the Codex rollout UUID or the fresh Claude
review/session identifier. A count is not a finding: a later reader needs the
actionable list, not merely proof that a list once existed.

For example:

```text
gpt-5.6-sol via codex review at 901445f (merge base 6eab0cd), effort xhigh;
rollout <codex-rollout-uuid>: changes requested, 4 findings (3 P1).
  P1 PASS.md:138 the invocation combines non-composable scope and prompt selectors.
  P1 PASS.md:165 the Claude prompt omits the item's only always-present description.
  P1 PASS.md:192 only the finding count is persisted before relay.
  P2 PASS.md:240 the legacy path stores a verdict in an overwritten note.
```

Record a non-clean review before relaying it:

```sh
alder attempt edit <attempt> --failed cross-review \
  --evidence "$(cat "$scratch/cross-review-<id>.txt")" \
  --meta reviewed-by=gpt-5.6-sol \
  --meta reviewed-sha="$(git rev-parse work/<id>)" \
  --meta reviewed-base="$(git merge-base main work/<id>)" \
  --meta reviewed-effort=xhigh
```

A clean round is the same command with `--satisfied` and one-line evidence.
The verdict belongs in the check status, not duplicated in metadata. The four
metadata keys—`reviewed-by`, `reviewed-sha`, `reviewed-base`, and
`reviewed-effort`—record facts the status cannot: who reviewed, how hard, and
the exact two endpoints. Rounds are successive reviews of fresh endpoints;
their later evidence and metadata supersede the earlier round rather than
pretending an earlier green check covers new code.

`reviewed-sha` is the branch head and `reviewed-base` is its merge base, not
main's current tip:

```sh
git rev-parse work/<id>
git merge-base main work/<id>
```

The reviewer reads `git diff main...work/<id>`, so the merge base identifies
the start of that three-dot delta. At merge both comparisons must still hold:
`reviewed-sha` equals `git rev-parse work/<id>` **and** `reviewed-base` equals
`git merge-base main work/<id>`. A new author commit moves the head; a rebase
moves the base. Either makes the review stale and requires another round.
Main advancing alone moves neither endpoint. It can still make the merge
result incompatible, which is why gates run again on the local merge result.
This rule knowingly does not require a second review of that result.

## Check declaration and legacy items

Declare `cross-review` at admission, with the other checks:

```sh
alder work add --handoff <handoff> --priority <n> \
  --check "$(cat "$scratch/check-<key>.txt")" \
  --check cross-review:"reviewed by the other vendor's ladder; the leader records this one, not the worker"
```

`work add` takes `--check`; `--add-check` belongs to `work edit`. A check or
dependency cannot be changed while an attempt is active, so never end a live
attempt merely to widen its contract. The leader records this check; a worker
stops after its own checks and says `ready for review`, rather than waiting on
the independent reviewer.

An item admitted before this rule has no `cross-review` check, and no check
result can be appended to it. Review it anyway. Put the durable result in
metadata—not its single-valued note, which a worker milestone overwrites:

```sh
alder attempt edit <attempt> \
  --meta reviewed-by=gpt-5.6-sol \
  --meta reviewed-sha="$(git rev-parse work/<id>)" \
  --meta reviewed-base="$(git merge-base main work/<id>)" \
  --meta reviewed-effort=xhigh \
  --meta cross-review=failed \
  --meta cross-review-findings="$(cat "$scratch/cross-review-<id>.txt")"
```

For a legacy item, `cross-review=<verdict>` and its findings are the durable
substitute for the absent check. Metadata merges while a later note replaces
the old one. The leader must still hold merge on the recorded verdict even
though `work finish` cannot enforce a check that did not exist at admission.

## Findings and feedback rounds

Record findings first, then relay them. A crash after recording and before
delivery is repairable: the next leader reads and relays the durable evidence.
A relay with no preceding record is a review that disappears at rotation.

The default fix path is a **fresh worker**: start a new attempt on the same
item and branch. Before changing code, the new worker reads the prior attempts'
durable review evidence and notes with `alder show <item>`; a launch goal alone
contains title, spec, checks, and gates. Choose the tier by the size of the
findings, not the original item; narrow enumerated findings are down-tier work,
so `luna` or `sonnet` can fix what `sol` found in `terra`'s change.

Do not compact or preserve the original author session for a feedback round.
Compaction pays a full-context read at author tier and preserves laundered
memory. The spec, checks, branch, commit messages, findings, and rulings must
let a fresh worker proceed; if they do not, repair the durable record rather
than the process. The narrow exception is an original session that is still
alive and has only trivial findings, where a quoted `send-keys` nudge is
cheaper than a spawn. Decisions worth keeping must therefore be committed:
explain them in commit messages and attempt notes, and put binding spec rulings
in `work edit`, so a fresh worker does not re-litigate them.

A real defect returns as precise feedback. A disagreement that needs authority
becomes `alder work ask <id>` with options and a recommendation. Until a later
fresh review satisfies the check, the branch cannot merge. There is no
override: a finding is fixed, ruled on, or argued down on the record.

## Known limits

These are deliberately named rather than hidden by prose. They require a new
durable fact or a change to the worker brief and are tracked as `al-q8qwhy`.

- **Feedback delivery has no durable receipt.** A verdict is durable before
  relay, but crash-before-send and send-completed look the same in the log.
  Pane observation remains a limited workaround.
- **Spawn does not itself carry a prior attempt's findings.** The fresh-worker
  default works only because the findings are durable check evidence and
  attempt notes that the new worker must read; a launch goal alone contains
  only title, spec, and check definitions.
- **Review intent is not recorded before launch.** A crash during either
  charged review leaves no durable target or in-progress review to adopt, so a
  later pass may have to pay again.
- **A two-vendor partial verdict has nowhere durable to live.** One review
  cannot safely satisfy the whole check, and a pending check records no first
  verdict. This needs per-reviewer state, not a prose workaround.

Until that work lands, relay carefully, ensure a fresh worker reads the
recorded findings, expect a crashed review to be paid twice, and keep a branch
to one vendor where practical.
