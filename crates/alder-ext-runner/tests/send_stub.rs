//! `send` delivers literal file text without post-send confirmation, and
//! `kill` verifies before it reports.
//!
//! The runner's `send` is the old relay craft, so this holds it to the same
//! claims that script was held to: the file's bytes reach tmux as a buffer
//! (claude) or as an armored resume command (codex), never as shell syntax
//! or tmux argv; nothing reads the pane afterwards except the one engine
//! re-check between paste and Enter; and an exited interactive engine is
//! refused rather than typed at. Delivery routes by the provider `start`
//! stamped into the session — never by the current tier table. The tmux on
//! PATH is a stub that records its argv, so what the runner does to a
//! terminal is an assertion here, not a claim. The refusal exit codes are
//! pinned here too, because scripts branch on them: 4 means another
//! operation holds the handle lock (treat the message as already served),
//! 5 means the execution cannot receive the delivery (the caller may
//! rotate); anything else mechanical is 1.

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;

const HANDLE: &str = "alder-ext-work-hm";

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Stub {
    _temporary: TempDir,
    worktree: PathBuf,
    /// The runner's machine-local state directory; per-handle files live in
    /// `state/<handle>/`.
    state: PathBuf,
    calls: PathBuf,
    finding: PathBuf,
    sentinel: PathBuf,
    /// While this file exists, the stub's `send-keys` (Enter) fails.
    enter_fails: PathBuf,
    /// While this file exists, a `paste-buffer` flips the engine marker to
    /// `exited`, modelling an engine that died between paste and re-check.
    flip_on_paste: PathBuf,
    /// The stub's stateful engine marker: written by the flip above; when it
    /// exists, `show-environment ENGINE` answers from it instead of the
    /// exported `STUB_ENGINE`.
    engine_state: PathBuf,
    /// The stub's stateful torn marker: `set-environment` writes it,
    /// `set-environment -u` removes it, `show-environment` answers from it.
    torn: PathBuf,
    /// Touched by `kill-session`; once present, `has-session` and
    /// `show-environment` answer as if the session is gone.
    killed: PathBuf,
    path: std::ffi::OsString,
}

