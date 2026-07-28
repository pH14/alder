# Alder v0 state model

This document defines the durable facts and invariants. It is intentionally
implementation-oriented, but it is not yet a database schema. The schema
should be frozen only after the paper replay in
[ACCEPTANCE.md](ACCEPTANCE.md).

## Repository manifest

The project root contains `.alder/config.json`, the single user-facing Alder
manifest:

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

`schema` selects the manifest format. `prefix` supplies the repository object
prefix. `store` locates the shared log. `observers` contains executable trusted
observation commands and may change without producing a durable event.

The prefix becomes immutable when the first work item or handoff is appended.
Every later operation verifies the configured prefix against the log. A
mismatch is a configuration error, never an instruction to rename existing
objects.

`alder init --prefix <prefix>` creates the manifest when absent and verifies
the configured store. The same invocation is idempotent: if the manifest and
existing log are compatible, it preserves the file byte-for-byte, including
observer edits, appends no event, and creates no additional commit. A
conflicting prefix, store location, schema, or incompatible log rejects the
operation without changing either the manifest or log.

## Durable events

The Git log contains typed events. Every event has this envelope:

| Field | Meaning |
| --- | --- |
| `id` | Client-generated unique ID used to resolve unknown append outcomes |
| `seq` | Store-assigned total order |
| `at` | Advisory wall-clock timestamp |
| `actor` | Best-effort caller identity for audit |
| `type` | Closed event type |
| `body` | Type-specific payload |
| `schema` | Event schema version |

`seq`, not `at`, determines order. Every mutation uses the same expected-head
conditional append path. `actor` is provenance, not an authorization claim;
Alder does not assign durable writer roles or distinguish a leader, human,
worker, or side session for permission purposes. Repository skills and their
host environment own those workflow policies.

The initial event types are:

- `handoff.submitted`
- `handoff.integrated`
- `work.changed`
- `work.finished`
- `work.dropped`
- `work.reopened`
- `attempt.started`
- `attempt.bound`
- `attempt.updated`
- `attempt.ended`
- `question.asked`
- `question.answered`

The loop's types are namespaced separately, because they record how the project
is being driven rather than what the project owes:

- `pass.started`
- `pass.ended`
- `loop.paused`
- `loop.resumed`
- `loop.engine_selected`
- `loop.rotation_requested`
- `loop.nudge_requested`

These are storage types, not a requirement that every type become a separate
user-facing concept.

## Identifiers

Every Alder object ID begins with a repository-configured prefix. The examples
use `hm`, as a Harmony repository might:

| Object | Form | Example |
| --- | --- | --- |
| Work | `<prefix>-<token>` | `hm-9a1` |
| Attempt | `<work>-attempt-<ordinal>` | `hm-9a1-attempt-1` |
| Question | `<work>-question-<ordinal>` | `hm-9a1-question-1` |
| Handoff | `<prefix>-handoff-<token>` | `hm-handoff-f27` |
| Pass | `<prefix>-pass-<ordinal>` | `hm-pass-19` |

The prefix is chosen once for the repository and cannot change after its first
object is appended. Generated tokens contain no hyphens, keeping the five
forms unambiguous.

A pass uses a repository-scoped ordinal rather than a token because passes
belong to the singleton loop and are serialized: at most one is open, so the
next ordinal is never contended.

Attempt and question ordinals start at one, increase independently within
their work item, and are never reused. Every attempted launch consumes an
attempt ordinal, including one that ends as `not_started`. A revised answer
keeps its question ID; a later distinct question receives the next question
ordinal.

Attempts and questions still store an explicit `work_id`. The readable ID is
not the authoritative relationship and need not be parsed during replay.
Handoffs have no work component because they exist before admission; an
integrated handoff stores its resulting `work_id`.

Event envelope IDs remain separately generated unique values. They provide
append idempotency and do not use this human-facing domain ID grammar.

### Atomic work changes

`work.changed` contains one or more normalized operations:

- `add`, with the complete initial work definition;
- `edit`, with a work ID and explicit field, dependency, check, or state
  changes.

A successful `alder work add` or `alder work edit` invocation produces this
event. Structured input may place many operations in it, including additions
and edits together. The event ID identifies the entire mutation and resolves
an unknown push outcome without duplicating any newly admitted work.

