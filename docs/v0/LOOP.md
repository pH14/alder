# Alder v0 loop

[README.md](README.md) describes one bounded pass of the driving loop. This
document defines what makes that loop durable: who wakes the executor, what is
recorded where, and why a crash anywhere in the wake path is harmless.

## The one rule

**The log never mentions its own readers.** It carries statements about work,
attempts, questions, and observations — never about the processes that read
it. There are no pass records: nothing durable says the loop ran, is running,
or should run next. What earlier versions put into pass records survives
elsewhere, each fact where it belongs:

- decisions land on the items they concern, which already carry actor and
  timestamp — a pass reappears in the log as a cluster of appends;
- "check again at 3pm" is a statement on the work item
  (`work block --until`), not on the loop;
- crash forensics live in session transcripts;
- the loop's heartbeat lives in the driver's own local log.

The payoff is the crash story. A wake is delivered by typing one line into a
terminal and noting, machine-locally, which head it was for. Nothing durable
records it, so there is nothing for a crash to half-say: a missed wake is made
up by the next poll, a duplicated wake finds an executor with nothing new to do,
and both cost nothing because **passes are idempotent** — the executor rebuilds
its picture from the fold every time.

The loop is a singleton per log. There is one loop, so it needs no ID, and
`alder status` reports it rather than a `loop show` command.

## Division of labour

Alder stores. The driver schedules. The executor thinks.

The **driver** (`alderd`, or anything that behaves like it) decides *when* to
run the executor. It exercises no judgment about work, and it appends
nothing. Its complete read surface is one thing:

1. `alder status --json`: the current head, and the loop section. It ignores
   the rest of the document.

When a trigger fires it runs one configured shell command with the trigger
names in `ALDERD_TRIGGERS`; sessions, engines, and the observation sweep
(`alder refresh`) are that command's business, never the driver's.

Its complete durable write surface is one machine-local file:
`.alder/alderd-notes.json`, the last head it acted on and when. That file is
the wake rule's whole memory, it belongs to the machine rather than to the
project, and losing it costs exactly one redundant wake.

A driver may additionally stat `.alder/last-append`, a marker the CLI touches
after each confirmed append, to shorten the sleep before its next read. A
stat is not a read of the log: the marker carries no state, only an mtime,
and its absence merely means the next read happens on the ordinary schedule.

That list is the contract. A driver that reads the ready frontier to decide
whether waking is worthwhile has started doing the executor's job, and it will
be wrong in ways nobody can see, because its reasoning is not in the log.

The **executor** is an agent in an interactive session. Woken, it reads the
pass document, rebuilds its picture from the fold, and acts on whatever the
state demands — open work with no attempt, an unanswered question, a dead
execution, an observation nothing is addressing — then exits or idles.
Anything a pass would once have reported goes as a statement about the
specific item it concerns.

**Alder** validates and folds. It never launches a session, never validates an
engine name, and never enforces the pause it stores.

## Supervision tree

The process tree has a root outside the daemon:

```text
launchd (KeepAlive, RunAtLoad)
  └─ alderd
       └─ the configured command
            └─ whatever it drives (sessions are its business, not alderd's)
```

`scripts/alderd-install.sh` renders the launchd agent for one checkout and
loads it; `scripts/alderd-uninstall.sh` unloads and removes it. The rendered
agent uses the repository as its working directory and sends both standard
output and standard error to `.alder/alderd.log`. Loading is opt-in: nothing
in the build or test path invokes `launchctl`.

The daemon must remain safe to kill at any instant. Its one durable file is
advisory, so after a kill the next daemon starts from the log, the notes if
they survive, and whatever sessions it can observe. This statelessness is
what makes `KeepAlive` sufficient: launchd only needs to bring back the
daemon, not reconstruct a checkpoint.

## The wake

### The rule

> Wake the executor when the observed head differs from the last head this
> driver acted on.

