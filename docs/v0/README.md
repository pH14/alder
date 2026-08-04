# Alder v0

## Purpose

Alder is a durable work-and-attempt log for autonomous engineering
workflows.

It must let a fresh agent answer, without relying on a previous session's
memory:

1. What work is actionable now?
2. What has already been launched?
3. What is blocked, missing, or waiting on a person?
4. What changed while the agent was away?
5. What downstream work is affected by a change?

Alder coordinates work. It does not perform the work, choose models, encode a
project's engineering doctrine, or attempt to be a general scheduler.

## Product boundary

The durable model contains six core structures:

- events;
- handoffs;
- work;
- dependencies;
- attempts;
- acceptance checks.

Work may also carry a durable question when progress requires an asynchronous
human decision. Questions are subordinate to work rather than a second kind of
work or a general messaging system.

It also records the driving loop itself: passes, which are the loop's run
records, and a small set of loop controls. Work is to an attempt what the loop
is to a pass. These are namespaced separately from the six core structures
because they describe how the project is being driven rather than what the
project owes. [LOOP.md](LOOP.md) defines them.

Events are stored in Git. Current state, including observations from external
systems, is a deterministic fold of those events into a local SQLite database.
Observer execution diagnostics remain local; an observer's reported level is
durable only when it changes the folded picture.

The primary user is an agent driving a project. A repository skill may call
that agent the leader, but Alder stores no leader, generation, lease, or
writer role. The human operates Alder through an agent or another chat
surface; the human should not need a shell.

Every command also accepts `--json`. This is Alder's stable agent-facing
surface for reads, mutations, and expected errors; human-readable output
remains the default. V0 has no YAML, TOML, or generic output-format switch.

## The driving loop

One bounded pass is:

1. Read `alder status`.
2. Integrate submitted handoffs.
3. Refresh observations and reconcile active attempts.
4. Resolve completed, failed, or missing attempts.
5. Surface open questions, blocked work, and observation failures.
6. Start the highest-priority actionable work that fits current judgment.
7. Stop.

Alder supplies the state needed for this loop. It does not contain the loop's
engineering judgment.

A pass is that iteration made durable. `alder loop wake` opens one before an
agent is prompted and `alder pass end` closes it with an outcome, an optional
report, and an optional request to be woken again. Alder records passes; it
does not run them. Deciding *when* to wake an agent belongs to a small external
driver whose read surface is deliberately tiny, and deciding *what* to do
belongs to the agent.

## Principles

### Admission is a decision

Nothing is Alder work until a writer runs `alder work add`. Alder does not
authorize one writer over another. Repository skills decide which agent may
admit work and should direct workers and side sessions to the handoff path.

Alder provides a small asynchronous inbox for side sessions:

- `alder handoff add` records a title, artifact reference, and optional note;
- the handoff is `submitted`, not work, and does not participate in readiness
  or dependencies;
- the driving agent drains submitted handoffs before selecting more work;
- `alder work add --handoff` atomically creates admitted work and marks the
  handoff `integrated`;
- `alder handoff withdraw` retires a submitted handoff without admitting it,
  marking it `withdrawn`.

These are the only three handoff states, and `integrated` and `withdrawn` are
both terminal. If integration cannot validate, the handoff remains submitted
and visible.

Submission is intentionally weaker than admission. A repository-tuned handoff
skill should invoke it only after an explicit human request such as "handoff
to leader." Alder does not pretend that a free-form claim of human delegation
is enforceable; even an unwanted submission cannot enter the work graph until
a writer explicitly admits it with `work add`.

### IDs carry context

Each repository chooses a short ID prefix, such as `hm` for Harmony. Work uses
a generated ID such as `hm-9a1`; attempts and questions extend their work ID
with a never-reused ordinal, such as `hm-9a1-attempt-1` and
`hm-9a1-question-1`. A handoff has no work yet, so it uses a
repository-scoped ID such as `hm-handoff-f27`.

### Graph changes are atomic

One decision may add or edit many work items. Alder validates the
resulting graph and records the whole decision as one event, so a crash cannot
leave half of a re-plan durable.

The ordinary `work add` and `work edit` forms remain convenient for one item.
Structured input lets the same commands perform larger changes. It is an input
format, not a durable batch, draft, or transaction lifecycle.

Before committing a change document, the caller may run `status --with` or
`next --with`. Alder applies the document to an in-memory projection and runs
the ordinary query against that hypothetical state. This is bounded
introspection over one real change, not a separate impact report or a general
planning language.