/// A stub tmux that records every call and answers `show-environment` from
/// the variables this test exports, plus a per-handle state directory
/// carrying a resume script and (optionally) a codex-session marker. A
/// variable exported empty reads as absent, the way real tmux answers for an
/// unset session variable. The torn, killed, and flipped-engine markers are
/// stateful across invocations, so what one runner invocation does is
/// visible to the next.
fn stub() -> Stub {
    let temporary = TempDir::new().unwrap();
    let worktree = temporary.path().join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    let state = temporary.path().join("state");
    let handle_state = state.join(HANDLE);
    fs::create_dir_all(&handle_state).unwrap();
    write_executable(&handle_state.join("resume"), "#!/bin/sh\nexit 0\n");

    let calls = temporary.path().join("tmux-calls");
    let enter_fails = temporary.path().join("enter-fails");
    let flip_on_paste = temporary.path().join("flip-on-paste");
    let engine_state = temporary.path().join("engine-state");
    let torn = temporary.path().join("torn-marker");
    let killed = temporary.path().join("killed");
    let tools = temporary.path().join("tools");
    fs::create_dir_all(&tools).unwrap();
    write_executable(
        &tools.join("tmux"),
        &format!(
            r##"#!/bin/sh
printf '%s\n' "$*" >> "{calls}"
case "$1" in
  has-session)
    [ -e "{killed}" ] && exit 1
    [ -n "$STUB_ABSENT" ] && exit 1
    exit 0 ;;
  kill-session)
    [ -n "$STUB_KILL_FAILS" ] && exit 1
    : > "{killed}"
    exit 0 ;;
  paste-buffer)
    if [ -e "{flip}" ]; then printf 'exited' > "{engine_state}"; fi
    exit 0 ;;
  send-keys) [ -e "{enter_fails}" ] && exit 1 || exit 0 ;;
  set-environment)
    if [ "$2" = "-u" ]; then rm -f "{torn}"; else printf '%s' "$5" > "{torn}"; fi
    ;;
  show-environment)
    [ -e "{killed}" ] && exit 1
    case "$4" in
      ALDER_EXT_RUNNER_HANDLE)
        [ -n "$STUB_HANDLE" ] || exit 1
        printf 'ALDER_EXT_RUNNER_HANDLE=%s\n' "$STUB_HANDLE" ;;
      ALDER_EXT_RUNNER_ENGINE)
        if [ -e "{engine_state}" ]; then
          printf 'ALDER_EXT_RUNNER_ENGINE=%s\n' "$(cat "{engine_state}")"
          exit 0
        fi
        [ -n "$STUB_ENGINE" ] || exit 1
        printf 'ALDER_EXT_RUNNER_ENGINE=%s\n' "$STUB_ENGINE" ;;
      ALDER_EXT_RUNNER_PROVIDER)
        [ -n "$STUB_PROVIDER" ] || exit 1
        printf 'ALDER_EXT_RUNNER_PROVIDER=%s\n' "$STUB_PROVIDER" ;;
      ALDER_EXT_RUNNER_WORKTREE)
        [ -n "$STUB_WORKTREE" ] || exit 1
        printf 'ALDER_EXT_RUNNER_WORKTREE=%s\n' "$STUB_WORKTREE" ;;
      ALDER_EXT_RUNNER_TORN)
        [ -e "{torn}" ] || exit 1
        printf 'ALDER_EXT_RUNNER_TORN=%s\n' "$(cat "{torn}")" ;;
      *) exit 1 ;;
    esac
    ;;
esac
"##,
            calls = calls.display(),
            killed = killed.display(),
            flip = flip_on_paste.display(),
            engine_state = engine_state.display(),
            enter_fails = enter_fails.display(),
            torn = torn.display()
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
        state,
        calls,
        finding,
        sentinel,
        enter_fails,
        flip_on_paste,
        engine_state,
        torn,
        killed,
        path,
        _temporary: temporary,
    }
}

fn runner_with(stub: &Stub, engine: &str, provider: &str, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_alder-ext-runner"))
        .args(arguments)
        .env("PATH", &stub.path)
        .env("STUB_HANDLE", HANDLE)
        .env("STUB_ENGINE", engine)
        .env("STUB_PROVIDER", provider)
        .env("STUB_WORKTREE", &stub.worktree)
        .env("STUB_ABSENT", "")
        .env("STUB_KILL_FAILS", "")
        .env("ALDER_EXT_RUNNER_STATE_DIR", &stub.state)
        .env_remove("ALDER_EXT_RUNNER_CONFIG")
        .output()
        .expect("the runner runs")
}

fn send_with(stub: &Stub, engine: &str, provider: &str, extra: &[&str]) -> Output {
    let mut arguments = vec!["send", HANDLE, "--file", stub.finding.to_str().unwrap()];
    arguments.extend_from_slice(extra);
    runner_with(stub, engine, provider, &arguments)
}

fn send(stub: &Stub, engine: &str, provider: &str) -> Output {
    send_with(stub, engine, provider, &[])
}

fn calls(stub: &Stub) -> String {
    fs::read_to_string(&stub.calls).unwrap_or_default()
}

