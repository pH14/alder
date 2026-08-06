# alderd

`alderd` decides when to run one configured command. In the standard Alder
setup, that command wakes the **executor**, the agent that reads current
project state and decides what work to do. The daemon does not make those
decisions itself.

The daemon also does not know how the executor runs. It has no model client,
API token, tmux logic, engine names, sessions, or panes. When it is time to
act, it runs the shell command from `.alder/driver.json` and waits for that
command to finish.

## What the daemon reads and writes

The daemon learns about the shared log by running exactly one Alder command:

~~~text
alder status --json
~~~

From that document it reads the current **head**, which is the sequence number
of the latest log event, and three fields from the loop section: whether the
loop is paused, the sequence of the latest nudge, and the review deadlines. A
**nudge** is a request made with `alder loop nudge` to run the command as soon
as possible. The daemon ignores work items, attempts, questions, and every
other field.

The daemon never appends to the Alder log. Wakes are not project facts, so the
log does not record that the daemon ran or that an executor read it. The only
state the daemon writes is `.alder/alderd-notes.json`. That local file records
the last log head for which this daemon successfully ran the command and the
time it made that decision.

Losing the notes file is safe. The daemon may run the command one extra time,
but the executor reads the current state again and has nothing to do if
nothing needs attention. A missed wake is also safe because a later poll sees
the head mismatch. Running the command is called a **wake** in this document.

The daemon runs no Git or tmux commands. Anything that needs judgment belongs
to the executor. Anything that starts or manages the executor belongs to the
configured command and tools that command uses, such as
`alder-ext-runner`.

The dependency boundary goes one way: Alder never calls `alderd`, and
`alderd` reaches the log only by running the `alder` CLI. The daemon does not
link any Alder crate.

See [`docs/v0/LOOP.md`](../../docs/v0/LOOP.md) for the log rules behind this
design.

## Running it

~~~text
alderd [--root <project>]
~~~

The project must already have been initialized with `alder init`, and
`.alder/driver.json` must exist. The daemon reads the store remote and ref
from `.alder/config.json` so it watches the same log that Alder writes.

The daemon supports macOS and Linux. It needs the `alder` binary on `PATH`,
unless the config names a different path, and it needs `/bin/sh` to run the
configured command.

Logs go to standard error. The configured command's output goes there too.

The repository includes launchd support for macOS:

- `scripts/alderd-install.sh` writes
  `~/Library/LaunchAgents/com.alder.alderd.plist` for the current checkout
  and loads it.
- `scripts/alderd-uninstall.sh` unloads and removes that file.
- Running the install script again updates the existing launchd job to use
  the current checkout paths.

Linux supervision is not included. Run `alderd` under a supervisor of your
choice.

## Configuration

`.alder/driver.json` is ignored by Git because its command and timing settings
belong to one machine, not to the shared project log.

~~~json
{
  "command": "scripts/ensure-executor",
  "pollSeconds": 60,
  "hintPollSeconds": 1,
  "debounceSeconds": 20,
  "maxIntervalSeconds": 1800,
  "commandTimeoutSeconds": 600,
  "notify": "terminal-notifier -title alder -message",
  "alder": "alder"
}
~~~

`scripts/ensure-executor` is this repository's standard command. On every
wake it first runs `alder refresh` to update observations. It then uses
`alder-ext-runner` to keep one executor session available, replace an old
session or honor a rotation request, and send the trigger names to the
session. The command starts or messages the executor and returns; it does not
wait for the executor to finish its work.

For `alder refresh` to observe runs started by the runner,
`.alder/config.json` includes this execution probe:

~~~json
{"observer": "runner", "probe": "scripts/observe-runner.sh \"$1\""}
~~~

| Field | Default | Meaning |
| --- | --- | --- |
| `command` | required | Shell command run for each wake. The daemon runs `/bin/sh -c` in the project root, closes standard input, and puts the comma-separated trigger names in `ALDERD_TRIGGERS`. |
| `pollSeconds` | 60 | Time between full status reads. |
| `hintPollSeconds` | 1 | Time between checks of the local `.alder/last-append` file. A changed file causes an early full status read. |
| `debounceSeconds` | 20 | Time a fire condition must remain present before the command runs. This combines a burst of log writes into one wake. |
| `maxIntervalSeconds` | 1800 | Longest allowed time between successful wakes. Reaching this limit ignores the debounce delay and also makes a missed review deadline run again. |
| `commandTimeoutSeconds` | 600 | Maximum time for one command run. At the limit, the daemon kills the command, does not update its notes, and retries on a later poll. |
| `notify` | none | Shell command called with one message argument after repeated store failures or unreadable status documents. One continuing problem is reported once, not on every poll. |
| `alder` | `alder` | Path to the Alder CLI. Every `alder status` call has its own fixed 60-second timeout. |