### Record intent before effects

An attempt is recorded before its worker is launched. The external worker is
stamped with the attempt ID, then its external handle is attached to the
attempt with `alder attempt edit`.

This makes both crash windows repairable:

- recorded attempt, no worker: launch one for it, or end the attempt as
  `not_started` if the work no longer wants one;
- worker exists, handle was not recorded: find it by attempt ID and attach its
  handle with `alder attempt edit`.

A pass follows the same rule. `alder loop wake` is appended before an agent is
prompted, and it returns the pass ID the prompt carries:

- recorded pass, agent never prompted: the pass stays open until someone ends
  it as `crashed` or, past its time budget, `timeout`;
- prompted agent, no record: impossible, because the prompt carries the pass ID
  the wake produced.

An open pass rejects the next wake, so the repair is forced rather than
optional, and any reader with a terminal can perform it.

### Attempts outlive callers

An attempt is not owned by the process or agent that started it. A later
caller adopts every external execution whose attempt is still active. An
attempt is invalid only when Alder has ended it.

### Writers use optimistic concurrency

Alder has no durable leader or writer generation. Each mutation reads one log
head, validates against the projection for that head, and conditionally
appends to it. If another writer advances the log before the append, the
mutation changes nothing and the caller must reread and reconsider it.

Ordinary mutations are never silently replayed against the new head.
`handoff add` is the exception because submission is inert and uniquely
identified, making reconsideration automatic and safe.

A caller can only reconsider a loss it recognizes, so losing is reported as a
fact about the command — nothing was appended, and here is the event that was
not written — and never in a form that reads as a receipt.

The expected head is internal to the command. V0 has no public `--if-head`
option. A repository skill that wants one operational leader must ensure that
role above Alder; an older agent that later reads current state is technically
another valid writer.

### One active attempt per work item

`alder work start` rejects work that already has an active attempt. Starting
again requires explicitly ending that attempt with `alder attempt end`, so a
second launch cannot silently hide the first.

One open pass per loop is the same rule at the loop's scale. `alder loop wake`
rejects a second pass while one is open, so two drivers cannot quietly run two
leaders.

### Completion criteria precede execution

Checks belong to the work item and are set at admission or by an explicit
`work edit` while the work is not running. Every declared check gates ordinary
completion, and a later attempt cannot weaken that contract.

### Outcomes matter

Only successfully finished work satisfies a dependency. Dropped work does not.
Dropping or reopening a prerequisite surfaces affected downstream work. If it
would invalidate work with an active attempt, Alder rejects the change until
those attempts are resolved; v0 has no confirmation or override flag.

### Time is evidence

Alder records and exposes progress and observation times. It does not classify
an attempt as stale or define a threshold for when work has stopped moving.
That judgment belongs to the driving agent.

### History should not force item proliferation

Attempts preserve execution history. Work may be reopened with a reason when
the underlying requirement is still the same. A successor item is created
only when the work itself has changed identity.

## Execution and environment boundary

`alder work start` records an attempt and returns its ID. A repository-tuned
skill then launches the work, stamps the external execution with that ID, and
attaches an external handle to the attempt with `alder attempt edit`.

A handle has the form `<kind>:<opaque-value>`, such as:

- `tmux:box-17/alder-hm-9a1-attempt-1`
- `codex:019f...`
- `github-actions:owner/repo/run/4212`

The kind selects a configured observation command. The value is opaque to
Alder. Removing an observation command never invalidates a stored handle or
prevents replay.

An attempt also has an open-ended metadata map. Skills may record useful
provenance such as engine, host, model, or toolchain. Alder stores and displays
that metadata but never makes core state transitions depend on its keys.

Observation commands are small shell pipelines over tools already present in
the environment. Each command owns one observer name and prints a normalized
JSON array of current levels keyed by subject and field. `alder refresh`
appends only changed levels and retires omitted keys, so the shared log is the
current observation picture. `alder reconcile` compares that picture with
durable attempts and proposes repairs; it never acts on a provider.

Alder runs each command with fixed pipefail semantics, a 20-second timeout per
execution, and up to three retries after the initial execution. Only an
exit-zero, valid, complete result can report a current level. Exhausted
failures, timeouts, malformed JSON, and invalid result sets append no new
belief.

Finite capacity is initially enforced by the repository's allocator or the
external platform. The cloud provider remains authoritative for what exists
and what it costs. Alder can surface observed capacity, sessions, and cost
metadata without claiming to allocate or account for them. If Alder later
needs to choose among and reserve scarce resources itself, that capability
must earn an explicit extension rather than hiding behind metadata keys.

