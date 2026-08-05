//! Stub-based tests for the user-space glue scripts: `scripts/dispatch` and
//! `scripts/ensure-executor`.
//!
//! Each test runs the real script in a scratch git repository with stub
//! `alder` and `alder-ext-runner` binaries that record their argv to a log,
//! so the claims under test are ordering claims — records before launch,
//! refresh before anything, kill before restart — plus the crash-re-run
//! adoption paths, never the stubs' own arithmetic.

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
/// the scripts perform from files under `$STUB_STATE`.
const STUB_ALDER: &str = r#"#!/usr/bin/env bash
set -eu
echo "alder $*" >> "$STUB_LOG"
case "$1" in
refresh) exit 0 ;;
status)
  seq=$(cat "$STUB_STATE/rotate-seq" 2>/dev/null || echo null)
  echo "{\"loop\":{\"paused\":false,\"rotate_requested_seq\":$seq}}" ;;
work)
  # work start <id> --tier <tier>
  if [ -f "$STUB_STATE/attempt-started" ]; then
    echo "error [work_not_ready]: work \`$3\` is not ready" >&2
    exit 1
  fi
  touch "$STUB_STATE/attempt-started"
  echo "$3-attempt-1" ;;
show)
  case "$2" in
  *-attempt-*)
    if [ -f "$STUB_STATE/bound-handle" ]; then
      echo "{\"current\":{\"handle\":\"$(cat "$STUB_STATE/bound-handle")\"}}"
    else
      echo '{"current":{"handle":null}}'
    fi ;;
  *)
    if [ "${3-}" = "--json" ]; then
      if [ -f "$STUB_STATE/attempt-started" ]; then
        echo "{\"current\":{\"id\":\"$2\"},\"history\":[\"$2-attempt-1\"]}"
      else
        echo "{\"current\":{\"id\":\"$2\"},\"history\":[]}"
      fi
    else
      echo "the item as alder shows it: THE SPEC TEXT"
    fi ;;
  esac ;;
attempt)
  # attempt edit <id> --handle <handle>
  echo "$5" > "$STUB_STATE/bound-handle" ;;
*)
  echo "stub alder: unexpected $*" >&2
  exit 64 ;;
esac
"#;

/// The stub `alder-ext-runner`: `start` refuses with the real refusal shape
/// while `$STUB_STATE/live` exists, `status` answers from
/// `$STUB_STATE/status-word`, `send` refuses while `$STUB_STATE/refuse-send`
/// exists, and the started prompt and sent message are captured for
/// content assertions.
const STUB_RUNNER: &str = r#"#!/usr/bin/env bash
set -eu
echo "alder-ext-runner $*" >> "$STUB_LOG"
case "$1" in
start)
  if [ -f "$STUB_STATE/live" ]; then
    echo "alder-ext-runner: handle \`$STUB_HANDLE\` is already running; kill it before starting another execution on \`$STUB_HANDLE\`" >&2
    exit 1
  fi
  prompt=""
  while [ $# -gt 0 ]; do
    [ "$1" = --prompt-file ] && prompt=$2
    shift
  done
  cp "$prompt" "$STUB_STATE/prompt"
  touch "$STUB_STATE/live"
  echo "$STUB_HANDLE" ;;
status)
  cat "$STUB_STATE/status-word" 2>/dev/null || echo running
  echo "tier terra, worktree $STUB_STATE/worktree" ;;
send)
  if [ -f "$STUB_STATE/refuse-send" ]; then
    echo "alder-ext-runner: refusing to send" >&2
    exit 1
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

    fn state_file(&self, name: &str) -> String {
        fs::read_to_string(self.state.join(name)).unwrap()
    }

    fn touch_state(&self, name: &str) {
        fs::write(self.state.join(name), "").unwrap();
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

/// The records-first property: the attempt is recorded (`alder work start`)
/// before the execution exists (`alder-ext-runner start`), and the handle is
/// bound after it, with the worker's worktree seeded in between.
#[test]
fn dispatch_records_the_attempt_before_launching_and_binds_the_handle() {
    let sandbox = Sandbox::new(HANDLE);
    sandbox.succeed("dispatch", &[WORK], &[]);

    let recorded = sandbox.call_index(&format!("alder work start {WORK} --tier terra"));
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

    // The worker's worktree can reach the log: the manifest was seeded.
    let seeded = sandbox.state.join("worktree/.alder/config.json");
    assert!(seeded.is_file(), "worktree manifest was not seeded");
}

/// A re-run after a crash between the record and the bind adopts everything
/// already in place: the recorded attempt, the live execution the runner
/// refuses to double, and it never kills anything.
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
    fs::write(sandbox.state.join("bound-handle"), format!("{HANDLE}\n")).unwrap();
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

/// With no executor on record, a wake refreshes first, then starts a fresh
/// executor whose launch prompt carries the brief pointer and the triggers,
/// and notes the new handle.
#[test]
fn ensure_executor_refreshes_first_and_starts_when_nothing_is_alive() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.succeed("ensure-executor", &[], &[("ALDERD_TRIGGERS", "log,due")]);

    let calls = sandbox.calls();
    assert_eq!(calls[0], "alder refresh", "refresh always first: {calls:?}");
    assert!(sandbox.call_index("alder-ext-runner start") > 0);
    assert!(
        !calls.iter().any(|line| line.contains("runner send")),
        "a fresh start carries the wake in its prompt: {calls:?}"
    );

    let prompt = sandbox.state_file("prompt");
    for needed in ["PASS.md", "log,due", "merge --ff-only main"] {
        assert!(
            prompt.contains(needed),
            "prompt lacks `{needed}`:\n{prompt}"
        );
    }
    let notes = sandbox.executor_notes();
    assert_eq!(notes[0], EXECUTOR_HANDLE);
    // The executor's worktree was seeded with the log manifest.
    assert!(sandbox.state.join("worktree/.alder/config.json").is_file());
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

/// A durable rotation request (`loop rotate`) outstanding past the noted
/// sequence rotates even a young executor, and the fresh notes consume it.
#[test]
fn ensure_executor_honors_a_durable_rotation_request() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.write_executor_notes(Sandbox::now(), 7);
    fs::write(sandbox.state.join("rotate-seq"), "42\n").unwrap();
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

/// A dead executor is simply replaced: no kill, one fresh start.
#[test]
fn ensure_executor_starts_fresh_when_the_executor_is_dead() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.write_executor_notes(Sandbox::now(), 0);
    fs::write(sandbox.state.join("status-word"), "dead\n").unwrap();
    sandbox.succeed("ensure-executor", &[], &[]);

    let calls = sandbox.calls();
    assert_eq!(calls[0], "alder refresh", "refresh always first: {calls:?}");
    assert!(
        !calls.iter().any(|line| line.contains("kill")),
        "nothing to kill when dead: {calls:?}"
    );
    assert!(sandbox.call_index("alder-ext-runner start") > 0);
}

/// A refused send — an exited interactive engine, a torn pane — rotates the
/// session rather than wedging the wake: kill, then a fresh start.
#[test]
fn ensure_executor_rotates_when_the_send_is_refused() {
    let sandbox = Sandbox::new(EXECUTOR_HANDLE);
    sandbox.write_executor_notes(Sandbox::now(), 0);
    sandbox.touch_state("refuse-send");
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