The baseline is the driver's notes, not the log. Immediately after a wake the
noted head *is* the observed head, so a driver's own delivery cannot
self-trigger; anything anyone appends afterwards — the executor acting, a
worker's milestone, a phone command — moves the head past the note and wakes
the executor again. Being woken by your own writes is harmless: the executor
reads the fold, and a fold with nothing new demands nothing.

Two more conditions wake without a head movement, both read from the same
status document:

- **due**: a deferral deadline (any entry in the loop section's
  `review_deadlines`) has arrived and no wake has been delivered since it
  passed, or `maxIntervalSeconds` elapsed since the last wake — the ceiling
  that keeps a quiet loop honest;
- **manual**: a nudge request is later in the log than the noted head.

### The delivery

Firing is strictly ordered: run the configured command to completion, then
write the notes. The crash windows this leaves are benign, and pinned by the
crash simulator and the model checker:

- **Ran, not noted.** The command completed; the restarted daemon does not
  know that, and runs it again. A duplicate wake finds an executor with
  nothing new to do.
- **Command failed or died.** Nothing was noted; the next poll runs the same
  wake again. The command must therefore be idempotent, which the waking
  design already demands of everything downstream of a wake.

The command receives one thing beyond its configuration: the trigger names,
comma-joined, in `ALDERD_TRIGGERS` (for example `log,due`).

The delivery is deliberately not a prompt. Everything the executor needs to
know about *how* to act lives in the repository, under review, behind the
command. Anything the driver said instead would be operational instruction
smuggled past the place where it can be read and changed.

**Trigger kinds are informational. They are never scope limiters.** A command
run for `manual` reads the complete state exactly like one run for `due`;
the driver cannot know what else changed while it was not looking.

## Fold rules

The loop's durable state is small and every rule is stated precisely.

**Pause.** Last writer wins. `loop.paused` sets `paused` to true with the
reason from the event; `loop.resumed` sets it to false and clears the reason.
No count, no nesting, no owner.

**Engine.** Last writer wins. `loop.engine_selected` replaces the desired
engine name. The name is an opaque string that Alder never validates; the
driver never reads it — it is served for the configured command, which is
what runs engines now.

**Rotation and nudge.** Each request records the sequence it was asked at:
`rotate_requested_seq`, `nudge_requested_seq`. That is all the fold says.
Whether a request has been *acted on* is not a log fact — the log does not
record its readers — so a reader compares the request sequence with its own
machine-local note: later than the note means outstanding, and acting (which
moves the note past it) consumes it. The driver applies that rule to the
nudge, which is its manual trigger. The rotation request is the command's to
honor the same way — the driver no longer maintains any session to rotate —
and the request's own append still moves the head, so the command is woken
for it like for any other write. Two readers with separate notes each honor
a request once, which is the harmless direction.

**Deferral.** `work block --until <RFC3339>` stores a review deadline on the
work item. The loop section serves every blocked item's deadline, sorted, as
`review_deadlines`, plus `review_at`, the earliest, for the human status
line. The fold is a
pure function of the log and cannot read a clock, so nothing unblocks by
itself — when the deadline passes, `alder status` surfaces the item as a
`block_expired` attention finding, and the driver's `due` trigger wakes the
executor to review it. Historical `pass.started`/`pass.ended` events decode and
replay as inert history (a historical `pass end --rotate` still reads as a
rotation request); no append path can produce a new one.

## Eras and rotation

A **session era** is one engine process serving a run of wakes. Eras belong
to the configured command now: whether the executor is a long-lived
interactive session, a one-shot process per wake, or something else entirely
is the command's design, and the driver neither knows nor cares. What the
durable model provides is the era boundary an operator can request:
`loop rotate` appends a rotation request, the fold serves its sequence, and
the command — which reads status itself — ends whatever era it maintains and
notes the request consumed, by the same note-comparison rule the driver uses
for nudges.

Rotation is the operator's manual era boundary: `loop rotate` after upgrading
an engine, or when a session has drifted. It is *not* an emergency stop — it
changes which process serves the next wake; `loop pause` stops wakes.

## Nudging

