# Alder v0 CLI

The CLI is organized around the project-driving workflow, not the storage
model. Alder itself stores no executor or writer role; repository skills may
assign those roles above the CLI.

## Grammar

Five rules decide where a command goes. They are not style preferences; each
one removes a class of ambiguity that cost a reader a lookup.

**Queries are global.** A reader wants one answer about the project, not one
answer per noun. `status`, `next`, `show`, `observations`, `refresh`, and
`reconcile` take no noun.

**Mutations name their noun.** Every command that appends starts with the thing
it changes: `alder work start`, `alder attempt end`, `alder loop pause`.

**The noun is the ID type.** `alder work drop <id>` takes a work ID; `alder
attempt end <id>` takes an attempt ID. Alder never infers the resource from an
ID's shape, even though the shapes differ.

**Parents create; records answer for themselves.** Work creates attempts
(`work start`), because the parent knows whether another one is allowed. A
record that already exists closes itself: `attempt end`.

**`edit` never changes state; verbs transition.** `work edit` changes fields,
dependencies, and checks. Blocking is `work block`, unblocking is
`work unblock`, ending an attempt is `attempt end`. This is why reading a
transcript never requires checking which flags an `edit` carried.

The complete surface:

| Global | Work | Attempt | Question | Observation | Loop |
| --- | --- | --- | --- | --- | --- |
| `init` | `add` | `edit` | `answer` | `report` | `pause` |
| `status` | `edit` | `end` | | `retire` | `resume` |
| `next` | `start` | | | | `use` |
| `show` | `finish` | | | | `rotate` |
| `observations` | `drop` | | | | `nudge` |
| `refresh` | `reopen` | | | | |
| `reconcile` | `block` | | | | |
| `debug` | `unblock` | | | | |
| | `ask` | | | | |

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
attempt and question IDs extend their work ID.

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

The prefix becomes immutable after the first work item is appended.
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
loser's output has the shape of a receipt, in either channel: a loss read as
success would leave a caller believing a decision was recorded when nothing
was.

The head comes from the configured remote ref, not a local branch or
remote-tracking ref. Every ordinary read and mutation contacts that remote;
the implementation fetches the ref when its objects are not already
available. A mutation succeeds only after its event commit has been pushed as
a normal fast-forward update. GitHub may host the remote, but Alder uses
standard Git transport and requires no GitHub API integration.

The expected head is not a public command argument. V0 has no `--if-head`
option. A change already present when the command begins is part of the state
against which the command is validated.

Every ordinary named read and mutation first establishes the current shared
head. If that fails, the command returns `store_unavailable`; it does not
present the local projection as current.

## Orientation

### `alder status [--with <changes>] [--full] [--section <name> ...]`

The default output is an index, not a report: the loop line plus a count for
each of five sections. A drained log — nothing anywhere — costs a few dozen
tokens instead of the full pack's few thousand:

```text
$ alder status
head 4211

loop
  engine claude

counts
  attention  2
  in flight  1
  ready      2
  waiting on human  1
  blocked    0
```

The five counted sections are `attention`, `in_flight`, `ready`,
`waiting_on_human`, and `blocked`. In `--json` they sit under a `counts`
object at the top level, present on every call regardless of `--full` or
`--section`:

```json
{"counts": {"attention": 2, "in_flight": 1, "ready": 2, "waiting_on_human": 1, "blocked": 0}, ...}
```

Counts are full current state, just summarized — level-triggered like the
rest of Alder, never a delta. A nonzero count is not itself the detail; a
caller that needs to act on one still fetches the section before treating its
sync as done.

`--section <name>` expands exactly one of the five sections back to its full
list, alongside the counts:

```text
$ alder status --section attention
head 4211

loop
  engine claude

counts
  attention  2
  in flight  1
  ready      2
  waiting on human  1
  blocked    0

attention
  hm-2b7  attempt hm-2b7-attempt-1 absent; last progress 3h ago
  -  `hm-8c3` was deferred until 2026-07-27T15:00:00+00:00 and that time has passed — review it
```

