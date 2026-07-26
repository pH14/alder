# Alder v0 acceptance

The design is not frozen until it passes a paper replay of real work. This
document defines the minimum cases and the evidence each case must produce.

## V0 acceptance cases

### A1. Drive known work

Given admitted work with dependencies and checks:

- `next` returns only actionable work;
- `start` records an attempt before launch;
- progress and evidence survive caller restart;
- `finish` refuses incomplete checks;
- finishing one item updates the ready frontier.

### A2. Concurrent writers

Have two writers validate ordinary mutations against the same log head and
attempt to append them concurrently.

- each writer must obtain that head from the configured remote rather than
  trusting a local branch or remote-tracking ref;
- exactly one append may advance that head;
- the winning append is not durable until its event commit has been pushed to
  the remote ref;
- the loser must receive a structured head conflict and change nothing;
- the losing mutation must not be replayed automatically against the new
  state;
- after rereading, the loser must see the winner's event and may form a new
  decision;
- every existing active attempt must remain intact.

Repeat with a uniquely identified `add handoff` submission. Its inert append
may be reconsidered automatically and must not be duplicated.

The expected head must remain internal to each command. No mutation accepts a
public `--if-head`, and Alder exposes no leader takeover or generation.

Run the protocol against a bare Git remote in automated tests and perform a
smoke test against a private GitHub repository. GitHub must be reached through
the configured Git remote, without requiring a provider-specific API. The
dedicated ref must accept direct fast-forward pushes and reject history
rewrites.

### A3. Crash before launch

Record `attempt.started`, then simulate a process crash before the worker is
created.

Reconciliation must find a starting attempt with no matching worker and allow
it to end as `not_started`.

### A4. Crash after launch

Record `attempt.started`, create a stamped external worker, then crash before
recording its handle binding.

Reconciliation must find the worker by attempt ID and allow a later caller to
attach its handle with `edit attempt` and adopt it.

### A5. Unknown append outcome

Simulate a push whose result is not returned to the client.

The client must search the remote suffix by event ID:

- if present, report the original success;
- if absent, report the current head and require caller reconsideration;
- never manufacture a second attempt while resolving the unknown outcome.

### A6. Reject duplicate launch

With one active attempt, run `start` again.

The command must change nothing and identify the active attempt. To run the
work again, the repository skill must stop the old external execution, confirm
the result, record `alder edit attempt <attempt> --end <outcome>`, and only
then create a new attempt. If the handle cannot be observed, Alder must not
imply that the external execution was fenced.

The first start for work `hm-9a1` must produce `hm-9a1-attempt-1`. After that
attempt ends, the next start must produce `hm-9a1-attempt-2`; an ordinal is
never reused, including when an attempt ended as `not_started`.

### A7. Reject late evidence

End an attempt, begin a new attempt, then submit a delayed check result against
the old attempt.

The `alder edit attempt` must be rejected and must not affect the new
attempt's checks.

### A8. Preserve the completion contract

Create work with two checks, fail and end its first attempt, and start a second
attempt.

The second attempt must still require both checks. Every declared check must
gate ordinary completion; `start` may not remove them.

### A9. Drop a prerequisite

Create B depending on A, then drop A.

B must remain non-actionable. Alder must report the dependency impact rather
than treating any terminal state as success.

Repeat with an active attempt for A. `drop` must require that attempt's ID and
a non-success outcome, then end the attempt and drop A in one append. It must
reject a mismatched attempt, a missing outcome, or attempt fields when A has no
active attempt. It must not claim to terminate the external execution.

### A10. Re-scope work

While work is not running:

- add newly discovered work;
- change dependencies;
- amend checks;
- reopen an earlier item when its requirement remains the same.

Every newly allocated work ID must use the repository's immutable configured
prefix.

The ready frontier and introspection queries must follow the edits without
creating successor items merely to preserve history.

