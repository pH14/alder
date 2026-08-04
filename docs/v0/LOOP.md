# Alder v0 loop

[README.md](README.md) describes one bounded pass of the driving loop. This
document defines what makes that loop durable: who wakes it, what it records,
and how a reader repairs it after a crash.

## The parallel

Work is to an attempt what the loop is to a pass.

| | Parent | Run record | Creator | Closer |
| --- | --- | --- | --- | --- |
| Execution | work item | attempt | `work start` | `attempt end` |
| Iteration | the loop | pass | `loop wake` | `pass end` |

The parallel is not decoration. It is why the same reasoning transfers: intent
is recorded before effects, at most one run is open at a time, the parent
creates because only the parent knows whether another one is allowed, and the
record closes itself because by then it exists.

The loop is a singleton per log. There is one loop, so it needs no ID, and
`alder status` reports it rather than a `loop show` command.

## Division of labour

Alder stores. The driver schedules. The leader thinks.

The **driver** (`alderd`, or anything that behaves like it) decides *when* to
wake a leader. It exercises no judgment about work. Its complete read surface
is three things:

1. `alder status --json`: the current head, and the loop section. It ignores
   the rest of the document.
2. `alder refresh --json` → `.changed`.
3. `alder show <pass-id> --json`, while it is waiting for a pass to end.

Everything the driver needs about the log is in the first of those, including
the wake deadline the last pass requested. It runs no Git command of its own:
the head is already in `status`, and a second view of the store would only be
another thing that can disagree.

A driver may additionally stat `.alder/last-append`, a marker the CLI touches
after each confirmed append, to shorten the sleep before its next read. A
stat is not a read of the log: the marker carries no state, only an mtime,
and its absence merely means the next read happens on the ordinary schedule.

That list is the contract. A driver that reads the ready frontier to decide
whether waking is worthwhile has started doing the leader's job, and it will
be wrong in ways nobody can see, because its reasoning is not in the log.

The **leader** is an agent in an interactive session. It reads the pass
document, runs one bounded pass, and ends it with a report. Every judgment
lives there.

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

The daemon must remain safe to kill at any instant. Its useful state is either
in the durable Alder log or observable from the leader and worker sessions;
after a kill, the next daemon starts from those facts and applies the stale
pass repair rule above. This statelessness is what makes `KeepAlive`
sufficient: launchd only needs to bring back the daemon, not reconstruct an
in-memory checkpoint or coordinate a binary self-restart. Rebuilding a binary
does not restart an already-running daemon.

## Pass lifecycle

### Intent before effects

The driver appends `loop wake` **before** it types anything into the leader's
terminal. This is the same rule as `work start`, and it makes both crash
windows repairable:

- **Recorded pass, nothing injected.** The next poll finds an open pass, waits
  for it, and eventually ends it as `timeout`. Nothing was launched, so nothing
  leaks; the log honestly shows a pass that produced no report.
- **Injected, wake not recorded.** This cannot happen, because the wake is what
  produces the pass ID the injection carries. A leader that receives no
  injection does nothing.

If instead the injection came first, a driver crash between the two would leave
a leader running a pass no record mentions, and a second driver would happily
start another one.

### Who writes what

| Event | Written by | Why |
| --- | --- | --- |
| `pass.started` | the driver, via `loop wake` | Only the driver knows it is about to launch. |
| `pass.ended` (`ok`) | the leader, via `pass end` | Only the leader knows what it did. |
| `pass.ended` (`crashed`, `timeout`) | the driver | The leader is, by definition, not available to say so. |

`crashed` and `timeout` are the driver's two honest statements. `crashed` means
the engine session is gone; `timeout` means the pass outlived its budget. The
driver never writes `ok`, because it cannot know that anything was
accomplished.

### The stale pass

A pass left open by a crashed engine, a killed daemon, or a machine restart
blocks the next wake, which is the point: `loop wake` returns `pass_open`
rather than opening a second one. Recovery is one rule, and any reader can
apply it:

> End the open pass with `crashed` if its session is observably gone, with
> `timeout` if it has outlived the pass budget, and leave it alone otherwise.

A driver applies this rule where it belongs — when a poll *finds* a pass open,
never when its own wake loses a race. Those two cases look identical to the
CLI and are opposites in fact:

- **A pass found open at poll start** predates this driver's attention. It may
  be minutes or hours old, its session may be gone, and resolving it is the
  only way the loop moves again.
- **A `pass_open` rejection from a wake** means someone opened a pass in the
  seconds since this poll read status — a second driver, or a human. That pass
  is new and almost certainly alive. The right response is to concede: log it,
  do nothing, and read the fold again next poll.

A driver that treats the second case like the first kills a live pass and
steals its slot. The rule is therefore: **a driver ends only passes it found
already open, never a pass that beat it to the wake.**

`crashed` also requires a session the driver can actually look at. A pass
records the handle of whatever runs it, and that handle may name a different
tmux session or no tmux session at all — another writer's pass might be
`codex:019f…`. A driver may only report `crashed` for a `tmux:<session>` handle
whose session it has checked and found gone. For any other handle it can
observe nothing, and time is the only fact it has, so `timeout` is the only
verdict available to it.

A human with a terminal applies the same rule with `alder pass end <id>
--outcome crashed --why "…"`. Nothing about the repair is privileged.

### Reading a pass

`alder show <pass-id>` returns the full record — engine, handle, trigger kinds,
the head the wake was appended at, outcome, report, requested wake time — plus
its two events. `alder status` carries the open pass and the last ended pass,
which is what a fresh agent needs to answer "did the loop run, and what
happened."

