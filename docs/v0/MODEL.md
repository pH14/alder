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
      "list": "nimbus ls --json | jq '[.boxes[] | {subject: .name, field: \"liveness\", level: .state}]'"
    }
  ]
}
```

`schema` selects the manifest format. `prefix` supplies the repository object
prefix. `store` locates the shared log. `observers` contains executable trusted
observation commands and may change without producing a durable event. Their
output becomes durable only when Alder applies it through `refresh`.

The prefix becomes immutable when the first work item is appended.
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
Alder does not assign durable writer roles or distinguish an executor, human,
worker, or side session for permission purposes. Repository skills and their
host environment own those workflow policies.

The event types Alder may append are:

- `observation.reported`
- `observation.retired`
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

The loop's control types are namespaced separately, because they record how
the project is to be driven rather than what the project owes:

- `loop.paused`
- `loop.resumed`
- `loop.engine_selected`
- `loop.rotation_requested`
- `loop.nudge_requested`

These are storage types, not a requirement that every type become a separate
user-facing concept.

**The log never mentions its own readers.** Every type above is a statement
about the project or a standing instruction to whoever drives it; none is a
record of a process reading the log. Pass events were the last such machinery
records, and they are gone from the live schema.

Historical logs may contain `handoff.submitted`, `handoff.integrated`,
`handoff.withdrawn`, `pass.started`, and `pass.ended`. The decoder retains
those wire forms so history remains readable, and the append layer refuses
every one of them, so no path in the workspace can write a new one. Handoff
submission and withdrawal are inert history; integration still adds its
embedded work record, because later historical events and dependency edges
can refer to work created that way. Pass events are inert with one exception:
a historical `pass.ended` that asked for rotation still reads as a rotation
request, because that half of the event was a statement about the loop rather
than about the pass.

## Identifiers

Every Alder object ID begins with a repository-configured prefix. The examples
use `hm`, as a Harmony repository might:

| Object | Form | Example |
| --- | --- | --- |
| Work | `<prefix>-<token>` | `hm-9a1` |
| Attempt | `<work>-attempt-<ordinal>` | `hm-9a1-attempt-1` |
| Question | `<work>-question-<ordinal>` | `hm-9a1-question-1` |

The prefix is chosen once for the repository and cannot change after its first
object is appended. Generated tokens contain no hyphens, keeping the three
forms unambiguous. (Historical logs also hold `<prefix>-pass-<ordinal>` IDs;
they name no live object.)

Attempt and question ordinals start at one, increase independently within
their work item, and are never reused. Every attempted launch consumes an
attempt ordinal, including one that ends as `not_started`. A revised answer
keeps its question ID; a later distinct question receives the next question
ordinal.

Attempts and questions still store an explicit `work_id`. The readable ID is
not the authoritative relationship and need not be parsed during replay.

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
  `work.changed` and starts `open`.
- Open work can be blocked, finished, dropped, or started.
- Blocked work can be unblocked, finished externally, dropped, or edited.
- Blocking and unblocking remain `edit` operations in `work.changed`, not
  separate event types or durable objects. Both require a reason. Their public
  operations are `alder work block` and `alder work unblock`: `edit` never
  changes state, so the transition is a verb even though the storage is one
  operation shape.
- A block may carry a review deadline, `work block --until <RFC3339>`, stored
  on the item as `block_until`. The latest block's statement wins whole: a
  re-block without a deadline clears the previous one, and unblocking,
  finishing, dropping, or reopening clears it too. The fold is a pure
  function of the log and reads no clock, so passing the instant changes no
  state — an expired deadline surfaces as a `block_expired` attention finding
  in `status`, and unblocking remains an explicit, reasoned act. "Check again
  at 3pm" is thereby a statement on the work item, never on the loop.
- Blocking work with an active attempt prevents a later attempt from starting
  but does not stop the existing external execution.
- Work with an unanswered question cannot be unblocked.
- Done or dropped work can be reopened with a reason when the requirement is
  still the same.
- Reopening lands in `open`, unless an unanswered question survives the
  transition, in which case it lands in `blocked` with `block_reason` set to
  that question — the same rule `unblock` already enforces, so the two paths
  back to `open` agree instead of one silently dropping a pending question.
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
| `tier` | The runner's rung name, opaque to Alder; explicit null when unset |
| `handle` | Optional opaque foreign name for the execution, bound once |
| `metadata` | Open-ended JSON supplied by project skills |
| `started_seq` | Intent recorded before launch |
| `bound_seq` | External handle binding |
| `updated_seq` | Last durable progress update |
| `ended_seq` | Attempt end |

There may be at most one active attempt for a work item in v0.

The attempt is the one joint between judgment and execution: work on one
side, the handle on the other, the outcome when it closes. The handle and
the tier are the runner's names, held verbatim; no environment variable,
stamp, or marker of Alder's is planted in the execution. The connection is
re-established each sweep by comparing the handle the attempt records with
the handles an observer lists.

### Starting

`attempt.started` is appended before launch and may carry the runner's tier
name and project-defined metadata. Alder then returns the attempt ID and the
runner launches the work.

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

## Handles, tiers, and metadata

A handle is an opaque foreign name for something outside Alder — a tmux
session, a sandbox, a CI run. Alder stores the string a runner hands it,
compares it for equality, and never parses it: no grammar, no kind prefix,
no field inside it selects anything. Within a project, the complete handle
identifies one observed object, because the runner that names its executions
also supplies the observer that lists them.

The tier is the runner's rung name for the execution — `luna`, `terra`,
`sol`, or whatever ladder the runner climbs. Alder validates only that it is
non-empty; no table of valid tiers exists anywhere in Alder. It is stored so
a later reader can say "retry this at a higher rung" from the log alone.

The direction is deliberately asymmetric, and it is a rule about the schema:
**the log stores the runner's names; nothing in Alder's schema or protocol
requires the runner to store anything of Alder's.** Work must not know how
it is executed —
the same item could be run by an agent in tmux, an agent in a web sandbox,
or a deterministic script — so no engine name, session kind, or execution
vocabulary appears in the work schema, and no Alder mechanism depends on a
mark planted in the execution environment. (Transitionally, the in-tree
alderd runner still stamps `ALDER_ATTEMPT` into the sessions it creates as
its own crash-adoption bookkeeping; that is the runner's private convention,
due to leave with the runner extraction, not part of this model.)

V0 stores at most one primary handle on an attempt. Metadata is open-ended
JSON. Alder stores and displays it but does not use its keys for readiness,
completion, conflict detection, or any other core transition. Repository
skills define useful conventions — review provenance, consult records —
and own their meaning.

Handle validity does not depend on any observer currently answering for it.
A handle nothing observes remains replayable and visible; its attempt simply
has no fresh liveness level.

V0 has no observer plugin system or provider-specific Rust adapter. The
manifest's `observers` array supplies exactly one command for each observer
name: a `list` command for complete generic snapshots, or a `probe` command
for per-handle execution liveness. The name is the first component of every
observation key.

Each command defines its own scope through its arguments, environment, and
native tool configuration. If several scopes contribute to one `list`
observer, the command must aggregate them into one complete result.
Credentials remain in the native environment rather than Alder metadata.

On success, a `list` command's standard output contains exactly one JSON
array. Each entry has:

| Field | Meaning |
| --- | --- |
| `subject` | The observed thing, opaque to Alder |
| `field` | A stable lower-case field name, such as `ci` |
| `level` | The current value for that key, such as `passing` |

Rows are generic observations keyed by their subject verbatim. `liveness` is
not a `list` field — execution liveness flows only through probes — so a
list row claiming it appends nothing.

A `probe` command is invoked once per relevant handle with the handle as its
single argument (`$1`) and prints exactly one word: `present`, `absent`, or
`unknown`. `unknown` means "not a name I recognize; I cannot say" and writes
nothing. The handle is passed verbatim and matched against attempt records
by equality, so Alder stays fully opaque to handle contents — the
runner-side script owns recognition of its own names. Answers are recorded
under the attempt's own ID; the durable key is
`(observer, attempt-id, liveness)`.

Duplicate keys, surrounding prose, or any other schema violation invalidates
the complete result; the retired handle-inventory shape (`value`,
`attempt_id`, `metadata`) is no longer valid observer output, and a probe
answer that is not exactly one of the three words is invalid.

Observation configuration is executable trusted configuration. Alder does
not interpolate event data or handle values into the command string; a
probed handle rides in as a real process argument. Launching remains the
runner's responsibility.

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

### Stranded questions

A question is actionable only while its work is in a non-terminal state. When
the work becomes `done` or `dropped`, its unanswered questions are **stranded**:
there is no longer a requirement to decide about, so they stop appearing under
`status`'s waiting-on-human list. They are not hidden. `show` renders every
question, stranded or not, with its complete history.

Stranding is derived from the current work state, never stored. There is no
event kind, flag, or repair step for it, which is what makes reopening correct
for free: `work reopen` returns the work to a non-terminal state and its
unanswered questions become actionable again in the same fold. That state is
`open`, unless a question is still unanswered, in which case it is `blocked`
on that question — reopening never hands back work that looks actionable but
has a decision silently pending, matching what `unblock` already refuses to
do.

Answering a stranded question remains legal. Recording a late ruling is
harmless, and answers already support revision. `work drop` and `work finish`
report the questions they strand, so a caller sees that consequence when
deciding rather than discovering it afterwards.

## Loop controls

The loop is a singleton, so its controls are folded fields rather than
objects. They are the only loop state the log carries — the log never records
its own readers, so there are no run records, no "open pass", and no account
of when or whether any driver acted.

| Field | Fold rule |
| --- | --- |
| `paused`, `pause_reason` | Last writer wins. `loop.paused` sets both; `loop.resumed` clears both. No count, nesting, or owner. |
| `engine` | Last writer wins. `loop.engine_selected` replaces the desired name. The name is opaque and never validated. |
| `rotate_requested_seq` | The sequence of the latest `loop.rotation_requested` (or of a historical `pass.ended` that asked to rotate). |
| `nudge_requested_seq` | The sequence of the latest `loop.nudge_requested`. |

The request sequences are deliberately raw. Whether a request has been acted
on is each driver's machine-local knowledge — it compares the sequence with
the last head it acted on — so the fold cannot and does not say "pending".
Two drivers with separate notes each honor a request once, which is the
harmless direction.

The loop section of `alder status` also serves `review_at`: the earliest
`work block --until` deadline over all blocked work. It is derived from work
items, not stored on the loop; it exists so a driver can wake the executor at
the deferral's instant without reading work state.

Pause is desired state, not a lock: enforcement belongs to whatever schedules
the loop. Alder does not store which driver, host, or process owns the loop,
for the same reason it stores no executor role.

[LOOP.md](LOOP.md) states the driver's read surface and why a missed or
duplicated wake is harmless.

## Concurrent writers

Alder stores no executor, generation, lease, or writer role. For each mutation:

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
head; the caller must reread and decide again.

Reapplying is not withheld out of caution. A mutation is validated against one
projection and materialized at one sequence, so replaying it against a log
that has moved would append a decision nobody made — and the decision is
sometimes the whole payload, as with an answer that rules on the state the
answerer read. Because reconsideration is the caller's, a loss is reported as
a fact about the command: nothing was appended, and this is the event that
was not written. See [CLI.md](CLI.md) for how that reaches each output
channel.

The expected head is internal to the command. There is no public `--if-head`
option. A change committed before a command begins is part of the state
against which that command is validated. A repository skill may still assign
one agent the executor role, but that role has no representation or enforcement
inside Alder.

Every ordinary read and mutation must first establish the current shared-log
head from the configured remote, even when a local branch, remote-tracking
ref, or SQLite projection appears current. Failure returns
`store_unavailable`; local state is not silently treated as current.
Observation-command failure is narrower: it affects only that observer run
and does not append a replacement level.

## Observations

Observations are durable beliefs about the external world. Their folded
snapshot has one entry per `(observer, subject, field)` key:

| Field | Meaning |
| --- | --- |
| `observer` | Configured script name that owns the key |
| `subject` | Opaque observed thing |
| `field` | Stable aspect of that subject |
| `level` | The newest reported value |
| `reported_seq` | Sequence of the winning report |

`observation.reported` replaces a key's level and `observation.retired`
removes it when the key no longer exists. A same-level report appends nothing:
this is a belief log, not a sensor trace.

For each configured kind, Alder runs its command through a fixed shell
wrapper with pipefail enabled. One execution may run for 20 seconds. A
failed execution, timeout, malformed output, or invalid result is retried up
to three times after the initial execution, for at most four executions per
result. The first valid result wins: for `list`, the first valid complete
snapshot; for `probe`, the first valid one-word answer per handle, and a
sweep whose handles cannot all be answered fails whole.

Failed standard output is discarded. After all executions fail, Alder retains
bounded final-execution diagnostics but appends no replacement belief. A
timeout terminates the complete shell pipeline, not only its parent shell.

After a valid `list` snapshot, each returned level is applied through the
observation append path and every omitted prior key for that observer is
retired.

A probe sweep asks about every live attempt's bound handle, plus every
handle bound to an ended attempt whose liveness key is still current. An
active attempt's answer is reported as its level — `absent` establishes the
key even when none existed, because a dead worker is a statement the fold
must carry: a reader with no observer of its own can only learn the death
from a level, never from silence. An active attempt's `unknown` writes
nothing. An ended attempt's key stays while the probe answers `present` —
the execution is outliving its attempt — and retires on `absent` or
`unknown`, because an ended attempt cannot be watched forever. When an ended
and a live attempt hold the same handle string, the live one owns the
answer and the ended key retires. Any other key under the probe observer's
name is not a statement it can renew and retires.

`alder refresh` performs this scheduled ingestion. `alder reconcile` normally
refreshes first, then compares durable attempts with the attempt-keyed
liveness levels:

- active attempt whose level is `present`: healthy;
- active attempt whose level is `absent`: possible lost attempt (`missing`);
- ended attempt whose level is still `present`: an execution outliving its
  attempt (`orphan`);
- unbound starting attempt: launch or end it through an ordinary repair;
- failed observer command: no new liveness belief and no destructive action.

Reconciliation turns these comparisons into findings and suggested ordinary
commands. It never acts on a provider. A caller that accepts a repair invokes
the suggested mutation separately. Provider-reported existence, lifecycle, and
cost remain observations; the provider remains authoritative.

## Projection lifecycle

SQLite records the exact shared-log head represented by its durable
projection tables. Before Alder reads or validates through those tables, it
compares that value with the current log head. A mismatch causes a complete
fold of the ordered log and replacement of all durable projection tables in
one SQLite transaction; the represented head is committed in the same
transaction.

V0 has no incremental projection update path. A successful append does not
write SQLite, so the next command observes the mismatch and rebuilds. The
observation snapshot is part of that durable fold. Diagnostic `--run` output
is transient and never becomes an observation source.

## Required projections

The initial SQLite projection exposes:

- `work_current`
- `dependencies`
- `attempts`
- `attempt_checks`
- `questions_open`
- `ready`
- `in_flight`
- `blocked`
- `downstream`
- `loop_control`

`alder status` is built from these projections. Raw SQL is diagnostic; agents
should rely on the named commands and their stable `--json` results.

## Derived predicates

Work is **ready** when:

- its state is `open`;
- it has no active attempt;
- every dependency is `done`.

A question is **stranded** when it is unanswered and its work is `done` or
`dropped`. `questions_open` excludes stranded questions for the same reason
`status` does: an open question is one someone can still act on.

Alder exposes event and observation times but derives no stale-attempt
predicate. Deciding whether an attempt has stopped moving belongs to the
caller.