Resource-less forms such as `alder edit hm-9a1` and
`alder edit hm-9a1-attempt-1` must be rejected rather than inferred.

Finish A, start downstream B, and then try to reopen A. Alder must reject the
reopen and return B's active attempt. After that attempt is resolved, reopening
A may proceed and must make B non-actionable. There is no confirmation or
override flag.

### A11. Silent worker death

An active worker disappears without producing an event.

A fresh observation must report `absent`; Alder must surface the attempt as
needing reconciliation. Ending it as `lost` returns its work to open. To leave
the work blocked, block the work first and then end the attempt.

Status must expose the relevant progress and observation times, but Alder must
not derive a `stale` state or require a configured inactivity threshold. The
driving agent decides whether elapsed time indicates a problem.

### A12. Observation outage

Make an observation command or code host unreachable.

The observation must be `unknown`, not `absent`. Reconciliation must not
suggest a destructive repair based on the outage alone.

### A13. External completion

Complete work outside Alder while it is open or blocked.

The caller must be able to mark it finished with external evidence. The event
must remain distinguishable from ordinary checked attempt completion.

### A14. Human question

Ask a question against open or running work. The append must create the
question and block the work atomically; it must not terminate an active
attempt. Starting another attempt must be rejected.

The first question against `hm-9a1` must be `hm-9a1-question-1`. Revising its
answer retains that ID; asking a distinct later question produces
`hm-9a1-question-2`.

Restart the driving agent, answer the question, and start another fresh agent.
The answer must not silently unblock the work. Every caller must see:

- the pending or current answer;
- the affected blocked work;
- all prior answers if the human revised the decision.

Unblocking must be rejected while any attached question is unanswered. Once
the questions are answered, the driving agent may incorporate the decision
and explicitly return it to open with
`alder edit work <work> --unblock`.

### A15. Explicit admission

Have workers and side sessions produce many possible follow-ups.

None becomes work merely by being mentioned, observed, or submitted as a
handoff. A side session explicitly told "handoff to leader" may create a
submitted handoff, but that handoff must not affect work counts, readiness,
dependencies, or attempts.

Only an explicit `add work` invocation may turn a requirement or handoff into
work. Repository skills must reserve that invocation for the agent responsible
for admission; Alder itself must not claim to enforce writer roles.

### A16. World and record disagree

Exercise each disagreement:

- active attempt, no external handle;
- starting attempt, matching unbound handle;
- ended attempt, still-running external handle;
- active attempt, mismatched attempt identity on its handle.

Each must be visible and repairable through normal commands. None may require
editing event history or the SQLite database.

### A17. Hypothetical introspection

Create a valid graph-change document that adds work and rewires existing
dependencies. Query both `status --with <changes>` and `next --with <changes>`.

- each must return the same domain result the ordinary command would return
  after applying the change, using local names for new work;
- each must identify the base head and clearly mark the output hypothetical;
- neither may append, commit, allocate durable work IDs, or persist the
  hypothetical change in SQLite; an ordinary head-mismatch rebuild is allowed;
- `status --with` must use current external observations;
- invalid input must be rejected under the same rules as a real append;
- a later real append must reread and revalidate its current head.

There must be no top-level `impact` command and no `edit work --dry-run`. V0
does not accept preview-only terminal outcomes or multi-step scenarios.

### A18. Phone check-in

From a chat surface, ask:

- what is running?
- what is stuck?
- what needs me?
- what can start next?
- what changed since the last check-in?

The serving agent must answer from named Alder reads, with observation
freshness, rather than reconstructing a narrative from its context window.

### A19. Environment inventory

Configure a `nimbus` observation command that reports several provisioned
boxes, including one not associated with an Alder attempt.

`refresh` must retain the read-only inventory locally. `reconcile` must surface
the unbound box and its provider-reported cost metadata without creating work,
an attempt, or an accounting event.

If Nimbus is unreachable, its objects become unknown rather than absent.

### A20. Handle variety

