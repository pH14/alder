//! Stub-based tests for the user-space glue scripts: `scripts/dispatch` and
//! `scripts/ensure-executor`.
//!
//! Each test runs the real script in a scratch git repository with stub
//! `alder` and `alder-ext-runner` binaries that record their argv to a log,
//! so the claims under test are ordering claims — records before launch,
//! refresh before anything, kill before restart — plus the crash-re-run
//! adoption paths, never the stubs' own arithmetic.
//!
//! The stubs answer with the REAL shapes of both programs, pinned elsewhere
//! so drift is caught: `alder` prints its `alder.error.v0` envelope (a
//! machine `code` plus context) on stdout under `--json` and its
//! `alder.work.start.v0` / `alder.show.v0` documents on success
//! (`src/main.rs`, `tests/cli.rs`); the runner speaks its exit-code contract
//! — start: 0 with `<handle>` then `tier <served>` on stdout, 3 already
//! running with `handle <h>` on stdout, 4 lock held, 5 unproven; send: 0
//! delivered, 4 lock held (already served), 5 cannot receive (rotate) — as
//! pinned in `crates/alder-ext-runner/tests/contract.rs` and
//! `crates/alder-ext-runner/tests/send_stub.rs`.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

use tempfile::TempDir;

const WORK: &str = "al-t1";
const HANDLE: &str = "alder-ext-work-al-t1";
const EXECUTOR_HANDLE: &str = "alder-ext-executor";

/// The stub `alder`: records argv, then answers the few reads and mutations
/// the scripts perform from files under `$STUB_STATE`, in alder's real
/// output shapes.
const STUB_ALDER: &str = r#"#!/usr/bin/env bash
set -eu
echo "alder $*" >> "$STUB_LOG"
case "$1" in
refresh) exit 0 ;;
status)
  seq=$(cat "$STUB_STATE/rotate-seq" 2>/dev/null || echo null)
  echo "{\"loop\":{\"paused\":false,\"rotate_requested_seq\":$seq}}" ;;
work)
  # work start <id> --tier <tier> --json. Refusals are the real coded
  # envelope, printed on stdout because --json is in force.
  if [ -f "$STUB_STATE/refuse-start-code" ]; then
    code=$(cat "$STUB_STATE/refuse-start-code")
    echo "{\"schema\":\"alder.error.v0\",\"ok\":false,\"code\":\"$code\",\"message\":\"work \`$3\` refused\",\"context\":{\"work_id\":\"$3\"}}"
    exit 1
  fi
  if [ -f "$STUB_STATE/attempt-started" ]; then
    echo "{\"schema\":\"alder.error.v0\",\"ok\":false,\"code\":\"active_attempt\",\"message\":\"work \`$3\` already has an active attempt\",\"context\":{\"work_id\":\"$3\",\"active_attempt_id\":\"$3-attempt-1\"}}"
    exit 1
  fi
  touch "$STUB_STATE/attempt-started"
  echo "{\"work_id\":\"$3\",\"attempt_id\":\"$3-attempt-1\",\"tier\":\"$5\",\"schema\":\"alder.work.start.v0\",\"head\":7,\"revision\":\"deadbeef\",\"event_id\":\"al-evt-1\"}" ;;
show)
  case "$2" in
  *-attempt-*)
    state=$(cat "$STUB_STATE/attempt-state" 2>/dev/null || echo active)
    if [ -f "$STUB_STATE/bound-handle" ]; then
      handle="\"$(cat "$STUB_STATE/bound-handle")\""
    else
      handle=null
    fi
    echo "{\"schema\":\"alder.show.v0\",\"head\":7,\"id\":\"$2\",\"kind\":\"attempt\",\"current\":{\"id\":\"$2\",\"work_id\":\"${2%%-attempt-*}\",\"state\":\"$state\",\"outcome\":null,\"tier\":\"terra\",\"handle\":$handle,\"metadata\":{},\"note\":null},\"history\":[]}" ;;
  *)
    echo "the item as alder shows it: THE SPEC TEXT" ;;
  esac ;;
attempt)
  # attempt edit <id> --handle <handle> [--tier <tier>]
  shift 2
  shift
  while [ $# -gt 0 ]; do
    case "$1" in
    --handle) echo "$2" > "$STUB_STATE/bound-handle"; shift 2 ;;
    --tier) echo "$2" > "$STUB_STATE/bound-tier"; shift 2 ;;
    *) shift ;;
    esac
  done ;;
