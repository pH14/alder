# Alder v0 loop

[README.md](README.md) describes one bounded pass of the driving loop. This
document defines what makes that loop durable: who wakes the leader, what is
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
up by the next poll, a duplicated wake finds a leader with nothing new to do,
and both cost nothing because **passes are idempotent** — the leader rebuilds
its picture from the fold every time.

The loop is a singleton per log. There is one loop, so it needs no ID, and
`alder status` reports it rather than a `loop show` command.

## Division of labour

Alder stores. The driver schedules. The leader thinks.

The **driver** (`alderd`, or anything that behaves like it) decides *when* to
wake a leader. It exercises no judgment about work, and it appends nothing.
Its complete read surface is two things:

1. `alder status --json`: the current head, and the loop section. It ignores
   the rest of the document.
2. `alder refresh --json` → `.changed`.

Its complete durable write surface is one machine-local file:
`.alder/alderd-notes.json`, the last head it acted on and when. That file is
the wake rule's whole memory, it belongs to the machine rather than to the
project, and losing it costs exactly one redundant wake.

A driver may additionally stat `.alder/last-append`, a marker the CLI touches
after each confirmed append, to shorten the sleep before its next read. A
stat is not a read of the log: the marker carries no state, only an mtime,
and its absence merely means the next read happens on the ordinary schedule.

That list is the contract. A driver that reads the ready frontier to decide
whether waking is worthwhile has started doing the leader's job, and it will
be wrong in ways nobody can see, because its reasoning is not in the log.

The **leader** is an agent in an interactive session. Woken, it reads the
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
       └─ leader session
            └─ worker sessions
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

> Wake the leader when the observed head differs from the last head this
> driver acted on.

The baseline is the driver's notes, not the log. Immediately after a wake the
noted head *is* the observed head, so a driver's own delivery cannot
self-trigger; anything anyone appends afterwards — the leader acting, a
worker's milestone, a phone command — moves the head past the note and wakes
the leader again. Being woken by your own writes is harmless: the leader
reads the fold, and a fold with nothing new demands nothing.

Three more conditions wake without a head movement, each read from the same
status document or the driver's own observation:

- **observations**: `alder refresh` reported a semantic change;
- **due**: a deferral deadline (any entry in the loop section's
  `review_deadlines`) has arrived and no wake has been delivered since it
  passed, or `maxIntervalSeconds` elapsed since the last wake — the ceiling
  that keeps a quiet loop honest;
- **manual**: a nudge request is later in the log than the noted head.

### The delivery

Firing is strictly ordered: reconcile the session, inject the line, then
write the notes. The two crash windows this leaves are both benign, and both
are pinned by the crash simulator and the model checker:

- **Injected, not noted.** The leader was handed the line; the restarted
  daemon does not know that, and delivers it again. A duplicate wake finds a
  leader with nothing new to do.
- **Torn injection.** The text was typed, the Enter was not. Nothing was
  delivered and nothing was noted; the next fire restarts the pane it does
  not recognize and delivers a fresh line.

The injected message takes one of two forms and nothing else:

```text
Read <passDoc>, then read the current Alder state and act on it (triggers: <kinds>).
Read the current Alder state and act on it (triggers: <kinds>).
```

The first is the bootstrap form, used on a fresh session that has not read
the pass document. There is no identifier in the line, because nothing
durable exists to identify.

The message is deliberately not a prompt. Everything the leader needs to know
about *how* to act lives in the pass document, in the repository, under
review. Anything the driver said instead would be operational instruction
smuggled past the place where it can be read and changed.

**Trigger kinds are informational. They are never scope limiters.** A leader
woken by `observations` reads the complete state exactly like one woken by
`due`; the driver cannot know what else changed while it was not looking.

## Fold rules

The loop's durable state is small and every rule is stated precisely.

**Pause.** Last writer wins. `loop.paused` sets `paused` to true with the
reason from the event; `loop.resumed` sets it to false and clears the reason.
No count, no nesting, no owner.

