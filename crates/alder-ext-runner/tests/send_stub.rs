//! `send` delivers literal file text without post-send confirmation.
//!
//! The runner's `send` is the old relay craft, so this holds it to the same
//! claims that script was held to: the file's bytes reach tmux as a buffer
//! (claude) or as an armored resume command (codex), never as shell syntax
//! or tmux argv; nothing reads the pane afterwards; and an exited
//! interactive engine is refused rather than typed at. The tmux on PATH is a
//! stub that records its argv, so what the runner does to a terminal is an
//! assertion here, not a claim.

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Stub {
    _temporary: TempDir,
    worktree: PathBuf,
    calls: PathBuf,
    finding: PathBuf,
    sentinel: PathBuf,
    path: std::ffi::OsString,
}

/// A stub tmux that records every call and answers `show-environment` from
/// the variables this test exports, plus a worktree carrying a resume script
/// and (optionally) a codex-session marker.
fn stub() -> Stub {
    let temporary = TempDir::new().unwrap();
    let worktree = temporary.path().join("worktree");
    let runner_dir = worktree.join(".alder-ext-runner");
    fs::create_dir_all(&runner_dir).unwrap();
    write_executable(&runner_dir.join("resume"), "#!/bin/sh\nexit 0\n");

    let calls = temporary.path().join("tmux-calls");
    let tools = temporary.path().join("tools");
    fs::create_dir_all(&tools).unwrap();
    write_executable(
        &tools.join("tmux"),
        &format!(
            r##"#!/bin/sh
printf '%s\n' "$*" >> "{calls}"
case "$1" in
  has-session) exit 0 ;;
  show-environment)
    case "$4" in
      ALDER_EXT_RUNNER_HANDLE) printf 'ALDER_EXT_RUNNER_HANDLE=%s\n' "$STUB_HANDLE" ;;
      ALDER_EXT_RUNNER_ENGINE) printf 'ALDER_EXT_RUNNER_ENGINE=%s\n' "$STUB_ENGINE" ;;
      ALDER_EXT_RUNNER_TIER) printf 'ALDER_EXT_RUNNER_TIER=%s\n' "$STUB_TIER" ;;
      ALDER_EXT_RUNNER_WORKTREE) printf 'ALDER_EXT_RUNNER_WORKTREE=%s\n' "$STUB_WORKTREE" ;;
    esac
    ;;
esac
"##,
            calls = calls.display()
        ),
    );
    let mut path = vec![tools];
    path.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    let path = env::join_paths(path).unwrap();

    let finding = temporary.path().join("finding.txt");
    let sentinel = temporary.path().join("must-not-run");
    fs::write(
        &finding,
        format!(
            "reviewer text: $(touch {}) and `backticks`\nsecond line\n",
            sentinel.display()
        ),
    )
    .unwrap();

    Stub {
        worktree,
        calls,
        finding,
        sentinel,
        path,
        _temporary: temporary,
    }
}

fn send(stub: &Stub, engine: &str, tier: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_alder-ext-runner"))
        .args([
            "send",
            "alder-ext-work-hm",
            "--file",
            stub.finding.to_str().unwrap(),
        ])
        .env("PATH", &stub.path)
        .env("STUB_HANDLE", "alder-ext-work-hm")
        .env("STUB_ENGINE", engine)
        .env("STUB_TIER", tier)
        .env("STUB_WORKTREE", &stub.worktree)
        .env_remove("ALDER_EXT_RUNNER_CONFIG")
        .output()
        .expect("the runner runs")
}

fn calls(stub: &Stub) -> String {
    fs::read_to_string(&stub.calls).unwrap_or_default()
}