In `--json`, that adds a matching top-level key — here, `attention` — holding
the array. The other four section keys stay absent. Repeat `--section` to add
several keys at once; they are rendered in canonical order and duplicate names
are ignored.

`--full` expands every section, matching today's full pack, and is the only
way to see `recent_events` — the last ten log entries, dropped from the
default pack entirely because the five sections already fold events into
state. If combined with `--section`, `--full` wins.

`status` includes the durable observation snapshot. The snapshot is the last
reported belief, not a hidden local inventory; a failed refresh appends no
replacement belief. Alder does not derive a stale-attempt classification; the
caller judges elapsed time.

`waiting_on_human` lists the unanswered questions someone can still act on. A
question whose work has since been dropped or finished is stranded and is
omitted from that list — and its count — in both human and `--json` output;
nobody is waiting on a decision about a requirement that no longer stands.
Stranded questions are not hidden — `show` renders them, and reopening the
work returns them to the list. The `--json` `questions` array, always present
regardless of `--full` or `--section`, carries every question with a derived
`stranded` field.

The `loop` section is not one of the five counted sections and is never
gated: it reports the loop's durable desired state — whether it is paused and
why, the desired engine, the raw sequences of the latest rotation and nudge
requests, and `review_at`, the earliest `work block --until` deadline over
all blocked work. It carries no run records: the log never mentions its own
readers, so whether a driver has acted on a request is that driver's
machine-local knowledge, not something `status` can report. `attention`
additionally surfaces every blocked item whose `--until` deadline has passed,
as a `block_expired` finding with its suggested `work unblock`. The loop
section is omitted from human output when the loop has nothing to say; in
`--json` it is always present under the `loop` key. See [LOOP.md](LOOP.md).

With a structured graph change:

```text
$ alder status --with replan.json
hypothetical · based on head 4211 · replan.json · not written
...
```

