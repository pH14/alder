# alderd

`alderd` decides *when* to wake an Alder leader agent, and launches the workers
that leader dispatches. It never decides *what* either of them should do.

The daemon holds no API token, links no model client, and reads no work,
attempt, or question state. It shells out to the `alder` CLI for everything it
knows about the log, and it drives tmux for everything it does to the world.
The driving loop's complete read surface is three commands:

1. `alder status --json` — the head, and the loop section. Everything else in
   the document is ignored.
2. `alder refresh --json` — the `"changed"` bool.
3. `alder show <pass-id> --json` — only while awaiting an open pass.

The loop runs no Git command. Its log trigger is `head > last_pass.ended_seq`,
both read from that one `status` document, so the baseline lives in the log
rather than in the daemon and a restarted daemon recovers it for free. Only
`alderd spawn` runs `git`, and only to cut a worker its worktree.

Anything that requires judgment — which work to start, which rung to start it
on, whether an attempt is stale, what a report means — belongs to the leader.

The boundary runs one way: Alder never calls `alderd`, `alderd` reaches the log
only through the `alder` CLI, and it links no Alder crate.

See [`docs/v0/LOOP.md`](../../docs/v0/LOOP.md) for the durable model behind
this: passes, loop controls, and the crash windows the pass lifecycle repairs.

## Running it

```text
alderd [--root <project>]                     run the driving loop
alderd [--root <project>] spawn <work-id> [tier]
alderd [--root <project>] budget [--hours <n>] [--json]
alderd [--root <project>] limit <provider> [--minutes <n>] [--clear] [--why <text>]
```

The project must already be initialized (`alder init`). The loop additionally
needs `.alder/driver.json`; the one-shot commands do not. `alderd` reads the
store remote and ref from `.alder/config.json` so it watches exactly the ref
Alder writes.

Logs go to standard error. `contrib/com.alder.alderd.plist` is a sample launchd
agent; copy it to `~/Library/LaunchAgents/`, edit the paths, and
`launchctl load` it.

## Dispatch

`alderd spawn <work-id> [tier]` launches one worker for one item, in this
order: read the item (`alder show`), record the attempt (`alder work start`, or
adopt an open unbound one), cut or adopt `../alder-work-<id>` on `work/<id>`,
copy in `.alder/config.json` and the `alder` binary, start or adopt the tmux
session, and bind the handle with the tier stamped on it.

Three rules make that ordering worth having:

- **The goal is argv.** The item's title, spec, checks and gates are composed
  into one string and passed as the engine's final argument. Nothing is typed
  at the pane, so nothing waits for an engine to boot and nothing in the goal
  can read as a key name. There is no sleep on the path.
- **The pane outlives the engine.** The command ends `; exec bash`, so a
  one-shot `codex exec` leaves a live session behind: the handle stays
  observable, and a ruling can be relayed into the shell afterwards. For a
  codex rung the spawn also writes `<worktree>/.alder/resume`, which is how
  that relay is typed — `.alder/resume [<session-id>] "<the ruling>"`. It
  exists because `codex exec resume` inherits *nothing* from the session it
  resumes: no model, no effort, no sandbox. Resumed by hand, a luna worker
  quietly continues at another model, with no network access and no writable
  git dir, and can neither commit nor reach the log. The script repeats the
  launch exactly, because the same table writes both.
- **Crash residue is adoptive.** Re-running spawn after any completed effect
  converges on the same attempt. An existing worktree is accepted only after
  Git proves it is on `work/<id>`. `tmux new-session` stamps
  `ALDER_ATTEMPT=<attempt>` as part of pane creation, so a crash cannot leave
  an unattributable pane between creation and binding. An unbound attempt
  adopts a session bearing its identity; an exited holding pane is adopted or
  replaced. A bound session whose engine is still live is refused because it
  is genuinely already running.

The worktree is given `alder` and nothing else, so a worker cannot dispatch.

### Tiers

Six rungs, each pinning a model **and** a reasoning effort. The default is
`terra`. An unknown name is an error, never a fall-through to a CLI default:
falling through would launch a worker at an unknown model and record nothing.

| rung | provider | model | effort | falls back to |
| --- | --- | --- | --- | --- |
| `luna` | codex | `gpt-5.6-luna` | high | `sonnet` |
| `terra` | codex | `gpt-5.6-terra` | xhigh | `opus` |
| `sol` | codex | `gpt-5.6-sol` | xhigh | `fable` |
| `sonnet` | claude | `claude-sonnet-5` | high | `luna` |
| `opus` | claude | `claude-opus-5` | xhigh | `terra` |
| `fable` | claude | `claude-fable-5` | xhigh | `sol` |