Attach tmux, Codex, and GitHub Actions handles to attempts with
`edit attempt --handle`, and discover unbound Nimbus handles.

All must use the same core handle representation. Provider-specific values
remain opaque, and provider metadata must not alter readiness or completion
rules.

Adding a new observable kind must require only a configured command producing
the normalized JSON contract, not a provider plugin or Rust adapter.

Claude Code running inside tmux must work as a tmux handle with metadata; it
must not require a new core type.

Trying to replace or clear an attached handle must be rejected. Attaching the
first handle records `attempt.bound`; there is no public `bind` command.

### A21. Missing observation command

Record a handle on an attempt, then remove its observation command.

The log must still replay, and the attempt and handle must remain visible.
Status must say that no fresh observation is available rather than treating
the handle as invalid, absent, or ended.

### A22. Observation execution and refresh

Change the external environment after a refresh.

- `refresh` must update local observations without appending;
- ordinary `reconcile` must refresh before comparing;
- `reconcile --no-refresh` must use the existing inventory and show its age;
- `reconcile` must never append an event or act on a provider;
- a caller that accepts a suggested repair must invoke its ordinary mutating
  command separately.

Exercise one configured `list` command for each result:

- an exit-zero valid array is treated as one complete snapshot;
- returned values are present and an omitted durable handle of that kind is
  absent;
- pipeline failure must be visible even when its final command would
  otherwise exit successfully;
- a nonzero exit, timeout, malformed JSON, duplicate value, or conflicting
  attempt ID must invalidate the complete result;
- each failure receives three retries after the initial execution, with a
  20-second timeout for each execution;
- failed standard output must be discarded, and four failed executions must
  make the kind unknown without inferring absence;
- a timeout must terminate the complete command pipeline.

The first valid retry result must replace the current snapshot exactly once.
An observation command may not receive event or handle values through shell
interpolation.

### A23. Disposable projection and diagnostics

Build SQLite at head `H`, append an event through the shared log without
updating SQLite, and then run a query.

- Alder must detect the head mismatch and rebuild every durable projection
  table from the complete ordered log;
- the rebuilt tables and represented head must commit in one SQLite
  transaction;
- local observation tables must survive the rebuild;
- no incremental projection update path is required.

Log inspection, database rebuild and verification, raw read-only queries, and
observation diagnostics must exist only under:

- `alder debug log`
- `alder debug db`
- `alder debug query`
- `alder debug observations`

There must be no generic append command and no top-level `log`, `db`, or
`query` or `observations` namespace competing with the ordinary workflow.

`alder debug observations` must expose configured and unconfigured kinds plus
the latest execution summary. Its `<kind> --run` form must show executions,
validation, normalized output, and bounded stderr without updating observation
tables.

### A24. Asynchronous handoff

While the driving agent is busy in a long-running turn, run
`alder add handoff` from a side session.

- its ID must use the repository-scoped form `<prefix>-handoff-<token>` and
  must not imply a work relationship before integration;
- submission must complete without waiting for the driving agent;
- submission must reject priority, dependency, and check fields;
- the handoff must be durable and visible as `submitted`;
- restarting or replacing the driving agent must not lose it;
- `next` and `ready` must ignore it;
- `alder add work --from-handoff` must atomically create exactly one work item
  and link its ID;
- repeating either append by event ID must not duplicate the handoff or work;
- the resulting state must be `integrated`, with no intermediate handoff
  state.

If integration fails validation, the handoff must remain submitted. There
must be no top-level `handoff` command namespace, and resource-less `add` must
be rejected rather than inferring intent from omitted fields.

### A25. Atomic graph change

Create one structured change that adds several work items, refers to the new
items by local name, and rewires several existing dependencies.

- `status --with` and `next --with` must validate the complete change without
  appending;
- applying must allocate stable work IDs and resolve every local reference;
- the durable result must be exactly one `work.changed` event in one Git
  commit;