A nudge is the operator's "wake it now". `loop nudge` appends a request; the
append itself moves the head, and the request being later in the log than the
driver's note makes it the `manual` trigger, which overrides the driver's
debounce the way the `maxIntervalSeconds` ceiling does. It does not override
`loop pause`. A nudge changes *when* the executor is next woken, never *what*
it does.

## Workers

An executor may dispatch implementation to worker sessions rather than doing
it itself — one work item per worker, each in its own git worktree, bound to
the attempt through the ordinary handle. This is entirely process layer:
Alder stores attempts, handles, questions, and observations exactly as
before, the configured probe answers for each bound handle like any other
observed object, and `reconcile` catches dead ones with the same rules. No
Alder mechanism knows the word "worker". See the
[pass](../../.agent/skills/pass/SKILL.md) and
[worker](../../.agent/skills/worker/SKILL.md) skills for the process itself.

Actually running a worker — cut a worktree on a branch, hand a prompt to a
model at some effort, get back a handle to ask after, send into, or kill —
is `alder-ext-runner`'s job (`crates/alder-ext-runner`), a tool that is
deliberately generic: it imports no Alder crate, never touches the Alder
log, and stamps only its own names into the sessions it creates; nothing in
Alder depends on it back. The glue that composes an item's brief into a
prompt, starts an execution through the runner, and binds the printed handle
to the attempt is the executor-side process the skills describe — Alder
stores the results and reads nothing into the mechanics.

The rungs the runner dispatches on — `luna`, `terra`, `sol` on codex and
`sonnet`, `opus`, `fable` on claude, each pinning a model *and* a reasoning
effort — are the runner's table, machine-locally configurable, with an
unknown rung a hard error rather than a fall-through to a CLI default. A
rung whose provider is rate-limited is served by the rung of equal standing
on the other ladder; `alder-ext-runner budget` reads trailing spend per
provider off local transcripts, and `alder-ext-runner limit` is how a limit
gets recorded.

Two conventions in that process are worth naming here because they show up
in the log rather than in the repository. A worker records an up-tier
consult as `consulted` metadata on its own attempt, and a dispatch records
the rung it launched at as `tier`, `engine` and `effort` metadata. Between
them, an item resent up the ladder carries the whole climb. All are ordinary
open-ended metadata: Alder stores and displays them and reads nothing into
them.

## Driver configuration

`.alder/driver.json`, local to the machine and gitignored:

```json
{
  "command": "alder-pass",
  "pollSeconds": 60,
  "debounceSeconds": 20,
  "maxIntervalSeconds": 1800,
  "notify": "terminal-notifier -title alder -message"
}
```

`command` is the shell command a wake runs, and the only required field; the
remaining fields are timings and a notification hook. How that command runs
the executor — which engines exist on this box, where the pass document
lives, what sessions look like — is the command's own configuration, not the
driver's.

This file is local on purpose. What command drives the executor and how
aggressively to poll are properties of a box, not durable project facts, and
putting them in the log would invite one machine's configuration to become
another's problem. See
[crates/alderd/README.md](../../crates/alderd/README.md) for the field table
and the poll sequence.

## Deferral of the wake

One condition holds a wake without cancelling it:

- **Debounce.** A burst of commits should produce one run, not one per
  commit.

It survives neither `maxIntervalSeconds` nor a pending nudge. A loop that
never runs is a worse failure than a redundant run, and a deferral with no
ceiling is indistinguishable from a hang.

## What the loop is not

- **Not a scheduler.** It decides when to wake one agent, not what runs.
- **Not an executor role.** Alder still stores no executor, generation, or lease.
  Two drivers pointed at one log are not an error; each keeps its own notes,
  and the worst case is a duplicate wake, which costs nothing for the same
  reason a crash costs nothing.
- **Not a sensor trace.** `refresh` records only changed current levels. The
  log is the folded observation picture, so a flip and return between observer
  runs is intentionally absent.
- **Not a place for driver diagnostics.** A driver that cannot reach the store
  or whose command keeps failing says so to its operator. It does not write
  its own troubles into the project's log — the log never mentions its
  readers.
