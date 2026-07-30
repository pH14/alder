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
The reviewer is the rung across from the author's or higher: never the
author's own vendor or session. A reviewer that shares the author's blind
spots returns a rubber stamp with a receipt on it. The leader's reading of a
diff is useful, but it is not the independent review because the leader
dispatched the work.

The author is every vendor that has written on the branch, not merely the
latest attempt. `alderd spawn` can reuse a branch after a respawn, so inspect
the `engine` metadata of every attempt, not just the newest. A reviewer may not
share the authoring vendor or session. The paired rungs are:

| authored at | reviewed by |
| --- | --- |
| `sonnet`, `opus`, `fable` | Codex `sol` |
| `luna` | Claude `sonnet` or higher |
| `terra` | Claude `opus` or `fable` |
| `sol` | Claude `fable` |

The standing equivalences are `luna` ↔ `sonnet`, `terra` ↔ `opus`, and
`sol` ↔ `fable`, as the counterpart column in
`crates/alderd/README.md` records. Thus the hardest Codex work is reviewed by
`fable`, not `opus`; reviewing it one rung down is exactly what “across from
the author or higher” rules out. Every engine invocation names its full model
identifier and its reasoning effort explicitly; no alias, config default, or
inherited effort is evidence of a review.

If both vendors have written on one branch, it needs one whole-diff review from
each ladder: no single review can be outside the authoring vendor for every
commit. Record both reviewers comma-joined in `reviewed-by`
(`--meta reviewed-by=gpt-5.6-sol,claude-fable-5`); the check is satisfied only
after every authoring vendor has been read by the other. That requires two heavy
operations and therefore two passes.

Between those reviews, the first verdict has nowhere durable to live:
satisfying the check after one greens a gate over a half-read diff, while
withholding it leaves a pending check with no evidence. That needs a durable
fact Alder does not yet have, tracked as `al-q8qwhy`; do not invent one here.
Until it lands, avoid this case by cutting a fresh branch when a respawn
switches provider rather than reviewing half a diff or holding a verdict in
memory. The log does not record which attempt wrote which commit, so when
authorship cannot be attributed, count that vendor as an author.

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
out. The two mechanics are not symmetric; treating them as one shape is how the
weaker one gets run wrong. The reviewer reads `AGENTS.md` as the repository's
review lens and the relevant v0 contract document before treating something as
a defect.

### Claude-authored branch: Codex review

Run a Codex `sol` review in the branch worktree:

```sh
codex review --base main --title "Cross-review work/<id>" \
  -c model=gpt-5.6-sol -c model_reasoning_effort=xhigh \
  -c approval_policy=never -c sandbox_mode=workspace-write
```

Each explicit setting is deliberate:

- None is defaulted anywhere worth trusting. The model and effort say what
  reviewed the branch; omitting either runs whatever
  `~/.codex/config.toml` said that week while the log still calls it `sol`.
- `--base main` makes the harness review the branch delta from its fork point.
- `--title` gives the summary a leader-authored work ID; it is display text,
  not a prompt channel. Do not put an item title or other stored text in it:
  this client has no ratified file-valued title argument.
- `-c model=gpt-5.6-sol` pins the full review model. `codex review` has no
  `-m`; that flag belongs to `codex exec` and review rejects it with
  `error: unexpected argument '-m' found`.
- `-c model_reasoning_effort=xhigh` records the actual review effort rather
  than a moving config default.
- `approval_policy=never` and `sandbox_mode=workspace-write` say the review
  runs unattended: nothing answers an approval request inside a review, and a
  review stopped waiting on one is indistinguishable from a slow one.

The item cannot be handed to this reviewer. `--base`, `--uncommitted`,
`--commit`, and a positional prompt are alternative scope selectors in
codex-cli 0.146.0-alpha.3.1; they do not compose. A scoped review therefore
carries **no custom instructions**. `AGENTS.md` is the entire lens on this
route: `codex review` reads it unprompted, and there is no second channel.
This was measured in a throwaway repository built so the candidate scopes could
be told apart.