*)
  echo "stub alder: unexpected $*" >&2
  exit 64 ;;
esac
"#;

/// The stub `alder-ext-runner`: speaks the runner's real exit-code contract.
/// `start` answers 4 while `$STUB_STATE/start-lock-held` exists, 5 while
/// `unproven` exists, 3 (stdout `handle <h>`) while `live` exists, and
/// otherwise performs each `--seed` copy into `$STUB_STATE/worktree`,
/// captures the prompt, and prints the handle then `tier <served>`. `send`
/// exits with `$STUB_STATE/send-exit`'s code when present; `status` answers
/// from `status-word`.
const STUB_RUNNER: &str = r#"#!/usr/bin/env bash
set -eu
echo "alder-ext-runner $*" >> "$STUB_LOG"
case "$1" in
start)
  if [ -f "$STUB_STATE/start-lock-held" ]; then
    echo "alder-ext-runner: another operation on \`$STUB_HANDLE\` holds its lock; refusing to race it" >&2
    exit 4
  fi
  if [ -f "$STUB_STATE/unproven" ]; then
    echo "alder-ext-runner: handle \`$STUB_HANDLE\` exists but cannot prove its engine exited; kill it before starting another execution" >&2
    exit 5
  fi
  if [ -f "$STUB_STATE/live" ]; then
    echo "handle $STUB_HANDLE"
    echo "alder-ext-runner: handle \`$STUB_HANDLE\` is already running; kill it before starting another execution" >&2
    exit 3
  fi
  prompt=""
  shift
  while [ $# -gt 0 ]; do
    case "$1" in
    --prompt-file) prompt=$2; shift 2 ;;
    --seed)
      src=${2%:*}
      rel=${2##*:}
      mkdir -p "$STUB_STATE/worktree/$(dirname "$rel")"
      cp "$src" "$STUB_STATE/worktree/$rel"
      shift 2 ;;
    *) shift ;;
    esac
  done
  cp "$prompt" "$STUB_STATE/prompt"
  touch "$STUB_STATE/live"
  echo "$STUB_HANDLE"
  echo "tier $(cat "$STUB_STATE/served-tier" 2>/dev/null || echo terra)" ;;
status)
  cat "$STUB_STATE/status-word" 2>/dev/null || echo running
  echo "tier $(cat "$STUB_STATE/served-tier" 2>/dev/null || echo terra), worktree $STUB_STATE/worktree" ;;
send)
  if [ -f "$STUB_STATE/send-exit" ]; then
    code=$(cat "$STUB_STATE/send-exit")
    echo "alder-ext-runner: send refused (stub exit $code)" >&2
    exit "$code"
  fi
  file=""
  while [ $# -gt 0 ]; do
    [ "$1" = --file ] && file=$2
    shift
  done
  cp "$file" "$STUB_STATE/sent" ;;
kill) exit 0 ;;
*)
  echo "stub runner: unexpected $*" >&2
  exit 64 ;;
esac
"#;

struct Sandbox {
    _temporary: TempDir,
    root: PathBuf,
    state: PathBuf,
    log: PathBuf,
    handle: String,
}

impl Sandbox {
    fn new(handle: &str) -> Self {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("project");
        let state = temporary.path().join("state");
        let log = temporary.path().join("calls.log");
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::create_dir_all(root.join(".alder")).unwrap();
        fs::create_dir_all(state.join("worktree")).unwrap();
        fs::write(log.as_path(), "").unwrap();
        fs::write(root.join(".alder/config.json"), "{\"stub\":true}\n").unwrap();

        // The scripts resolve the project root through git's common
        // directory, so the sandbox root must be a real repository.
        let init = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(init.status.success());

        let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts");
        for name in ["dispatch", "ensure-executor"] {
            fs::copy(scripts.join(name), root.join("scripts").join(name)).unwrap();
        }
        write_executable(&temporary.path().join("alder"), STUB_ALDER);
        write_executable(&temporary.path().join("alder-ext-runner"), STUB_RUNNER);

        Self {
            _temporary: temporary,
            root,
            state,
            log,
            handle: handle.to_owned(),
        }
    }

