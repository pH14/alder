//! `start` against a real tmux server and a real git repository, both this
//! test's own.
//!
//! The unit tests in `start.rs` prove the ordering rules against a fake host.
//! This proves the parts only the world can answer: that a pane really does
//! receive the prompt as one argv element, that identity exists at pane
//! creation, that a live engine is refused, that an exited pane is replaced,
//! that `send` really lands bytes in a pane, and that a worktree really is
//! cut on the requested branch.
//!
//! The sandbox is a `tmux` shim first on PATH that unsets `TMUX`/`TMUX_PANE`
//! and hands the real tmux an explicit `-S <socket>`, because inside a tmux
//! pane — where this test usually runs — `$TMUX` names the real server and
//! `TMUX_TMPDIR` isolates nothing. PATH cannot be changed in-process (this
//! crate forbids unsafe code), so the parent lays the sandbox out and re-runs
//! this binary inside it; the `#[ignore]`d child is what touches tmux.
//!
//! Teardown is scripts/tmux-sandbox.sh (this crate's own copy, so the crate
//! stays movable), sourced rather than copied inline: one session, by exact
//! name, and only once the sandbox server is proved to hold nothing else.

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use alder_ext_runner::{
    host::Host,
    start::{self},
    tier::{TIERS, lookup},
};
use tempfile::{Builder, TempDir};

const SESSION_ENV: &str = "RUNNER_START_TEST_SESSION";
const SOCKET_ENV: &str = "RUNNER_START_TEST_SOCKET";
const WORK_ENV: &str = "RUNNER_START_TEST_WORK";
const CHILD: &str = "sandboxed_start_cuts_a_worktree_and_leaves_a_live_pane";
/// The branch the sandboxed start runs against.
const BRANCH: &str = "work/wv-1";

#[test]
fn start_runs_against_a_private_tmux_server_and_a_throwaway_repo() {
    let Some(real_tmux) = which("tmux") else {
        eprintln!("skipping {CHILD}: tmux is not installed");
        return;
    };
    if which("git").is_none() {
        eprintln!("skipping {CHILD}: git is not installed");
        return;
    }
    let teardown = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/tmux-sandbox.sh");
    assert!(
        teardown.is_file(),
        "the crate-local sandbox teardown is missing: {}",
        teardown.display()
    );

    // /tmp, because a unix socket path is capped near 104 bytes.
    let sockets = temp_dir("runner-start-sock");
    let socket = sockets.path().join("tmux.sock");
    let real_socket = real_socket();
    assert_ne!(
        Some(socket.as_path()),
        real_socket.as_deref(),
        "refusing to run: the sandbox socket is the real server's"
    );
    let bin = temp_dir("runner-start-bin");
    let work = temp_dir("runner-start-work");

    // Every tmux call the start and `send` make lands on the sandbox server,
    // and every one is logged. The child checks the start itself injects
    // nothing, then runs `send` against this real server.
    // Entries are separated by a record marker rather than newlines, because
    // a prompt is delivered verbatim and may itself span lines.
    write_executable(
        &bin.path().join("tmux"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n--tmux--\\n' \"$*\" >>'{log}'\nunset TMUX TMUX_PANE\nexec '{tmux}' -f /dev/null -S '{socket}' \"$@\"\n",
            log = work.path().join("tmux-calls.log").display(),
            tmux = real_tmux.display(),
            socket = socket.display()
        ),
    );

    let session = start::handle_for_branch(BRANCH);
    let before = real_sessions(&real_tmux, real_socket.as_deref());

    // Nothing between here and the teardown may panic: from the moment the
    // child starts there may be a server to clean up.
    let outcome = Command::new(env::current_exe().expect("this test binary has a path"))
        .args(["--exact", CHILD, "--ignored", "--nocapture"])
        .args(["--test-threads", "1"])
        .env("PATH", path_with(bin.path()))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("TMUX_TMPDIR", sockets.path())
        .env(SESSION_ENV, &session)
        .env(SOCKET_ENV, &socket)
        .env(WORK_ENV, work.path())
        .output()
        .expect("the sandboxed half of this test runs");

    let status = sandbox_teardown(&teardown, &real_tmux, &socket, sockets.path(), &session);
    let after = real_sessions(&real_tmux, real_socket.as_deref());

    // The regression this exists to disprove: that the sandbox reached the
    // default server.
    assert!(
        !after.contains(&session),
        "`{session}` was created on the default server, not the sandbox"
    );
    assert!(
        before.is_empty() || !after.is_empty(),
        "the default server held {before:?} and now holds nothing"
    );

    assert!(
        outcome.status.success(),
        "the sandboxed start test failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&outcome.stdout),
        String::from_utf8_lossy(&outcome.stderr)
    );
    // A filter that matches nothing also exits zero, so the child says so
    // itself rather than passing by silence.
    assert!(
        work.path().join("done").is_file(),
        "the sandboxed test never ran\n--- stdout ---\n{}",
        String::from_utf8_lossy(&outcome.stdout)
    );
    // The child kills the session it made as its last act.
    assert_eq!(
        status, "empty",
        "teardown reported `{status}`: the session outlived the test"
    );
}