| form | measured result |
| --- | --- |
| `--base main "<prompt>"` | errors: `--base <BRANCH>` cannot be used with `[PROMPT]` |
| `--base main -`, prompt on stdin | the same error; `-` is still positional |
| `"<prompt>"`, no scope flag | runs, but the model chose its scope: `git diff HEAD^ HEAD`, one commit, and missed a second change |
| `-c instructions="…<token>…"` | accepted by config, but the token never surfaced, so nothing measured says it arrives |
| `--base main --title "<text>"` | runs |

The branch must therefore argue for itself through its diff, tests, and commit
messages—which is already why commits here explain why rather than what. A
prompt buys item context and gives up harness-computed scope, and scope is the
one thing a merge gate cannot trade: a review of the last commit on a
five-commit branch is worse than no review, because it reports clean. `--title`
names the work in the summary; it is display text, not a way to smuggle the
brief through. Do not try to use config as a second channel either.

### Codex-authored branch: fresh Claude review

Run a fresh, one-shot Claude review at the matching rung in the branch
worktree. This is a `claude` invocation, not a subagent, precisely because
a subagent inherits the leader's session effort, which is nowhere in the log;
otherwise `reviewed-by` could name a rung the review did not run at.

Claude's one-shot `-p` currently takes its prompt as argv; it has no ratified
file-valued input. Do not turn a local review-prompt file into a command
substitution here. This route is an explicitly remaining surface in PASS.md:
it needs a ratified external-client adapter before it can carry item-controlled
brief text. `$scratch` may still hold the proposed brief outside the worktree,
but that fact does not make an argv interpolation safe. The title leads in that
brief because it is the one requested-change description that is always present:
a spec is optional in v0 (`docs/v0/MODEL.md`), and `Brief::goal` falls back to
the title for exactly that reason. A reviewer given an empty spec and check
keys can judge code, but not whether it is the requested change.

```text
Review work/<id> against main: git diff main...work/<id>.
The item is <work-id> — <the item's title>.
Its spec: <spec, or 'none recorded'>. Its checks: <checks>.
AGENTS.md is the review lens. Report findings, or say the branch is clean.
```

Use the full model ID (`claude-fable-5`, not the moving `fable` alias) and
name the effort for the same reason the Codex path does. `--permission-mode
auto` is the counterpart of `approval_policy=never`: nobody is watching for
a prompt. Freshness matters as much as the rung: a session carrying this pass's
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

That heuristic is for the streaming Codex route. A `claude -p` review buffers
its output until exit and can sit near zero CPU while thinking, so silence plus
low CPU is not a wedge signal there. On that route, watch the session
transcript's growth instead.

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
actionable list, not merely proof that a list once existed. The full transcript
is not the record; the short actionable list is.

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
  --evidence-file <local-findings-file> \
  --meta reviewed-by=gpt-5.6-sol \
  --meta reviewed-sha=<reviewed-sha> \
  --meta reviewed-base=<reviewed-base> \
  --meta reviewed-effort=xhigh