- replay must expose only the state before or after the change, never an
  intermediate graph;
- one invalid operation must reject the entire change;
- a forbidden dependency or check edit to active work must reject the entire
  change;
- a head conflict must reject the whole mutation for caller reconsideration;
- resolving an unknown append outcome by event ID must not duplicate the
  event or any newly added work.

Exercise `add work --from` separately and verify that it accepts several
additions atomically but rejects an `edit` section. `edit work --from` must
require at least one edit, leaving no synonymous command for an additions-only
document.

### A26. Structured command output

Run representative reads, successful mutations, rejected mutations, and
diagnostic commands with `--json`.

- every command must accept the same global flag;
- standard output must contain exactly one valid JSON document;
- every document must carry a command-specific schema identifier;
- mutation results must include their durable IDs, event ID, and resulting
  head;
- expected errors must use a stable code and structured context while
  returning a nonzero process status;
- no table formatting, color, progress indicator, or surrounding prose may
  contaminate the document;
- repeated execution against the same state must preserve field types and any
  semantically meaningful array order.

Without `--json`, the same commands retain concise human-readable output.
There is no YAML or TOML output and no generic output-format option in v0.

### A27. Idempotent initialization

In an uninitialized project, run:

```text
alder init --prefix hm
```

- Alder creates `.alder/config.json` with the v0 schema, `hm` prefix,
  default store remote and ref, and an observers array.
- Alder validates that the selected shared log is absent or compatible.
- Initialization does not append a domain event.

Run the same command again:

- it succeeds and reports that Alder is already initialized,
- it preserves `.alder/config.json` byte for byte, including observer edits,
- it does not append an event, and
- it does not create an additional Git commit.

Run `init` with a conflicting prefix, remote, ref, schema, or incompatible
existing log:

- it fails with `config_conflict`, and
- it leaves the manifest and shared log unchanged.

If the manifest is absent but a compatible Alder log already exists, `init`
may adopt it only after validating its format and prefix. After the first work
item or handoff exists, changing the prefix is rejected. Editing observer
configuration does not append a domain event.

If the shared Git head cannot be read, an ordinary read or mutation fails with
`store_unavailable`; Alder does not present cached SQLite data as current. A
failed observation command instead makes only that observer kind `unknown`.

## Paper replay gate

Before freezing event bodies or SQLite tables, replay:

1. one representative Harmony week;
2. the worst dispatch/restart incidents;
3. one campaign with substantial re-scoping;
4. one case where external reality contradicted recorded state;
5. one case with a large number of agent-proposed follow-ups;
6. one turn that atomically adds and rewires substantial work.

For every real action, record:

- the Alder command that would represent it;
- the durable event produced;
- the resulting `status`;
- whether any information had to be invented;
- whether the action required a concept outside the documented model.

An awkward command is a CLI bug. An event that cannot represent the action is
a model bug. A desired optimization is not automatically a v0 requirement.

## Explicitly deferred cases

The paper replay may record, but v0 need not automate:

- allocating several scarce resources to one attempt;
- desired versus observed machine configuration;
- budget reservation and spend attribution;
- batching work around expensive reconfiguration;
- engine quota and vendor-routing policy;
- blind review fan-out and judging;
- public dashboard publication;
- durable writer roles, leadership generations, or leases;
- arbitrary multi-step hypothetical plans.

Opaque handles and attempt metadata should preserve enough evidence to revisit
these later without pretending they already exist.

## Freeze criteria

The v0 model may freeze when:

- every acceptance case has a written replay;
- the representative week requires no manual state outside Alder;
- a fresh agent can resume using only `status`, `show`, and reconciliation;
- no candidate or handoff becomes work without an explicit `add work`;
- the event vocabulary has no synonym pairs;
- every durable field is required by at least one acceptance case;
- deleting any table or command would fail a named case.

The last two criteria are the guard against rebuilding the discarded design.
