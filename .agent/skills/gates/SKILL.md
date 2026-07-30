---
name: gates
description: Run the repository's required Cargo gates and record branch-SHA evidence without unsafe tmux or review-waiting machinery.
---

# Gates

This manual is advisory craft for the `gates` check. The item's check
description is the binding criterion. Read this manual before satisfying or
verifying that check.

Run the gates in the branch worktree and preserve their outputs with the named
branch SHA they tested. The evidence is not “tests ran”: it is the command
outputs at `git rev-parse HEAD` (or the explicit `work/<id>` SHA) on that
branch. A later commit needs fresh gate evidence.

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --quiet
cargo test --workspace --quiet
```

`cargo fmt --check` is already terse when it succeeds and shows the needed
diff on failure, so it takes no quiet flag. `--quiet` on clippy and test drops
success-path build chatter; warnings, the formatting diff, and failed-test
output remain verbose. Clippy passes only with zero warnings. Capture the
SHA and the outputs before recording a satisfied `gates` check.

## Tmux-spawning tests

A test that spawns tmux must use a private server on every tmux call:
`tmux -S <socket>` (or `tmux -L <label>`) with `TMUX` unset. Inside a tmux
pane, inherited `$TMUX` points at the real server and takes precedence, so
`TMUX_TMPDIR` alone is not isolation. Tear down only the exact session name,
after proving the private server contains only this test's sessions. Never use
a bare `kill-server`, a pattern, or anything aimed at the default server: it
can kill unrelated workers.

The same rule of thumb applies to review waiters: a check must observe the
thing under test, not its own machinery. Do not wait for `codex review` with
`pgrep -f 'codex review'`; the waiter's command line matches itself. Do not use
`pgrep -x codex` either, because the long-lived Codex process inside the
ChatGPT application matches it. Capture the child review PID and wait on that
PID (`while kill -0 <pid> 2>/dev/null; do sleep n; done`), or use a pattern
that cannot match the waiter.

When gates are run inside a review sandbox, the real-tmux host test cannot
create its private Unix socket. A failure shaped like `error: test failed …
--test host_tmux` in that reviewer transcript is an environmental limitation,
not a failure of the branch. Record it as such rather than calling it a
finding; run the normal gates in their proper branch environment for the
actual gate result.
