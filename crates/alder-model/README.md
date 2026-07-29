# alder-model

A [stateright](https://docs.rs/stateright) model of Alder's protocol core.
It checks four properties of the loop protocol under **every interleaving**
of a small cast — the shared CAS log, the daemon's decide loop, the leader
engine, and an optional second writer (a phone session) — including crash
transitions. It is a dev-only crate: nothing depends on it, and it ships
nothing.

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
| CAS append, idempotency, head conflicts | `alder_log::MemoryLog` (states replay through its public `append`) |
| Event payloads and codec | `alder::domain::{EventPayload, encode_draft, decode_record}` |
| Log interpretation | `alder::domain::ProjectState::fold` on every state |
| The daemon's read | `alder::app::loop_section` + `alderd::loop_state::LoopState::from_status` |
| The daemon's judgment | `alderd::decide::{decide, resolve_engine, session_action, observable_session}` |
| **Modeled by hand (the drift surface)** | each actor's atomic steps: where a process can be interrupted between reading and writing, and what a crash erases |

The one production change made for this crate: `alder::app::loop_section`
became `pub`, so the model reads the loop through the same projection the
daemon does.

## The actors and their atomic steps

- **The log** is the shared state: a `Vec<Record>` replayed through a real
  `MemoryLog` on every transition. Appends are the linearization points.
- **The daemon** takes three steps per wake, mirroring `alderd`'s driver:
  poll-decide-reconcile (session restarted *before* the wake, as the driver
  comments insist), then the wake CLI's own snapshot (which concedes to an
  open pass — the `pass_open` path), then the CAS push against the pinned
  head (which can lose the race — the `HeadConflict` path). It resolves an
  open pass as `crashed` only when the pass's handle is an observable tmux
  session it can see is gone.
- **The leader** ends the open pass `ok`, optionally with `rotate: true`,
  or crashes (the tmux session dies).
- **The phone** optionally races a wake, requests a rotation, pauses the
  loop with a stated reason, or submits a handoff — including losing the
  append's response and retrying the identical draft.
- **Crashes**: the daemon process can die (forgetting its session memory),
  and the leader session can die, each under a scenario budget.

Time is not a modeled dimension: every event carries the same instant, and
"the max-interval ceiling eventually elapses" is offered as an explicit
step (`DaemonCeilingFires`), which is the fairness assumption made honest.

## The four properties

1. **At most one open pass** under concurrent wake attempts, with the loser
   conceding rather than ending the winner's pass. Encoded as `always`
   invariants — every reachable log folds cleanly, and never holds two
   unended passes — plus `sometimes` coverage that both concede paths are
   actually reached. Checked in `concurrent_wakes_leave_at_most_one_open_pass`.
2. **A pending rotation is consumed exactly once, and a crashed pass never
   silently consumes one.** `rotate_pending` derived by the real fold must
   mirror an independently tracked ghost in every state (exactly-once by
   log order), and — with a single waker — a rotation the daemon consumes
   was always performed (a fresh session) first, whatever crashed in
   between. Checked in `crashes_never_silently_consume_a_rotation` with a
   daemon crash and a session crash injected at every point.
3. **CAS append loses no updates under interleaved writers.** The log stays
   a cleanly folding total order; an acknowledged (or landed-but-lost)
   handoff is present exactly once; a lost response retried with the
   identical draft is absorbed as `AlreadyPresent`. Checked in
   `interleaved_writers_lose_no_updates`.
4. **Liveness under fairness: from any crash state the system reaches
   progressing, or blocked-and-named.** Encoded as safety over the bounded
   graph: every *terminal* state (no action enabled) has no open pass —
   in particular none stranded by a crash, so every crashed pass got its
   `pass.ended crashed` attribution — and is either free to run the next
   trigger or paused with a stated reason. Because every transition either
   grows the log or advances a monotone counter/latch, the state graph is
   acyclic and finite, so "all maximal paths end recovered" is exactly
   "every fair execution eventually recovers" within the bounds. (This
   sidesteps stateright's documented caveat that `eventually` properties
   are unsound on cyclic graphs.)

## State-space bounds

Budgets bound the space: total passes (`max_passes`, ±1 under a lost wake
race), one phone wake, one rotation request per source, one pause, one
handoff, and per-scenario crash counts. The log length is bounded by
2·passes + 4 control events. Counts from `cargo test -- --nocapture`:

| Scenario (test) | Faults and writers | Unique states |
| --------------- | ------------------ | ------------- |
| lone daemon | none | 9 (one linear chain) |
| wake race | phone wake | 63 |
| CAS writers | handoff + lost response | 223 |
| rotation race | phone wake + phone rotation | 546 |
| rotation under crashes | rotate + pause + 1 daemon crash + 1 session crash | 2,388 |

Complete exploration of all five scenarios takes about a second.

## Findings

**A racing wake can swallow a rotation request.** Reachable, kept visible
as an asserted discovery in `a_racing_wake_can_swallow_a_rotation`, in two
shapes:

- *Daemon-side*: the request lands in the window between the driver's
  poll/reconcile and its wake's CAS append. The wake was armed before the
  request existed, so nothing was restarted, yet the append consumes the
  request by log order. Shortest trace: `DaemonPollFires,
  PhoneRotationRequest, DaemonWakeSnapshot, DaemonWakeAppend` — the
  rotation is consumed, no restart ever happens.
- *Phone-side*: a phone wake wins while a rotation is pending; the phone
  restarts nothing, and by the daemon's next fire `rotate_pending` is
  already false, so the old session is reused.

The crash-ordering guarantee the driver documents (rotate first, wake
second, so a crash between them merely re-rotates) **does hold** — that is
property 2. The swallow is a concurrency wart, not a crash wart: consumed
means "a wake is later in the log than the request", not "somebody
rotated". If it is worth fixing, the natural shapes are (a) the wake
carries the rotation state it acted on, so a consuming wake that performed
no rotation is detectable, or (b) `rotate_pending` stays derived but the
daemon re-checks it between its own wake's snapshot and push.

**Mutation checks.** The properties bite: reverting the driver's
rotate-before-wake order (treating the rotation restart as a reuse) is
caught by property 2 with a 16-step counterexample; ignoring the pinned
head in the daemon's wake append (blind append instead of CAS) is caught
by property 1/3 with a 10-step counterexample ending in a log the real
fold rejects as a duplicate pass.

## Abstractions, so nobody over-reads the green

- Timeout resolution is out of scope (it needs real time); crash
  attribution is modeled only for observable tmux handles, which is also
  the driver's own rule. The phone always ends its own pass.
- Injection, debounce, client-attachment, engine ambiguity, pass budgets
  per session, and the SQLite projection are out of scope.
- All events share one timestamp and record IDs are deterministic per
  actor; ULID collisions are assumed impossible rather than modeled.
- `MemoryLog` heads are length-deterministic, so a pinned head is
  recovered by replaying the prefix a writer saw. The model's log is
  linear history — Git-level forks or lost pushes beyond a rejected CAS
  are not modeled.
