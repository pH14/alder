# alderd

`alderd` decides *when* to wake an Alder leader agent, and launches the workers
that leader dispatches. It never decides *what* either of them should do.

The daemon holds no API token, links no model client, and reads no work,
attempt, or question state. It shells out to the `alder` CLI for everything it
knows about the log, and it drives tmux for everything it does to the world.
The driving loop's complete read surface is two commands:

1. `alder status --json` — the head, and the loop section. Everything else in
   the document is ignored.
2. `alder refresh --json` — the `"changed"` bool.

And it appends **nothing**: the log never mentions its own readers. The wake
rule is one comparison — the head has moved past the last head this daemon
acted on — where the baseline is the daemon's machine-local notes,
`.alder/alderd-notes.json` (the last head acted on, and when). Losing the
notes is harmless: the daemon wakes the leader once more than it needed to,
the leader reads the fold, finds nothing new, and idles.

The loop runs no Git command. Only `alderd spawn` runs `git`, and only to cut
a worker its worktree.

Anything that requires judgment — which work to start, which rung to start it
on, whether an attempt is stale, what a report means — belongs to the leader.

The boundary runs one way: Alder never calls `alderd`, `alderd` reaches the log
only through the `alder` CLI, and it links no Alder crate.

See [`docs/v0/LOOP.md`](../../docs/v0/LOOP.md) for the durable model behind
this: the loop controls, deferrals, and why a missed or duplicated wake is
harmless.

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

The daemon runs on macOS and Linux. It needs `git`, `tmux`, and the `alder`
binary on PATH, and nothing else platform-specific: a pane runs the engine
directly, so keeping the host awake under a long worker is the host's business
— a launchd or systemd unit, or the machine's own power settings.

Logs go to standard error. Supervision is supplied for launchd only. To opt in
for this checkout, run `scripts/alderd-install.sh`; it renders
`contrib/com.alder.alderd.plist` into `~/Library/LaunchAgents/` and loads it.
Run `scripts/alderd-uninstall.sh` to unload and remove it. Re-running install
adopts the existing label and converges to the current checkout paths. On Linux
there is no equivalent yet; run the loop under a supervisor of your own.

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
  observable. Spawn writes `<worktree>/.alder/relay <session> <file>` for
  literal delivery of a ruling already recorded in the log. The adapter reads
  the leader-local file, reports one delivery to a working engine, and never
  treats pane input as an acknowledgement or synchronizes on worker progress.
  The worker's fresh `attempt.updated` is observed on the next normal pass,
  not demanded in the same instant. Delivery is at-least-once, so a duplicate
  relay is harmless; milestones are not expected on every poll.
  For a Codex holding shell it uses the private `<worktree>/.alder/resume` script;
  leaders never type that command themselves. It exists because `codex exec
  resume` inherits *nothing* from the session it resumes: no model, effort, or
  sandbox. The generated script repeats the launch exactly, and requires the
  exact session ID rather than unsafe `--last`. The strongest resumed-engine
  signal is its process-table argv, carrying both that UUID and the ruling
  text. Before starting Codex, spawn also starts a local sidecar that snapshots
  the Codex rollouts and stamps the first new one for this worktree onto the
  attempt. If that append is unavailable, the sidecar leaves its UUID for the
  tmux observer and `alder reconcile` names the repair as
  `codex_session_unstamped`.
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
  "passDoc": ".agent/skills/pass/SKILL.md",
  "tmuxSession": "alder-leader",
  "pollSeconds": 60,
  "debounceSeconds": 20,
  "maxIntervalSeconds": 1800,
  "maxSessionAgeSeconds": 21600,
  "notify": "terminal-notifier -title alder -message",
  "alder": "alder"
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `engines` | required | Engine name to the interactive CLI that provides it. The name matches `alder loop use <engine>`; Alder itself never validates it. |
| `passDoc` | required | The pass prompt document. A bootstrap injection points the engine at it, and changing its contents ends the current session era. |
| `tmuxSession` | `alder-leader` | The tmux session name the leader runs in. |
| `pollSeconds` | 60 | Poll interval. |
| `hintPollSeconds` | 1 | How often to stat the local append marker between full polls. |
| `debounceSeconds` | 20 | How long a fire condition must hold before injecting. |
| `maxIntervalSeconds` | 1800 | Ceiling between wakes. It also overrides both deferrals, and it is the backstop for a deferral deadline the leader has not yet reviewed. |
| `maxSessionAgeSeconds` | 21600 | How long one engine session may serve wakes before rotation. Nothing durable counts passes, so rotation is by wall-clock age. |
| `notify` | none | Shell command invoked with one message argument, on an unknown engine name and a repeated store outage. A standing condition is reported once, not once per poll. |
| `alder` | `alder` | Path to the `alder` binary. |

