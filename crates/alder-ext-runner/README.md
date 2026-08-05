# alder-ext-runner

Give a prompt to a model at some effort that runs somewhere; get a handle.

That is the whole of it. The runner launches one execution — a git worktree
on a branch you name, a tmux session running a model CLI with your prompt as
its final argument — and gives you back an opaque handle. You ask the handle
how it is doing, send it more input, or kill it. The result's location is the
branch you gave at start; the runner never reads or interprets it.

## The contract

```text
alder-ext-runner start --repo <path> --branch <name> --tier <name> --prompt-file <path>
alder-ext-runner status <handle>
alder-ext-runner send <handle> --file <path> [--force]
alder-ext-runner kill <handle>
```

**`start`** cuts (or adopts) a worktree beside the repository on the given
branch — a new branch is cut from the repository's current `HEAD` — writes
the runner's own resume machinery into its per-handle state directory
(`<state dir>/<handle>/`, never into the worktree; see the trust model
below), stamps the session with the resolved provider, and starts a detached
tmux session running the tier's engine with the prompt file's contents
(verbatim) as the engine's final argument. It prints the
handle on stdout — nothing else — and exits. It does not wait for the engine
to boot; there is no sleep anywhere on the path, and the tests read the
source for one.

The handle is deterministic per branch (it is derived from the branch name),
so a crashed or repeated `start` on the same branch converges on the same
execution instead of doubling it: a live engine under the handle is refused,
an exited pane is replaced (its result is already safe on the branch), an
existing worktree is adopted only after git proves it is on the expected
branch, and residue from a torn `git worktree add`/`remove` is swept using
git's registry as the authority. *Concurrent* starts of one branch serialize
too: the whole sequence runs under an exclusive per-handle file lock (in the
runner's state directory), so of two simultaneous starts one wins and the
other refuses — either immediately on lock contention, or because it then
sees the winner's live session — and never removes a worktree the winner is
using.

**`status`** prints one word:

- `running` — the engine process is still going;
- `done` — best effort, venue-specific. For tmux it means the engine process
  exited and its holding shell remains (the pane command ends `; exec bash`,
  so a one-shot engine leaves a live session behind). It says nothing about
  whether the run *succeeded* — the branch holds whatever the run left, and
  judging that is the caller's business;
- `dead` — nothing answers to the handle.

An optional second line carries detail (tier, worktree). A session that
exists but carries no runner marker reads `running`, because a session of
unknown provenance must never be presumed finished.

**`send`** delivers a local file's contents as input to the execution. The
route is decided by the provider **stamped into the session at start**,
never by the current tier table: reclassifying a tier in the config after a
start cannot change a live session's delivery protocol. The mechanics are
internal and invisible in the contract: an interactive engine (claude) gets
the file loaded into a tmux buffer and pasted raw, so no byte of it can
become shell syntax or a key name; an exited interactive engine is refused
rather than typed at, and so is a session that cannot *prove* an engine is
running (no engine marker) — the runner never pastes at a pane of unknown
provenance. A one-shot engine (codex) gets the bytes base64-armored into a
command that resumes the recorded codex session through the generated
`resume` script in the per-handle state directory — queued in the pane while
the engine still runs, executed by the holding shell once it exits. `codex
exec resume` inherits nothing from the session it resumes, so the resume
script repeats the model, effort and sandbox exactly as the launch pinned
them; it requires the exact session ID (recorded by a launcher-owned sidecar
into the state directory's `codex-session` marker, and validated as a
lowercase UUID before it reaches any command line) and never guesses from
`--last`.

A send file larger than **64 KiB** is refused by name: the armored route
rides tmux argv, which has a hard ceiling, and past it the delivery
mechanics stop being trustworthy. Deliver a pointer to the file instead.
Concurrent sends to one handle serialize on the same per-handle lock as
`start`; the loser refuses (for symmetry with `start`) rather than queueing,
so two sends can never interleave their paste and Enter in one pane.

Delivery is **at-least-once** and the runner never reads the pane afterwards
— with one deliberate exception. The pane sets its engine-exited marker
*before* it `exec`s the holding shell, and the interactive route re-checks
that marker between the paste and the Enter: if the engine died in that
window, the pasted bytes are sitting at (or ahead of) a shell where Enter
would *execute* them, so `send` clears the pasted text (`C-u`), refuses
loudly naming the session, and submits nothing. A residual window remains —
the engine can still die between that re-check and the Enter a few
milliseconds later — and is accepted honestly: closing it entirely would
require the pane to prove receipt, which no engine offers; the marker-first
pane ordering plus the re-check reduce the exposure to that sliver.

A delivery has two effects — paste, then one submitting Enter — and can tear
between them, leaving pasted text sitting unsubmitted. When Enter fails,
`send` retries it once immediately; if that also fails it stamps the session
with a torn marker and reports loudly. From then on the pane refuses every
further send — pasting more text at unsubmitted residue would mix two
messages — until a human kills or submits the pane, or a send with `--force`
delivers anyway (its Enter submits the residue along with the new message and
clears the marker).

**`kill`** ends the session, under the same per-handle lock, and **verifies**
it: the exit status of `tmux kill-session` is checked, and success is
reported only once no session answers to the handle. Killing a handle
nothing answers to is not an error — the caller kills to be sure — but it is
a distinct message (`nothing to kill`), so "I ended it" and "there was
nothing" read apart. The worktree and branch remain — they are the result.

There is deliberately **no ensure/residency verb**: keeping an execution
alive, rotating it, or restarting it on a schedule is the caller's policy,
not the runner's.

Two auxiliary verbs ride along because they are about the same machine-local
model accounts the tiers name:

```text
alder-ext-runner limit <provider> [--minutes <n>] [--clear] [--why <text>]
alder-ext-runner budget [--hours <n>] [--json]
```

`limit` records that a provider is rate-limited (the entry expires on its
own); until then `start` serves that provider's rungs from the counterpart
rung on the other ladder, and says so on stderr. The rate-limit file is
updated under an exclusive lock and written via an atomic rename, so
concurrent `limit` commands cannot drop each other's entries; a corrupt file
**fails open** — complained about loudly, treated as empty, rewritten whole —
because limits are hygiene, not authority. `budget` reads trailing-window
token spend per provider off the transcripts the CLIs already write
(`CODEX_HOME`, `CLAUDE_CONFIG_DIR` move where it looks).

`ALDER_EXT_RUNNER_CMD` replaces the whole engine invocation, which is how the
tests start a stub instead of a model; the prompt is still appended as the
final argument and the tier is still what the session records. The value is
**split on whitespace** — there is no shell quoting, so a path with spaces
cannot be expressed — and a set-but-empty value is a hard error at start,
never a silent fallback.

## Tiers

A tier pins a provider, a model, and a reasoning effort in one table, so
"which model did this run on" is answered by the launch rather than by
whatever the CLI's own default happened to be that week. An unknown tier is a
hard error before anything exists, never a fall-through to a CLI default.

The built-in table:

| rung | provider | model | effort | falls back to |
| --- | --- | --- | --- | --- |
| `luna` | codex | `gpt-5.6-luna` | high | `sonnet` |
| `terra` | codex | `gpt-5.6-terra` | xhigh | `opus` |
| `sol` | codex | `gpt-5.6-sol` | xhigh | `fable` |
| `sonnet` | claude | `claude-sonnet-5` | high | `luna` |
| `opus` | claude | `claude-opus-5` | xhigh | `terra` |
| `fable` | claude | `claude-fable-5` | xhigh | `sol` |

Codex rungs run `codex exec` with `approval_policy=never`,
`sandbox_mode=workspace-write`, network access on, and one extra writable
root: the launching repository's `.git`. That last one is not optional. An
execution lives in a *linked* worktree, whose index, objects and branch ref
all live in the repository's `.git`, outside the sandbox's workspace;
without it the first commit dies on `index.lock`. It does not make the
repository's working tree writable, which is the part that matters. Claude
rungs run the interactive `claude` CLI with the model and effort pinned.

## Configuration

Machine-local, because which models exist and what they cost are properties
of the box, not of any repository:

- config file: `$ALDER_EXT_RUNNER_CONFIG`, or
  `~/.config/alder-ext-runner/config.json`. Missing file = the built-in
  table.
- state: `$ALDER_EXT_RUNNER_STATE_DIR`, or `~/.local/state/alder-ext-runner/`
  — the rate-limit file, the per-handle locks, and one `<handle>/` directory
  per execution holding the codex resume script, the `codex-session` marker,
  and the session watcher's log.

The config file's whole format is the tier table, which **replaces** the
built-in one when present:

```json
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
```

`provider` is `codex` or `claude`. Every `counterpart` must exist, sit on the
other provider's ladder, and pair back; the loader refuses anything else. The
engine command per provider — how `codex exec` and `claude` are invoked,
sandboxing included — is code, not configuration: it is the part of a launch
that has to stay in step with the generated resume script, and both are
produced from one table on purpose.

## Trust model

The runner trusts its host user. Session environment variables and the tmux
server are same-user surfaces: any process running as the operator can
rewrite the markers the runner stamps, take its locks, or type at its panes.
The runner's defenses — provider stamps, engine markers, torn markers,
per-handle locks, the paste-then-re-check ordering — are aimed at
**staleness and accidents**: crashed starts, dead engines, concurrent
invocations of the runner itself, config drift. They are not, and cannot be,
a defense against a hostile local user, and nothing here pretends otherwise.

What the runner does *not* trust is the worktree. The worktree is written by
the execution — a model, running whatever the prompt made of it — so
everything the runner later trusts or executes (the codex resume script, the
`codex-session` marker) lives in the runner-owned per-handle state directory
instead, and the runner never executes anything from a worktree. The session
carries only the handle name; everything trusted is resolved from the state
directory by that name.

The codex delivery route is a **text contract**: the message is armored
through `base64` into a shell command substitution, which strips trailing
newlines and cannot carry NUL bytes. A message is text; binary payloads and
trailing-newline-significant content are outside the contract, and the 64 KiB
ceiling above bounds the armored command's size.

## What the runner is not

The runner knows nothing about any caller's domain. It was extracted from a
daemon that had welded execution to one project's workflow, and the
extraction is only worth having if the boundary holds:

- it depends on **no other crate in this workspace**, and nothing in this
  workspace depends on it — `tests/boundary.rs` asserts both directions
  against `cargo metadata` for *every* workspace member (after proving each
  expected package is actually present, so a rename cannot silently drop it
  from coverage) and sweeps every file in this crate for alder's log ref
  namespaces, loudly;
- it stamps only its own names into sessions (`ALDER_EXT_RUNNER_HANDLE`,
  `ALDER_EXT_RUNNER_ENGINE`, `ALDER_EXT_RUNNER_TIER`,
  `ALDER_EXT_RUNNER_PROVIDER`, `ALDER_EXT_RUNNER_WORKTREE`) and writes
  nothing at all into a worktree — its own machinery lives in its state
  directory;
- it composes no prompts, records no attempts, and never decides whether a
  result is good.

Because nothing reaches into it and it reaches into nothing, this crate is
**trivially movable to its own repository**: copy the directory, and
everything except the boundary test's counterpart list still passes. That
test then fails loudly — deliberately, rather than passing vacuously — and
deleting the workspace counterpart list along with the workspace is the one
conscious edit the move requires. Its tmux sandbox teardown is a crate-local
copy (`scripts/tmux-sandbox.sh`) for exactly that reason.

## Testing

The ordering rules — sweep, verify, cut, launch; refusal of a live engine;
convergence after a crash at every effect — are unit tested in `src/start.rs`
against a fake host. `tests/send_stub.rs` holds `send` to the old relay
craft's claims against a stub tmux that records its argv. The parts only the
world can answer run in `tests/start_host.rs` against a real git repository
and a real tmux server that is nobody else's: a `tmux` shim first on PATH
aims every call at a private socket (`$TMUX` unset — `TMUX_TMPDIR` alone
isolates nothing inside a tmux pane), and teardown kills one session, by
exact name, only after proving the sandbox server holds nothing else.

`scripts/verify-start.sh` runs the same ground end to end against the built
binary: an unknown tier refused before anything exists, the prompt as one
argv element, `status` walking running → done → dead, and the real tmux
server provably untouched.