```

A clean round is the same command with `--satisfied` and one-line evidence.
The verdict belongs in the check status, not duplicated in metadata. The four
metadata keys—`reviewed-by`, `reviewed-sha`, `reviewed-base`, and
`reviewed-effort`—record facts the status cannot: who reviewed, how hard, and
the exact two endpoints. Rounds are successive reviews of fresh endpoints;
their later evidence and metadata supersede the earlier round rather than
pretending an earlier green check covers new code.

`--evidence-file` reads the local file now and carries its contents in the
event; delete the file and the review record remains whole. `--meta` has no
file-valued form. Obtain each endpoint with the commands below, then supply
the resulting SHA as its literal placeholder value—not a command substitution.
That remaining argv surface is named in PASS.md and needs separate
ratification to change.

Findings first, feedback second: a review that only sends keys and appends
nothing disappears after a rotation or crash, leaving a reviewed branch
indistinguishable from an unread one. Appending findings before relay lets the
next leader read and relay them instead of paying for the review again; it is
the level-triggered rule in `AGENTS.md` applied to leader work.
`reviewed-by` lands beside the dispatch-stamped `engine`, so author ≠ reviewer
is auditable from the log alone by a reader who was not there. Without
`reviewed-effort`, a default-effort review reads exactly like one run at
`xhigh`.

`reviewed-sha` and `reviewed-base` make the verdict a fact about a diff, not a
branch name. The endpoints are the branch head and its **merge base**, because
that is where the three-dot delta read by `git diff main...work/<id>` and
`codex review --base main` begins. Record the merge base, never
`git rev-parse main`: main's tip is a commit the reviewer did not read, so
comparing it would attest to an integration context that never existed.

```sh
git rev-parse work/<id>
git merge-base main work/<id>
```

Either endpoint can move: a new author commit moves the head, leaving a green
check over code nobody read; a rebase moves the merge base by replaying the
delta onto a different parent. At merge, `reviewed-sha` must equal
`git rev-parse work/<id>` **and** `reviewed-base` must equal
`git merge-base main work/<id>`; otherwise the review is stale and must run
again.

Main advancing alone moves neither endpoint, so merging one branch does not
stale the reviews of the others. It can change the merge result: clean deltas
can still be semantically incompatible together. This rule knowingly does not
require a second review of that result—it would double every merge's cost, and
such incompatibility is not a reviewer's opinion. It requires gates on the
local merge result instead, where that incompatibility shows up.

## Check declaration and legacy items

Declare `cross-review` at admission, with the other checks:

```sh
alder work add --handoff <handoff> --priority <n> \
  --check <leader-authored-key:description> \
  --check cross-review:"reviewed by the other vendor's ladder; the leader records this one, not the worker"
```

Repeat the first form once per leader-authored check. A handoff-proposed check
description is submitter text and `--check` has no ratified file-valued form:
do not recover the old command-substitution template. Leave that description
behind a durable pointer or obtain a separate operator ruling before admission.
`work add` takes `--check`; `--add-check` is `work edit`'s flag and does not
exist on add (`src/cli.rs`), so using it here fails admission outright.
Admission is the only time to declare it because a check or dependency cannot
change while an attempt is active:

```text
$ alder work edit al-zptgbz --add-check cross-review:"…"
error [active_attempt]: dependencies and checks cannot change while
  `al-zptgbz-attempt-1` is active
```

That is the ordinary `docs/v0/MODEL.md` rule. Do not end a live attempt just
to widen its contract: an item is admitted before its branch exists, so there
is no pass where declaring the check late is the only option. The leader
records this check; the dispatched goal names it apart from worker-owned checks
through `LEADER_CHECKS` and `Brief::goal` in `crates/alderd/src/spawn.rs`.
`WORKER.md` therefore has a worker stop after its own checks and say
`ready for review`; otherwise a leader-only check would strand the worker one
step short of the marker and deadlock the protocol.

Every item admitted before this rule has no `cross-review` check, including the
one that introduced it, and no check result can be appended to it:

```text
$ alder attempt edit al-zptgbz-attempt-1 --failed cross-review --evidence-file <local-findings-file>
error [unknown_check]: attempt `al-zptgbz-attempt-1` has no check named
  `cross-review`
```

Do not end its attempt to add one. Review the branch anyway. Put the durable
findings into the log first with the ratified note file flag. Its returned
event sequence is a durable pointer even when a later worker note replaces the
folded current note:

```sh
alder attempt edit <attempt> --note-file <local-findings-file>
# Read the returned `head` and use that literal event sequence below.
alder attempt edit <attempt> \
  --meta reviewed-by=gpt-5.6-sol \
  --meta reviewed-sha=<reviewed-sha> \
  --meta reviewed-base=<reviewed-base> \
  --meta reviewed-effort=xhigh \
  --meta cross-review=failed \
  --meta cross-review-findings=<findings-event-seq>
