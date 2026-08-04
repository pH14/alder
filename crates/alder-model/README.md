# alder-model

A [stateright](https://docs.rs/stateright) model of Alder's protocol core.
It checks the wake protocol under **every interleaving** of a small cast —
the shared log, the daemon's decide loop with its machine-local notes, the
leader engine, and an optional second writer (a phone session) — including
crash transitions. It is a dev-only crate: nothing depends on it, and it
ships nothing.

Run it:

    cargo test -p alder-model                 # the whole check suite, ~1s
    cargo test -p alder-model -- --nocapture  # with explored state counts

## What is real and what is modeled

The crate depends on `alder-log`, `alder`, and `alderd` so that the *data*
and the *judgment* are the shipped code, and only the *sequencing* is
modeled. If the fold or the decide rules change, this model re-checks the
new behavior with no edits here.

| Concern | Source of truth |
| ------- | --------------- |
| Records, drafts, heads | `alder_log::{Record, RecordDraft, Head}` |
| Append semantics | `alder_log::MemoryLog` (states replay through its public `append`) |
| Event payloads and codec | `alder::domain::{EventPayload, encode_draft, decode_record}` |
| Log interpretation | `alder::domain::ProjectState::fold` on every state |
| The daemon's read | `alder::app::loop_section` + `alderd::loop_state::LoopState::from_status` |
| The daemon's judgment | `alderd::decide::{decide, resolve_engine, session_action, rotate_pending}` over real `Notes` |
| The safety predicates | `alder::domain::invariants` — the same sentences the crash simulator asserts |
| **Modeled by hand (the drift surface)** | each actor's atomic steps: where a process can be interrupted between reading and writing, and what a crash erases |

The safety predicates are shared on purpose. Two harnesses check this
protocol from opposite directions — this model explores every interleaving of
a small cast, the simulator tears every effect every way its footprint allows
— and they check the same things about a log and the state it folds to.
Stated twice they drift invisibly, both green while meaning different things
by "correct"; stated once in `alder::domain::invariants`, both assert the
same sentence.

The central claim is stated by the shape of the model itself: **the daemon
has no append step, because it appends nothing.** A wake is an injection
plus a machine-local notes write, both outside the log. The log carries
statements about work, never about its own readers, and the property that
pins it — the shared `mentions_no_readers`, plus "no record carries the
daemon's actor" — holds in every reachable state of every scenario.

## The actors and their atomic steps

- **The log** is the shared durable state: a `Vec<Record>` replayed through
  a real `MemoryLog` on every transition.
- **The notes** are one machine's durable state about itself: the last head
  the daemon acted on, and when. They survive a daemon crash; a scenario may
  erase them (`NotesLost`), which is the "machine lost `.alder/`" fault.
- **The daemon** takes three steps per wake, mirroring `alderd`'s driver:
  poll-decide-reconcile (session restarted *before* anything else, so a
  crash re-rotates rather than losing a rotation), then the injection
  (`tmux_send_keys`), then the notes write. The order of the last two is the
  duplicate-wake window: a crash between them leaves a delivered wake that
  nothing anywhere records, so the restarted daemon delivers it again.
- **The leader**, when woken, reads the fold and acts: appends one work
  statement (budget-bounded), or finds nothing demanding and idles. Or its
  session dies.
- **The phone** optionally requests a rotation, pauses the loop with a
  stated reason, or appends a work statement of its own — every append is
  the wake rule firing from a second writer.

Time is not a modeled dimension: every event carries the same instant, so
the daemon's time triggers reduce to their untimed cases — fresh notes fire
immediately, and nothing else is ever "due". Deferral deadlines
(`review_at`) are therefore out of this model's scope; the CLI suite and the
driver tests cover them.

## Core properties

1. **The log never mentions its own readers.** No reachable history holds a
   pass event or a daemon-actor record, whatever crashes or duplicate wakes
   occur. Asserted in every state of every scenario.
2. **Missed and duplicated wakes are harmless.** The daemon, the session,
   and the notes file each fail at every point. A crash between the
   injection and the notes write strands a delivered wake nothing recorded
   (`sometimes`, so the window is actually entered); the same head is then
   woken twice (`sometimes`); and every `always` property holds through
   both, because a wake carries no work of its own — the leader reads the
   fold either way. Checked in
   `crashes_cost_duplicate_wakes_and_nothing_else`.
3. **A consumed rotation was performed first.** The driver reconciles the
   session before it notes the head, so a rotation request is only ever
   consumed (noted past) after a restart answered it; a crash between the
   two merely re-rotates, and a notes loss re-pends an already-honored
   request as a redundant — harmless — second rotation. The fold-side
   derivation is held against a ghost mirror, and the mirror is itself
   audited against the fold in every state. The old concurrent-writer
   swallow is gone by construction: a wake is not an append, so there is no
   racing wake to consume a request by log order.
4. **Liveness: every terminal state is progressing or blocked-and-named.**
   Because every transition either grows the log or advances a monotone
   counter or latch, the state graph is acyclic and finite, so "all maximal
   paths end recovered" is exactly "every fair execution eventually
   recovers" within the bounds. Recovered now means: the log folds, and a
   paused loop states its reason. There is no "stranded open run" clause
   left, because nothing durable can be open — which is the design.

## State-space bounds

Budgets bound the space: one leader statement, one phone statement, one
rotation request, one pause, and per-scenario fault counts. Counts from
`cargo test -- --nocapture`:

| Scenario (test) | Faults and writers | Unique states |
| --------------- | ------------------ | ------------- |
| lone daemon | none | 13 |
| phone writer | phone statement + rotation + pause | 1,191 |
| faults everywhere | rotation + 1 daemon crash + 1 session crash + 1 notes loss | 4,506 |

Complete exploration of all three scenarios takes about a second.

Each test asserts its own number exactly, so this table and the suite are one
claim rather than two: a count that moves fails a test, and the number here is
meant to be changed in the same commit that explains why. That assertion is
load-bearing rather than decorative. A property catches a model that *reaches*
a bad state; nothing but the size of the space catches a model that quietly
stopped reaching a good one, or started reaching states its budgets say it
cannot — a fault injected past its budget, a counter that stopped counting so
two states hash alike. Each of those leaves every property green and the
space a different size.

## Checking the checker

This crate is in the workspace mutation sweep with no exclusion, which asks a
sharper question than "do the properties bite": *who checks the checker?* A
checker has two failure modes an ordinary suite does not, and both stay green
under exploration, because a check that is never made has no counterexample
to find.

- **A question nobody asks.** The flags decide which properties a scenario
  registers, and an unregistered `sometimes` cannot fail — a gate that stops
  registering one makes the run go green *faster*. So
  `each_scenario_registers_exactly_the_properties_its_flags_ask_for` pins the
  set, in order, for all three scenarios.
- **A question that answers itself.** A `sometimes` property claims the model
  can *reach* something, and one that already holds in the initial state
  claims nothing: it is witnessed by the empty log, and it stays witnessed
  however the protocol breaks. So
  `no_sometimes_property_is_witnessed_before_the_model_moves` evaluates every
  registered `sometimes` against the initial state and requires it false.
- **A helper that answers too readily.** `recovered` is consulted by exactly
  one property, and a wrong answer makes that property vacuous rather than
  false — saying yes too readily leaves liveness green over a silently
  stopped loop. Its sentences are pinned by unit tests in `lib.rs` beside it.

The state counts above close the remaining gap: the model's own sequencing —
budgets, guards, and the monotone counters that keep two states from hashing
alike — is checked by the size of the space, not by any property.

## Abstractions, so nobody over-reads the green

- The injection is assumed to land when the session exists: `tmux_send_keys`
  failures are not modeled, only the crash windows around the call and a
  session that died before it.
- Debounce, client-attachment, engine ambiguity, session age rotation,
  deferral deadlines, and the SQLite projection are out of scope.
- All events share one timestamp and record IDs are deterministic per
  actor; ULID collisions are assumed impossible rather than modeled.
- The model's log is linear history — Git-level forks or lost pushes are
  not modeled, and no modeled writer has a read-write gap against the CAS,
  because the one writer that used to race (the wake) no longer appends.
