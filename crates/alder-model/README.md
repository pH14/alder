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
| The safety predicates | `alder::domain::invariants` — the same five the crash simulator asserts |
| **Modeled by hand (the drift surface)** | each actor's atomic steps: where a process can be interrupted between reading and writing, and what a crash erases |

The safety predicates are shared on purpose. Two harnesses check this
protocol from opposite directions — this model explores every interleaving of
a small cast, the simulator tears every effect every way its footprint allows
— and they check the same five things about a log and the state it folds to.
Stated twice they drift invisibly, both green while meaning different things
by "correct"; stated once in `alder::domain::invariants`, both assert the same
sentence. Two of the five compare the log against a fact no log can hold — a
crash that really happened, an append whose writer is owed a record — so the
caller passes that witness in. Here the witnesses are the model's own injected
session deaths and the phone's handoff state.

Liveness, the `sometimes` properties, and one audit of the model's own ghost
bookkeeping stay local, and that is deliberate rather than an omission: they
are claims about reachability across a state space, and only a model checker
has one.

Two production changes were made for this crate: `alder::app::loop_section`
became `pub`, so the model reads the loop through the same projection the
daemon does, and `PassTrigger` gained `Hash`, so a recorded trigger list can
live in a model state that stateright hashes.

Trigger kinds reach the log the way the driver sends them, not by a mapping
copied into this crate: `alderd`'s `Trigger::as_str` is what the driver puts
on `alder loop wake --trigger`, so the model parses that string with the
CLI's own `TriggerKind` and converts with `alder`'s own `From`. If the two
enums ever diverge, this panics rather than quietly recording a trigger kind
the contract does not have.

## The actors and their atomic steps

- **The log** is the shared state: a `Vec<Record>` replayed through a real
  `MemoryLog` on every transition. Appends are the linearization points.
- **The daemon** takes four steps per wake, mirroring `alderd`'s driver:
  poll-decide-reconcile (session restarted *before* the wake, as the driver
  comments insist), then the wake CLI's own snapshot (which concedes to an
  open pass — the `pass_open` path), then the CAS push against the pinned
  head (which can lose the race — the `HeadConflict` path), then
  `tmux_send_keys` — the injection, which is a separate step because the
  durable wake deliberately precedes it. The engine and trigger kinds the
  decision produced ride along from the first step to the third, because
  those are what the wake records. It repairs a pass it *found* open by the
  stale-pass rule: `crashed` when the pass's handle is an observable tmux
  session it can see is gone, `timeout` otherwise.
- **The leader** ends the open pass `ok`, optionally with `rotate: true`,
  or crashes (the tmux session dies).
- **The phone** optionally races a wake, requests a rotation, pauses the
  loop with a stated reason, or submits a handoff — including losing the
  append's response and retrying the identical draft.
- **Crashes**: the daemon process can die (forgetting its session memory),
  and the leader session can die, each under a scenario budget.

Time is not a modeled dimension: every event carries the same instant, and
the two places the protocol genuinely waits are offered as explicit steps
rather than pretended away — "the max-interval ceiling eventually elapses"
(`DaemonCeilingFires`) and "the pass budget eventually elapses"
(`DaemonResolveTimeout`). That is the fairness assumption made honest. What
it costs is that both are always available rather than only after a delay,
so the model explores a daemon that times out a live pass; that is legal
under the stale-pass rule, just pessimistic about when.

### The window between intent and effect

`LOOP.md` names two crash windows around a wake and says the first is
repairable. The model represents it: `DaemonCtl::Recorded` is the state
where the CAS append has committed and nothing has been typed at the leader
yet. A daemon crash there strands a pass that the log shows open and that no
engine was ever told to run.

The bit that makes this window real is `leader_injected`. A live tmux
session is not a running pass — the driver reconciles the session *before*
the wake, so after a crash in this window the session is perfectly healthy
and perfectly idle. Without that bit the leader could end a pass it was
never handed, which is precisely what would make a broken window look
repaired.

The repair is `timeout`, and it has to be: `crashed` requires a session
observably gone, and this one is alive. That is the stale-pass rule's "time
is the only fact it has" clause, and it is why modelling the window at all
required modelling the timeout verdict.

A crash *during* the CAS push is the other window, and it stays out of
scope: the daemon cannot know whether its append landed, and telling that
era apart from a pass still running needs real time rather than a fairness
step. The crash injection stops short of `DaemonCtl::Appending` for that
reason and no other.

## The four properties

1. **At most one open pass** under concurrent wake attempts, with the loser
   conceding rather than ending the winner's pass. Encoded as `always`
   invariants — the shared `log_folds_cleanly` and `at_most_one_open_pass`,
   the latter asserting both harnesses' spellings of "open" and that they
   still agree pass by pass — plus `sometimes` coverage that both concede
   paths are actually reached. Checked in
   `concurrent_wakes_leave_at_most_one_open_pass`.
