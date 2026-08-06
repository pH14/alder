# alder-ext-runner

`alder-ext-runner` starts one model run and gives the caller a name for
managing it. That name is the **handle**. The caller can use the handle to
check the run, send it more input, or stop it.

Each run gets its own Git worktree and branch. The runner starts a model
command in a detached tmux session and passes the prompt as the command's last
argument. It does not interpret the prompt or judge the result. The branch
contains whatever the model leaves behind.

## Commands

~~~text
alder-ext-runner start --repo <path> --branch <name> --tier <name> --prompt-file <path>
                       [--from <ref>] [--seed <src>:<relpath>]...
alder-ext-runner status <handle>
alder-ext-runner send <handle> --file <path> [--force]
alder-ext-runner kill <handle>
~~~

### `start`

`start` prepares a worktree beside the repository. If the branch does not
exist, the runner creates it from the repository's current `HEAD`, or from the
ref named by `--from`. If the branch already exists, the runner uses it
unchanged; `--from` does not reset an existing branch.

Before the model starts, every `--seed <src>:<relpath>` argument copies one
file into the worktree. The destination must be relative to the worktree. The
runner refuses a destination if it or any parent directory inside the
worktree is a symbolic link. A model can change its worktree between starts,
so following such a link could copy a file outside the worktree.

The runner then starts the tier's model command in a detached tmux session.
A **provider** is a supported model CLI, currently Codex or Claude. A
**tier** is a named configuration that selects a provider, model, and
reasoning effort. The prompt file's contents are passed unchanged as the
model command's final argument. Nothing is typed into the tmux pane, and the
runner does not wait for the model to become ready. There is no sleep in the
start path.

On success, standard output has exactly two lines:

~~~text
<handle>
tier <served>
~~~

The served tier can differ from the requested tier when one provider is
temporarily rate-limited. The caller must record the served value.

The handle is derived from the branch name, so repeated starts for the same
branch use the same handle. The runner takes an exclusive file lock for that
handle before changing its worktree or session. Two starts for the same
branch therefore cannot create two runs:

- If the handle already has a live model process, `start` refuses it.
- If the tmux pane remains but the model process has exited, `start` replaces
  the pane. The earlier run has already left its files on the branch.
- If a tmux session exists but the runner cannot prove that its model exited,
  `start` refuses to replace it.
- If the expected worktree already exists, the runner uses it only after Git
  confirms that it belongs to the requested branch.
- If an interrupted `git worktree add` or `git worktree remove` left an
  unregistered directory at the expected path, the runner removes that
  directory before trying again. It never removes a worktree that Git still
  lists.

The lock and all files needed to resume a run live under
`<state-dir>/<handle>/`, outside the worktree. This keeps a model from
changing files that the runner will later trust or execute.

### `status`

`status` prints one word:

- `running` means the model process is still running.
- `done` means the model process has exited but its holding shell remains in
  tmux. This is only a process-state report; it does not mean the model
  succeeded. The branch contains the result.
- `dead` means no tmux session answers to the handle.

A second line may report details such as the served tier or worktree. If a
tmux session exists but has no model-state marker from the runner, `status`
reports `running`. The runner will not assume an unfamiliar session is
finished.

### Exit codes

Callers should use exit codes and the documented standard-output shapes
instead of parsing error messages.

| Code | Meaning |
| --- | --- |
| 0 | The command completed. For `start`, standard output is the handle followed by `tier <served>`. |
| 1 | The command failed and no more specific code applies. |
| 3 | `start` found a live model under the handle. Standard output is exactly `handle <h>` so the caller can use that existing run. |
| 4 | Another `start`, `send`, or `kill` operation holds this handle's lock. A `send` caller should treat the message as already handled by the lock winner and must not kill the session because of this code. |
| 5 | `start` found a session whose model cannot be proved finished. For `send`, the run cannot receive the message: the model exited, no session answers to the handle, an earlier send stopped halfway, or a Codex session cannot be resumed. The caller may replace the run. |

These codes and the two `start` output shapes are tested in
`tests/contract.rs` and `tests/send_stub.rs`.

### `send`

`send` reads a local file and delivers its contents to the run. The runner
records the provider in the tmux session when `start` runs, and `send` uses
that recorded value. Changing the tier configuration later cannot change how
an existing session receives input.

For an interactive Claude run, `send` loads the file into a tmux buffer and
pastes it as raw text. This prevents the contents from becoming shell syntax
or tmux key names. The runner refuses to paste if the model has exited or if
the session has no marker proving that a model is running.