**Engine.** Last writer wins. `loop.engine_selected` replaces the desired
engine name. The name is an opaque string that Alder never validates.

**Rotation and nudge.** Each request records the sequence it was asked at:
`rotate_requested_seq`, `nudge_requested_seq`. That is all the fold says.
Whether a request has been *acted on* is not a log fact — the log does not
record its readers — so each driver compares the request sequence with its
own noted head: later than the note means outstanding, and acting (which
moves the note past it) consumes it. Two drivers with separate notes each
honor the request once, which is the harmless direction.

**Deferral.** `work block --until <RFC3339>` stores a review deadline on the
work item. The loop section serves every blocked item's deadline, sorted, as
`review_deadlines`, plus `review_at`, the earliest, for the human status
line. The fold is a
pure function of the log and cannot read a clock, so nothing unblocks by
itself — when the deadline passes, `alder status` surfaces the item as a
`block_expired` attention finding, and the driver's `due` trigger wakes the
leader to review it. Historical `pass.started`/`pass.ended` events decode and
replay as inert history (a historical `pass end --rotate` still reads as a
rotation request); no append path can produce a new one.

## Eras and rotation

A **session era** is one engine process serving a run of wakes. An era ends
for one of five reasons, and the driver checks them in this order:

1. the running session is not one this daemon started;
2. the desired engine changed;
3. a rotation request is later in the log than the noted head;
4. the pass document changed;
5. the session outlived `maxSessionAgeSeconds`.

The first is why a driver restart replaces the session rather than adopting
it: the daemon cannot tell what engine is running or how much context it has
accumulated, and adopting a stranger would silently defeat every other rule.
The last is wall-clock age on purpose: nothing durable counts passes, so
nothing counts them here either.

Rotation is the operator's manual era boundary: `loop rotate` after upgrading
an engine, or when a session has drifted. It is *not* an emergency stop — it
changes which process serves the next wake; `loop pause` stops wakes.

## Nudging

A nudge is the operator's "wake it now". `loop nudge` appends a request; the
append itself moves the head, and the request being later in the log than the
driver's note makes it the `manual` trigger, which overrides both of the
driver's deferrals — the debounce and the attached-client hold — the way the
`maxIntervalSeconds` ceiling does. It does not override `loop pause`. A nudge
changes *when* the leader is next woken, never *what* it does.

## Workers

A leader may dispatch implementation to worker sessions rather than doing it
itself — one work item per worker, each in its own git worktree and tmux
session, stamped with its attempt ID and bound to the attempt through the
ordinary handle. This is entirely process layer: Alder stores attempts,
handles, questions, and observations exactly as before, the tmux observer
lists worker sessions like any other observed object, and `reconcile`
catches dead ones with the same rules. No Alder mechanism knows the word
"worker". See the [pass](../../.agent/skills/pass/SKILL.md) and
[worker](../../.agent/skills/worker/SKILL.md) skills for the process itself,
and `alderd spawn` for the dispatch.

`alderd spawn <work-id> [tier]` is the whole dispatch: it reads the item,
records the attempt, cuts the worktree and branch, launches the pane with the
item's goal **as argv**, and binds the handle. Nothing is typed at the
session and nothing waits for an engine to boot, so there is no sleep on the
path; the pane command ends `; exec bash`, so a one-shot engine leaves a live
session behind and the handle stays true. The daemon still reaches the log
only by running `alder`, and it hands the worktree a copy of `alder` alone —
a worker cannot dispatch.

A Codex pane starts a launcher-owned sidecar before `codex exec`. It snapshots
the existing local Codex rollouts, finds the first new `session_meta` whose
`cwd` is the worktree, and stamps that UUID as `codex-session` on the attempt.
This is deliberately outside the worker turn: a one-shot that dies before its
first tool call still has a durable resume handle. The sidecar leaves a local
marker first; when a log append is temporarily unavailable, the tmux
observer recovers the fresh rollout UUID and supplies it to `reconcile`, which names a
`codex_session_unstamped` finding and its exact `attempt edit` repair. A
resume without that UUID is refused rather than guessing from `--last`, since
a consult can be newer than the worker session.

