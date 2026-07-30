# Alder v0 CLI

The CLI is organized around the project-driving workflow, not the storage
model. Alder itself stores no leader or writer role; repository skills may
assign those roles above the CLI.

## Grammar

Five rules decide where a command goes. They are not style preferences; each
one removes a class of ambiguity that cost a reader a lookup.

**Queries are global.** A reader wants one answer about the project, not one
answer per noun. `status`, `next`, `show`, `refresh`, and `reconcile` take no
noun.

**Mutations name their noun.** Every command that appends starts with the thing
it changes: `alder work start`, `alder attempt end`, `alder loop wake`.

**The noun is the ID type.** `alder work drop <id>` takes a work ID; `alder
attempt end <id>` takes an attempt ID; `alder pass end <id>` takes a pass ID.
Alder never infers the resource from an ID's shape, even though the shapes
differ.

**Parents create; records answer for themselves.** Work creates attempts
(`work start`) and the loop creates passes (`loop wake`), because the parent
knows whether another one is allowed. A record that already exists closes
itself: `attempt end`, `pass end`.

**`edit` never changes state; verbs transition.** `work edit` changes fields,
dependencies, and checks. Blocking is `work block`, unblocking is
`work unblock`, ending an attempt is `attempt end`. This is why reading a
transcript never requires checking which flags an `edit` carried.

The complete surface:

| Global | Work | Attempt | Question | Handoff | Loop | Pass |
| --- | --- | --- | --- | --- | --- | --- |
| `init` | `add` | `edit` | `answer` | `add` | `wake` | `end` |
| `status` | `edit` | `end` | | `withdraw` | `pause` | |
| `next` | `start` | | | | `resume` | |
| `show` | `finish` | | | | `use` | |
| `refresh` | `drop` | | | | `rotate` | |
| `reconcile` | `reopen` | | | | `nudge` | |
| `debug` | `block` | | | | | |
| | `unblock` | | | | | |
| | `ask` | | | | | |

## Output

Every command supports the global `--json` flag. Human-readable output is the
default; JSON is the only structured output format in v0.

With `--json`, standard output contains exactly one JSON document. This applies
to reads, successful mutations, and expected failures. A failure still returns
a nonzero process status, and its document carries `"ok": false`, which no
successful document carries. JSON output contains no tables, color, progress
indicators, or surrounding prose.

Without `--json`, a failure goes to standard error as an `error [<code>]:`
line and one indented `key: value` line per context field. It is never a JSON
document: a printed object is the shape of a result, and a caller skimming a
terminal reads it as one.

Each result carries a command-specific schema identifier, which follows the
grammar: `alder.<noun>.<verb>.v0` for mutations, `alder.<query>.v0` for
queries. Field names and types are stable, absent values are explicit `null`,
and arrays with meaningful order are deterministic. Mutation results include
their durable IDs, event ID, and resulting head. Errors use stable codes and
structured context rather than requiring a caller to interpret prose.

For example:

```text
$ alder work start hm-9a1 --json
{"schema":"alder.work.start.v0","head":4212,"work_id":"hm-9a1","attempt_id":"hm-9a1-attempt-1","event_id":"01K..."}
```

Examples below illustrate intent; detailed JSON schemas remain to be frozen by
paper replay.

The examples assume the repository prefix `hm`. Work IDs use that prefix;
attempt and question IDs extend their work ID, while handoff and pass IDs
extend only the repository prefix because they belong to no work item.

## Initialization

### `alder init --prefix <prefix> [--remote <remote>] [--ref <ref>]`

Create or verify `.alder/config.json`:

```text
$ alder init --prefix hm
initialized .alder/config.json · origin refs/heads/alder
```

`origin` and `refs/heads/alder` are the default store remote and ref. Explicit
store arguments may select different values.

`init` is idempotent. Repeating it with compatible prefix and store arguments:

- verifies the manifest and any existing log;
- preserves the manifest byte-for-byte, including observer edits;
- appends no event;
- creates no additional commit;
- reports success as already initialized.

If the manifest is absent but the selected ref already contains a compatible
Alder log, `init` may create the manifest and adopt it. A conflicting prefix,
remote, ref, schema, malformed manifest, or incompatible existing log changes
nothing and returns `config_conflict`.

The prefix becomes immutable after the first work item or handoff is appended.
Changing observer entries does not append an event. `init` also supports
`--json`.