    fn run(&self, script: &str, arguments: &[&str], environment: &[(&str, &str)]) -> Output {
        let parent = self.log.parent().unwrap();
        let mut command = Command::new("bash");
        command
            .arg(self.root.join("scripts").join(script))
            .args(arguments)
            .current_dir(&self.root)
            .env("ALDER_BIN", parent.join("alder"))
            .env("ALDER_EXT_RUNNER_BIN", parent.join("alder-ext-runner"))
            .env("STUB_LOG", &self.log)
            .env("STUB_STATE", &self.state)
            .env("STUB_HANDLE", &self.handle);
        for (key, value) in environment {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn succeed(&self, script: &str, arguments: &[&str], environment: &[(&str, &str)]) {
        let output = self.run(script, arguments, environment);
        assert!(
            output.status.success(),
            "{script} failed\nstdout: {}\nstderr: {}\nlog: {:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            self.calls()
        );
    }

    fn fail(&self, script: &str, arguments: &[&str], environment: &[(&str, &str)]) -> Output {
        let output = self.run(script, arguments, environment);
        assert!(
            !output.status.success(),
            "{script} unexpectedly succeeded\nstdout: {}\nlog: {:?}",
            String::from_utf8_lossy(&output.stdout),
            self.calls()
        );
        output
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn call_index(&self, prefix: &str) -> usize {
        let calls = self.calls();
        calls
            .iter()
            .position(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("no call starts with `{prefix}` in {calls:?}"))
    }

    fn never_called(&self, needle: &str) {
        let calls = self.calls();
        assert!(
            !calls.iter().any(|line| line.contains(needle)),
            "`{needle}` was called: {calls:?}"
        );
    }

    fn state_file(&self, name: &str) -> String {
        fs::read_to_string(self.state.join(name)).unwrap()
    }

    fn touch_state(&self, name: &str) {
        fs::write(self.state.join(name), "").unwrap();
    }

    fn write_state(&self, name: &str, contents: &str) {
        fs::write(self.state.join(name), contents).unwrap();
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn write_executor_notes(&self, epoch: u64, rotate: u64) {
        fs::write(
            self.root.join(".alder/executor-handle"),
            format!("{EXECUTOR_HANDLE}\n{epoch}\n{rotate}\n"),
        )
        .unwrap();
    }

    fn executor_notes(&self) -> Vec<String> {
        fs::read_to_string(self.root.join(".alder/executor-handle"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The records-first property: the attempt is recorded (`alder work start
/// --json`) before the execution exists (`alder-ext-runner start`), and the
/// handle is bound after it. Seeding is the runner's (`--seed`), before the
/// engine — dispatch performs no post-launch copy of its own.
#[test]
fn dispatch_records_the_attempt_before_launching_and_binds_the_handle() {
    let sandbox = Sandbox::new(HANDLE);
    sandbox.succeed("dispatch", &[WORK], &[]);

    let recorded = sandbox.call_index(&format!("alder work start {WORK} --tier terra --json"));
    let launched = sandbox.call_index("alder-ext-runner start");
    let bound = sandbox.call_index(&format!(
        "alder attempt edit {WORK}-attempt-1 --handle {HANDLE}"
    ));
    assert!(recorded < launched, "records-first: {:?}", sandbox.calls());
    assert!(
        launched < bound,
        "bind follows launch: {:?}",
        sandbox.calls()
    );
    assert_eq!(sandbox.state_file("bound-handle").trim(), HANDLE);

    // Seeding rides the start itself, so the worker cannot boot before its
    // log configuration exists.
    let start_call = sandbox.calls()[launched].clone();
    assert!(
        start_call.contains("--seed")
            && start_call.contains("/.alder/config.json:.alder/config.json"),
        "the start does not seed the log manifest: {start_call}"
    );
    let seeded = sandbox.state.join("worktree/.alder/config.json");
    assert!(seeded.is_file(), "worktree manifest was not seeded");

    // The composed brief names the item, the attempt, the worker manual, and
    // carries the item's recorded text verbatim.
    let prompt = sandbox.state_file("prompt");
    for needed in [
        WORK,
        &format!("{WORK}-attempt-1"),
        ".agent/skills/worker/SKILL.md",
        "THE SPEC TEXT",
    ] {
        assert!(
            prompt.contains(needed),
            "prompt lacks `{needed}`:\n{prompt}"
        );
    }
}

/// A re-run after a crash between the record and the bind adopts everything
/// already in place: the recorded attempt (named by the real `active_attempt`
/// error envelope's `active_attempt_id`, never grepped out of history), the
/// live execution the runner refuses with exit 3 (adopted from the `handle
/// <h>` stdout line), and it never kills anything.
#[test]
fn dispatch_rerun_adopts_the_recorded_attempt_and_live_execution() {
    let sandbox = Sandbox::new(HANDLE);
    sandbox.touch_state("attempt-started");
    sandbox.touch_state("live");
    sandbox.succeed("dispatch", &[WORK], &[]);

    let calls = sandbox.calls();
    assert!(
        !calls.iter().any(|line| line.contains("kill")),
        "adoption must not kill: {calls:?}"
    );
    // Adoption verified the recorded attempt is really adoptable.
    assert!(
        calls
            .iter()
            .any(|line| line.starts_with(&format!("alder show {WORK}-attempt-1 --json"))),
        "the adopted attempt was never verified: {calls:?}"
    );
    let binds: Vec<_> = calls
        .iter()
        .filter(|line| line.starts_with("alder attempt edit"))
        .collect();
    assert_eq!(
        binds,
        vec![&format!(
            "alder attempt edit {WORK}-attempt-1 --handle {HANDLE}"
        )],
        "exactly one bind of the adopted handle"
    );
    assert_eq!(sandbox.state_file("bound-handle").trim(), HANDLE);
}

/// A re-run after a fully successful dispatch changes nothing: the binding
/// already stands and is only verified.
#[test]
fn dispatch_rerun_after_success_reports_the_binding_and_changes_nothing() {
    let sandbox = Sandbox::new(HANDLE);
    sandbox.touch_state("attempt-started");
    sandbox.touch_state("live");
    sandbox.write_state("bound-handle", &format!("{HANDLE}\n"));
    sandbox.succeed("dispatch", &[WORK], &[]);

    let calls = sandbox.calls();
    assert!(
        !calls
            .iter()
            .any(|line| line.starts_with("alder attempt edit")),
        "a settled dispatch must not re-bind: {calls:?}"
    );
    assert!(
        !calls.iter().any(|line| line.contains("kill")),
        "a settled dispatch must not kill: {calls:?}"
    );
}

/// Any `work start` refusal other than `active_attempt` — not ready, not
/// found, a store failure — aborts loudly and launches nothing: dispatch has
/// no converging answer for those codes, and a worker launched against an
/// unrecorded attempt is a fiction.
#[test]
fn dispatch_aborts_on_any_other_alder_refusal_and_never_launches() {
    for code in ["work_not_ready", "not_found", "store_unavailable"] {
        let sandbox = Sandbox::new(HANDLE);
        sandbox.write_state("refuse-start-code", code);
        let output = sandbox.fail("dispatch", &[WORK], &[]);
        assert!(
            stderr(&output).contains(code),
            "the abort does not name the code: {}",
            stderr(&output)
        );
        sandbox.never_called("alder-ext-runner start");
        sandbox.never_called("alder attempt edit");
    }
}

/// Adoption is verified, not assumed: a recorded attempt that has ended, or
/// that is bound to some other execution's handle, refuses before anything
/// is launched.
#[test]
fn dispatch_refuses_to_adopt_an_ended_or_elsewhere_bound_attempt() {
    // Ended attempt: nothing to adopt.
    let sandbox = Sandbox::new(HANDLE);
    sandbox.touch_state("attempt-started");
    sandbox.write_state("attempt-state", "ended");
    let output = sandbox.fail("dispatch", &[WORK], &[]);
    assert!(
        stderr(&output).contains("not active"),
        "{}",
        stderr(&output)
    );
    sandbox.never_called("alder-ext-runner start");

    // Bound to a different handle: this branch's execution is not its.
    let sandbox = Sandbox::new(HANDLE);
    sandbox.touch_state("attempt-started");
    sandbox.write_state("bound-handle", "alder-ext-somewhere-else\n");
    let output = sandbox.fail("dispatch", &[WORK], &[]);
    assert!(
        stderr(&output).contains("alder-ext-somewhere-else"),
        "{}",
        stderr(&output)
    );
    sandbox.never_called("alder-ext-runner start");
}

/// The runner's exit 4 (a racing dispatch holds the start lock) and exit 5
/// (a session that cannot prove its engine exited) both abort: 4 converges
/// by rerunning later, 5 needs a human, and neither binds anything.
#[test]
fn dispatch_aborts_on_runner_lock_and_unproven_refusals() {
    let sandbox = Sandbox::new(HANDLE);
    sandbox.touch_state("start-lock-held");
    let output = sandbox.fail("dispatch", &[WORK], &[]);
    assert!(
        stderr(&output).contains("rerun later"),
        "{}",
        stderr(&output)
    );
    sandbox.never_called("alder attempt edit");
    sandbox.never_called("kill");

    let sandbox = Sandbox::new(HANDLE);
    sandbox.touch_state("unproven");
    let output = sandbox.fail("dispatch", &[WORK], &[]);
    assert!(
        stderr(&output).contains("cannot prove its engine exited"),
        "the runner's own message must reach the operator: {}",
        stderr(&output)
    );
    sandbox.never_called("alder attempt edit");
    sandbox.never_called("kill");
}

/// Rate-limit substitution can serve a different rung than requested; the
/// runner reports it on the `tier <served>` stdout line and the bind records
/// it, so cross-review vendor selection reads what actually ran.
#[test]
fn dispatch_binds_the_served_tier_when_substitution_changed_it() {
    let sandbox = Sandbox::new(HANDLE);
    sandbox.write_state("served-tier", "opus\n");
    sandbox.succeed("dispatch", &[WORK], &[]);

    let calls = sandbox.calls();
    assert!(
        calls.iter().any(|line| line
            == &format!("alder attempt edit {WORK}-attempt-1 --handle {HANDLE} --tier opus")),
        "the bind does not record the served tier: {calls:?}"
    );
    assert_eq!(sandbox.state_file("bound-tier").trim(), "opus");
}

/// A missing binary is a loud, named failure — never a launch against half a
/// toolchain.
#[test]
fn dispatch_fails_loudly_when_a_binary_is_missing() {
    let sandbox = Sandbox::new(HANDLE);
    let parent = sandbox.log.parent().unwrap().to_path_buf();
    let output = Command::new("bash")
        .arg(sandbox.root.join("scripts").join("dispatch"))
        .arg(WORK)
        .current_dir(&sandbox.root)
        .env("ALDER_BIN", "")
        .env("ALDER_EXT_RUNNER_BIN", parent.join("alder-ext-runner"))
        .env("STUB_LOG", &sandbox.log)
        .env("STUB_STATE", &sandbox.state)
        .env("STUB_HANDLE", HANDLE)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("alder not found"),
        "{}",
        stderr(&output)
    );
    sandbox.never_called("alder-ext-runner start");
}

/// With no executor on record, a wake refreshes first, then starts a fresh
/// executor whose launch prompt carries the brief pointer and the triggers,
/// seeds through the runner's own --seed, and notes the new handle.
#[test]
fn ensure_executor_refreshes_first_and_starts_when_nothing_is_alive() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.succeed("ensure-executor", &[], &[("ALDERD_TRIGGERS", "log,due")]);

    let calls = sandbox.calls();
    assert_eq!(calls[0], "alder refresh", "refresh always first: {calls:?}");
    let launched = sandbox.call_index("alder-ext-runner start");
    assert!(launched > 0);
    assert!(
        !calls.iter().any(|line| line.contains("runner send")),
        "a fresh start carries the wake in its prompt: {calls:?}"
    );
    // Seeding rides the start; there is no post-launch copy to race the
    // engine against.
    assert!(
        calls[launched].contains("--seed")
            && calls[launched].contains("/.alder/config.json:.alder/config.json"),
        "the start does not seed the log manifest: {}",
        calls[launched]
    );
    assert!(sandbox.state.join("worktree/.alder/config.json").is_file());

    let prompt = sandbox.state_file("prompt");
    for needed in ["PASS.md", "log,due", "merge --ff-only main"] {
        assert!(
            prompt.contains(needed),
            "prompt lacks `{needed}`:\n{prompt}"
        );
    }
    let notes = sandbox.executor_notes();
    assert_eq!(notes[0], EXECUTOR_HANDLE);
}

/// A live executor younger than the rotation age gets the wake as one sent
/// line naming the triggers — no kill, no second start.
#[test]
fn ensure_executor_sends_the_triggers_to_a_young_live_executor() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.write_executor_notes(Sandbox::now(), 0);
    sandbox.succeed("ensure-executor", &[], &[("ALDERD_TRIGGERS", "manual")]);

    let calls = sandbox.calls();
    assert_eq!(calls[0], "alder refresh", "refresh always first: {calls:?}");
    assert!(
        !calls.iter().any(|line| line.contains("runner start")),
        "no restart under a young executor: {calls:?}"
    );
    assert!(
        !calls.iter().any(|line| line.contains("kill")),
        "no kill under a young executor: {calls:?}"
    );
    let sent = sandbox.state_file("sent");
    assert!(
        sent.contains("manual"),
        "sent line lacks the triggers: {sent}"
    );
    assert!(
        sent.contains("PASS.md"),
        "sent line lacks the brief: {sent}"
    );
}

/// An executor older than the rotation age is killed and replaced.
#[test]
fn ensure_executor_rotates_an_old_executor() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.write_executor_notes(1, 0);
    sandbox.succeed("ensure-executor", &[], &[]);

    let calls = sandbox.calls();
    assert_eq!(calls[0], "alder refresh", "refresh always first: {calls:?}");
    let killed = sandbox.call_index(&format!("alder-ext-runner kill {EXECUTOR_HANDLE}"));
    let restarted = sandbox.call_index("alder-ext-runner start");
    assert!(killed < restarted, "kill before restart: {calls:?}");
}

/// A note claiming a start in the future — the clock rolled back under it —
/// reads as ancient, which rotates: the safe direction.
#[test]
fn ensure_executor_rotates_on_a_negative_age() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.write_executor_notes(Sandbox::now() + 100_000, 0);
    sandbox.succeed("ensure-executor", &[], &[]);

    let calls = sandbox.calls();
    let killed = sandbox.call_index(&format!("alder-ext-runner kill {EXECUTOR_HANDLE}"));
    let restarted = sandbox.call_index("alder-ext-runner start");
    assert!(killed < restarted, "kill before restart: {calls:?}");
}

/// Corrupt or torn notes read as ancient and rotate rather than wedge: a
/// wake killed mid-write must not leave the loop stuck on unreadable state.
#[test]
fn ensure_executor_rotates_on_corrupt_or_torn_notes() {
    for garbage in [
        format!("{EXECUTOR_HANDLE}\nnot-a-number\nalso-bad\n"),
        format!("{EXECUTOR_HANDLE}\n"),
    ] {
        let sandbox = Sandbox::new(EXECUTOR_HANDLE);
        fs::write(sandbox.root.join(".alder/executor-handle"), &garbage).unwrap();
        sandbox.succeed("ensure-executor", &[], &[]);

        let calls = sandbox.calls();
        let killed = sandbox.call_index(&format!("alder-ext-runner kill {EXECUTOR_HANDLE}"));
        let restarted = sandbox.call_index("alder-ext-runner start");
        assert!(
            killed < restarted,
            "corrupt notes must rotate ({garbage:?}): {calls:?}"
        );
    }
}

/// A durable rotation request (`loop rotate`) outstanding past the noted
/// sequence rotates even a young executor, and the fresh notes consume it —
/// the kill+start path is the ONE place the honored seq advances.
#[test]
fn ensure_executor_honors_a_durable_rotation_request() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.write_executor_notes(Sandbox::now(), 7);
    sandbox.write_state("rotate-seq", "42\n");
    sandbox.succeed("ensure-executor", &[], &[]);

    let calls = sandbox.calls();
    let killed = sandbox.call_index(&format!("alder-ext-runner kill {EXECUTOR_HANDLE}"));
    let restarted = sandbox.call_index("alder-ext-runner start");
    assert!(killed < restarted, "kill before restart: {calls:?}");
    assert_eq!(
        sandbox.executor_notes()[2],
        "42",
        "the fresh notes consume the rotation request"
    );
}

/// A fresh start that killed nothing — the previous executor is dead — does
/// NOT consume an outstanding rotation request: only a kill+start advances
/// the honored seq, so the request stays outstanding for the next wake.
#[test]
fn ensure_executor_starts_fresh_when_dead_without_consuming_a_rotation_request() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.write_executor_notes(Sandbox::now(), 7);
    sandbox.write_state("rotate-seq", "42\n");
    sandbox.write_state("status-word", "dead\n");
    sandbox.succeed("ensure-executor", &[], &[]);

    let calls = sandbox.calls();
    assert!(
        !calls.iter().any(|line| line.contains("kill")),
        "nothing to kill when dead: {calls:?}"
    );
    assert!(sandbox.call_index("alder-ext-runner start") > 0);
    assert_eq!(
        sandbox.executor_notes()[2],
        "7",
        "a start that killed nothing must not consume the rotation request"
    );
}

/// The runner's exit 3 on start names a live executor these notes had no
/// record of: the wake adopts it, delivers to it, and — because nothing
/// rotated — leaves the honored rotation seq exactly where it was.
#[test]
fn ensure_executor_adopts_a_live_executor_without_consuming_a_rotation_request() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.touch_state("live");
    sandbox.write_state("rotate-seq", "42\n");
    sandbox.succeed("ensure-executor", &[], &[("ALDERD_TRIGGERS", "log")]);