## Storage

Each project has one Alder log in a Git repository. Runtime bindings can be
sensitive, so a private repository is the safe default.

The project root contains one user-facing manifest at
`.alder/config.json`:

```json
{
  "schema": "alder.config.v0",
  "prefix": "hm",
  "store": {
    "remote": "origin",
    "ref": "refs/heads/alder"
  },
  "observers": []
}
```

The manifest contains repository identity, shared-log location, and executable
observation commands. The prefix becomes immutable after the first work item
or handoff is appended; later commands verify it against the log. Observer
configuration may change without appending an event.

The configured remote ref is authoritative, not a local branch or
remote-tracking ref. The intended v0 deployment keeps that ref in a private
GitHub repository, but Alder reaches it through standard Git transport rather
than the GitHub API. Every ordinary read and mutation queries the remote head
and fetches its objects when they are not already local. A mutation is durable
only after its event commit is successfully pushed to that ref; creating the
commit locally is only preparation for the append.

The dedicated ref must allow authenticated direct fast-forward pushes. It
should reject force pushes and deletion, and it cannot require a pull request
for each event. Git authentication and authorization remain outside Alder.

`alder init --prefix <prefix>` creates or verifies this configuration. It is
idempotent: rerunning it with compatible identity and store arguments
preserves the existing file and observers, ensures the same usable state, and
does not append or create another commit. A conflicting prefix, remote, ref,
schema, or incompatible existing log fails without rewriting anything.

The initial storage contract is deliberately narrow:

- the append-only event log is authoritative;
- one logical mutation is one event and one Git commit;
- a work-change event may contain many operations, all applied or rejected
  together;
- reads establish the current head from the configured remote;
- appends use an expected remote head;
- a successful remote push is the append's durability point;
- every event has a client-generated ID, making unknown append outcomes
  resolvable;
- on a head conflict, the client asks the caller to reread and reconsider the
  action rather than blindly resubmitting it;
- `state.db` is local and disposable;
- derived table dumps are not committed in v0.

The event log itself is the Git-readable diff. If that proves insufficient in
real use, a small generated summary can be added later without changing the
state model.

The implementation uses a small `Store` trait with three shared-log
operations: read the current head, read the ordered events at an immutable
head, and conditionally append one event to an expected head. Git is the v0
durable implementation. An in-memory implementation exists for isolated
business logic tests; this internal seam is not a promise of user-selectable
storage backends.

SQLite is a memoized projection, not a second participant in a mutation. A
command compares the projection's recorded head with the log head before
querying it. If they differ, Alder rebuilds all durable projection tables from
the complete log in one SQLite transaction and records the head in that same
transaction. V0 performs no incremental projection updates, and a successful
append deliberately leaves the projection out of date for the next command to
rebuild.

The observation snapshot is a fold of durable `observation.*` events. A
successful `refresh` appends only levels that change that picture; an unchanged
run leaves both the log and SQLite projection untouched.

An observer name has one writer. A successful run is a complete snapshot for
that observer, and its omissions retire keys — so two machines running the
same observer name would retire each other's keys on every refresh. Multi-host
observation needs distinct observer names; nothing else about the retirement
rule is host-aware.

An ordinary read or mutation must establish the current shared-log head. If it
cannot, Alder returns `store_unavailable`; it never silently presents the
local projection as current. An unavailable observation command instead
appends no replacement belief.

## Non-goals for v0

V0 does not include:

- resource lifecycle or desired configuration;
- budget accounting or reservations;
- model selection, failover, or authority tiers;
- committed full-roadmap plans;
- general hypothetical planning;
- durable graph-change drafts, transactions, or rollback plans;
- reviewer-seat or judge orchestration;
- project runbooks or vocabulary profiles;
- public dashboard publication;
- durable leader roles, generations, or leases;
- generic storage backends;
- committed SQLite projections or snapshots;
- engine validation, or any driver diagnostic as a durable event.

An attempt may record an external handle and open-ended metadata. They provide
reconciliation and provenance without becoming a resource scheduler.

The handoff inbox is not a general proposal tracker. It exists only to deliver
explicit asynchronous handoffs for later admission.

## Success criterion

V0 succeeds when it can drive a representative Harmony week, survive the
incident cases in [ACCEPTANCE.md](ACCEPTANCE.md), and let a fresh agent
continue without human reconstruction of prior work.