## Concurrent writes

Each mutation internally reads a log head, validates against that head's
projection, and conditionally appends to it. If another writer advances the
log first, the command changes nothing and returns a structured head-conflict
error. Ordinary mutations are not retried against the new state; the caller
rereads and decides again.

Because the caller is the one that has to reconsider, losing says so in terms
of the command rather than of the log. A head conflict reports that nothing
was appended, names the event that was not written, and carries
`"appended": false`; the JSON envelope carries `"ok": false`. Nothing on the
loser's output has the shape of a receipt, in either channel. That matters
most for `pass end`, where a loss read as success leaves the pass open and its
report unwritten.

The head comes from the configured remote ref, not a local branch or
remote-tracking ref. Every ordinary read and mutation contacts that remote;
the implementation fetches the ref when its objects are not already
available. A mutation succeeds only after its event commit has been pushed as
a normal fast-forward update. GitHub may host the remote, but Alder uses
standard Git transport and requires no GitHub API integration.

The expected head is not a public command argument. V0 has no `--if-head`
option. A change already present when the command begins is part of the state
against which the command is validated. `handoff add` alone may automatically
retry after a conflict because its uniquely identified submission is inert.

Every ordinary named read and mutation first establishes the current shared
head. If that fails, the command returns `store_unavailable`; it does not
present the local projection as current.

## Orientation

### `alder status [--with <changes>] [--full] [--section <name> ...]`

The default output is an index, not a report: the loop line plus a count for
each of six sections. A drained log — nothing anywhere — costs a few dozen
tokens instead of the full pack's few thousand:

```text
$ alder status
head 4211 · observations refreshed 38s ago

loop
  engine claude
  open hm-pass-19  claude  tmux:alder-leader  started 2026-07-27T09:41:02Z

counts
  attention  2
  handoffs   1
  in flight  1
  ready      2
  waiting on human  1
  blocked    0
```

The six counted sections are `attention`, `handoffs`, `in_flight`, `ready`,
`waiting_on_human`, and `blocked`. In `--json` they sit under a `counts`
object at the top level, present on every call regardless of `--full` or
`--section`:

```json
{"counts": {"attention": 2, "handoffs": 1, "in_flight": 1, "ready": 2, "waiting_on_human": 1, "blocked": 0}, ...}
```

Counts are full current state, just summarized — level-triggered like the
rest of Alder, never a delta. A nonzero count is not itself the detail; a
caller that needs to act on one still fetches the section before treating the
pass as done.

`--section <name>` expands exactly one of the six sections back to its full
list, alongside the counts:

```text
$ alder status --section attention
head 4211 · observations refreshed 38s ago

loop
  engine claude
  open hm-pass-19  claude  tmux:alder-leader  started 2026-07-27T09:41:02Z

counts
  attention  2
  handoffs   1
  in flight  1
  ready      2
  waiting on human  1
  blocked    0

attention
  hm-2b7  attempt hm-2b7-attempt-1 absent; last progress 3h ago
  hm-8c3  question hm-8c3-question-1 answered; still blocked
```

In `--json`, that adds a matching top-level key — here, `attention` — holding
the array. The other five section keys stay absent. Repeat `--section` to add
several keys at once; they are rendered in canonical order and duplicate names
are ignored.

`--full` expands every section, matching today's full pack, and is the only
way to see `recent_events` — the last ten log entries, dropped from the
default pack entirely because the six sections already fold events into
state. If combined with `--section`, `--full` wins.

`status` shows observation times and command failures. Alder does not derive a
stale-attempt classification; the caller judges elapsed time. A failed refresh
produces `unknown` rather than presenting an older observation as current.

`waiting_on_human` lists the unanswered questions someone can still act on. A
question whose work has since been dropped or finished is stranded and is
omitted from that list — and its count — in both human and `--json` output;
nobody is waiting on a decision about a requirement that no longer stands.
Stranded questions are not hidden — `show` renders them, and reopening the
work returns them to the list. The `--json` `questions` array, always present
regardless of `--full` or `--section`, carries every question with a derived
`stranded` field.

The `loop` section is not one of the six counted sections and is never
gated: it reports the loop's desired state and its two interesting passes —
whether it is paused and why, the desired engine, whether a rotation is
pending, the open pass, and the last ended pass with its outcome, the first
line of its report, any wake time it requested, and the head it ended at.
Comparing that `ended_seq` with the document's own `head` tells a reader
whether anything has been appended since the loop last ran, without the reader
remembering anything. It is omitted from human output when the loop has nothing
to say. In `--json` it is always present under the `loop` key. See
[LOOP.md](LOOP.md).