The Codex CLI exits after one response. For a Codex run, `send`
base64-encodes the text into a shell command. That command runs after the
current model command exits and resumes the exact Codex session through a
generated script in the handle's state directory.
The script repeats the launch's model, effort, and sandbox settings because
`codex exec resume` does not inherit them. A launcher-owned helper records
the Codex session ID in the state directory. The runner accepts only a
lowercase UUID and never guesses with `--last`.

The largest accepted send file is 64 KiB. The encoded Codex command travels
through tmux's argument list, which has a fixed size limit. Larger files are
refused instead of risking truncation; send a path or another short pointer
to the file instead.

The Codex route carries text, not arbitrary bytes. Shell command substitution
removes trailing newlines, and it cannot carry NUL bytes. Binary files and
messages whose final newlines matter are outside this command's contract.

All sends for one handle use the same exclusive lock as `start` and `kill`.
If another operation holds the lock, `send` exits with code 4 instead of
waiting. This prevents two messages from interleaving in one pane.

The runner does not wait for the model to acknowledge a message. If a process
stops after submission but before the caller receives success, the caller
cannot tell whether delivery happened. Retrying may deliver the message
twice, so callers must make duplicate delivery harmless.

An interactive send normally has two steps: paste the text, then send one
Enter key to submit it. The pane records that the model exited before it
starts the holding shell. After pasting and before pressing Enter, `send`
checks that record again. If the model died in that interval, Enter could run
the pasted text as a shell command. The runner clears the pasted text with
`C-u`, reports the failure, and does not press Enter.

The model can still exit in the few milliseconds between that final check and
Enter. Removing that remaining risk would require the model to confirm
receipt, and no supported model provides such a confirmation.

#### If a send stops halfway

Enter can fail after the text was pasted. The runner immediately retries
Enter once. If the retry also fails, it records a **torn-send marker**, which
means that unsubmitted text may remain in the pane, and returns an error that
names the session.

Later sends refuse that pane because adding another message could mix the two
messages. A person can stop the session or inspect and submit the existing
text. Alternatively, `send --force` pastes the new message despite the
marker; when its Enter succeeds, that Enter submits any old text together
with the new text and the runner clears the marker.

### `kill`

`kill` takes the handle's lock, runs `tmux kill-session`, and then checks that
the session is gone. It reports success only after no session answers to the
handle. If another process removed the session first, the final check still
allows `kill` to succeed.

Killing an already missing handle is not an error, but the output says
`nothing to kill` so callers can distinguish it from a session that this
command stopped. The worktree and branch remain because they contain the
result.

There is no command that keeps a run alive. Deciding when to restart or
replace a run belongs to the caller.

## Rate limits and token budgets

Two more commands report machine-local information about the provider
accounts used by the tiers:

~~~text
alder-ext-runner limit <provider> [--minutes <n>] [--clear] [--why <text>]
alder-ext-runner budget [--hours <n>] [--json]
~~~

`limit` records that a provider is temporarily rate-limited. A tier's
**counterpart** is the tier to use on the other provider during such a limit.
Until the entry expires, `start` uses that counterpart and reports the
substitution on standard error. If both providers are limited, it uses the
requested tier and explains why.

The rate-limit file is protected by its own lock and replaced atomically, so
simultaneous updates do not lose entries. If the file is corrupt, the runner
prints an error, treats the file as empty, and replaces it on the next write.
Rate limits affect convenience, not the truth of a run.

`budget` reads recent token use for each provider from the transcript files
that the model CLIs already write. `CODEX_HOME` and `CLAUDE_CONFIG_DIR`
change where it looks.

Setting `ALDER_EXT_RUNNER_CMD` replaces the complete model command. Tests use
this to run a stub instead of a model. The runner still appends the prompt as
the final argument and still records the served tier. The value is split on
whitespace; shell quoting is not supported, so it cannot express a path with
spaces. An empty or whitespace-only value is an error, not a request to use
the default command.

## Tiers

Every tier names its provider, full model name, and reasoning effort. The
runner passes all three explicitly. It never relies on a model CLI default,
and an unknown tier fails before a worktree or session is created.

The built-in tiers are:

| Tier | Provider | Model | Effort | Counterpart |
| --- | --- | --- | --- | --- |
| `luna` | Codex | `gpt-5.6-luna` | high | `sonnet` |
| `terra` | Codex | `gpt-5.6-terra` | xhigh | `opus` |
| `sol` | Codex | `gpt-5.6-sol` | xhigh | `fable` |
| `sonnet` | Claude | `claude-sonnet-5` | high | `luna` |
| `opus` | Claude | `claude-opus-5` | xhigh | `terra` |
| `fable` | Claude | `claude-fable-5` | xhigh | `sol` |

