# Alder v0 implementation choices

The product contract remains `README.md`, `MODEL.md`, `CLI.md`, `LOOP.md`, and
`ACCEPTANCE.md` in this directory. The following choices fill in details those
documents intentionally leave open.

- A Git log commit contains one new, pretty-printed event at
  `events/<20-digit-seq>-<event-id>.json`. The first event commit has no
  parent; every later event commit has the previous shared head as its single
  parent. A normal non-force push is the expected-head compare-and-swap.
- The configured remote ref is authoritative. `current_head` queries it with
  standard Git transport and fetches the ref before reading missing objects;
  it never substitutes a local or remote-tracking ref. Creating an event
  commit is preparatory, and only a successful push makes it durable.
- GitHub is the initial shared host but is not a code dependency. Credentials
  and ref permissions are handled by Git. The Alder ref accepts direct
  fast-forward pushes and rejects force pushes and deletion; it does not use a
  pull request per event.
- Git commit IDs are internal revisions. Stable JSON results expose the
  event-sequence head as an integer and also include the Git revision where it
  is useful for diagnostics.
- Work tokens use six lowercase characters drawn from a ULID.
  Event envelopes retain the complete uppercase ULID. Attempts and questions
  use the specified never-reused per-work ordinal.
- The SQLite projection stores normalized durable base tables and defines the
  required named projections as tables or views. Rebuild deletes and replaces
  every row in one transaction; the projection holds nothing that is not a
  fold of the log, observations included.
- CLI metadata values are parsed as JSON scalars or objects when possible and
  otherwise stored as strings. This keeps common `key=value` calls concise
  without assigning meaning to metadata keys.
- One `attempt edit` invocation performs exactly one event-kind transition:
  handle binding (with optional metadata) or a progress and check update.
  Ending is `attempt end`, a separate command over a separate event. This
  avoids combining independently meaningful attempt transitions in one storage
  event.
- A check result is recorded as `--satisfied <check>` or `--failed <check>`,
  each repeatable and each requiring `--evidence`. `pending` is the state every
  check starts in, so there is no flag that sets it.
- `work block --until` accepts an RFC 3339 instant and stores it verbatim.
  An absolute time means a reader of the event never needs to know when the
  block was recorded to know when review is due.
- The driver lives in the separate `alderd` crate and reaches the log only by
  running the `alder` CLI. It links no Alder code, so a change to Alder's
  internals cannot silently change the driver's behaviour, and the coupling
  that remains is the documented `--json` contract.
- Observer diagnostics retain at most 4,096 characters of standard error from
  each execution. Failed standard output is never retained as inventory.
- `cargo mutants` exercises the complete crate with locked dependencies and
  all features. Mutation runs must have no unexplained survivors; equivalent
  mutations are removed through clearer code or narrowly documented
  exclusions rather than broad file or module skips.
- The loop is mutation-tested as one surface, because its two halves are only
  correct together: the fold's loop-control arms and the `loop` section
  `status` reports in the `alder` crate, with `next_review_at` in
  `alder-work`; `decide`, `loop_state`, and `driver` in `alderd`. Both halves run with no survivors
  and no exclusions. What that buys is specific: a decision the driver
  reaches by a path no test walks — a diagnostic nobody reads, a match guard
  that catches more than it names, a hash that varies without being FNV — is
  exactly what a mutation exposes and an ordinary suite does not.