With a structured graph change:

```text
$ alder status --with replan.json
hypothetical · based on head 4211 · replan.json · not written
...
```

`--with` composes with the rest of `status` exactly as it always has: the
hypothetical durable state is combined with the current external
observations, and the resulting counts (or expanded sections, under `--full`
or `--section`) reflect it. Nothing is appended, and the hypothetical change
is not written to the local projection. Ordinary head synchronization may
first rebuild an out-of-date projection.

### `alder next [--with <changes>]`

Print actionable work in priority order:

```text
$ alder next
hm-9a1  Film projector seek  priority 80
```

`next` is a query, not an automatic scheduling decision.

```text
$ alder next --with replan.json
hypothetical · based on head 4211 · replan.json · not written
$build-index  Build frame index  priority 90
```

`--with` accepts the same structured document as `work add --from` or
`work edit --from`; `--with -` reads it from standard input. Alder validates
the document, applies it to an in-memory projection, and then runs the ordinary
query.

New work is shown by its input-local name because hypothetical queries do not
allocate durable IDs. Invalid changes are rejected under the same rules as a
real append. A later mutating command rereads and revalidates the current
head. `--with` also composes with `--json`.

### `alder show <id>`

Show current state and compact history for a handoff, work item, attempt,
question, or pass. `show` is global because a reader with an ID in hand should
not have to know which kind it is.

## Handoffs

### `alder handoff add`

Submit an asynchronous handoff without waiting for the driving agent:

```text
$ alder handoff add \
    --title "Frame index v2" \
    --ref specs/frame-index-v2.md \
    --note "Scoped in the side session; integrate behind hm-9a1"
hm-handoff-f27  submitted
```

Submission changes no work or dependency state. The command completes once the
handoff is durably appended; the driving agent may be busy.

Repository-tuned handoff skills should call this only after an explicit human
request such as "handoff to leader." Even if an agent submits one
unexpectedly, it cannot become actionable work without an explicit
`work add --handoff`.

`handoff add` does not accept work-shaping fields such as priority,
dependencies, or checks. Those are admission decisions made if the handoff is
admitted. The handoff's note may convey urgency or other context without
affecting scheduling.

Submitted handoffs appear in `alder status`; `alder show <handoff>` provides
their detail.

### `alder work add --handoff <handoff>`

A writer admits the handoff. The noun is `work` because the command creates
work; `--handoff` names its source.

```text
$ alder work add --handoff hm-handoff-f27 \
    --priority 70 \
    --requires hm-9a1 \
    --check report:"findings documented"
hm-a22  integrated from hm-handoff-f27
```

The handoff's title and reference become the defaults for the new work.
Integration atomically creates the work and changes the handoff from
`submitted` to `integrated`. Failed validation changes nothing.

### `alder handoff withdraw <handoff> --why <reason>`

Retire a submitted handoff without admitting it:

```text
$ alder handoff withdraw hm-handoff-f27 --why "superseded by hm-handoff-f31"
hm-handoff-f27  withdrawn
```

Withdrawal is the noun-preserving verb: it never becomes a `work` command
because it does not create work. `--why` is required, mirroring `work drop`
and `work reopen`.

Only a `submitted` handoff can be withdrawn; an already-`integrated` or
already-`withdrawn` handoff rejects with `invalid_transition`. Withdrawal is
terminal in v0 — there is no un-withdraw — and a withdrawn handoff remains
visible through `show` but drops out of `status`'s handoff inbox the same way
an integrated one does.

## Admission and editing

### `alder work add`

```text
$ alder work add \
    --title "Film projector seek" \
    --spec specs/film-projector-seek.md \
    --priority 80 \
    --requires hm-a15 \
    --check tests:"seek tests pass" \
    --check review:"change is approved"
hm-9a1
```

Running `work add` is the admission decision. There is no `propose` command in
v0.

Alder does not authorize one writer over another. Repository skills should
reserve `work add` for the agent responsible for admission and direct workers
and side sessions to `handoff add`. This is workflow policy rather than a
durable Alder role.

To admit several related items atomically:

```text
$ alder work add --from new-work.json
build-index     hm-b11
validate-index  hm-b12
```