Before appending, Alder:

1. reads the expected current head;
2. allocates stable IDs for every addition;
3. resolves input-local references to those IDs;
4. applies every operation to an in-memory projection;
5. validates the resulting state as a whole.

No intermediate operation order is durable or observable. A missing target,
duplicate work ID, repeated edit target, dependency cycle,
unanswered-question violation, or forbidden edit to active work rejects the
entire event. A head conflict also rejects the whole mutation. Alder does not
silently replay a decision against a changed graph; the caller must reread and
reconsider it.

The structured input document and its local names are CLI conveniences. They
are not stored as a plan or assigned a lifecycle. The durable event contains
only real work IDs and normalized operations.

### Hypothetical queries

`alder status --with <changes>` and `alder next --with <changes>` use the same
structured document and validator as the mutating commands. Alder applies the
operations to an in-memory copy of the current projection, then runs the
ordinary query against that copy.

The query:

- appends no event and creates no Git commit;
- does not persist the hypothetical change; ordinary head synchronization may
  rebuild an out-of-date SQLite projection before the query;
- does not allocate durable IDs for additions;
- renders new work using its input-local name;
- uses the current external observations when producing `status`;
- identifies the base log head and states that the output is hypothetical.

Invalid documents are rejected exactly as they would be during a real append.
A later `work add --from` or `work edit --from` invocation rereads and
revalidates against its then-current head.

Only `status` and `next` accept `--with` in v0. The overlay contains one
applicable graph change; it has no preview-only outcome assumptions or
multi-step scenario language.

## Handoffs

A handoff is a durable asynchronous message for later admission. It is not
work and has exactly two states:

- `submitted`
- `integrated`

| Field | Meaning |
| --- | --- |
| `id` | Stable handoff ID |
| `title` | Short description |
| `ref` | Spec, branch, report, commit, URL, or other artifact reference |
| `note` | Optional context for the admitting agent |
| `state` | `submitted` or `integrated` |
| `submitted_seq` | Inbox arrival |
| `work_id` | Work created by integration |
| `integrated_seq` | Integration event |

`handoff.submitted` changes no work, dependency, readiness, or attempt state.
It is the asynchronous side-channel write in v0. Its public operation is
`alder handoff add`. Unlike `work add`, it accepts no priority, dependency, or check fields.

On a head conflict, submission rereads, refolds, and revalidates. Because it
only creates a uniquely identified inert inbox record, it may then resubmit
automatically; an existing event with the same ID resolves the operation as
already submitted. Integration follows the ordinary reconsider-on-conflict
rule.

`handoff.integrated` contains the same fields as an `add` operation in
`work.changed`; folding it atomically creates the work item, records the link,
and changes the handoff to `integrated`. Its public operation is
`alder work add --handoff <handoff>`.

An integrated handoff cannot be integrated again. If its proposed work is
invalid—for example, it introduces a dependency cycle—the append is rejected
and the handoff remains submitted. There is no declined or deleted state in
v0.

Repository skills should submit a handoff only after an explicit human
instruction. This is an operational permission boundary, not a claim that
Alder can infer delegation from prose. An unsolicited submission remains inert
until a writer chooses to integrate it.

## Work

A work item is a durable requirement that should survive several execution
attempts.

| Field | Meaning |
| --- | --- |
| `id` | Stable Alder ID |
| `title` | Short human-readable identity |
| `spec` | Optional path, URL, or immutable reference |
| `priority` | Integer used to order otherwise-actionable work |
| `state` | `open`, `blocked`, `done`, or `dropped` |
| `block_reason` | Required explanation while blocked |
| `outcome` | Completion or drop summary |
| `opened_seq` | Admission event |
| `changed_seq` | Last durable mutation |

`running` is not stored as a work state. It is derived from the presence of an
active attempt.

The title is stored even when a richer public spec exists. A title is an
identity label, not a knowledge store, and Alder must remain usable if the
external spec moves.

The referenced spec is opaque to Alder. Consuming projects may interpret it,
but Alder does not define or validate its schema.

### State rules

