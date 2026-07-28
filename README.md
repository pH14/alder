# alder

Coordination for autonomous engineering work as **state-machine replication
over a single-leader shared log** — plus the refined, provider-neutral
process (leader, reviewer, handoff, watchdog roles) for acting on it.

Two documents anchor all design context:

- **[docs/SHARED-LOG.md](docs/SHARED-LOG.md)** — the why: the model, the
  protocol rules, placement and privacy, the layer architecture
  (core / process / bindings / profile), and the rulings to date.
- **[docs/MODEL.sql](docs/MODEL.sql)** — the what: the entire state model
  as executable SQL — tables, legal transitions (the `transitions` table is
  the state diagram), and the derived views (`ready`, `stale`, `budget`, …).
- **[docs/SCENARIOS.md](docs/SCENARIOS.md)** — the test suite: situations
  the design must handle, stated without mechanism. Design choices are
  argued against these.
- **[docs/WALKTHROUGH.md](docs/WALKTHROUGH.md)** — the feel: ten scenarios
  as full CLI sequences, for judging the API surface.

Read them first; when prose and DDL disagree, the DDL is the bug report.

Lineage: LogAct (arXiv:2604.07988), Corfu, Tango, Delos, FuzzyLog, Calvin.
The log is opaque, app-agnostic, and totally ordered; semantics live in
the apps above it — item tracking, resources, decisions, each its own
state machine (Tango's shared objects) — and all state is a deterministic
fold. Storage is git; queries are sqlite.

Status: design phase. First consumer: Harmony.