The JSON document contains an `add` array. Each entry may have a `local` name
that other entries use as `$name`; Alder allocates real work IDs before
validation and prints the resulting mapping. `--from -` reads the same format
from standard input.

Every item is admitted or none is. An `edit` section is rejected by
`work add --from`; mixed graph changes use the structured `work edit` form.

### `alder work edit [<work>]`

Edits title, spec, priority, dependencies, or check definitions. It never
changes work state.

Dependency and check edits are rejected while an attempt is active. Edits
that would create a dependency cycle are rejected.

```text
$ alder work edit hm-9a1 --add-requires hm-a22 \
    --why "seek now depends on rebuilt frame index"
```

For an atomic create-and-rewire operation, omit the work argument and provide
a graph-change document:

```text
$ alder work edit --from replan.json
build-index     hm-b11  added
validate-index  hm-b12  added
hm-9a1                   edited
hm-2b7                   edited
```

```json
{
  "why": "Split frame-index work before seek continues",
  "add": [
    {
      "local": "build-index",
      "title": "Build frame index",
      "priority": 90
    },
    {
      "local": "validate-index",
      "title": "Validate frame index",
      "requires": ["$build-index"]
    }
  ],
  "edit": [
    {
      "id": "hm-9a1",
      "add_requires": ["$validate-index"],
      "remove_requires": ["hm-a15"]
    },
    {
      "id": "hm-2b7",
      "priority": 40
    }
  ]
}
```

In this form, `work edit` means editing the work graph, so the document may
contain both additions and edits. This avoids a separate `apply`, `batch`, or
`transaction` command. The document has no durable state of its own. Its
top-level `why` records one reason for the complete change and is required
when the document contains edits. `work edit --from` requires at least one
edit operation; an additions-only document belongs to `work add --from`.

The document carries no state fields. `edit` never changes state, in the flags
or in the document, so a graph change cannot quietly block a work item.

The CLI allocates IDs, resolves local references, applies every operation to a
temporary projection, and validates the final graph. It then appends one
`work.changed` event in one Git commit. No intermediate state is observable.
If any operation is invalid, none is applied.

To inspect the resulting frontier before appending, use the `--with` form of
`alder status` or `alder next`. The eventual mutating invocation validates
again against the then-current head.

Dependency or check changes to work with an active attempt reject the entire
document. The atomic boundary covers Alder state only; it cannot stop or
rewrite external executions.

### `alder work block <work> --why <reason>`

Block work on something that is not another Alder work item:

```text
$ alder work block hm-9a1 --why "release credentials are not available"
```

There is no separate block object or condition language. If another Alder
work item is the prerequisite, use `work edit --add-requires` instead.

Blocking work with an active attempt prevents a later start but does not stop
the existing external execution. The repository skill may leave that
execution waiting. To terminate it and leave the work blocked, block the work
first, stop the external execution through its native system, and then record
the attempt's end with `alder attempt end`. Blocking first makes the sequence
safe if the caller crashes between mutations: another attempt cannot start.

### `alder work unblock <work> --why <reason>`

```text
$ alder work unblock hm-9a1 --why "release credentials were installed"
```

`unblock` is rejected while the work has an unanswered question.

### `alder work reopen <work>`

```text
$ alder work reopen hm-9a1 --why "merged implementation regressed frame zero"
```

Reopening keeps the same work identity and preserves its attempts. If it would
invalidate downstream work with an active attempt, Alder rejects the reopen
and returns those attempts. The caller must resolve them first; there is no
confirmation or override flag.

## Attempts

### `alder work start <work>`

Work creates its attempts, so the noun is `work`:

```text
$ alder work start hm-9a1 \
    --meta engine=opus-5 \
    --meta requested_host=box-a
hm-9a1-attempt-1
```

The command:

1. validates that the work is ready;
2. appends `attempt.started`;
3. commits and pushes it;
4. returns the attempt ID.

A repository-tuned skill then launches the worker, stamps it with
`hm-9a1-attempt-1`, and attaches the resulting external handle with
`alder attempt edit`. This wrapper owns choices such as engine, host, and cloud
allocation. Alder records those choices as metadata but does not interpret
them.

A second `work start` is rejected while an active attempt exists.

### `alder attempt edit <attempt>`