A rung whose provider is currently rate-limited is served by its counterpart
on the other ladder.

Codex rungs run `codex exec` with `approval_policy=never`,
`sandbox_mode=workspace-write`, network access on — `alder` appends by pushing
to the store remote — and one extra writable root: the dispatching project's
`.git`. That last one is not optional. A worker lives in a *linked* worktree,
whose index, objects and branch ref all live in the project's `.git`, outside
the sandbox's workspace; without it the worker's first commit dies on
`Unable to create '…/index.lock': Operation not permitted`. It does not make
the leader's working tree writable, which is the part that matters.

`ALDER_WORKER_CMD` replaces the whole engine invocation,
which is how the verification tests spawn a stub instead of a model; the goal
is still appended as its final argument, and the tier is still what the attempt
records.

## Budget

`alderd budget` prints trailing-window token spend per provider, read from the
transcripts both CLIs already write, plus any recorded rate limit. No caps, no
percentages, no thresholds — the leader reads the number and judges.

The two halves measure different things and say so: codex spend is the sum of
per-turn `last_token_usage` from `~/.codex/sessions` (real spend in the
window), while claude spend is the sum of each session's *last* assistant usage
from `~/.claude/projects` (a floor — summing every entry would count one
conversation's cache reads dozens of times). `CODEX_HOME` and
`CLAUDE_CONFIG_DIR` move where it looks.

`alderd limit <provider> --minutes <n> [--why …]` records that a provider is
rate-limited until then, in `.alder/rate-limits.json`; `--clear` removes it.
The entry expires on its own — nothing sweeps it — and until it does, dispatch
serves that provider's rungs from the other ladder.

## Configuration

`.alder/driver.json`. The `.alder/` directory is gitignored, so this file is
machine-local by design: which engines exist and how aggressively to poll are
properties of the box, not durable project facts.

```json
{
  "engines": {
    "claude": { "cmd": "claude", "args": [] },
    "codex": { "cmd": "codex", "args": ["--full-auto"] }
  },
  "passDoc": ".alder/PASS.md",
  "tmuxSession": "alder-leader",
  "pollSeconds": 60,
  "debounceSeconds": 20,
  "maxIntervalSeconds": 1800,
  "passTimeoutSeconds": 3600,
  "maxPassesPerSession": 25,
  "notify": "terminal-notifier -title alder -message",
  "alder": "alder"
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `engines` | required | Engine name to the interactive CLI that provides it. The name matches `alder loop use <engine>`; Alder itself never validates it. |
| `passDoc` | required | The pass prompt document. A bootstrap injection points the engine at it, and changing its contents ends the current session era. |
| `tmuxSession` | `alder-leader` | The tmux session name. It also becomes the pass handle, `tmux:<session>`. |
| `pollSeconds` | 60 | Poll interval, also the interval used while awaiting a pass. |
| `hintPollSeconds` | 1 | How often to stat the local append marker between full polls. |
| `debounceSeconds` | 20 | How long a fire condition must hold before injecting. |
| `maxIntervalSeconds` | 1800 | Ceiling between passes. It also overrides both deferrals. |
| `passTimeoutSeconds` | 3600 | How long an open pass may live before the driver ends it as `timeout`. |
| `maxPassesPerSession` | 25 | Passes one session may serve before rotation. |
| `notify` | none | Shell command invoked with one message argument, on a crashed or timed-out pass, an unknown engine name, and a repeated store outage. A standing condition is reported once, not once per poll. |
| `alder` | `alder` | Path to the `alder` binary. |

## What one poll does

1. Read the head and the loop fold. A `store_unavailable` result is retried
   silently and notified after three consecutive polls.
2. If a pass is open, adopt it and wait; this is also how a daemon restart
   recovers. Nothing else happens while a pass is open.
3. If the loop is paused, idle.
4. Refresh observations, then compute the trigger kinds that hold:
   `manual` (a nudge is pending),
   `log` (the head advanced past the last pass's `ended_seq`),
   `observations` (refresh reported a change), and
   `due` (a requested wake time arrived, or `maxIntervalSeconds` elapsed).
5. Decide: idle if nothing holds, hold if the injection should wait, fire
   otherwise.

Firing is strictly ordered:

1. **Reconcile the session.** Kill it if the desired engine changed, a rotation
   is pending, the pass document changed, the pass budget is spent, or the
   session is not one this daemon started. Create it with
   `tmux new-session -d -s <session> 'caffeinate -i <cmd> <args>'` and mark the
   next injection as a bootstrap.
2. **Record intent.** `alder loop wake --engine <engine> --handle tmux:<session>
   --trigger …` returns the pass ID. If the wake is rejected with `pass_open`,
   another writer opened a pass in the last few seconds; the driver concedes
   and does nothing. It never ends a pass it did not find already open.
3. **Inject.** `tmux send-keys` with either
   `Read <passDoc>, then run one pass (pass-id: <id>; triggers: <kinds>).` or
   `Run one pass (pass-id: <id>; triggers: <kinds>).`
4. **Await.** Poll `alder show <pass-id> --json` until the pass ends. If the
   pass's *own* recorded tmux session died, append
   `pass end <id> --outcome crashed`; past the timeout, `--outcome timeout`.
   Both notify.

The crash check reads the handle on the pass, not the driver's own session
name. A pass another writer opened may name a different tmux session, or none
at all (`codex:019f…`), and the driver may only claim `crashed` for a tmux
session it actually checked. For anything else, `timeout` is the only verdict
it can honestly reach.

Passes are serialized. Alder rejects a second `loop wake` while one is open, so
two passes cannot run even if two drivers do. The driver that loses the race
concedes; the next poll adopts whatever pass is open and resolves it under the
rules above.

## Local hint

Every confirmed append by the `alder` CLI touches `.alder/last-append`.
Between full polls — and between `show` polls while a pass runs — the driver
stats that marker every `hintPollSeconds` and runs its next full poll as soon
as the mtime moves past its last read, so an append made on this machine is
noticed in about a second rather than up to `pollSeconds`. The hint has zero
correctness weight: it only ever causes a status read that would have happened
anyway, appends from other machines still ride the ordinary poll, and a
missing or stale marker changes nothing.

## Deferrals

- **Debounce.** A burst of log commits produces one pass, not one per commit.
- **Attached client.** If `tmux list-clients` shows someone watching, the
  injection waits so it does not land under their cursor.

Neither deferral survives `maxIntervalSeconds` — a loop that never runs is
worse than an inconvenient injection — and neither survives a pending nudge,
because a nudge is the human overriding this politeness on purpose.

## Trigger kinds are not scope

`manual`, `log`, `observations`, and `due` are provenance recorded on the pass
and repeated in the injection. They never narrow the pass. A pass woken by
`observations` still runs its complete sync, because the driver cannot know
what else changed and is not allowed to guess.

## The canary

Everything above is tested against stubs and sandboxes. One thing is not, and
cannot be: a real model, on a real item, all the way through. That is the
canary, and it is run once by the leader after this lands — dispatch one
narrow, well-specified item with `alderd spawn <id> luna` and watch for five
things, in this order:

1. the worker **commits** on its branch from inside the sandbox;
2. it **appends** to the log from inside the sandbox (`alder attempt edit`);
3. it asks something (`alder work ask`), the answer is relayed back with
   `.alder/resume <codex-session> "<the ruling>"`, and the same session
   continues rather than a new one starting;
4. it leaves a `ready for review` note;
5. the leader reviews the branch, runs the gates on it, and merges.

The mechanisms underneath 1–3 were each probed before the canary was spent,
because each had a way to fail silently:

- a `workspace-write` worker in a linked worktree **cannot commit** without
  the project's `.git` as a writable root — it dies on `index.lock`. Fixed
  above, and verified both ways.
- an append needs `network_access=true`: `alder` pushes to the store remote
  over ssh, which works from inside the sandbox, and fails to resolve DNS
  without it.
- `codex exec resume` inherits no model, effort or sandbox, which is why the
  relay is `.alder/resume` and not a hand-typed line.
- a fresh worktree codex has never seen needs no trust prompt: `codex exec`
  with `approval_policy=never` runs and commits straight away.

## Testing

Decision logic lives in `src/decide.rs` as pure functions over a snapshot and
is unit tested without tmux or Git. `tests/driver.rs` runs the orchestration
against a fake world that models the loop fold, so the ordering rules — intent
before effects, one open pass, crash repair — are checked rather than asserted.
`src/spawn.rs` does the same for dispatch against a fake host.

The shell-outs themselves are tested against the real thing on a private tmux
server: `tests/host_tmux.rs` for the loop's, and `tests/spawn_host.rs` for a
whole dispatch — a real git repository, a real pane, a stub engine that records
the argv it was handed and then exits so the pane's survival is observable.
`scripts/tests/verify-spawn.sh` runs the same ground against a real `alder`
binary and a throwaway log.