#[test]
fn an_interactive_engine_receives_the_file_as_a_pasted_buffer() {
    let stub = stub();
    let output = send(&stub, "running", "claude");
    assert!(
        output.status.success(),
        "send failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("sent once to {HANDLE}")),
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
                && call.ends_with(&format!("-t ={HANDLE}:"))
        }),
        "send did not use the exact session pane: {calls}"
    );
    assert!(
        calls.contains(&format!("send-keys -t ={HANDLE}: Enter")),
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

    // The engine marker is re-checked between paste and Enter: an interactive
    // delivery's last look at the world is the marker, not the paste.
    let engine_reads: Vec<usize> = calls
        .lines()
        .enumerate()
        .filter(|(_, call)| call.contains("ALDER_EXT_RUNNER_ENGINE"))
        .map(|(index, _)| index)
        .collect();
    let paste = calls
        .lines()
        .position(|call| call.starts_with("paste-buffer"))
        .expect("a paste happened");
    let enter = calls
        .lines()
        .position(|call| call.ends_with("Enter"))
        .expect("an Enter happened");
    assert!(
        engine_reads
            .iter()
            .any(|&read| paste < read && read < enter),
        "the engine marker is no longer re-checked between paste and Enter: {calls}"
    );
}

#[test]
fn an_engine_that_exits_between_paste_and_recheck_is_backed_out_not_submitted() {
    let stub = stub();
    // The pane sets its exited marker before `exec bash`; the stub models an
    // engine dying exactly in the paste-to-recheck window.
    fs::write(&stub.flip_on_paste, "").unwrap();

    let output = send(&stub, "running", "claude");
    assert!(
        !output.status.success(),
        "a send into a dying pane was reported delivered: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        output.status.code(),
        Some(5),
        "a dead engine is exit 5: the caller may rotate"
    );
    let complaint = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        complaint.contains(HANDLE) && complaint.contains("exited between paste and submit"),
        "the refusal does not name the session and the window: {complaint}"
    );

    let calls = calls(&stub);
    assert!(
        calls.contains(&format!("send-keys -t ={HANDLE}: C-u")),
        "the pasted text was not cleared: {calls}"
    );
    assert!(
        !calls.contains("Enter"),
        "Enter was sent at a shell holding pasted text: {calls}"
    );
    assert!(
        stub.engine_state.exists(),
        "the stub never flipped; this test is not testing the window"
    );
}

#[test]
fn a_codex_engine_receives_an_armored_resume_never_raw_bytes() {
    let stub = stub();
    fs::write(
        stub.state.join(HANDLE).join("codex-session"),
        "019fb2ef-d507-7201-bc36-79d6d5b82336\n",
    )
    .unwrap();
    // A Codex execution receives the safe encoded resume command even while
    // its one-shot engine is still running; it must not receive raw ruling
    // bytes at either the engine or its eventual holding shell.
    let output = send(&stub, "running", "codex");
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
    let resume = stub.state.join(HANDLE).join("resume");
    assert!(
        set.contains(&format!(
            "'{}' 019fb2ef-d507-7201-bc36-79d6d5b82336",
            resume.display()
        )),
        "the resume command does not run the state-directory script on the \
         recorded session: {set}"
    );
    assert!(
        !set.contains(".alder-ext-runner"),
        "the resume route still reaches into the worker-writable worktree: {set}"
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
        calls.contains(&format!("send-keys -t ={HANDLE}: Enter")),
        "{calls}"
    );
    assert!(!calls.contains("display-message"), "{calls}");
}

#[test]
fn delivery_routes_by_the_stamped_provider_never_the_tier_table() {
    let stub = stub();
    fs::write(
        stub.state.join(HANDLE).join("codex-session"),
        "019fb2ef-d507-7201-bc36-79d6d5b82336\n",
    )
    .unwrap();
    // A config that (mis)classifies every tier as claude changes nothing:
    // the session was stamped codex at start, and the stamp is the route.
    let config = stub.state.join("drifted-config.json");
    fs::write(
        &config,
        r#"{"tiers": {
            "luna": {"provider": "claude", "model": "m", "effort": "e", "counterpart": "x"},
            "x": {"provider": "codex", "model": "m", "effort": "e", "counterpart": "luna"}
        }}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_alder-ext-runner"))
        .args(["send", HANDLE, "--file", stub.finding.to_str().unwrap()])
        .env("PATH", &stub.path)
        .env("STUB_HANDLE", HANDLE)
        .env("STUB_ENGINE", "running")
        .env("STUB_PROVIDER", "codex")
        .env("STUB_WORKTREE", &stub.worktree)
        .env("STUB_ABSENT", "")
        .env("STUB_KILL_FAILS", "")
        .env("ALDER_EXT_RUNNER_STATE_DIR", &stub.state)
        .env("ALDER_EXT_RUNNER_CONFIG", &config)
        .output()
        .expect("the runner runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = calls(&stub);
    assert!(
        calls.lines().any(|call| call.starts_with("set-buffer")),
        "the drifted config changed a live session's route off the armored \
         resume: {calls}"
    );
    assert!(
        !calls.contains("load-buffer"),
        "the drifted config rerouted a codex session to the interactive \
         paste: {calls}"
    );
}