Codex tiers run `codex exec` with approval policy `never`, network access
enabled, and either the `workspace-write` or `full-access` sandbox described
below. The built-in tiers use `workspace-write`. That sandbox can write the
run's worktree and the launching repository's `.git` directory. A linked
worktree keeps its index, objects, and branch ref inside that `.git`
directory, so the model could not commit without this extra writable path.
The repository's main working tree does not become writable.

Claude tiers run the interactive `claude` CLI with the model and effort
specified.

## Configuration

Tier configuration is machine-local because available models and account
costs depend on the machine:

- The config file is `$ALDER_EXT_RUNNER_CONFIG`, or
  `~/.config/alder-ext-runner/config.json` by default. If it is missing, the
  built-in tiers are used.
- State lives at `$ALDER_EXT_RUNNER_STATE_DIR`, or
  `~/.local/state/alder-ext-runner/` by default. It contains the rate-limit
  file, per-handle locks, and one directory per handle with the Codex resume
  script, session ID, and watcher log.

When a config file exists, its `tiers` table replaces the built-in table:

~~~json
{
  "tiers": {
    "luna": {
      "provider": "codex",
      "model": "gpt-5.6-luna",
      "effort": "high",
      "counterpart": "sonnet"
    },
    "sonnet": {
      "provider": "claude",
      "model": "claude-sonnet-5",
      "effort": "high",
      "counterpart": "luna"
    }
  }
}
~~~

`provider` must be `codex` or `claude`. Every `counterpart` must name an
existing tier on the other provider, and the two tiers must name each other.
The runner rejects any other arrangement.

The commands used to start Codex and Claude are part of the program, not the
config file. A Codex start command and its generated resume script must change
together, so both come from the same code.

A Codex tier may set `"sandbox"`:

- `"workspace-write"` is the default when the field is absent. The run can
  write its worktree and the repository's `.git` directory, but not other
  paths. Worker tiers should use this setting.
- `"full-access"` disables the filesystem sandbox. The run can write anywhere
  the operator's account can write and can start any program. The approval
  policy remains `never`, so the CLI will not ask first.

`full-access` is intended for an executor that must append to the shared Alder
log through the main checkout and start other tools such as a review run.
Workers do not need those permissions. Configure a full-access tier only for
a model you would allow to use your account directly. The generated resume
script repeats the sandbox setting, so a full-access run resumes with the same
access.

Claude tiers cannot set `sandbox` because the Claude CLI controls its own
permissions. The config loader reports this as an error instead of ignoring
the setting.

## What the runner trusts

The runner trusts every process running as the same operating-system user.
Such a process can change tmux session variables, modify runner state files,
take runner locks, or type into panes. The provider and model markers, locks,
and send checks protect against crashes, stale state, configuration changes,
and simultaneous runner commands. They do not protect against a hostile local
process running under the operator's account.

The runner does not trust the model's worktree. A model can write any file
there. Files that the runner later reads as trusted state or executes, such
as the Codex resume script and session ID, therefore live in the runner's
state directory. The runner never executes a file from the worktree. A tmux
session stores only the handle; the runner uses that handle to find its own
state directory.

## Project boundary

The runner does not know about Alder work items, attempts, logs, or prompts.
It only starts model commands and manages them by handle.

- The crate depends on no other crate in this workspace, and no workspace
  crate depends on it. `tests/boundary.rs` checks both directions with
  `cargo metadata`. It also checks every file in this crate for Alder log ref
  names.
- The runner writes only its own environment variable names into tmux:
  `ALDER_EXT_RUNNER_HANDLE`, `ALDER_EXT_RUNNER_ENGINE`,
  `ALDER_EXT_RUNNER_TIER`, `ALDER_EXT_RUNNER_PROVIDER`, and
  `ALDER_EXT_RUNNER_WORKTREE`. It writes no runner state into the worktree.
- It does not build prompts, record attempts, or decide whether a run's result
  is acceptable.

The crate can be moved to its own repository by copying its directory. After
such a move, the boundary test intentionally fails until its list of Alder
workspace crates is removed. The crate keeps its own tmux cleanup helper at
`scripts/tmux-sandbox.sh` for the same reason.

## Testing

Unit tests in `src/start.rs` check the required order: remove an unregistered
leftover directory, verify or create the worktree, then start the model. They
also cover live-model refusal and recovery after an interruption at every
external step.

`tests/send_stub.rs` checks `send` against a tmux stub that records its
arguments. `tests/start_host.rs` uses a real Git repository and a private tmux
server. A shim unsets `TMUX` and directs every tmux command to a private
socket; cleanup stops one named session only after confirming that no other
session uses that socket.

`scripts/verify-start.sh` runs an end-to-end check against the built binary.
It verifies that an unknown tier fails before creating anything, the complete
prompt arrives as one argument, `status` moves from `running` to `done` to
`dead`, and the user's normal tmux server remains untouched.