```text
$ alder attempt edit hm-9a1-attempt-1 \
    --handle tmux:nimbus-box-17/alder-hm-9a1-attempt-1 \
    --meta host=nimbus:box-17 \
    --meta toolchain=rustc-1.91-zzz
```

`--handle` attaches one external handle to the attempt. A handle is
`<kind>:<opaque-value>`; its kind selects a configured observation command and
the rest is opaque to Alder.

Used by the launch skill or reconciler after locating an external execution by
attempt ID. An unknown handle kind is accepted and preserved, but cannot be
refreshed until a matching observation command is configured.

Attaching a handle is a one-way transition: the attempt must not already have
one, and the handle cannot later be replaced or cleared. Alder records the
edit as `attempt.bound`. This is a strict field-specific rule of
`attempt edit`, not a separate command.

Attempt metadata is open ended. Repository skills define useful conventions;
Alder never gates core behavior on metadata keys.

```text
$ alder attempt edit hm-9a1-attempt-1 \
    --satisfied tests \
    --evidence "CI run 4212-a" \
    --meta pr=github:owner/repo/pull/171 \
    --note "PR 171 opened"
```

A check result names its verdict in the flag: `--satisfied <check>` or
`--failed <check>`, each repeatable and each requiring `--evidence`. `pending`
is the state every check starts in and is not a result a caller records.

Attempt edits record meaningful milestones. They are not expected on every
poll. An edit to an ended attempt is rejected.

When several checks need different evidence, repeat the command.

### `alder attempt end <attempt>`

The attempt is the record that exists, so it closes itself:

```text
$ alder attempt end hm-9a1-attempt-1 \
    --outcome failed \
    --why "worker exited before producing a patch"
```

End outcomes:

- `failed`
- `cancelled`
- `lost`
- `not-started`

Ending changes only the attempt. Ordinarily its work remains open; work
already blocked while the attempt was active remains blocked. There is no
`--block` or `--drop` option: those are work verbs.

Ending the durable Alder attempt rejects later progress edits. It does not
terminate the external execution. When the handle may still be live, the
repository skill must stop it through its native system and confirm that
result before recording the end. A new `work start` remains rejected until the
old attempt is durably ended.

## Completion and dropping

### `alder work finish <work>`

```text
$ alder work finish hm-9a1 --attempt hm-9a1-attempt-1
```

Ordinary completion requires every declared check for that attempt to be
satisfied. V0 has no optional or non-gating check type.

Work completed outside Alder uses:

```text
$ alder work finish hm-9a1 --external --evidence "PR 171 merged"
```

External completion is explicit because it bypasses the ordinary attempt
contract. It is also the way blocked work is finished, so a completion can
leave a question behind; the result names any question it strands.

### `alder work drop <work>`

```text
$ alder work drop hm-9a1 \
    --attempt hm-9a1-attempt-1 \
    --outcome cancelled \
    --why "spike showed the approach cannot work"
hm-9a1  dropped · affects hm-2b7 · also strands hm-9a1-question-1
```

Dropped work does not satisfy dependencies. A successful drop reports affected
downstream work and any unanswered question it strands, so both consequences
are visible at decision time rather than afterwards. If dropping the item would invalidate downstream work with an
active attempt, Alder rejects the drop and returns those attempts; the caller
must resolve them first.

If the work has an active attempt, `--attempt` must name it and `--outcome`
must be one of `failed`, `cancelled`, `lost`, or `not-started`. The drop ends
that attempt and drops the work in one append. If there is no active attempt,
both flags are omitted.

`drop` does not terminate an external execution. The repository skill must
stop and confirm any live execution before dropping work with its active
attempt.

## Questions

### `alder work ask <work> "<question>"`

Work creates its questions:

```text
$ alder work ask hm-3bwm \
    "Ship masked digest now, or wait for AA-6?"
hm-3bwm-question-1
```

Asking atomically records the question and blocks the work. V0 has no
standalone or informational questions.

### `alder question answer <question> "<answer>"`

```text
$ alder question answer hm-3bwm-question-1 \
    "Ship masked digest; AA-6 is not a gate."
```

The answer is durable but does not unblock the work. The driving agent reviews
it, adjusts the spec, dependencies, or checks if needed, and explicitly
unblocks it through `alder work unblock`. An answer can arrive while that agent
is busy or being replaced. The invoking environment supplies the best-effort
actor identity recorded on the event; Alder does not use it for authorization.

