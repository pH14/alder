# Reviewing alder

This file is the review lens for this repository: what "correct" means here,
stated in the terms the project actually argues in. It is not the contract.
The contract is [docs/v0](docs/v0) — `README.md` for purpose and boundaries,
`MODEL.md` for the state model, `CLI.md` for the command surface, `LOOP.md`
for the driving loop and its daemon, `ACCEPTANCE.md` for the cases a change
must survive. Read the relevant one before calling something a defect. A
change that contradicts those documents is a defect; a change they do not
cover is a design question, and saying so is more useful than guessing.

Weigh every finding by what it costs **a reader of the log who was not
there**. That reader is the whole point of the project.

## The log is the authority, and `alder` is its only writer

Durable truth is the event log in the store ref. SQLite is a memoized fold of
it and is disposable; observations are local input and never become ledger
facts. Flag anything that treats the projection as a source: a read that skips
head synchronization, a mutation that writes SQLite as part of committing, an
incremental projection update, or a fallback to a local branch or
remote-tracking ref when the remote is unreachable — that case is
`store_unavailable`, not a licence to serve stale state as current.

Nothing but the `alder` CLI appends. `alderd` reaches the log only by running
`alder`; a driver, script, or test helper that touches the store directly —
its own `git`, its own SQLite — is a second writer no matter how read-only it
looks today.

## Level-triggered, never event memory

Every decision is made from current state, freshly read. Flag anything that
remembers what it saw last time and acts on the difference: a count read as a
delta, a "changed since" cursor driving behaviour, a cached section reused
instead of refetched, or a flag that must be cleared by whoever served it —
pending-ness here is derived by comparing sequences, which is exactly why two
writers cannot disagree about whether a request was already served.

The test: a fresh agent with no memory of any earlier session must reach the
same decision from the same log.

## Intent before effects

The record of an intent is appended *before* the effect it describes:
`attempt.started` before a worker is launched, `loop wake` before a leader is
prompted. Flag the inverted order, and flag a new effect that acquires no such
record. Both crash windows must stay repairable, and the repair must be
sayable in ordinary commands.

## Repairs are adoptive over their own residue

A repair takes over what a previous run left rather than duplicating it:
`spawn` adopts an open unbound attempt instead of opening a second one, a
later caller adopts a live execution whose attempt is still active, `init`
re-run against a compatible manifest changes nothing at all. Flag any repair
that, run twice, leaves two of something — two attempts, two sessions, two
passes — or that fails on its own leftovers instead of converging on them.

## One at a time

At most one active attempt per work item; at most one open pass per loop. The
parent creates, because only the parent knows whether another is allowed; the
record closes itself, because by then it exists. Flag a second launch that
hides the first, and flag any path that ends a durable record before the
external thing behind it is confirmed stopped — an ended attempt with a live
worker is untracked, which is worse than an honest orphan.

## Sleeping is not coordination

Waiting is not a synchronization primitive. Flag a sleep used to let something
come up, to let a session settle, or to space retries that should be
conditional: dispatch types nothing at a session and waits for no engine to
boot. A driver's own schedule is pacing, and a bounded timeout on a command
that can hang is a limit — neither is this.

## Model and effort are always explicit

Every engine invocation names its model and its reasoning effort. A CLI or
config default is not an answer: it runs at an unknown model, records nothing
about it, and reads exactly like a successful one. Flag an invocation that
leaves either to a default, an alias in place of a full model name (an alias
moves under you), and an unknown rung that falls through to a default instead
of erroring before anything launches.

## Workers never push and never touch remotes

A worker commits to its own branch in its own worktree. It does not push,
fetch, add or change a remote, force anything, or write outside that worktree;
only the leader merges, and only locally. Flag anything on a worker's path,
including a script handed to it at spawn, that can reach a remote — except
`alder` itself, whose whole job is appending to the store.

## The CLI grammar is not a style preference

Queries are global and take no noun: `status`, `next`, `show`, `refresh`,
`reconcile`. Mutations name their noun first, and the noun is the ID type.
`edit` never changes state; a transition is a verb. Flag a command that breaks
any of those, or that infers the resource from an ID's shape. Flag a `--json`
change that is not additive: field names, types, explicit nulls, and
meaningful array order are the agent-facing contract.

## Vocabulary is kept minimum

The event vocabulary has no synonym pairs, every durable field is required by
at least one acceptance case, and deleting any table or command should fail a
named case. Flag a new event type, field, flag, or state that an existing one
already covers, and flag a second name for something already named. Growth is
the failure this project is most exposed to.

## Tests and prose

Behaviour worth a rule is worth a test that fails without it. Flag a change to
durable behaviour that no test pins, and a test that asserts a mock was called
rather than what a caller would observe. Flag a comment that says what the code
does — the comments here say why, and a stale "why" is as much a defect as
wrong code. A test that spawns tmux without an isolated server (`-S`/`-L`, no
inherited `TMUX`, teardown by exact session name) is a defect even when it
passes: it can kill every worker on the machine.