A pass records two heads, and both exist so that a reader needs no memory.
`at_head` is what the pass could see when it started. `ended_seq` is where the
log stood when it finished, which is why "has anything happened since the loop
last ran" is a comparison between two numbers in one `status` document rather
than a note the driver keeps to itself.

## Fold rules

The loop's durable state is small and every rule is stated precisely, because
a fold rule that is only *mostly* understood produces bugs nobody can localise.

**Passes.** `pass.started` inserts a pass in state `open` and is rejected if
any pass is already open. `pass.ended` moves it to `ended` and is rejected if
it already ended. Pass ordinals start at one, increase by one, and are never
reused; passes are serialized, so the ordinal is safe.

**Pause.** Last writer wins. `loop.paused` sets `paused` to true with the
reason from the event; `loop.resumed` sets it to false and clears the reason.
No count, no nesting, no owner. Two agents pausing and one resuming leaves the
loop running, which is the correct reading of "the latest instruction wins."

**Engine.** Last writer wins. `loop.engine_selected` replaces the desired
engine name. The name is an opaque string that Alder never validates: a driver
that cannot run it says so out of band rather than Alder refusing to store the
operator's stated intent.

**Rotation.** Derived from event order, never stored as a flag:

> A rotation is pending when the sequence of the most recent rotation request
> is greater than the sequence of the most recent `pass.started`, or when a
> rotation has been requested and no wake has ever happened.

A rotation request is either a `loop.rotation_requested` event or a
`pass.ended` carrying `rotate`. The next wake consumes the request simply by
being later in the log, so there is no clearing write, no window in which the
flag is stale, and no way for two drivers to disagree about whether a rotation
already happened.

**Nudge.** The identical derivation over its own request kind:

> A nudge is pending when the sequence of the most recent
> `loop.nudge_requested` event is greater than the sequence of the most recent
> `pass.started`, or when a nudge has been requested and no wake has ever
> happened.

The next wake consumes it by log order alone, exactly as with rotation.

## Eras and rotation

A **session era** is one engine process serving a run of passes. An era ends
for one of five reasons, and the driver checks them in this order:

1. the running session is not one this daemon started;
2. the desired engine changed;
3. a rotation is pending;
4. the pass document changed;
5. the session reached its pass budget.

The first is why a driver restart replaces the session rather than adopting it:
the daemon cannot tell what engine is running or how much context it has
accumulated, and adopting a stranger would silently defeat every other rule.

Rotation is the operator's manual era boundary. `loop rotate` after upgrading
an engine, and `pass end --rotate` when the leader itself concludes the session
has drifted — a context window near its limit, a tool in a bad state, an
engine that has started repeating itself. Only the leader can judge that, which
is why `--rotate` rides on `pass end` rather than being something the driver
infers.

Rotation is *not* an emergency stop. It changes which process serves the next
pass; it does not stop passes. `loop pause` stops passes.

## Nudging

A nudge is the operator's "wake it now". The last pass may have picked a long
wake honestly and then the world changed — an answer landed or a new work item
was filed — and `loop nudge` asks the driver to run the next pass ahead of that
schedule. The driver reports a pending nudge as the `manual` trigger and lets
it override both of its deferrals, the debounce and the attached-client hold,
the way the `maxIntervalSeconds` ceiling does: a nudge is the human overriding
the driver's politeness. It does not override `loop pause`, and it cannot open
a second pass while one is open. A nudge changes *when* the next pass runs,
never *what* it does.

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

## The trigger-message contract

A wake records why it fired: `log`, `observations`, `due`, or `manual`. The
driver repeats those kinds in the message it injects.

**Trigger kinds are informational. They are never scope limiters.**

A pass woken by `observations` runs its complete sync — status, reconcile,
questions, selection — exactly like a pass woken by `due`. The
driver cannot know what else changed while it was not looking, and the whole
point of a durable log is that the leader does not have to be told; it
reads.

The injected message therefore takes one of two forms and nothing else:

```text
Read <passDoc>, then run one pass (pass-id: <id>; triggers: <kinds>).
Run one pass (pass-id: <id>; triggers: <kinds>).
```

The first is the bootstrap form, used on a fresh session that has not read the
pass document. The pass ID is included so the leader can end the right pass and
so a human reading the terminal can find the record.

The message is deliberately not a prompt. Everything the leader needs to know
about *how* to run a pass lives in the pass document, in the repository, under
review. Anything the driver said instead would be operational instruction
smuggled past the place where it can be read and changed.

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
  "passTimeoutSeconds": 3600,
  "maxPassesPerSession": 25,
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

## Deferral

Two conditions hold an injection without cancelling it:

- **Debounce.** A burst of commits should produce one pass, not one per commit.
- **An attached client.** If someone is watching the tmux session, an injection
  would land under their cursor.

Neither survives `maxIntervalSeconds`. A loop that never runs is a worse
failure than an inconvenient injection, and a deferral with no ceiling is
indistinguishable from a hang.

## What the loop is not

- **Not a scheduler.** It decides when to wake one agent, not what runs.
- **Not a leader role.** Alder still stores no leader, generation, or lease.
  Two drivers pointed at one log are not an error; the one-open-pass rule makes
  the second one's wake fail, and it reads the loop fold again next poll. The
  loser concedes; it never ends the winner's pass to take the slot.
- **Not a sensor trace.** `refresh` records only changed current levels. The
  log is the folded observation picture, so a flip and return between observer
  runs is intentionally absent.
- **Not a place for driver diagnostics.** A driver that cannot reach the store
  or cannot find its engine says so to its operator. It does not write its own
  troubles into the project's log.