## What one poll does

1. Read the head and the loop fold. A `store_unavailable` result is retried
   silently and notified after three consecutive polls.
2. If the loop is paused, idle.
3. Refresh observations, then compute the trigger kinds that hold:
   `manual` (a nudge request is later in the log than the noted head),
   `log` (the head moved past the noted head),
   `observations` (refresh reported a change), and
   `due` (a deferral deadline — the loop section's `review_at` — arrived and
   no wake has been delivered since it passed, or `maxIntervalSeconds`
   elapsed since the last wake).
4. Decide: idle if nothing holds, hold if the injection should wait, fire
   otherwise.

Firing is strictly ordered:

1. **Reconcile the session.** Kill it if the desired engine changed, a rotation
   request is later in the log than the noted head, the pass document changed,
   the session outlived its age budget, or it is not one this daemon started.
   Create it with `tmux new-session -d -s <session> '<cmd> <args>'` and mark
   the next injection as a bootstrap.
2. **Inject.** `tmux send-keys` with either
   `Read <passDoc>, then read the current Alder state and act on it
   (triggers: <kinds>).` or
   `Read the current Alder state and act on it (triggers: <kinds>).`
3. **Note.** Write the head this wake acted on, and the time, to
   `.alder/alderd-notes.json`. The note comes last on purpose: a crash before
   it re-delivers the wake next poll, and a duplicate wake is harmless —
   the leader reads the fold, and nothing durable records wakes.

Nothing awaits. The leader acts and exits or idles; whatever it appends moves
the head, and the next poll sees it. Two daemons pointed at one log at worst
deliver duplicate wakes, which cost nothing for the same reason a crash
costs nothing.

## Local hint

Every confirmed append by the `alder` CLI touches `.alder/last-append`.
Between full polls the driver stats that marker every `hintPollSeconds` and
runs its next full poll as soon as the mtime moves past its last read, so an
append made on this machine is noticed in about a second rather than up to
`pollSeconds`. The hint has zero correctness weight: it only ever causes a
status read that would have happened anyway, appends from other machines still
ride the ordinary poll, and a missing or stale marker changes nothing. The
notes file is the same idea generalized — not "something was appended" but
"the last head I acted on" — and carries the same zero project-durable
weight.

## Deferrals

- **Debounce.** A burst of log commits produces one pass, not one per commit.
- **Attached client.** If `tmux list-clients` shows someone watching, the
  injection waits so it does not land under their cursor.

Neither deferral survives `maxIntervalSeconds` — a loop that never runs is
worse than an inconvenient injection — and neither survives a pending nudge,
because a nudge is the human overriding this politeness on purpose.

## Trigger kinds are not scope

`manual`, `log`, `observations`, and `due` are provenance repeated in the
injected line and recorded nowhere. They never narrow what the leader does. A
leader woken by `observations` still reads the complete state, because the
driver cannot know what else changed and is not allowed to guess.

## The canary

Everything above is tested against stubs and sandboxes. One thing is not, and
cannot be: a real model, on a real item, all the way through. That is the
canary, and it is run once by the leader after this lands — dispatch one
narrow, well-specified item with `alderd spawn <id> luna` and watch for five
things, in this order:

1. the worker **commits** on its branch from inside the sandbox;
2. it **appends** to the log from inside the sandbox (`alder attempt edit`);
3. it asks something (`alder work ask`), the recorded answer is relayed back
   with `.alder/relay <session> <file>`, and the same session continues rather
   than a new one starting;
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
  relay owns its generated `.alder/resume` implementation rather than exposing
  a hand-typed line.
- a fresh worktree codex has never seen needs no trust prompt: `codex exec`
  with `approval_policy=never` runs and commits straight away.

## Testing

Decision logic lives in `src/decide.rs` as pure functions over a snapshot and
is unit tested without tmux or Git. `tests/driver.rs` runs the orchestration
against a fake world that serves the loop fold, so the ordering rules —
session before injection before notes, duplicate wakes harmless, no `alder`
call beyond the two reads — are checked rather than asserted.
`src/spawn.rs` does the same for dispatch against a fake host.

The shell-outs themselves are tested against the real thing on a private tmux
server: `tests/host_tmux.rs` for the loop's, and `tests/spawn_host.rs` for a
whole dispatch — a real git repository, a real pane, a stub engine that records
the argv it was handed and then exits so the pane's survival is observable.
`scripts/tests/verify-spawn.sh` runs the same ground against a real `alder`
binary and a throwaway log.