- New work is created explicitly through a `work add` operation in
  `work.changed` or through `handoff.integrated`, and starts `open`.
- Open work can be blocked, finished, dropped, or started.
- Blocked work can be unblocked, finished externally, dropped, or edited.
- Blocking and unblocking remain `edit` operations in `work.changed`, not
  separate event types or durable objects. Both require a reason. Their public
  operations are `alder work block` and `alder work unblock`: `edit` never
  changes state, so the transition is a verb even though the storage is one
  operation shape.
- Blocking work with an active attempt prevents a later attempt from starting
  but does not stop the existing external execution.
- Work with an unanswered question cannot be unblocked.
- Done or dropped work can be reopened with a reason when the requirement is
  still the same.
- Reopening preserves all prior attempts and outcomes.
- Work with an active attempt cannot be edited in ways that change its
  dependencies or checks.
- Finishing ordinary work requires an active attempt whose checks are all
  satisfied; the finish ends that attempt as successful.
- Work completed outside Alder may be finished with `external` evidence and
  no active attempt.
- Dropping work with an active attempt requires its ID and a non-success
  outcome. One `work.dropped` event ends the attempt and drops the work.
- Dropping work without an active attempt accepts no attempt outcome.
- A drop never terminates the external execution; any live execution must be
  stopped and confirmed first.

## Dependencies

A dependency is a directed edge:

`work -> required work`

Work is actionable only when every requirement is `done`.

`dropped` does not satisfy a dependency. Dropping or reopening a completed
prerequisite can make downstream work non-actionable again. If any affected
downstream work has an active attempt, Alder rejects the operation and returns
those attempts. The caller must resolve them first; v0 has no confirmation or
override flag.

Dependencies have no automatic successor following. If work is replaced by
different work, its downstream edges are changed explicitly.

Hierarchy is presentation, not scheduling semantics. V0 has no parent/child
rule that silently turns a work item into a plan node or suspends a subtree.

## Attempts

An attempt is one external execution of one work item.

| Field | Meaning |
| --- | --- |
| `id` | Immutable attempt ID |
| `work_id` | Work being attempted |
| `state` | `starting`, `active`, or `ended` |
| `outcome` | `succeeded`, `failed`, `cancelled`, `lost`, or `not_started` |
| `handle` | Optional external handle once bound |
| `metadata` | Open-ended JSON supplied by project skills |
| `started_seq` | Intent recorded before launch |
| `bound_seq` | External handle binding |
| `updated_seq` | Last durable progress update |
| `ended_seq` | Attempt end |

There may be at most one active attempt for a work item in v0.

An attempt ID is the effect-boundary fence. External workers should expose the
attempt ID wherever their environment permits. A later caller adopts an
existing attempt when both of these are true:

- the attempt is active in Alder;
- the external execution presents the same attempt ID.

An external execution for an ended or unknown attempt is a collision requiring
caller action. Alder reports it; observation never kills it merely because it
was found.

### Starting

`attempt.started` is appended before launch and may contain project-defined
metadata. Alder then returns the attempt ID. A repository-tuned skill launches
the work and stamps the external execution with that ID.

After launch:

- `attempt.bound` attaches the external handle and any useful metadata; or
- `attempt.ended { outcome: not_started }` records launch failure.

The public operation for attaching the handle is
`alder attempt edit <attempt> --handle <handle>`. An attempt may transition
from no handle to one handle exactly once. Replacing or clearing that handle
is rejected. The specialized `attempt.bound` event preserves this invariant
without adding a `bind` command to the CLI.

Starting is rejected if the work is blocked, done, dropped, has unmet
dependencies, or already has an active attempt.

### Recording progress

`attempt.updated` records meaningful progress, check results, and evidence.
It is not a heartbeat stream. Routine liveness comes from refreshed external
observations.

An `alder attempt edit` against an ended attempt is rejected. Late external
results therefore cannot satisfy checks for a later attempt.

### Ending

`alder attempt end <attempt> --outcome <outcome>` records a failed, cancelled,
lost, or not-started attempt. It changes only the attempt. Ordinarily the work
remains `open`; if it was blocked while the attempt was active, it remains
`blocked`.

