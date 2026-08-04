# alderd

`alderd` decides *when* to run the executor, and nothing else. It never
decides *what* the executor should do, and — since the execution extraction —
it no longer knows *how* the executor runs: no tmux, no engines, no sessions,
no panes. When a trigger fires it runs one configured shell command; whatever
that command drives is its own business.

The daemon holds no API token, links no model client, and reads no work,
attempt, or question state. It shells out to the `alder` CLI for everything it
knows about the log. Its complete read surface is one command:

1. `alder status --json` — the head, and the loop section's paused flag,
   nudge sequence, and review deadlines. Everything else in the document is
   ignored.

And it appends **nothing**: the log never mentions its own readers. The wake
rule is one comparison — the head has moved past the last head this daemon
acted on — where the baseline is the daemon's machine-local notes,
`.alder/alderd-notes.json` (the last head acted on, and when). Losing the
notes is harmless: the daemon runs the command once more than it needed to,
the executor behind the command reads the fold, finds nothing new, and idles.

The loop runs no Git command and no tmux command.

Anything that requires judgment — which work to start, whether an attempt is
stale, what a report means — belongs to the executor. Anything mechanical
about running one — sessions, engines, rotation — belongs to the configured
command (and, underneath it, to tools like `alder-ext-runner`, which alderd
knows nothing about).

The boundary runs one way: Alder never calls `alderd`, `alderd` reaches the
log only through the `alder` CLI, and it links no Alder crate.

See [`docs/v0/LOOP.md`](../../docs/v0/LOOP.md) for the durable model behind
this: the loop controls, deferrals, and why a missed or duplicated wake is
harmless.

## Running it

```text
alderd [--root <project>]
```

The project must already be initialized (`alder init`), and `.alder/driver.json`
must exist. `alderd` reads the store remote and ref from `.alder/config.json`
so it watches exactly the ref Alder writes.

The daemon runs on macOS and Linux. It needs the `alder` binary on PATH (or
named in the config), `/bin/sh` for the command, and nothing else.

Logs go to standard error; the command's own output passes through to the
same place. Supervision is supplied for launchd only. To opt in for this
checkout, run `scripts/alderd-install.sh`; it renders
`contrib/com.alder.alderd.plist` into `~/Library/LaunchAgents/` and loads it.
Run `scripts/alderd-uninstall.sh` to unload and remove it. Re-running install
adopts the existing label and converges to the current checkout paths. On
Linux there is no equivalent yet; run the loop under a supervisor of your own.

## Configuration

`.alder/driver.json`. The `.alder/` directory is gitignored, so this file is
machine-local by design: what command runs the executor and how aggressively
to poll are properties of the box, not durable project facts.

```json
{
  "command": "alder-pass",
  "pollSeconds": 60,
  "hintPollSeconds": 1,
  "debounceSeconds": 20,
  "maxIntervalSeconds": 1800,
  "notify": "terminal-notifier -title alder -message",
  "alder": "alder"
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `command` | required | The shell command a wake runs, via `/bin/sh -c` in the project root with stdin closed. It receives the trigger names in `ALDERD_TRIGGERS`. |
| `pollSeconds` | 60 | Poll interval. |
| `hintPollSeconds` | 1 | How often to stat the local append marker between full polls. |
| `debounceSeconds` | 20 | How long a fire condition must hold before running. |
| `maxIntervalSeconds` | 1800 | Ceiling between runs. It also overrides the debounce, and it is the backstop for a deferral deadline the executor has not yet reviewed. |
| `notify` | none | Shell command invoked with one message argument, on a repeated store outage. A standing condition is reported once, not once per poll. |
| `alder` | `alder` | Path to the `alder` binary. |

**Migrating an older config.** `driver.json` rejects unknown fields. A config
written before the execution extraction must delete `engines`, `passDoc`,
`tmuxSession`, and `maxSessionAgeSeconds` — the engine table and the session
craft now live behind the configured `command` (see `crates/alder-ext-runner`
for the tool that took them) — and must add `command`, which is now the one
required field. Older configs still carrying `passTimeoutSeconds` or
`maxPassesPerSession` (from before session rotation replaced pass counting)
delete those too. Until then the daemon refuses to start with:

```text
invalid driver config `.alder/driver.json`: unknown field `engines`, expected
one of `command`, `pollSeconds`, `hintPollSeconds`, `debounceSeconds`,
`maxIntervalSeconds`, `notify`, `alder` at line 2 column 12
```

## What one poll does

1. Read the head and the loop fold. A `store_unavailable` result is retried
   silently and notified after three consecutive polls.
2. If the loop is paused, idle.
3. Compute the trigger kinds that hold:
   `manual` (a nudge request is later in the log than the noted head),
   `log` (the head differs from the noted head), and
   `due` (a deferral deadline — any entry in the loop section's
   `review_deadlines` — arrived and no run has been delivered since it
   passed, or `maxIntervalSeconds` elapsed since the last run).
4. Decide: idle if nothing holds, hold if the debounce has not settled (the
   ceiling and a pending nudge override it), fire otherwise.

Firing is strictly ordered:

1. **Run the command.** `/bin/sh -c <command>` in the project root, with
   `ALDERD_TRIGGERS=<kinds>` (for example `log,due`) in its environment, and
   wait for it. A non-zero exit is a failed poll: nothing is noted, and the
   next poll runs the same wake again.
2. **Note.** Write the head this run acted on, and the time, to
   `.alder/alderd-notes.json`. The note comes last on purpose: a crash before
   it re-runs the wake next poll, and a duplicate run is harmless — the
   executor reads the fold, and nothing durable records wakes.

Nothing else. The command exits; whatever it caused to be appended moves the
head, and the next poll sees it. Two daemons pointed at one log at worst run
duplicate wakes, which cost nothing for the same reason a crash costs
nothing.

Trigger kinds are provenance, never scope: a command run for `due` still has
to read the complete state, because the driver cannot know what else changed
and is not allowed to guess.

The rotation request in the loop section is deliberately not a daemon
trigger anymore. Its append moves the head, which wakes the command like any
other write; honoring the rotation — ending whatever session era the command
maintains — is the command's job, by reading status itself.

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

## Testing

Decision logic lives in `src/decide.rs` as pure functions over a snapshot and
is unit tested without a shell or Git. `tests/driver.rs` runs the
orchestration against a fake world that serves the loop fold, so the ordering
rules — command success before the notes write, duplicate runs harmless, no
`alder` call beyond the one read — are checked rather than asserted.
`tests/host_alder.rs` runs the real shell-outs against stub binaries.
`tests/sim_crash.rs` tears every effect of the wake lifecycle every way its
footprint allows and proves recovery converges, against the real driver over
a real in-memory CAS log.
