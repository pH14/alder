# alder

A durable work-and-attempt log for autonomous engineering workflows.

Alder exists so a fresh agent can answer, without a previous session's
memory: what is actionable now, what has already been launched, what is
blocked or waiting on a person, and what changed while it was away.

Events are appended to a shared log stored in a Git remote ref. The remote
is authoritative, so independent writers coordinate through compare-and-append
against it rather than through a server. Current state is a deterministic
fold of those events into a local SQLite database, rebuildable from the log
at any time. Observations of external systems are refreshed into separate
local tables and never masquerade as log facts.

The model: work items with dependencies and acceptance checks, attempts at
that work and questions that park work on a human decision. Human-readable
output is the default; every command
also takes `--json`, the stable agent-facing surface.

## A worked session

These are literal, unedited outputs from one session on 2026-07-31. I made a
fresh Git client and a local bare Git remote under
`/private/tmp/alder-readme-examples.2t5a97`, configured that remote as
`scratch`, then ran the built `alder` binary from the client. The generated
IDs, log heads, and timestamps are left in place. The final state reads follow
the earlier commands in this session.

Initialize the client against its `scratch` remote:

```text
$ alder init --prefix ex --remote scratch
initialized /private/tmp/alder-readme-examples.2t5a97/client/.alder/config.json · scratch refs/heads/alder
```

Admit work and name the checks that must be satisfied:

```text
$ alder work add --title "Document the parser" --spec README.md --priority 60 --check fmt:"cargo fmt --check" --check tests:"cargo test --workspace"
ex-qq6rkd
```

Start an attempt, bind its external handle, record a check result with
evidence, and then end the failed attempt:

```text
$ alder work start ex-qq6rkd --meta engine=gpt-5.6-terra --meta reasoning_effort=high
ex-qq6rkd-attempt-1

$ alder attempt edit ex-qq6rkd-attempt-1 --handle process:local-4242 --meta host=builder-1
ex-qq6rkd-attempt-1  bound process:local-4242

$ alder attempt edit ex-qq6rkd-attempt-1 --failed tests --evidence "cargo test --workspace: parser fixture fails" --note "Reproduced the parser failure."
ex-qq6rkd-attempt-1  updated

$ alder attempt end ex-qq6rkd-attempt-1 --outcome failed --why "The failing test needs a new parser design."
ex-qq6rkd-attempt-1  ended failed
```

Ask for a decision and record the answer. Answering leaves the work blocked
until a later, explicit `work unblock`:

```text
$ alder work ask ex-qq6rkd "Should the parser accept blank headings?"
ex-qq6rkd-question-1

$ alder question answer ex-qq6rkd-question-1 "No; reject them with a diagnostic."
ex-qq6rkd-question-1  answered
```

The loop's controls are standing instructions to whatever drives it —
the log records no runs of the loop itself:

```text
$ alder loop use gpt-5.6-terra
loop engine gpt-5.6-terra

$ alder loop rotate --why "Engine upgraded; start the next wake on a fresh session."
rotation requested
```

For a ready item to read, the same session then admitted another checked work
item:

```text
$ alder work add --title "Refresh the release notes" --spec docs/releases.md --priority 30 --check docs:"release notes reviewed"
ex-y60hza
```

Read the current state, actionable work, and the question's detail:

```text
$ alder status --section ready
head 11

loop
  engine gpt-5.6-terra

ready
  ex-y60hza  Refresh the release notes  priority 30

$ alder next
ex-y60hza  Refresh the release notes  priority 30

$ alder show ex-qq6rkd-question-1
{
  "answer": "No; reject them with a diagnostic.",
  "answered_by": "phemberger",
  "answered_seq": 7,
  "answers": [
    {
      "actor": "phemberger",
      "answer": "No; reject them with a diagnostic.",
      "seq": 7
    }
  ],
  "asked_seq": 6,
  "id": "ex-qq6rkd-question-1",
  "stranded": null,
  "text": "Should the parser accept blank headings?",
  "work_id": "ex-qq6rkd"
}

history
  #6  "question.asked"  "2026-07-31T19:15:59.889207Z"
  #7  "question.answered"  "2026-07-31T19:16:00.236487Z"
```

The repository is a four-crate workspace: the `alder` CLI, the
[`alderd` daemon](crates/alderd/README.md), `alder-log`, a reusable Git-backed
append-only record log with no knowledge of Alder's domain, and the
[`alder-model`](crates/alder-model/README.md) dev-only Stateright model checker.

Design docs live in [docs/v0](docs/v0): purpose and boundaries, the state
model, the CLI contract, the driving loop and its daemon, implementation
notes, and acceptance criteria.

Lineage: Corfu, Tango, Delos, FuzzyLog. The log is opaque and totally
ordered; semantics live in the state machines folded above it.