The six rungs — `luna`, `terra`, `sol` on codex and `sonnet`, `opus`, `fable`
on claude — are a table in the daemon, each pinning a model *and* a reasoning
effort. An unknown rung is an error rather than a fall-through, because
falling through to a CLI default would launch a worker at an unknown model
and record nothing about it. A rung whose provider is rate-limited is served
by the rung of equal standing on the other ladder; `alderd budget` reads
trailing spend per provider off local transcripts, and `alderd limit` is how
a limit gets recorded.

The spawn is checked end to end by `crates/alderd/tests/spawn_host.rs` under
`cargo test`, and by `scripts/tests/verify-spawn.sh` against a real `alder`
and a throwaway log. Both run a throwaway repository and their own tmux
server and assert what a live session actually received. That sandbox must
never reach the machine's own tmux server, so the teardown that enforces it —
one session, by exact name, only after proving the sandbox server holds
nothing else — lives in `scripts/tests/tmux-sandbox.sh` and is itself tested
by `scripts/tests/test-teardown-guard.sh`.

Two conventions in that process are worth naming here because they show up
in the log rather than in the repository. A worker records an up-tier
consult as `consulted` metadata on its own attempt, and a dispatch records
the rung it launched at as `tier`, `engine` and `effort` metadata; the Codex
sidecar also records `codex-session`. Between them, an item resent up the
ladder carries the whole climb and a one-shot worker carries its resume
handle. All are ordinary open-ended metadata: Alder stores and displays them
and reads nothing into them.

## Driver configuration

`.alder/driver.json`, local to the machine and gitignored:

```json
{
  "engines": {
    "claude": { "cmd": "claude", "args": [] },
    "codex": { "cmd": "codex", "args": ["--full-auto"] }
  },
  "passDoc": ".agent/skills/pass/SKILL.md",
  "tmuxSession": "alder-leader",
  "pollSeconds": 60,
  "debounceSeconds": 20,
  "maxIntervalSeconds": 1800,
  "maxSessionAgeSeconds": 21600,
  "notify": "terminal-notifier -title alder -message"
}
```

`engines` maps the opaque name recorded by `loop use` to a command on this
machine. `passDoc` names the pass prompt document; its content hash is an era
boundary. The remaining fields are timings and a notification hook, and only
`engines` and `passDoc` are required.

This file is local on purpose. Which engines are installed and how aggressively
to poll are properties of a box, not durable project facts, and putting them in
the log would invite one machine's configuration to become another's problem.
See [crates/alderd/README.md](../../crates/alderd/README.md) for the field
table and the poll sequence.

## Deferral of the injection

Two conditions hold an injection without cancelling it:

- **Debounce.** A burst of commits should produce one wake, not one per commit.
- **An attached client.** If someone is watching the tmux session, an injection
  would land under their cursor.

Neither survives `maxIntervalSeconds`, and neither survives a pending nudge.
A loop that never runs is a worse failure than an inconvenient injection, and
a deferral with no ceiling is indistinguishable from a hang.

## What the loop is not

- **Not a scheduler.** It decides when to wake one agent, not what runs.
- **Not a leader role.** Alder still stores no leader, generation, or lease.
  Two drivers pointed at one log are not an error; each keeps its own notes,
  and the worst case is a duplicate wake, which costs nothing for the same
  reason a crash costs nothing.
- **Not a sensor trace.** `refresh` records only changed current levels. The
  log is the folded observation picture, so a flip and return between observer
  runs is intentionally absent.
- **Not a place for driver diagnostics.** A driver that cannot reach the store
  or cannot find its engine says so to its operator. It does not write its own
  troubles into the project's log — the log never mentions its readers.