/// The half that touches tmux and git.
#[test]
#[ignore = "runs inside the sandbox start_runs_against_a_private_tmux_server_and_a_throwaway_repo lays out"]
fn sandboxed_start_cuts_a_worktree_and_leaves_a_live_pane() {
    let Ok(session) = env::var(SESSION_ENV) else {
        eprintln!("skipped: this test only runs inside its sandbox harness");
        return;
    };
    let socket = env::var(SOCKET_ENV).expect("the harness names the sandbox socket");
    let work = PathBuf::from(env::var(WORK_ENV).expect("the harness names its work directory"));

    // Prove the sandbox before creating anything.
    assert!(
        env::var_os("TMUX").is_none(),
        "TMUX is still set: this is not the sandbox"
    );
    let shim = env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .next()
        .expect("PATH has at least one entry")
        .join("tmux");
    let shimmed = fs::read_to_string(&shim).expect("the tmux shim is readable");
    assert!(
        shimmed.contains(&format!("-S '{socket}'")),
        "the first tmux on PATH is not the sandbox shim: {}",
        shim.display()
    );

    // A throwaway repository: git on main with one commit.
    let root = work.join("project");
    fs::create_dir_all(&root).expect("the project is created");
    fs::write(root.join("README.md"), "sandbox\n").expect("a file to commit");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "v@x"]);
    git(&root, &["config", "user.name", "v"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "sandbox"]);

    // The engine stub records the argv it was handed, then reads one
    // multi-line delivery in raw mode. This makes the real tmux bytes — not a
    // mocked tmux call — the delivery contract. It stays live until the test
    // releases it, which keeps the running/exited distinction observable too.
    let argv = work.join("engine-argv");
    let argc = work.join("engine-argc");
    let send_ready = work.join("engine-ready-for-send");
    let send_received = work.join("engine-send-bytes");
    let message = "ruling line one\nruling line two";
    let send_byte_count = message.len() + 1; // tmux sends one final Enter.
    let stub = write_executable(
        &work.join("engine.sh"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$#\" >'{argc}'\nprintf '%s\\n' \"$@\" >'{argv}.part' && mv '{argv}.part' '{argv}'\nstty -icanon -echo min 1 time 0\n: >'{ready}'\ndd bs=1 count={count} of='{received}.part' 2>/dev/null && mv '{received}.part' '{received}'\nstty sane\nwhile [ ! -f '{release}' ]; do sleep 0.05; done\n",
            argc = argc.display(),
            argv = argv.display(),
            ready = send_ready.display(),
            count = send_byte_count,
            received = send_received.display(),
            release = work.join("release-engine").display(),
        ),
    );

    let host = Host::new(root.clone());
    let prompt = "Sandbox prompt: fix the thing.\nSecond prompt line.";
    let luna = lookup(&TIERS, "luna").expect("luna is a rung");
    let started = start::start(
        &host,
        BRANCH,
        luna,
        prompt,
        Some(&stub.display().to_string()),
    )
    .expect("the start succeeds");

    assert_eq!(started.handle, session);
    assert_eq!(started.branch, BRANCH);
    assert_eq!(started.worktree, work.join(&session));
    assert!(!started.adopted_worktree);

    // Nothing on this path waits for the engine to boot — the half only the
    // real host can answer. What a start may run is the honest observation,
    // and the shim has been logging it all along. `Host` reaches the world by
    // running `git` and `tmux`, and of those only tmux can see a pane at all,
    // so a wait for the engine has to be a tmux call however it is spelled —
    // `wait-for` on a channel the pane signals, a blocking flag, a second
    // look in a loop. This is the whole list, in order, and both entries
    // return as soon as the server has answered.
    let start_ran = fs::read_to_string(work.join("tmux-calls.log")).unwrap_or_default();
    let ran = tmux_entries(&start_ran);
    let new_session = format!(
        "new-session -d -s {session} -c {worktree} \
         -e ALDER_EXT_RUNNER_HANDLE={session} -e ALDER_EXT_RUNNER_ENGINE=running \
         -e ALDER_EXT_RUNNER_TIER=luna -e ALDER_EXT_RUNNER_WORKTREE={worktree} ",
        worktree = work.join(&session).display()
    );
    assert_eq!(
        ran.len(),
        2,
        "the start ran tmux commands beyond the two it needs, and a wait for \
         the engine would be among them: {start_ran}"
    );
    assert_eq!(
        ran[0],
        format!("has-session -t ={session}"),
        "the first tmux call is no longer just the existence question: {start_ran}"
    );
    let pane_command = ran[1].strip_prefix(&new_session).unwrap_or_else(|| {
        panic!("the session is no longer created by that exact call: {start_ran}")
    });
    assert!(
        pane_command.starts_with(&format!(
            ".alder-ext-runner/stamp-codex-session; '{}' '",
            stub.display()
        )),
        "something precedes the engine in the pane command: {start_ran}"
    );
    assert!(
        pane_command.ends_with(&format!(
            "'; tmux set-environment -t '={session}' ALDER_EXT_RUNNER_ENGINE exited; exec bash"
        )),
        "something follows the pane command, and a tmux command appended after \
         it is a wait for the engine however it is spelled: {start_ran}"
    );

    // The prompt arrives as exactly one argument, byte for byte.
    let received = await_file(&argv);
    assert_eq!(
        await_file(&argc).trim(),
        "1",
        "the prompt was split into arguments"
    );
    assert_eq!(received, format!("{prompt}\n"));

    // Identity and live state are present on the session from the
    // new-session effect itself, under the runner's own names and nobody
    // else's.
    assert_eq!(
        session_environment(&session, "ALDER_EXT_RUNNER_HANDLE").as_deref(),
        Some(session.as_str())
    );
    assert_eq!(
        session_environment(&session, "ALDER_EXT_RUNNER_ENGINE").as_deref(),
        Some("running")
    );
    assert_eq!(
        session_environment(&session, "ALDER_EXT_RUNNER_TIER").as_deref(),
        Some("luna")
    );
    assert!(
        session_environment(&session, "ALDER_ATTEMPT").is_none(),
        "the runner stamped somebody else's marker into its session"
    );

    // `status` through the library agrees with the environment.
    let status = alder_ext_runner::ops::status(&host, &session).expect("status answers");
    assert_eq!(status.word, "running");

    // A second start while the engine is live is genuinely refused.
    let refused = start::start(
        &host,
        BRANCH,
        luna,
        prompt,
        Some(&stub.display().to_string()),
    )
    .expect_err("a second live execution is refused");
    assert!(refused.message.contains("already running"), "{refused}");

    // Run `send` against this test's live pane. The stub reads the terminal
    // in raw mode, so this catches both a bad pane target and tmux's default
    // LF-to-CR conversion without trusting a tmux mock. The interactive
    // route is selected from the session's own tier marker; the runner reads
    // it before delivery only and does not sample the pane after it sends.
    let tmux_before_send = fs::read_to_string(work.join("tmux-calls.log")).unwrap_or_default();
    assert!(
        !tmux_before_send.contains("send-keys"),
        "start typed into the pane: {tmux_before_send}"
    );
    await_true(
        || send_ready.is_file(),
        "the engine is reading its pane for a delivery",
    );
    let message_file = work.join("review-finding.txt");
    fs::write(&message_file, message).expect("the local send input is written");
    // The built-in table calls luna a codex rung, but this pane runs an
    // interactive stub; a config table that names the same rung interactive
    // is exactly what the config file is for.
    let config = work.join("runner-config.json");
    fs::write(
        &config,
        r#"{"tiers": {
            "luna": {"provider": "claude", "model": "stub", "effort": "high", "counterpart": "luna-codex"},
            "luna-codex": {"provider": "codex", "model": "stub", "effort": "high", "counterpart": "luna"}
        }}"#,
    )
    .expect("the config is written");
    let delivered = Command::new(env!("CARGO_BIN_EXE_alder-ext-runner"))
        .args(["send", &session, "--file", message_file.to_str().unwrap()])
        .env("ALDER_EXT_RUNNER_CONFIG", &config)
        .output()
        .expect("send runs");
    assert!(
        delivered.status.success(),
        "send failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&delivered.stdout),
        String::from_utf8_lossy(&delivered.stderr)
    );
    await_true(
        || send_received.is_file(),
        "the live pane receives the sent bytes",
    );
    assert_eq!(
        fs::read(&send_received).expect("the received message is readable"),
        format!("{message}\n").as_bytes(),
        "send must preserve embedded LF and add exactly one submitting Enter"
    );
    let send_tmux_calls = fs::read_to_string(work.join("tmux-calls.log")).unwrap_or_default();
    assert!(
        tmux_entries(&send_tmux_calls).iter().any(|call| call
            .starts_with("paste-buffer -d -r -b alder-ext-send-")
            && call.ends_with(&format!("-t ={session}:"))),
        "send did not paste raw bytes into the session pane: {send_tmux_calls}"
    );
    assert!(
        tmux_entries(&send_tmux_calls)
            .iter()
            .any(|call| *call == format!("send-keys -t ={session}: Enter")),
        "send did not submit once to the session pane: {send_tmux_calls}"
    );

    // A codex-tier send takes the encoded-resume route even while the
    // one-shot process is still running and cannot read its terminal. Once
    // that process ends, the holding shell receives only the generated
    // command, never the raw message.
    let resumed_message = work.join("resumed-message");
    write_executable(
        &started.worktree.join(".alder-ext-runner/resume"),
        &format!(
            "#!/bin/sh\nprintf '%s' \"$2\" >'{}'\nexec tmux wait-for send-resume-hold\n",
            resumed_message.display()
        ),
    );
    fs::write(
        started.worktree.join(".alder-ext-runner/codex-session"),
        "019fb2ef-d507-7201-bc36-79d6d5b82336\n",
    )
    .expect("the codex session marker is written");
    let codex_config = work.join("runner-config-codex.json");
    fs::write(
        &codex_config,
        r#"{"tiers": {
            "luna": {"provider": "codex", "model": "stub", "effort": "high", "counterpart": "luna-claude"},
            "luna-claude": {"provider": "claude", "model": "stub", "effort": "high", "counterpart": "luna"}
        }}"#,
    )
    .expect("the codex config is written");
    let running_codex = Command::new(env!("CARGO_BIN_EXE_alder-ext-runner"))
        .args(["send", &session, "--file", message_file.to_str().unwrap()])
        .env("ALDER_EXT_RUNNER_CONFIG", &codex_config)
        .output()
        .expect("the running-codex send runs");
    assert!(
        running_codex.status.success(),
        "send should report one encoded delivery while the engine is still running\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&running_codex.stdout),
        String::from_utf8_lossy(&running_codex.stderr)
    );
    assert!(
        !resumed_message.exists(),
        "the running engine read the queued resume command before it exited"
    );
    let running_codex_calls = fs::read_to_string(work.join("tmux-calls.log")).unwrap_or_default();
    assert!(
        tmux_entries(&running_codex_calls)
            .iter()
            .any(|call| call.starts_with("set-buffer -b alder-ext-send-")),
        "the codex route did not receive an encoded resume command: {running_codex_calls}"
    );
    assert!(
        !running_codex_calls.contains(message),
        "the codex route put the raw message in tmux argv: {running_codex_calls}"
    );
    assert!(
        !tmux_entries(&running_codex_calls)
            .iter()
            .any(|call| call.starts_with("display-message")),
        "send synchronously inspected the running pane: {running_codex_calls}"
    );

    // Once the engine exits, its pane remains, the handle reads `done`, and
    // the holding shell executes the queued resume command.
    fs::write(work.join("release-engine"), "go").expect("the engine is released");
    await_true(
        || session_environment(&session, "ALDER_EXT_RUNNER_ENGINE").as_deref() == Some("exited"),
        "the session records that its engine exited",
    );
    assert!(
        session_exists(&session),
        "the session died with the engine; the pane does not end `; exec bash`"
    );
    let status = alder_ext_runner::ops::status(&host, &session).expect("status answers");
    assert_eq!(status.word, "done");
    await_true(
        || resumed_message.is_file(),
        "the holding shell executes the queued resume command",
    );
    assert_eq!(
        fs::read_to_string(&resumed_message).expect("the resumed message is readable"),
        message,
        "the codex route changed the local message bytes"
    );

    // `start` means "run this prompt": the exited pane is replaced, its
    // worktree adopted, and no second worktree is cut.
    let restarted = start::start(
        &host,
        BRANCH,
        luna,
        "a second prompt for the same branch",
        Some(&stub.display().to_string()),
    )
    .expect("an exited pane is replaced");
    assert!(restarted.adopted_worktree);
    assert_eq!(restarted.handle, session);
    let calls = fs::read_to_string(work.join("tmux-calls.log")).unwrap_or_default();
    assert_eq!(
        tmux_entries(&calls)
            .iter()
            .filter(|call| call.starts_with("new-session"))
            .count(),
        2,
        "{calls}"
    );
    assert!(
        tmux_entries(&calls)
            .iter()
            .any(|call| call.starts_with("kill-session")),
        "the exited pane was not replaced: {calls}"
    );

    // The worktree is real, on its own branch, and carries only the runner's
    // own machinery — nothing of any caller's.
    let worktree = &started.worktree;
    assert!(
        worktree.join("README.md").is_file(),
        "the worktree is empty"
    );
    assert!(
        !worktree.join(".alder").exists(),
        "the runner wrote somebody else's directory into the worktree"
    );
    let resume = worktree.join(".alder-ext-runner/resume");
    assert!(
        resume.is_file(),
        "a codex execution has nothing to resume it"
    );
    assert!(
        resume
            .metadata()
            .expect("the resume script is statable")
            .permissions()
            .mode()
            & 0o111
            != 0,
        "the resume script is not executable"
    );
    let script = fs::read_to_string(&resume).expect("the resume script is readable");
    for part in [
        "codex exec resume",
        "-m",
        "gpt-5.6-luna",
        "model_reasoning_effort=high",
        "sandbox_workspace_write.network_access=true",
        "writable_roots",
    ] {
        assert!(
            script.contains(part),
            "the resume script omits {part}: {script}"
        );
    }
    let checked = Command::new("sh")
        .args(["-n"])
        .arg(&resume)
        .status()
        .expect("sh runs");
    assert!(checked.success(), "the resume script is not valid sh");
    assert_eq!(
        run(&root, "git", &["rev-parse", "--abbrev-ref", BRANCH]).trim(),
        BRANCH
    );

    kill_session(&session);
    await_true(|| !session_exists(&session), "the session goes away");
    fs::write(work.join("done"), "ok").expect("the marker is written");
}