    let calls = sandbox.calls();
    assert!(
        !calls.iter().any(|line| line.contains("kill")),
        "adoption must not kill: {calls:?}"
    );
    let notes = sandbox.executor_notes();
    assert_eq!(notes[0], EXECUTOR_HANDLE);
    assert_eq!(
        notes[2], "0",
        "an adopt must never consume a rotation request"
    );
    let sent = sandbox.state_file("sent");
    assert!(sent.contains("log"), "the wake was not delivered: {sent}");
}

/// Send exit 4 means another wake's delivery holds the lock: this wake logs
/// it and stands down with exit 0 — it NEVER kills the executor over a
/// message that is already being served.
#[test]
fn ensure_executor_stands_down_when_another_wake_delivered() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.write_executor_notes(Sandbox::now(), 0);
    sandbox.write_state("send-exit", "4\n");
    let output = sandbox.run("ensure-executor", &[], &[]);
    assert!(
        output.status.success(),
        "a lock-held send must exit 0: {}",
        stderr(&output)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("another wake delivered"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    sandbox.never_called("kill");
    let calls = sandbox.calls();
    assert!(
        !calls.iter().any(|line| line.contains("runner start")),
        "a lock-held send must not restart: {calls:?}"
    );
}

/// Send exit 5 — the executor cannot receive the wake (an exited interactive
/// engine, a torn pane) — rotates the session: kill, then a fresh start.
#[test]
fn ensure_executor_rotates_when_the_send_is_refused() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.write_executor_notes(Sandbox::now(), 0);
    sandbox.write_state("send-exit", "5\n");
    sandbox.succeed("ensure-executor", &[], &[]);

    let calls = sandbox.calls();
    let sent = sandbox.call_index("alder-ext-runner send");
    let killed = sandbox.call_index(&format!("alder-ext-runner kill {EXECUTOR_HANDLE}"));
    let restarted = sandbox.call_index("alder-ext-runner start");
    assert!(
        sent < killed && killed < restarted,
        "send, kill, restart: {calls:?}"
    );
}

/// A send failure with no contract meaning (exit 1) fails the wake loudly:
/// alderd retries next poll, and nothing is killed on a guess.
#[test]
fn ensure_executor_fails_the_wake_on_an_unclassified_send_failure() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.write_executor_notes(Sandbox::now(), 0);
    sandbox.write_state("send-exit", "1\n");
    let output = sandbox.fail("ensure-executor", &[], &[]);
    assert!(
        stderr(&output).contains("failing this wake"),
        "{}",
        stderr(&output)
    );
    sandbox.never_called("kill");
    let calls = sandbox.calls();
    assert!(
        !calls.iter().any(|line| line.contains("runner start")),
        "an unclassified failure must not restart: {calls:?}"
    );
}

/// Start exit 4 means another wake is already starting the executor: stand
/// down cleanly rather than racing it.
#[test]
fn ensure_executor_stands_down_when_another_wake_is_starting() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.touch_state("start-lock-held");
    let output = sandbox.run("ensure-executor", &[], &[]);
    assert!(
        output.status.success(),
        "a lock-held start must exit 0: {}",
        stderr(&output)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("another wake is starting"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    sandbox.never_called("kill");
}