There is no attempt-edit option that blocks or drops work. If work should
remain blocked, block it before ending its attempt; this prevents a new
attempt from starting during the two-step sequence. If work should be dropped,
`alder work drop <work> --attempt <attempt> --outcome <outcome>` performs both
durable transitions in one `work.dropped` event.

Successful completion is represented by `work.finished`, which ends the
active attempt and marks the work done in one logical append.

Ending an attempt rejects future progress edits; it does not terminate the
external execution behind its handle. When that execution may still be live,
the repository skill must stop it through its native system and confirm the
result before recording the attempt end. If the skill crashes after the
external stop, reconciliation sees an active attempt with an absent handle and
can repair the durable state. Ending the Alder attempt first risks leaving an
untracked worker running.

A later attempt may start only after the prior attempt has ended.

## Handles and metadata

A handle names something outside Alder:

`<kind>:<opaque-value>`

The kind is a stable, syntactically valid name used to select an observation
command. The value is interpreted only by that command. Within a project, the
complete handle identifies one observed object.

Examples:

| Kind | Example | Typical role |
| --- | --- | --- |
| `tmux` | `tmux:box-17/alder-hm-9a1-attempt-1` | Attempt execution |
| `codex` | `codex:019f...` | Attempt execution |
| `github-actions` | `github-actions:owner/repo/run/4212` | Attempt execution |
| `nimbus` | `nimbus:box-17` | Environment inventory |

Claude Code running inside tmux uses a tmux handle plus metadata such as
`agent=claude-code`. It earns its own handle kind only if it later exposes a
stable identity or observation API independent of tmux.

V0 stores at most one primary handle on an attempt. Related objects such as
its host may be recorded in metadata:

```json
{
  "agent": "claude-code",
  "engine": "opus-5",
  "host": "nimbus:box-17"
}
```

Metadata is open-ended JSON. Alder stores and displays it but does not use its
keys for readiness, completion, conflict detection, or any other core
transition. Repository skills define useful conventions. If requested and
observed values both matter, a skill may use distinct keys such as
`requested_host` and `host`; Alder does not define a separate request/facts
model.

Handle validity does not depend on an observation command currently being
configured. Unknown kinds remain replayable and visible; they simply have no
fresh observation.

V0 has no observer plugin system or provider-specific Rust adapter. The
manifest's `observers` array supplies at most one `list` command for each
observer name, which becomes the handle kind.

The command defines its own scope through its arguments, environment, and
native tool configuration. If several scopes contribute to one kind, the
command must aggregate them into one complete result. Credentials remain in
the native environment rather than Alder metadata.

On success, standard output contains exactly one JSON array. Each entry has:

| Field | Meaning |
| --- | --- |
| `value` | Opaque portion of the handle; Alder prefixes `<observer>:` |
| `attempt_id` | Optional attempt ID stamped on the external object |
| `metadata` | Optional open-ended JSON reported by the command |

Alder supplies status and observation time. Entries returned by a valid
snapshot are `present`; commands do not emit statuses. Duplicate values,
conflicting attempt IDs, surrounding prose, or any other schema violation
invalidate the complete result.

Observation configuration is executable trusted configuration. Alder does not
interpolate event data or handle values into `list`. Launching remains the
responsibility of repository-tuned skills.

## Acceptance checks

Checks define what must be true for work to finish successfully.

| Field | Meaning |
| --- | --- |
| `work_id` | Owning work |
| `key` | Short item-local name such as `tests` or `review` |
| `description` | Human-readable condition |

Check definitions belong to work. They are created and changed by operations
in `work.changed`, but may be changed only while no attempt is active.

Every declared check gates ordinary completion. Each attempt has an
independent result for every check:

- `pending`
- `satisfied`
- `failed`

A result includes evidence and the sequence that recorded it. Results do not
carry automatically to a later attempt. If a durable artifact legitimately
survives, the later attempt cites that artifact as fresh evidence.

Check keys are item-local strings. V0 has no global vocabulary or profile
revision system.

## Questions

A question records an asynchronous human decision needed by one work item.
It is subordinate to that work, not standalone work and not a general message.