Answering an already answered question records a revision while retaining the
prior answer. A stranded question — one whose work has since been dropped or
finished — can still be answered; a late ruling is harmless and worth keeping.
`show <question>` reports the derived visibility, `"stranded": "work dropped"`,
alongside the question's full history.

## The loop

The loop is a singleton per log and passes are its run records: work is to an
attempt what the loop is to a pass. [LOOP.md](LOOP.md) defines the design.

### `alder loop wake --engine <name> --handle <kind>:<value> [--trigger <kind>]...`

The loop is the parent, so the loop opens the pass:

```text
$ alder loop wake --engine claude --handle tmux:alder-leader \
    --trigger log --trigger due
hm-pass-19
```

The engine name is an opaque string. Alder stores it and never validates it.
Trigger kinds are `log`, `observations`, `due`, and `manual`; they are
informational provenance and never limit what the pass must do. A wake with no
stated trigger records `manual`, because it came from a person.

The wake also records the head it was appended at, so a later reader knows what
the pass saw.

A wake is rejected with `pass_open` while a pass is open. This mirrors one
active attempt per work item, and it is what makes two concurrent drivers
harmless.

### `alder pass end [<pass>] --outcome ok|crashed|timeout`

The pass is the record that exists, so it closes itself. Omitting the ID ends
the open pass; with no open pass the command returns `no_open_pass`.

```text
$ alder pass end --outcome ok \
    --report "Integrated hm-handoff-f27; started hm-9a1; ARM lane still blocked." \
    --wake 20m
hm-pass-19  ended ok
```

- `--report` is the iteration report, free text. `status` shows its first line.
- `--wake <duration>` requests the next wake at a point in the future. It
  accepts `270s`, `20m`, `1h`, or `2d` and is stored as an absolute time, so a
  reader never has to know when the pass ended.
- `--rotate` asks the next wake to start on a fresh session.
- `--why` explains a non-`ok` outcome.

Ending an already-ended pass returns `pass_ended`.

### `alder loop pause [--why <reason>]` and `alder loop resume`

Desired state, folded last-writer-wins:

```text
$ alder loop pause --why "release freeze until Thursday"
loop paused
$ alder loop resume
loop resumed
```

Pause is advisory to the driver, not an Alder-enforced lock: `loop wake` is
still accepted while paused, so a human can run one deliberate pass without
first resuming.

### `alder loop use <engine>`

```text
$ alder loop use codex
loop engine codex
```

The desired engine name, folded last-writer-wins. It is an opaque string;
Alder stores it and never validates it. The driver decides whether it can run
that engine.

### `alder loop rotate [--why <reason>]`

```text
$ alder loop rotate --why "engine upgraded"
rotation requested
```

A one-shot request that the next wake start on a fresh session. Rotation is
pending exactly when a rotation request is later in the log than the most
recent wake, so the next wake consumes it. There is no flag to clear.

### `alder loop nudge [--why <reason>]`

```text
$ alder loop nudge --why "answered the release question"
nudge requested
```

A one-shot request that the driver wake the loop now rather than at the next
scheduled trigger. A nudge follows the identical pending rule as rotation —
pending exactly when its request is later in the log than the most recent
wake, consumed by the next wake, no flag to clear. The driver reports a
pending nudge as the `manual` trigger and fires through its own deferrals; it
does not override `loop pause`, and it cannot open a second pass while one is
open.

## Repair

### `alder refresh`

Run configured observation commands without appending:

```json
{
  "schema": "alder.config.v0",
  "prefix": "hm",
  "store": {
    "remote": "origin",
    "ref": "refs/heads/alder"
  },
  "observers": [
    {
      "observer": "nimbus",
      "list": "nimbus ls --json | jq '[.boxes[] | {value: .name, attempt_id: .labels.alder_attempt, metadata: {state: .state, estimated_cost: .estimated_cost}}]'"
    }
  ]
}
```

The entries come from `.alder/config.json`. `observer` becomes the handle kind.
`list` defines its own external scope and must print one complete normalized
JSON array. Alder runs it through a fixed shell wrapper with pipefail enabled.
The default timeout is 20 seconds per execution, followed by at most three
retries. The first valid result wins.

```text
$ alder refresh
observed 7 handles: 5 present, 1 absent, 1 unknown
unbound:
  nimbus:box-22  present  state=running  estimated_cost=31.70
changed since the previous refresh
```