#[test]
fn an_interactive_engine_receives_the_file_as_a_pasted_buffer() {
    let stub = stub();
    let output = send(&stub, "running", "opus");
    assert!(
        output.status.success(),
        "send failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("sent once to alder-ext-work-hm"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !stub.sentinel.exists(),
        "a reviewer's shell syntax was evaluated by send"
    );

    let calls = calls(&stub);
    assert!(
        calls.lines().any(|call| {
            call.starts_with("load-buffer -b alder-ext-send-")
                && call.ends_with(&format!("-- {}", stub.finding.display()))
        }),
        "send did not give tmux the source file: {calls}"
    );
    assert!(
        calls.lines().any(|call| {
            call.starts_with("paste-buffer -d -r -b alder-ext-send-")
                && call.ends_with("-t =alder-ext-work-hm:")
        }),
        "send did not use the exact session pane: {calls}"
    );
    assert!(
        calls.contains("send-keys -t =alder-ext-work-hm: Enter"),
        "{calls}"
    );
    assert!(
        !calls.contains("capture-pane"),
        "send read the pane: {calls}"
    );
    assert!(
        !calls.contains("display-message"),
        "send synchronously probed the pane after sending: {calls}"
    );
    assert!(
        !calls.contains("must-not-run"),
        "send put finding text in tmux argv: {calls}"
    );
}

#[test]
fn a_codex_engine_receives_an_armored_resume_never_raw_bytes() {
    let stub = stub();
    fs::write(
        stub.worktree.join(".alder-ext-runner/codex-session"),
        "019fb2ef-d507-7201-bc36-79d6d5b82336\n",
    )
    .unwrap();
    // A Codex execution receives the safe encoded resume command even while
    // its one-shot engine is still running; it must not receive raw ruling
    // bytes at either the engine or its eventual holding shell.
    let output = send(&stub, "running", "luna");
    assert!(
        output.status.success(),
        "codex send failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = calls(&stub);
    let set = calls
        .lines()
        .find(|call| call.starts_with("set-buffer -b alder-ext-send-"))
        .unwrap_or_else(|| panic!("codex send did not prepare a resume command: {calls}"));
    assert!(
        set.contains(".alder-ext-runner/resume 019fb2ef-d507-7201-bc36-79d6d5b82336"),
        "the resume command does not name the recorded session: {set}"
    );
    assert!(
        set.contains("base64 -d") && set.contains("base64 -D"),
        "the payload is not armored for both userlands: {set}"
    );
    assert!(
        !calls.contains("load-buffer"),
        "codex send pasted the ruling into a shell: {calls}"
    );
    assert!(
        !calls.contains("must-not-run"),
        "codex send put reviewer text in a shell command: {calls}"
    );
    assert!(
        calls
            .lines()
            .any(|call| call.starts_with("paste-buffer -d -r -b alder-ext-send-")),
        "{calls}"
    );
    assert!(
        calls.contains("send-keys -t =alder-ext-work-hm: Enter"),
        "{calls}"
    );
    assert!(!calls.contains("display-message"), "{calls}");
}

#[test]
fn an_exited_interactive_engine_is_refused_rather_than_typed_at() {
    let stub = stub();
    let output = send(&stub, "exited", "opus");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("exited interactive engine"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = calls(&stub);
    assert!(
        !calls.lines().any(|call| call.starts_with("set-buffer")
            || call.starts_with("load-buffer")
            || call.starts_with("paste-buffer")
            || call.starts_with("send-keys")),
        "an exited interactive engine received a delivery: {calls}"
    );
}

#[test]
fn a_codex_send_without_a_recorded_session_is_refused() {
    let stub = stub();
    // No codex-session marker was ever written: resuming would have to guess
    // from `--last`, which the runner refuses to do.
    let output = send(&stub, "exited", "luna");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no codex session recorded"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = calls(&stub);
    assert!(
        !calls.contains("paste-buffer"),
        "an unresumable execution received a delivery: {calls}"
    );

    // A marker that is not a session ID is refused the same way.
    fs::write(
        stub.worktree.join(".alder-ext-runner/codex-session"),
        "$(rm -rf /)\n",
    )
    .unwrap();
    let output = send(&stub, "exited", "luna");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not a session ID"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