| Field | Meaning |
| --- | --- |
| `id` | Stable question ID |
| `work_id` | Affected work |
| `text` | The question |
| `answer` | Current answer, if any |
| `asked_seq` | Creation |
| `answered_seq` | Latest answer |
| `answered_by` | Actor |

`question.asked` atomically creates the question and moves open work to
`blocked`. If the work is already blocked, it remains so. Asking against work
with an active attempt does not end or terminate that attempt.

`question.answered` records the response but does not unblock the work. A
caller first incorporates the decision into the spec, dependencies, or checks
when necessary, then explicitly unblocks it. This separation also lets an
answer arrive while the driving agent is busy or being replaced.

An answer may be revised. The latest answer is current; earlier answers remain
in history. V0 does not create replacement-question chains.

## Passes

A pass is one run of the driving loop. Work is to an attempt what the loop is
to a pass, and the same invariants follow from that: intent is recorded before
effects, and at most one pass is open at a time.

| Field | Meaning |
| --- | --- |
| `id` | Immutable pass ID |
| `engine` | Opaque engine name supplied by the caller; never validated |
| `handle` | Session handle, `<kind>:<value>`, such as `tmux:alder-leader` |
| `triggers` | Why the loop was woken: `log`, `observations`, `due`, `manual` |
| `state` | `open` or `ended` |
| `outcome` | `ok`, `crashed`, or `timeout` |
| `report` | Free-text iteration report |
| `wake_at` | Absolute time the pass asked to be woken again |
| `rotate` | Whether the pass requested a fresh session next time |
| `why` | Explanation of a non-`ok` outcome |
| `at_head` | The log head the wake was appended at |
| `started_at`, `started_seq` | Intent recorded before the agent was prompted |
| `ended_at`, `ended_seq` | Pass end |

`pass.started` is rejected while another pass is open, and `pass.ended` is
rejected against an already-ended pass. Pass ordinals start at one, increase by
one, and are never reused.

`at_head` records what the pass could have seen. Trigger kinds are provenance:
they say why the wake happened and never limit what the pass must do.

A pass ends `ok` only when the agent that ran it says so. `crashed` and
`timeout` are what an external driver can honestly assert when the agent is not
available to speak for itself. An open pass therefore blocks the next wake
until someone records one of those, which makes the crash window a forced
repair rather than an optional one.

## Loop controls

The loop is a singleton, so its controls are folded fields rather than objects.

| Field | Fold rule |
| --- | --- |
| `paused`, `pause_reason` | Last writer wins. `loop.paused` sets both; `loop.resumed` clears both. No count, nesting, or owner. |
| `engine` | Last writer wins. `loop.engine_selected` replaces the desired name. The name is opaque and never validated. |
| `rotate_pending` | Derived, never stored: true when the latest rotation request has a greater sequence than the latest `pass.started`, or when a rotation was requested and no wake has ever happened. |
| `nudge_pending` | Derived the same way from the latest `loop.nudge_requested`. A nudge asks the driver to wake the loop now; the next wake consumes it. |

A rotation request is a `loop.rotation_requested` event or a `pass.ended` whose
`rotate` is set. The next wake consumes the request by being later in the log,
so nothing clears a flag and two writers cannot disagree about whether a
rotation has already been served. A nudge request follows the identical rule
over its own event kind.

Pause is desired state, not a lock. Alder still accepts `loop wake` while
paused; enforcement belongs to whatever schedules the loop. Alder does not
store which driver, host, or process owns the loop, for the same reason it
stores no leader role.

[LOOP.md](LOOP.md) states the driver's read surface and the crash-window
reasoning in full.

## Concurrent writers

Alder stores no leader, generation, lease, or writer role. For each mutation:

1. the caller queries the configured remote ref and reads log head `H`;
2. Alder validates the command against the projection for `H`;
3. the store creates an event commit whose parent is `H`;
4. the store pushes that commit to the remote ref as a normal fast-forward
   update.

The successful remote update is the durability point. A local event commit
that has not been pushed is not part of the Alder log. The Git implementation
uses the configured remote's standard Git transport; GitHub may host that
remote, but Alder does not use a GitHub-specific API.