An exit-zero, valid array is a complete snapshot. Returned values are present;
omitted durable handles of that kind are absent. After four failed executions,
the kind is unknown and failed output cannot establish absence. Timeouts
terminate the complete shell pipeline.

The result carries `"changed": bool` — whether this snapshot differs from the
stored one. The comparison covers handle identity, presence or absence, and
attempt binding only. Observation metadata is non-semantic by design, so a
moving cost ticker or a changing uptime must never report change. This bool is
what a driver polls to decide whether the world moved.

The inventory includes unbound objects, which is how leaked cloud boxes or
sessions become visible. Removing an observation command does not invalidate
existing handles. They remain replayable but have no fresh observation.

### `alder reconcile`

Refresh by default, compare durable attempts with the observed inventory, and
propose repairs:

```text
$ alder reconcile
hm-2b7-attempt-1  recorded active, observed absent
  suggested: alder attempt end hm-2b7-attempt-1 --outcome lost --why "external handle absent"

hm-9a1-attempt-1  recorded starting
  found: tmux:nimbus-box-17/alder-hm-9a1-attempt-1
  suggested: alder attempt edit hm-9a1-attempt-1 --handle tmux:nimbus-box-17/alder-hm-9a1-attempt-1

hm-6e3-attempt-2  recorded active, observation unknown
  no destructive action suggested

hm-4c8-attempt-1  an open attempt has never been bound to a handle; no worker was launched
  suggested: alderd spawn hm-4c8

hm-5d1-attempt-1  a live Codex worker has a session UUID but its attempt is missing codex-session metadata
  suggested: alder attempt edit hm-5d1-attempt-1 --meta codex-session=019f...

nimbus:box-22  present, no associated attempt
  attention: unclaimed environment handle
```

Reconcile does not treat `unknown` as absent. It is durably read-only: it
refreshes local observations and prints findings and suggested ordinary
commands, but never appends an event or acts on a provider. A caller performs
any accepted repair separately. A suggestion is a string for a human or an
agent to run: `alderd spawn` appears in one of them, and Alder still never
calls `alderd`.

Use `alder reconcile --no-refresh` to compare against the current local
inventory.

## Diagnostics

Diagnostics live under `alder debug` so the ordinary workflow remains small.

### `alder debug log`

Read-only commands:

- `alder debug log head`
- `alder debug log tail`
- `alder debug log show <seq>`
- `alder debug log verify`

There is no generic append command.

### `alder debug db`

- `alder debug db rebuild`
- `alder debug db verify`

### `alder debug query`

`alder debug query '<sql>'` runs a read-only SQLite query for debugging and
development. It is not part of the stable agent contract; automation uses the
named reads and `--json`.

### `alder debug observations [<kind>] [--run]`

Without a kind, list configured and durably referenced observation kinds with
their latest refresh result, object count, executions, duration, and
freshness. Kinds referenced by handles but lacking configuration are shown as
`unconfigured`.

With a kind, show its configured command, effective shell, timeout and retry
settings, latest normalized snapshot, validation error, and bounded stderr.

`--run` executes that kind alone and shows every execution plus the normalized
result. It is diagnostic: it does not update observation tables. Use
`alder refresh` to store observations.

All forms support `--json`.

## Normal iteration

```text
$ alder status
$ alder reconcile
# Status includes submitted handoffs; admit them before selecting more work.
$ alder next
$ alder work start hm-9a1 --meta engine=opus-5 --meta requested_host=box-a
hm-9a1-attempt-1

# The repository skill launches the worker, then:
$ alder attempt edit hm-9a1-attempt-1 \
    --handle tmux:nimbus-box-17/alder-hm-9a1-attempt-1 \
    --meta host=nimbus:box-17
```

Later:

```text
$ alder attempt edit hm-9a1-attempt-1 \
    --satisfied tests --evidence "CI 4212-a"
$ alder attempt edit hm-9a1-attempt-1 \
    --satisfied review --evidence "review 171"
$ alder work finish hm-9a1 --attempt hm-9a1-attempt-1
$ alder status
```

That loop is the center of Alder. New commands should be judged by whether
they make it more reliable.

When a driver runs that iteration on a schedule, it brackets the same commands
with a pass:

```text
$ alder loop wake --engine claude --handle tmux:alder-leader --trigger log
hm-pass-19
# ... the iteration above ...
$ alder pass end --outcome ok --report "Started hm-9a1." --wake 20m
```