2. **A pending rotation is consumed exactly once, and a crashed pass never
   silently consumes one.** The shared
   `rotate_pending_mirrors_the_request_ledger` holds the fold's sequence
   arithmetic against a straight scan of the history in log order, in every
   state; the shared `crashed_verdicts_follow_real_crashes` holds the log's
   crash attributions against the deaths this run actually injected; and —
   with a single waker — a rotation the daemon consumes was always performed
   (a fresh session) first, whatever crashed in between. Checked in
   `crashes_never_silently_consume_a_rotation` with a daemon crash and a
   session crash injected at every point.
3. **CAS append loses no updates under interleaved writers.** The log stays
   a cleanly folding total order, and the shared
   `acknowledged_handoffs_are_never_lost` requires that a submission the
   writer is owed appears in the history exactly once *and* folded into a
   handoff the state still knows about — with every other submission held to
   the no-duplicates half. A lost response retried with the identical draft
   is absorbed as `AlreadyPresent`. Checked in
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
| codex engine | none, one pass, Codex-configured | 7 |
| lone daemon | none | 19 |
| wake race | phone wake | 152 |
| CAS writers | handoff + lost response | 659 |
| rotation race | phone wake + phone rotation | 1,393 |
| rotation under crashes | rotate + pause + 1 daemon crash + 1 session crash | 8,826 |

Complete exploration of all six scenarios takes about five seconds.

The baseline is no longer the single linear chain it was before the
injection step and the timeout verdict existed. It is now arm, snapshot,
append, inject, end, twice over, with the timeout branch available at each
of the two open passes — 19 states. Growth beyond that still means the model
gained transitions, and still wants investigating before it is accepted.

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

**Mutation checks.** The properties bite. Two mutations of the protocol
itself: reverting the driver's rotate-before-wake order (treating the
rotation restart as a reuse) is caught by property 2 with a 16-step
counterexample; ignoring the pinned head in the daemon's wake append (blind
append instead of CAS) is caught by the shared `log_folds_cleanly` with a
10-step counterexample ending in a log the real fold rejects as a duplicate
pass.

Three more confirm the shared predicates did not go vacuous when the
closures started delegating to them, each mutation aimed at one sentence:
dropping the handoff append instead of only its response is caught by
`acknowledged_handoffs_are_never_lost`; making the fold's `rotate_pending`
ignore the consuming wake is caught by
`rotate_pending_mirrors_the_request_ledger`; writing a clean pass end as a
`crashed` verdict is caught by `crashed_verdicts_follow_real_crashes` in
every scenario, including the fault-free one.

Three more hold the wake's recorded content and the crash window in place —
each of these mutations restores a defect this model once had:

- Hard-coding `engine: "claude"` again passes every other scenario in the
  file and fails only `a_codex_configured_loop_records_codex`, on "a daemon
  wake records a configured engine". A literal that matches the only engine
  anybody configured is invisible until somebody configures another one,
  which is why that scenario exists.
- Hard-coding `triggers: vec![Due]` again fails "a wake records the log
  trigger that woke it" in the CAS-writers scenario, where a handoff
  advances the log and the wake it provokes must say `log`.
- Removing `DaemonResolveTimeout` fails **liveness** — "every terminal state
  is progressing or blocked-and-named" — with a 16-step counterexample: the
  pass stranded by a crash between the wake and the injection has no repair,
  so exploration ends with it still open. Removing the injection gate as
  well does not restore the green; it fails "a stranded pass is repaired by
  timeout" instead. That pair is the point of the window: before it existed
  the model could not enter the state at all, and liveness stayed green by
  never being asked.

`at_most_one_open_pass` has no such mutation here, and that is a fact about
the fold rather than a gap: every way of reaching two open passes in this
model produces a history the real fold rejects outright, so
`log_folds_cleanly` fires first. The predicate is exercised directly against
those shapes in the shared module's own tests.

## Abstractions, so nobody over-reads the green

- The timeout *verdict* is modeled; the timeout *deadline* is not. The
  budget elapsing is a fairness step available whenever a pass is open, so
  the model explores a daemon that times out a live pass — legal under the
  stale-pass rule, pessimistic about when. Crash attribution is modeled only
  for observable tmux handles, which is the driver's own rule. The phone
  always ends its own pass.
- The injection is assumed to land: `tmux_send_keys` failures are not
  modeled, only the crash window before the call.
- Injection, debounce, client-attachment, engine ambiguity, pass budgets
  per session, and the SQLite projection are out of scope.
- All events share one timestamp and record IDs are deterministic per
  actor; ULID collisions are assumed impossible rather than modeled.
- `MemoryLog` heads are length-deterministic, so a pinned head is
  recovered by replaying the prefix a writer saw. The model's log is
  linear history — Git-level forks or lost pushes beyond a rejected CAS
  are not modeled.