### Updating an old config

Unknown fields are errors. A config from before executor startup moved out of
the daemon must remove `engines`, `passDoc`, `tmuxSession`, and
`maxSessionAgeSeconds`, then add the required `command` field. Still older
configs must also remove `passTimeoutSeconds` and `maxPassesPerSession`.

Until those fields are removed, startup reports an error such as:

~~~text
invalid driver config `.alder/driver.json`: unknown field `engines`, expected
one of `command`, `pollSeconds`, `hintPollSeconds`, `debounceSeconds`,
`maxIntervalSeconds`, `commandTimeoutSeconds`, `notify`, `alder` at line 2
column 12
~~~

## What happens during one poll

First the daemon runs `alder status --json`, with a 60-second timeout, and
checks that the result has enough information to make a safe decision.

These results count as a failed read:

- Alder reports `store_unavailable`.
- The command reaches its timeout.
- The status document has no head.
- The loop section has no `paused` value. A missing value is not treated as
  `false`.

The daemon retries failed reads without running the configured command. After
three consecutive failed polls, it calls the configured notification command.
It does not repeat the same notification on every later poll.

If the loop is paused, the daemon does nothing else during that poll.

Otherwise it determines which of these **triggers** are present. A trigger is
a reason to run the command:

- `manual`: the latest nudge was appended after the head in this daemon's
  notes.
- `log`: the current head differs from the head in the notes.
- `due`: a review deadline has arrived and this daemon has not run since that
  deadline passed, or `maxIntervalSeconds` has elapsed since the last run.

If no trigger is present, the poll ends. If a trigger has been present for
less than `debounceSeconds`, the daemon normally waits for another poll. A
manual trigger or the `maxIntervalSeconds` limit skips that delay.

### Running and then recording a wake

When the daemon decides to run, it performs two steps in this order:

1. It runs `/bin/sh -c <command>` in the project root, with standard input
   closed and `ALDERD_TRIGGERS=<kinds>` in the environment. For example,
   `ALDERD_TRIGGERS=log,due` reports both reasons. The daemon waits no longer
   than `commandTimeoutSeconds`.
2. Only after the command exits successfully, it writes the current head and
   the decision time to `.alder/alderd-notes.json`.

The recorded time is when the daemon decided to run, before the command
started. If a review deadline passes while the command is running, the next
poll still sees that the daemon has not run for that deadline.

If the command exits with a nonzero status, reaches its timeout, or the daemon
stops before writing the notes, the notes remain unchanged. The next poll
runs the same wake again. The command and executor must therefore be safe to
run more than once for the same head.

If the system clock moves backward and a recorded time appears to be in the
future, the daemon reports a warning and uses the current time instead.

The command may append new events. Those events move the log head, so the next
poll sees them. Two daemons can watch the same log and both run their
commands. In the standard setup this can cause an extra wake, but not a second
worker: the runner locks each handle and refuses to start another live run.

Trigger names explain why the daemon ran; they do not limit what the executor
should inspect. A command invoked for `due` must still read all current state.
The daemon cannot know what else changed.

The daemon does not treat a rotation request as a separate trigger. Appending
the request changes the head and causes the ordinary `log` trigger. The
configured command reads current status and decides how to replace the
executor session.

## Faster local notices

After a successful append, the `alder` CLI updates
`.alder/last-append`. Between full polls, the daemon checks that file every
`hintPollSeconds`. If its modification time changes, the daemon performs the
next full status read early, often within a second of a local append.

This file is only a speed hint. The daemon never uses its contents to make a
decision. An append from another machine, a missing file, or a stale
modification time is still handled by the regular full poll.

`.alder/alderd-notes.json` is also local, but it has a separate narrow job:
it records which head this daemon last ran for. It never changes the project
log or what the executor decides.

## Testing

`src/decide.rs` contains pure decision functions that accept a status
snapshot. Unit tests cover those functions without running Git or a shell.

`tests/driver.rs` runs the daemon against a fake status source and checks
observable ordering: the command must succeed before the notes change,
repeating a wake is safe, and one poll makes only one Alder read.

`tests/host_alder.rs` checks real subprocess behavior with stub commands.
`tests/sim_crash.rs` interrupts the wake at every external step and verifies
that another poll reaches the expected state using a real driver and an
in-memory log that rejects writes based on stale state.