If another writer advances the log before step 4, the append changes nothing
and returns a head conflict. Ordinary mutations are not reapplied to the new
head; the caller must reread and decide again. Submission of a uniquely
identified, inert handoff is the only automatic reconsideration in v0.

The expected head is internal to the command. There is no public `--if-head`
option. A change committed before a command begins is part of the state
against which that command is validated. A repository skill may still assign
one agent the leader role, but that role has no representation or enforcement
inside Alder.

Every ordinary read and mutation must first establish the current shared-log
head from the configured remote, even when a local branch, remote-tracking
ref, or SQLite projection appears current. Failure returns
`store_unavailable`; local state is not silently treated as current.
Observation-command failure is narrower and produces `unknown` only for that
observation kind.

## Observed state

External observations are local SQLite rows, not durable events. They form an
inventory rather than merely annotating the attempts Alder already knows:

| Field | Meaning |
| --- | --- |
| `handle` | Observed external handle |
| `attempt_id` | Optional attempt ID found on the external object |
| `status` | `present`, `absent`, or `unknown` |
| `metadata` | Open-ended JSON reported by the observation command |
| `observed_at` | Freshness stamp |
| `detail` | Observation-command diagnostic text |

The inventory includes unbound objects. A Nimbus command, for example, may
report every provisioned box whether or not a work attempt currently uses it.
This is how leaked or unexpectedly expensive environment state becomes
visible without turning Alder into the allocator.

For each configured kind, Alder runs `list` through a fixed shell wrapper with
pipefail enabled. One execution may run for 20 seconds. A failed execution,
timeout, malformed JSON, or invalid result set is retried up to three times
after the initial execution, for at most four executions. The first valid
complete snapshot wins.

Failed standard output is discarded. After all executions fail, handles of
that kind are `unknown`; Alder retains bounded final-execution diagnostics and
never infers absence. A timeout terminates the complete shell pipeline, not
only its parent shell.

After a valid snapshot:

- every returned value is `present`;
- a durable bound handle of that kind which is omitted is `absent`;
- the current unbound inventory for that kind is replaced by the returned
  unbound values.

`unknown` is not `absent`. An unreachable command or provider must not trigger
automatic cancellation.

`alder refresh` performs the read-only sweep. `alder reconcile` normally
refreshes first, then compares the inventory with durable attempts:

- active attempt with its handle present: healthy;
- active attempt with its handle confirmed absent: possible lost attempt;
- unbound starting attempt with a discovered matching attempt ID: bindable;
- live handle stamped with an ended or unknown attempt: orphan or collision;
- any object observed through an unavailable provider: unknown.

Reconciliation turns these comparisons into findings and suggested ordinary
commands. It never appends an event or acts on a provider. A caller that
accepts a repair invokes the suggested mutation separately. Provider-reported
existence, lifecycle, and cost remain observations; the provider remains
authoritative.

## Projection lifecycle

SQLite records the exact shared-log head represented by its durable
projection tables. Before Alder reads or validates through those tables, it
compares that value with the current log head. A mismatch causes a complete
fold of the ordered log and replacement of all durable projection tables in
one SQLite transaction; the represented head is committed in the same
transaction.

V0 has no incremental projection update path. A successful append does not
write SQLite, so the next command observes the mismatch and rebuilds. Local
observation tables are not derived from the durable log and survive this
rebuild.

Local observation storage also retains the latest `refresh` execution summary
for each configured kind: execution count, duration, success or failure,
bounded stderr, validation error, and snapshot time. This powers
`alder debug observations`; it is not durable history.

## Required projections

The initial SQLite projection exposes:

- `work_current`
- `handoffs_submitted`
- `dependencies`
- `attempts`
- `attempt_checks`
- `questions_open`
- `ready`
- `in_flight`
- `blocked`
- `downstream`
- `observed_handles`
- `passes`
- `pass_open`
- `loop_control`

`alder status` is built from these projections. Raw SQL is diagnostic; agents
should rely on the named commands and their stable `--json` results.

## Derived predicates

Work is **ready** when:

- its state is `open`;
- it has no active attempt;
- every dependency is `done`.

Alder exposes event and observation times but derives no stale-attempt
predicate. Deciding whether an attempt has stopped moving belongs to the
caller.