`--with` composes with the rest of `status` exactly as it always has: the
hypothetical durable state is combined with the current folded observations,
and the resulting counts (or expanded sections, under `--full`
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

Show current state and compact history for a work item, attempt, or
question. `show` is global because a reader with an ID in hand should
not have to know which kind it is.

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
reserve `work add` for the agent responsible for admission. This is workflow
policy rather than a durable Alder role.

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

### `alder work block <work> --why <reason> [--until <RFC3339>]`

Block work on something that is not another Alder work item:

```text
$ alder work block hm-9a1 --why "release credentials are not available"
```

There is no separate block object or condition language. If another Alder
work item is the prerequisite, use `work edit --add-requires` instead.

`--until` adds a review deadline — "come back to this at …":

```text
$ alder work block hm-9a1 --why "vendor outage" --until 2026-07-30T09:00:00Z
hm-9a1  blocked until 2026-07-30T09:00:00+00:00
```

The deadline is stored on the work item as `block_until`, and the latest
block's statement wins whole: re-blocking without `--until` clears it. The
earliest deadline over all blocked work is served as `review_at` in the
`status` loop section, which is what wakes the driving loop at that instant.
Nothing unblocks by itself — when the deadline passes, the item surfaces
under `attention` as a `block_expired` finding, and unblocking stays an
explicit, reasoned act.

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
$ alder work start hm-9a1 --tier terra
hm-9a1-attempt-1
```

The command:

1. validates that the work is ready;
2. appends `attempt.started`;
3. commits and pushes it;
4. returns the attempt ID.

`--tier <name>` records the runner's rung name on the attempt. It is opaque:
any non-empty string is legal, no table of valid tiers exists anywhere in
Alder, and its meaning lives with whatever runs the work. Storing it lets a
later reader say "retry this at a higher rung" from the log alone.

The runner then launches the worker and attaches the resulting external
handle with `alder attempt edit`. The runner owns every execution choice;
Alder records the names it is handed and interprets none of them.

A second `work start` is rejected while an active attempt exists.

### `alder attempt edit <attempt>`

```text
$ alder attempt edit hm-9a1-attempt-1 \
    --handle tmux:alder-work-hm-9a1
```

`--handle` attaches one external handle to the attempt. A handle is a
non-empty opaque string — a foreign name the runner chose. Alder stores it
verbatim and compares it for equality; it never parses it, and no part of it
selects anything inside Alder. A probe observer asked about the same string
is what connects it back to a liveness level.

Attaching a handle is a one-way transition: the attempt must not already have
one, and the handle cannot later be replaced or cleared. Alder records the
edit as `attempt.bound`. This is a strict field-specific rule of
`attempt edit`, not a separate command.

`--tier <name>` records a new rung name on the attempt, with the same
opaque-name rule as `work start --tier`.

Attempt metadata (`--meta KEY=VALUE`) is open ended. Repository skills define
useful conventions; Alder never gates core behavior on metadata keys.

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

`--evidence-file <path>` is the file-valued form of `--evidence`, and
`--note-file <path>` is the file-valued form of `--note`. Each reads a local
file when the command runs and stores its text in the ordinary `attempt.updated`
event; the path is never stored. The inline and file forms for the same field
are mutually exclusive. This lets a caller record externally written prose
without making it a shell argument, while keeping the log self-contained for
later readers, repairs, and workers on another machine. Evidence is still
prose plus pointers: a bulky artifact belongs behind a ref, SHA, or event
sequence rather than in an evidence file.

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

The loop is a singleton per log, and its commands are standing instructions
to whoever drives it — never records of a run. The log never mentions its own
readers: there is no `loop wake`, no pass noun, and nothing durable that says
the loop ran. [LOOP.md](LOOP.md) defines the design.

### `alder loop pause [--why <reason>]` and `alder loop resume`

Desired state, folded last-writer-wins:

```text
$ alder loop pause --why "release freeze until Thursday"
loop paused
$ alder loop resume
loop resumed
```

Pause is advisory to the driver, not an Alder-enforced lock: enforcement
belongs to whatever schedules the loop.

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

A request that the driver's next wake start on a fresh session. The fold
records only the sequence the request was asked at; each driver treats a
request later in the log than the last head it acted on as outstanding, and
acting consumes it. There is no flag to clear, and nothing in the log says
whether any driver has served it — the log does not record its readers.

### `alder loop nudge [--why <reason>]`

```text
$ alder loop nudge --why "answered the release question"
nudge requested
```

A request that the driver wake the executor now rather than at the next
scheduled trigger. A nudge follows the identical rule as rotation — the fold
records its sequence, and each driver treats one later than its noted head as
outstanding. The driver reports it as the `manual` trigger and fires through
its own deferrals; it does not override `loop pause`. A nudge changes *when*
the executor is next woken, never *what* it does.

## Observations

### `alder observations`

List the folded current picture. This is the snapshot command: it reads the
shared log and lists one row for every current `(observer, subject, field)`
key, ordered by that key. It does not run scripts or inspect SQLite.

```text
$ alder observations
github  owner/repo#171  ci  passing
tmux    hm-9a1-attempt-1  liveness  present
```

### `alder observation report <observer> <subject> <field> <level>`

Report one current level. The command appends `observation.reported` only when
the current fold has a different level for that key; repeating the exact report
is successful and returns `"appended": false`. This is the noun-first mutation
form. The script that discovers a level does not need to remember or compare
anything.

### `alder observation retire <observer> <subject> <field>`

Retire a key an observer has established no longer exists. Retiring an already
absent key is also a successful no-op. A retirement removes the key from the
snapshot; it does not leave a second `absent` state behind.

### `alder refresh`

Run configured observers and apply what they report. An observer entry has
exactly one command form: `list` for complete generic snapshots, or `probe`
for per-handle execution liveness.

A `list` command prints a JSON array of level reports:

```json
[
  {"subject":"owner/repo#171", "field":"ci", "level":"passing"},
  {"subject":"owner/repo#172", "field":"ci", "level":"running"}
]
```

The manifest entry supplies the first key part:

```json
{"observer":"github", "list":"ci list --json | jq '[.[] | {subject: .id, field: \"ci\", level: .state}]'"}
```

An exit-zero valid array is complete for that observer: reported keys are
updated through the append layer, and previously current keys omitted from
the array are retired. Rows keep their subject verbatim. `liveness` is not a
`list` field — it flows only through probes — so a list row claiming it
appends nothing.

A `probe` command is invoked once per relevant handle, with the handle as its
single argument (`$1`), and prints exactly one word: `present` (the
execution this handle names is running), `absent` (the probe recognizes the
name and nothing runs under it), or `unknown` (not a name the probe
recognizes; Alder writes nothing).

```json
{"observer":"tmux", "probe":"scripts/observe-tmux.sh \"$1\""}
```

The handle stays fully opaque to Alder: it is passed verbatim and matched
against attempt records by equality; recognition of a runner's names lives
in the runner's script. Refresh probes every Starting/Active attempt's bound
handle, plus every handle bound to an ended attempt whose liveness key is
still current, and records each answer under the attempt's own ID — the
durable key is `(observer, attempt-id, liveness)`:

- active + `present` or `absent`: that level is reported — `absent`
  establishes the key even on the first sweep, so a worker that died before
  it was ever observed still becomes a durable statement;
- active + `unknown`: nothing is written, and reconcile keeps saying
  `observation_unknown`, which is honest;
- ended + `present`: the level stays, so `reconcile` names the `orphan`;
- ended + `absent` or `unknown`: the key retires — an ended attempt is not
  watched forever once its execution is gone or unrecognizable.

When an ended and a live attempt hold the same handle string (respawns reuse
session names), the live attempt owns the probe answer and the ended
attempt's key retires.

A failure appends no belief. Alder runs each command through a fixed
`bash -o pipefail` wrapper with a 20-second timeout and up to three retries
per execution; the first valid result wins, and a probe sweep fails whole
when any handle stays unanswerable.

`refresh` returns `changed`, the number of appended changes, and retired-key
count. It is the normal scheduled ingestion command for alderd or cron. A
head advance from it therefore always means a belief changed.

### `alder reconcile`

Refresh by default, compare durable attempts with folded liveness levels, and
propose repairs:

```text
$ alder reconcile
hm-2b7-attempt-1  recorded active, observed absent
  suggested: alder attempt end hm-2b7-attempt-1 --outcome lost --why "external handle absent"

hm-4c8-attempt-1  an open attempt has never been bound to a handle; no worker was launched
  suggested: dispatch a worker for hm-4c8
```

An execution outliving its ended attempt surfaces the same way: the probe
keeps the ended attempt's liveness key `present` while the execution runs,
so the default refresh-first flow names the `orphan` and suggests the
repair — killing the execution is the runner's act on its own name, so the
suggestion names the handle verbatim rather than spelling a command Alder
would have to parse the handle to build.

Reconcile does not treat an unknown level as absent. It refreshes by default,
so its observation changes are ordinary `observation.*` appends; it never
performs a provider action. A caller performs any suggested repair separately.
Use `alder reconcile --no-refresh` to compare against the folded snapshot
without running scripts.

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

Without a kind, list configured observation kinds and the observer names in
the folded picture, with their folded current keys. A kind present in the fold
but lacking configuration is shown as unconfigured.

With a kind, show its configured command, effective shell, timeout and retry
settings, and folded current keys.

`--run` executes that kind alone and shows every execution plus the normalized
result. It is diagnostic: it does not append. Use `alder refresh` to apply
reported levels.

All forms support `--json`.

## Normal iteration

```text
$ alder status
$ alder reconcile
$ alder next
$ alder work start hm-9a1 --tier terra
hm-9a1-attempt-1

# The runner launches the worker, then:
$ alder attempt edit hm-9a1-attempt-1 \
    --handle tmux:alder-work-hm-9a1
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

When a driver wakes an agent to run that iteration, the commands are the
whole record of it: decisions land on the items they concern, and the log
says nothing about the run itself. A conclusion like "held off because the
vendor is down until Thursday" is a statement on the item —
`alder work block <id> --why "vendor outage" --until 2026-07-30T09:00:00Z` —
never a report about the iteration.