#[test]
fn a_torn_enter_marks_the_pane_and_later_sends_refuse_until_force_resolves_it() {
    let stub = stub();

    // First send tears between paste and Enter: the stub makes every
    // send-keys fail. The runner retries Enter once, then stamps the torn
    // marker and reports loudly.
    fs::write(&stub.enter_fails, "").unwrap();
    let torn = send(&stub, "running", "claude");
    assert!(
        !torn.status.success(),
        "a torn Enter was reported as delivered: {}",
        String::from_utf8_lossy(&torn.stdout)
    );
    assert_eq!(
        torn.status.code(),
        Some(5),
        "a torn delivery is exit 5: the caller may rotate"
    );
    let complaint = String::from_utf8_lossy(&torn.stderr).into_owned();
    assert!(complaint.contains("DELIVERY TORN"), "{complaint}");
    assert!(
        complaint.contains(HANDLE) && complaint.contains("Unsubmitted text"),
        "the torn diagnostic does not name the session and the residue: {complaint}"
    );
    let after_tear = calls(&stub);
    assert_eq!(
        after_tear
            .lines()
            .filter(|call| call.ends_with("Enter"))
            .count(),
        2,
        "Enter was not retried exactly once: {after_tear}"
    );
    assert!(
        after_tear.contains(&format!(
            "set-environment -t ={HANDLE} ALDER_EXT_RUNNER_TORN 1"
        )),
        "the torn marker was not stamped on the session: {after_tear}"
    );
    assert!(stub.torn.exists(), "the stub recorded no torn marker");

    // Enter works again, but the pane still holds unsubmitted text: the next
    // send refuses before pasting anything.
    fs::remove_file(&stub.enter_fails).unwrap();
    let refused = send(&stub, "running", "claude");
    assert!(!refused.status.success());
    assert_eq!(
        refused.status.code(),
        Some(5),
        "a torn pane refusal is exit 5: the caller may rotate"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("holds unsubmitted text"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let after_refusal = calls(&stub);
    assert_eq!(
        after_refusal
            .lines()
            .filter(|call| call.starts_with("load-buffer"))
            .count(),
        1,
        "the refused send still pasted at the dirty pane: {after_refusal}"
    );

    // --force delivers anyway; its Enter lands, which submits the residue
    // along with this message and clears the marker.
    let forced = send_with(&stub, "running", "claude", &["--force"]);
    assert!(
        forced.status.success(),
        "--force did not deliver: {}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(
        String::from_utf8_lossy(&forced.stdout).contains(&format!("sent once to {HANDLE}")),
        "{}",
        String::from_utf8_lossy(&forced.stdout)
    );
    assert!(
        !stub.torn.exists(),
        "a delivered --force send left the torn marker behind"
    );
    assert!(
        calls(&stub).contains(&format!(
            "set-environment -u -t ={HANDLE} ALDER_EXT_RUNNER_TORN"
        )),
        "{}",
        calls(&stub)
    );
}

#[test]
fn a_pane_that_cannot_prove_a_running_engine_is_refused_not_pasted_at() {
    let stub = stub();
    // No engine marker at all: a session of unknown provenance. Fail-safe is
    // to never paste at a pane that cannot prove an engine is listening.
    let output = send(&stub, "", "claude");
    assert!(!output.status.success());
    assert_eq!(
        output.status.code(),
        Some(5),
        "an unprovable engine is exit 5: the caller may rotate"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot prove an engine is running"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = calls(&stub);
    assert!(
        !calls.lines().any(|call| call.starts_with("set-buffer")
            || call.starts_with("load-buffer")
            || call.starts_with("paste-buffer")
            || call.starts_with("send-keys")),
        "an unproven engine received a delivery: {calls}"
    );
}

#[test]
fn a_session_without_a_provider_stamp_is_refused_whole() {
    let stub = stub();
    let output = send(&stub, "running", "");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no provider stamp"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = calls(&stub);
    assert!(
        !calls.lines().any(|call| call.starts_with("set-buffer")
            || call.starts_with("load-buffer")
            || call.starts_with("paste-buffer")
            || call.starts_with("send-keys")),
        "an unstamped session received a delivery: {calls}"
    );
}

#[test]
fn an_exited_interactive_engine_is_refused_rather_than_typed_at() {
    let stub = stub();
    let output = send(&stub, "exited", "claude");
    assert!(!output.status.success());
    assert_eq!(
        output.status.code(),
        Some(5),
        "an exited interactive engine is exit 5: the caller may rotate"
    );
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
fn a_codex_send_without_a_recorded_lowercase_uuid_is_refused() {
    let stub = stub();
    // No codex-session marker was ever written: resuming would have to guess
    // from `--last`, which the runner refuses to do.
    let output = send(&stub, "exited", "codex");
    assert!(!output.status.success());
    assert_eq!(
        output.status.code(),
        Some(5),
        "an unresumable codex execution is exit 5: the caller may rotate"
    );
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

    // A marker that is not a lowercase UUID — the stamp sidecar's exact
    // output shape — is refused the same way, whatever it looks like.
    for wrong in [
        "$(rm -rf /)\n",
        "--last\n",
        "019FB2EF-D507-7201-BC36-79D6D5B82336\n",
        "not-a-session\n",
    ] {
        fs::write(stub.state.join(HANDLE).join("codex-session"), wrong).unwrap();
        let output = send(&stub, "exited", "codex");
        assert!(!output.status.success(), "`{wrong}` was accepted");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("not a session ID"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn a_file_past_the_send_ceiling_is_refused_by_name() {
    let stub = stub();
    let big = stub.state.join("too-big.txt");
    fs::write(&big, vec![b'x'; 64 * 1024 + 1]).unwrap();
    let output = runner_with(
        &stub,
        "running",
        "claude",
        &["send", HANDLE, "--file", big.to_str().unwrap()],
    );
    assert!(!output.status.success());
    assert_eq!(
        output.status.code(),
        Some(1),
        "an oversized file is a caller mistake, not a rotatable engine state"
    );
    let complaint = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(complaint.contains("64 KiB"), "{complaint}");
    assert!(
        calls(&stub).is_empty(),
        "an oversized send still reached tmux: {}",
        calls(&stub)
    );

    // Exactly at the ceiling is still deliverable.
    fs::write(&big, vec![b'y'; 64 * 1024]).unwrap();
    let output = runner_with(
        &stub,
        "running",
        "claude",
        &["send", HANDLE, "--file", big.to_str().unwrap()],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_send_refuses_while_another_operation_holds_the_handle_lock() {
    let stub = stub();
    let lock_path = stub.state.join(format!("start-{HANDLE}.lock"));
    let lock = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    lock.lock().unwrap();

    let output = send(&stub, "running", "claude");
    assert!(!output.status.success());
    assert_eq!(
        output.status.code(),
        Some(4),
        "lock contention is exit 4: the caller treats the wake as already served"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("holds its lock"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        calls(&stub).is_empty(),
        "the lock loser still touched the pane: {}",
        calls(&stub)
    );
    drop(lock);

    let output = send(&stub, "running", "claude");
    assert!(
        output.status.success(),
        "a released lock still refuses: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_send_to_a_handle_nothing_answers_to_is_exit_5() {
    let stub = stub();
    let output = Command::new(env!("CARGO_BIN_EXE_alder-ext-runner"))
        .args(["send", HANDLE, "--file", stub.finding.to_str().unwrap()])
        .env("PATH", &stub.path)
        .env("STUB_HANDLE", HANDLE)
        .env("STUB_ENGINE", "running")
        .env("STUB_PROVIDER", "claude")
        .env("STUB_WORKTREE", &stub.worktree)
        .env("STUB_ABSENT", "1")
        .env("STUB_KILL_FAILS", "")
        .env("ALDER_EXT_RUNNER_STATE_DIR", &stub.state)
        .env_remove("ALDER_EXT_RUNNER_CONFIG")
        .output()
        .expect("the runner runs");
    assert!(!output.status.success());
    assert_eq!(
        output.status.code(),
        Some(5),
        "a dead handle is exit 5: the caller may rotate"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no execution answers"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn kill_verifies_the_session_is_gone_before_reporting() {
    let stub = stub();
    let output = runner_with(&stub, "running", "claude", &["kill", HANDLE]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("killed {HANDLE}")),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let calls = calls(&stub);
    assert!(
        calls.contains(&format!("kill-session -t ={HANDLE}")),
        "{calls}"
    );
    // The verification is real: `has-session` is asked again after the kill.
    let kill_line = calls
        .lines()
        .position(|call| call.starts_with("kill-session"))
        .unwrap();
    assert!(
        calls
            .lines()
            .enumerate()
            .any(|(index, call)| index > kill_line && call.starts_with("has-session")),
        "kill never looked back to verify the session is gone: {calls}"
    );
    assert!(stub.killed.exists());
}

#[test]
fn killing_a_nonexistent_handle_is_a_distinct_clean_message() {
    let stub = stub();
    let output = Command::new(env!("CARGO_BIN_EXE_alder-ext-runner"))
        .args(["kill", HANDLE])
        .env("PATH", &stub.path)
        .env("STUB_HANDLE", HANDLE)
        .env("STUB_ENGINE", "running")
        .env("STUB_PROVIDER", "claude")
        .env("STUB_WORKTREE", &stub.worktree)
        .env("STUB_ABSENT", "1")
        .env("STUB_KILL_FAILS", "")
        .env("ALDER_EXT_RUNNER_STATE_DIR", &stub.state)
        .env_remove("ALDER_EXT_RUNNER_CONFIG")
        .output()
        .expect("the runner runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("nothing to kill"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let calls = calls(&stub);
    assert!(
        !calls.contains("kill-session"),
        "a kill was aimed at nothing: {calls}"
    );
}

#[test]
fn a_kill_that_leaves_the_session_alive_is_an_error_not_a_report() {
    let stub = stub();
    let output = Command::new(env!("CARGO_BIN_EXE_alder-ext-runner"))
        .args(["kill", HANDLE])
        .env("PATH", &stub.path)
        .env("STUB_HANDLE", HANDLE)
        .env("STUB_ENGINE", "running")
        .env("STUB_PROVIDER", "claude")
        .env("STUB_WORKTREE", &stub.worktree)
        .env("STUB_ABSENT", "")
        .env("STUB_KILL_FAILS", "1")
        .env("ALDER_EXT_RUNNER_STATE_DIR", &stub.state)
        .env_remove("ALDER_EXT_RUNNER_CONFIG")
        .output()
        .expect("the runner runs");
    assert!(
        !output.status.success(),
        "a kill tmux refused was reported as done: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("kill-session failed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