/// One entry per tmux invocation, as the shim records them. A prompt may
/// span lines, so the log is record-separated rather than line-based.
fn tmux_entries(log: &str) -> Vec<&str> {
    log.split("\n--tmux--\n")
        .filter(|entry| !entry.trim().is_empty())
        .collect()
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

fn run(root: &Path, program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .unwrap_or_else(|error| panic!("{program} runs: {error}"));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn session_exists(session: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", &format!("={session}")])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn kill_session(session: &str) {
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", &format!("={session}")])
        .output();
}

fn session_environment(session: &str, name: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args(["show-environment", "-t", &format!("={session}"), name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .strip_prefix(&format!("{name}="))
        .map(str::to_owned)
}

fn temp_dir(prefix: &str) -> TempDir {
    Builder::new()
        .prefix(prefix)
        .tempdir_in("/tmp")
        .unwrap_or_else(|error| panic!("cannot make a {prefix} directory: {error}"))
}

fn write_executable(path: &Path, body: &str) -> PathBuf {
    fs::write(path, body).expect("the script is written");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("the script is executable");
    path.to_path_buf()
}

fn which(program: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

fn path_with(first: &Path) -> std::ffi::OsString {
    let mut directories = vec![first.to_path_buf()];
    directories.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    env::join_paths(directories).expect("the sandbox PATH is joinable")
}

fn real_socket() -> Option<PathBuf> {
    let value = env::var("TMUX").ok()?;
    let socket = value.split(',').next().unwrap_or_default();
    (!socket.is_empty()).then(|| PathBuf::from(socket))
}

fn sessions_on(tmux: &Path, socket: &Path) -> Vec<String> {
    let output = Command::new(tmux)
        .arg("-S")
        .arg(socket)
        .args(["list-sessions", "-F", "#{session_name}"])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn real_sessions(tmux: &Path, socket: Option<&Path>) -> Vec<String> {
    socket
        .map(|socket| sessions_on(tmux, socket))
        .unwrap_or_default()
}

/// Run the crate's own `sandbox_teardown` and report what it did.
fn sandbox_teardown(
    script: &Path,
    real_tmux: &Path,
    socket: &Path,
    sockets: &Path,
    session: &str,
) -> String {
    let output = Command::new("bash")
        .arg("-c")
        .arg(r#"set -eu; . "$1"; sandbox_teardown; printf '%s' "$SANDBOX_TEARDOWN_STATUS""#)
        .arg("runner-start-teardown")
        .arg(script)
        .env("REAL_TMUX", real_tmux)
        .env("SOCK", socket)
        .env("SOCKDIR", sockets)
        .env("SESSION_NAME", session)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("the sandbox teardown runs");
    let complaint = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "the teardown failed: {complaint}");
    let status = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert_ne!(
        status, "aborted",
        "the sandbox server held sessions this run does not own: {complaint}"
    );
    status
}

fn await_true(mut ready: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting until {what}");
}

fn await_file(path: &Path) -> String {
    let mut content = String::new();
    await_true(
        || {
            content = fs::read_to_string(path).unwrap_or_default();
            !content.is_empty()
        },
        &format!("{} is written", path.display()),
    );
    content
}