```

For a legacy item, `cross-review=<verdict>` earns its place: without a check
to hold status, metadata is the only persistent field for its verdict and the
event-sequence pointer. The finding text is durable in that earlier note event;
the folded note may change, which is exactly why the metadata points to the
event rather than copying submitter text into `--meta`. Metadata merges instead
(`attempt.metadata.extend(...)` versus `attempt.note = Some(note)` in
`src/domain/state.rs`). A later round overwrites its own keys, which is the
level-triggered behaviour wanted here.

Finish a legacy item on the checks it actually carries. The missing gate is
held by reading this durable metadata rather than by `work finish` refusing;
the grandfathered set shrinks with every admission and never grows.

## Findings and feedback rounds

Record findings first, then relay them. A crash after recording and before
delivery is repairable: the next leader reads and relays the durable evidence.
A relay with no preceding record is a review that disappears at rotation.

The default fix path is a **fresh worker**: start a new attempt on the same
item and branch. Before changing code, the new worker reads the previous
authoring attempt's durable review evidence and notes with
`alder show <attempt-id>`, using the exact reviewed attempt ID from the
feedback record; `alder show <work-id>` shows the work and event-type history,
not attempt evidence. A launch goal alone contains title, spec, checks, and
gates. Choose the tier by the size of the findings, not the original item;
narrow enumerated findings are down-tier work, so `luna` or `sonnet` can fix
what `sol` found in `terra`'s change.

Do not compact or preserve the original author session for a feedback round.
Compaction pays a full-context read at author tier and preserves laundered
memory. The spec, checks, branch, commit messages, findings, and rulings must
let a fresh worker proceed; if they do not, repair the durable record rather
than the process. The narrow exception is an original session that is still
alive and has only trivial findings: record them first, then use its
`.alder/relay <session> <local-findings-file>` adapter—never a quoted
`send-keys` nudge. Decisions worth keeping must therefore be committed:
explain them in commit messages and attempt notes, and put binding spec rulings
in `work edit`, so a fresh worker does not re-litigate them.

A real defect returns as precise feedback into the live worker session,
confirmed landed under PASS step 4, or into a fresh worker on the same item and
branch if that session is gone. The branch stays in flight; a finding is
evidence, not an order, and the author may argue it down with reasoning. A
disagreement that needs authority becomes `alder work ask <id>` with options
and a recommendation. Until a later fresh review satisfies the check, the
branch cannot merge. There is no override: a finding is fixed, ruled on, or
argued down on the record.

A re-review is a fresh review of new endpoints: the same command, new
`reviewed-sha` and `reviewed-base`, and `--satisfied` only after an engine
outside the authoring vendor has read the code being merged. A cross-review is
a heavy operation; running it is the pass's heavy operation whether or not a
merge follows.

## Known limits

These are deliberately named rather than hidden by prose. They require a new
durable fact or a change to the worker brief and are tracked as `al-q8qwhy`.

- **Feedback delivery has no durable receipt.** A verdict is durable before
  relay, but crash-before-send and send-completed look the same in the log.
  PASS step 4's pane observation is a limited workaround.
- **Spawn does not itself carry a prior attempt's findings.** The fresh-worker
  default works only because the findings are durable check evidence and
  attempt notes that the new worker must read with `alder show <attempt-id>`;
  a launch goal alone contains title, spec, check definitions, and gates. If a
  reviewed session is gone, its fresh replacement otherwise starts with its
  own pending checks and never reads what it was meant to fix—especially a
  Codex one-shot that begins work immediately.
- **Review intent is not recorded before launch.** A crash during either
  charged review leaves no durable target SHA or in-progress review to adopt,
  so a later pass may have to pay again. This is the repository's
  intent-before-effects rule, not yet applied to leader review.
- **A two-vendor partial verdict has nowhere durable to live.** One review
  cannot safely satisfy the whole check, and a pending check records no first
  verdict. A per-reviewer check result would change the acceptance-check model,
  which is the operator's call, not a leader's; this needs new state, not a
  prose workaround.

Until `al-q8qwhy` lands, these are costs the leader absorbs knowingly: relay
carefully, make the fresh worker read the recorded findings, expect a crashed
review to be paid twice, and keep a branch to one vendor where practical.
