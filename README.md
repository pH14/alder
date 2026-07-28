# alder

A durable work-and-attempt ledger for autonomous engineering workflows.

Alder exists so a fresh agent can answer, without a previous session's
memory: what is actionable now, what has already been launched, what is
blocked or waiting on a person, and what changed while it was away.

Events are appended to a shared log stored in a Git remote ref. The remote
is authoritative, so independent writers coordinate through compare-and-append
against it rather than through a server. Current state is a deterministic
fold of those events into a local SQLite database, rebuildable from the log
at any time. Observations of external systems are refreshed into separate
local tables and never masquerade as ledger facts.

The model: work items with dependencies and acceptance checks, attempts at
that work, questions that park work on a human decision, and handoffs
submitted from outside. Human-readable output is the default; every command
also takes `--json`, the stable agent-facing surface.

    alder init --prefix hm
    alder work add --title "Ship the parser" --check tests:"cargo test passes"
    alder next
    alder work start hm-x7k2pq
    alder work finish hm-x7k2pq --attempt hm-x7k2pq-attempt-1

The repository is a two-crate workspace: the `alder` CLI, and
[crates/alder-log](crates/alder-log), a reusable Git-backed append-only
record log with no knowledge of Alder's domain.

Design docs live in [docs/v0](docs/v0): purpose and boundaries, the state
model, the CLI contract, the driving loop and its daemon, implementation
notes, and acceptance criteria.

Lineage: Corfu, Tango, Delos, FuzzyLog. The log is opaque and totally
ordered; semantics live in the state machines folded above it.
