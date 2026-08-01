//! The tmux shell-outs, against a real tmux server that is nobody else's.
//!
//! `Host` runs a bare `tmux`, so the only way to reach a private server
//! without editing what ships is the shim scripts/tests/verify-spawn.sh
//! already uses: a script named `tmux`, first on PATH, that unsets `TMUX` and
//! `TMUX_PANE` and hands the real tmux an explicit `-S <socket>`. That is the
//! whole safety story, and TMUX_TMPDIR does not do it: inside a tmux pane —
//! where this test usually runs — `$TMUX` names the real server and tmux
//! prefers it for every client command.
//!
//! PATH cannot be changed in this process (alderd forbids unsafe code, and
//! `std::env::set_var` is unsafe in edition 2024), so the work happens one
//! process down: [`host_tmux_effects_run_against_a_private_tmux_server`] lays
//! out the sandbox and re-runs this test binary inside it, and the `#[ignore]`d
//! [`sandboxed_host_drives_a_real_tmux_session`] is what actually drives
//! `Host`.
//!
//! Teardown is scripts/tests/tmux-sandbox.sh itself, sourced rather than
//! copied: one session, by exact name, and only once the sandbox server is
//! proved to hold nothing else. No kill-server, no patterns, and nothing at
//! all aimed at the default server, anywhere in this file.

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use alderd::{
    config::Engine,
    effects::{Effects, Host},
};
use tempfile::{Builder, TempDir};

/// How the sandbox tells its child half where it is.
const SESSION_ENV: &str = "ALDERD_TEST_TMUX_SESSION";
const WATCHER_ENV: &str = "ALDERD_TEST_TMUX_WATCHER";
const SOCKET_ENV: &str = "ALDERD_TEST_TMUX_SOCKET";
const WORK_ENV: &str = "ALDERD_TEST_TMUX_WORK";
const CHILD: &str = "sandboxed_host_drives_a_real_tmux_session";

#[test]
fn host_tmux_effects_run_against_a_private_tmux_server() {
    let Some(real_tmux) = which("tmux") else {
        eprintln!("skipping {CHILD}: tmux is not installed");
        return;
    };
    let teardown = workspace_root().join("scripts/tests/tmux-sandbox.sh");
    assert!(
        teardown.is_file(),
        "the shared sandbox teardown is missing: {}",
        teardown.display()
    );

    // Both throwaway directories sit in /tmp: a unix socket path is capped
    // near 104 bytes, and the system temp directory is already long.
    let sockets = temp_dir("alderd-host-sock");
    let socket = sockets.path().join("tmux.sock");
    let real_socket = real_socket();
    assert_ne!(
        Some(socket.as_path()),
        real_socket.as_deref(),
        "refusing to run: the sandbox socket is the real server's"
    );
    let bin = temp_dir("alderd-host-bin");
    let work = temp_dir("alderd-host-work");

    // Every tmux call the daemon makes lands on the sandbox server. `-f
    // /dev/null` keeps a machine's tmux.conf — which may create sessions of
    // its own — out of a server the teardown expects to hold one session.
    write_executable(
        &bin.path().join("tmux"),
        &format!(
            "#!/bin/sh\nunset TMUX TMUX_PANE\nexec '{}' -f /dev/null -S '{}' \"$@\"\n",
            real_tmux.display(),
            socket.display()
        ),
    );
    let session = format!("alderd-host-{}", std::process::id());
    let watcher = format!("client-of-{session}");
    let before = real_sessions(&real_tmux, real_socket.as_deref());

    // Nothing between here and the teardown may panic: from the moment the
    // child starts there is a server to clean up.
    let outcome = Command::new(env::current_exe().expect("this test binary has a path"))
        .args(["--exact", CHILD, "--ignored", "--nocapture"])
        .args(["--test-threads", "1"])
        .env("PATH", path_with(bin.path()))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        // Belt and braces: with $TMUX gone, a call that slipped past the shim
        // would fall back to TMUX_TMPDIR rather than to the real server.
        .env("TMUX_TMPDIR", sockets.path())
        .env(SESSION_ENV, &session)
        .env(WATCHER_ENV, &watcher)
        .env(SOCKET_ENV, &socket)
        .env(WORK_ENV, work.path())
        .output()
        .expect("the sandboxed half of this test runs");

    // The attached-client session is this run's too, but the shared teardown
    // owns exactly one name. Kill it here — exact name, private socket — and
    // only while the server holds nothing but the two names this run made, so
    // a stranger still reaches the teardown's abort.
    let held = sessions_on(&real_tmux, &socket);
    if held.contains(&watcher) && held.iter().all(|name| *name == watcher || *name == session) {
        kill_exactly(&real_tmux, &socket, &watcher);
    }
    let status = sandbox_teardown(&teardown, &real_tmux, &socket, sockets.path(), &session);
    let after = real_sessions(&real_tmux, real_socket.as_deref());

    // The regression this exists to disprove: that the sandbox reached the
    // default server. Sessions there come and go while this runs — other
    // workers live on that server — so the check is that none of this run's
    // names appear on it and that it did not lose everything at once.
    for name in [&session, &watcher] {
        assert!(
            !after.contains(name),
            "`{name}` was created on the default server, not the sandbox"
        );
    }
    assert!(
        before.is_empty() || !after.is_empty(),
        "the default server held {before:?} and now holds nothing"
    );

    assert!(
        outcome.status.success(),
        "the sandboxed tmux test failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
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
    // The child kills the session through Host as its last act, so an
    // independent teardown must find nothing left to kill.
    assert_eq!(
        status, "empty",
        "teardown reported `{status}`: the session outlived Host::tmux_kill_session"
    );
}

/// The half that actually touches tmux.
///
/// It is ignored because it is safe only inside the sandbox its parent lays
/// out: run on its own, `tmux` would mean whatever server the environment
/// says, so it proves it is on a private socket before it creates anything.
#[test]
#[ignore = "runs inside the sandbox host_tmux_effects_run_against_a_private_tmux_server lays out"]
fn sandboxed_host_drives_a_real_tmux_session() {
    let Ok(session) = env::var(SESSION_ENV) else {
        eprintln!("skipped: this test only runs inside its sandbox harness");
        return;
    };
    let watcher = env::var(WATCHER_ENV).expect("the harness names the client session");
    let socket = env::var(SOCKET_ENV).expect("the harness names the sandbox socket");
    let work = PathBuf::from(env::var(WORK_ENV).expect("the harness names its work directory"));

    // Prove the sandbox before creating anything: `$TMUX` gone, and the first
    // `tmux` on PATH a shim aimed at this run's private socket.
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

    // The engine stub records the argv it was handed, then reads exactly the
    // line typed at it, then stays alive so the session is observable like any
    // live worker's.
    let argv = work.join("engine-argv");
    let typed = work.join("engine-typed");
    let engine = write_executable(
        &work.join("engine.sh"),
        &format!(
            "#!/bin/sh
printf '%s\\n' \"$@\" >'{argv}.part' && mv '{argv}.part' '{argv}'
IFS= read -r line
printf '%s\\n' \"$line\" >'{typed}.part' && mv '{typed}.part' '{typed}'
sleep 300
",
            argv = argv.display(),
            typed = typed.display()
        ),
    );
    let engine = Engine {
        cmd: engine.display().to_string(),
        // A space and an apostrophe, because the words are quoted into one
        // shell command before tmux ever sees them.
        args: vec!["a b".to_owned(), "it's".to_owned()],
    };

    let host = Host::new(
        work.clone(),
        &alderd::decide::config_for(&[("claude", "claude")]),
    );

    assert!(
        !host
            .tmux_session_exists(&session)
            .expect("has-session answers on an empty server"),
        "the sandbox server already holds `{session}`"
    );
    assert!(
        alderd::spawn::SpawnHost::tmux_session(&host, &session)
            .expect("a missing session is observable")
            .is_none(),
        "a missing session is not a session with unknown identity"
    );
    host.tmux_new_session(&session, &engine)
        .expect("the session is created");
    assert!(
        host.tmux_session_exists(&session)
            .expect("has-session answers"),
        "the session tmux just created is not there"
    );

    // What the pane is really running: the engine itself, nothing wrapped
    // around it, one word per argument however it was spelled.
    assert_eq!(lines(&await_file(&argv)), vec!["a b", "it's"]);
    let observed = alderd::spawn::SpawnHost::tmux_session(&host, &session)
        .expect("the dispatch host observes its own session")
        .expect("the session exists");
    assert_eq!(observed.attempt_id, None);
    assert!(
        observed.engine_live,
        "an unmarked pane running the engine is not its holding shell"
    );

    // Nothing is attached to a session started detached.
    assert!(
        !host
            .tmux_has_clients(&session)
            .expect("list-clients answers"),
        "a detached session reported a client"
    );

    // The literal text and the Enter are sent separately, so a word that names
    // a key arrives as text and nothing else.
    let line = "ready al-1 Enter C-c -- 'quoted'";
    host.tmux_send_keys(&session, line)
        .expect("the keys are sent");
    assert_eq!(await_file(&typed), format!("{line}\n"));

    // A client exists only if something attaches, so the sandbox attaches one:
    // a second session on the same private server whose only job is to run
    // `tmux attach` at the first. The shim unsets $TMUX for it, so tmux does
    // not refuse to nest.
    let attached = Command::new("tmux")
        .args(["new-session", "-d", "-s", &watcher])
        .arg(format!("tmux attach -t '={session}'"))
        .status()
        .expect("tmux runs");
    assert!(attached.success(), "the client session was not created");
    await_true(
        || {
            host.tmux_has_clients(&session)
                .expect("list-clients answers")
        },
        "a client shows up on the session",
    );
    host.tmux_kill_session(&watcher)
        .expect("the client session is killed");
    await_true(
        || {
            !host
                .tmux_has_clients(&session)
                .expect("list-clients answers")
        },
        "the client goes away again",
    );

    // Failures tmux reports are carried, with its own words attached. What
    // those words are is tmux's business and varies by version, so what is
    // checked is that they arrive at all.
    let duplicate = host
        .tmux_new_session(&session, &engine)
        .expect_err("a duplicate session name fails");
    assert!(
        explains(&duplicate.message, "tmux new-session failed: "),
        "{duplicate}"
    );
    let nowhere = host
        .tmux_send_keys("no-such-session", "hello")
        .expect_err("keys cannot be sent to a session that is not there");
    assert!(
        explains(&nowhere.message, "tmux send-keys failed: "),
        "{nowhere}"
    );

    // Killing what is not there is deliberately not an error: the driver kills
    // to be sure, not because it knows.
    host.tmux_kill_session("no-such-session")
        .expect("killing nothing is fine");
    assert!(
        !host
            .tmux_session_exists("no-such-session")
            .expect("has-session answers")
    );

    alderd::spawn::SpawnHost::tmux_kill_session(&host, &session).expect("the session is killed");
    await_true(
        || {
            !host
                .tmux_session_exists(&session)
                .expect("has-session answers")
        },
        "the session goes away",
    );
    assert!(
        alderd::spawn::SpawnHost::tmux_session(&host, &session)
            .expect("the removed session is observable")
            .is_none(),
        "the spawn host agrees that the session is gone"
    );

    fs::write(work.join("done"), "ok").expect("the marker is written");
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

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/alderd sits two levels below the workspace root")
        .to_path_buf()
}

/// The real server's socket, so this run can prove it left it alone. `None`
/// when there is no outer server to compare against.
fn real_socket() -> Option<PathBuf> {
    let value = env::var("TMUX").ok()?;
    let socket = value.split(',').next().unwrap_or_default();
    (!socket.is_empty()).then(|| PathBuf::from(socket))
}

/// Read a server's session list, read-only; empty if there is no server.
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
    lines(&String::from_utf8_lossy(&output.stdout))
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn real_sessions(tmux: &Path, socket: Option<&Path>) -> Vec<String> {
    socket
        .map(|socket| sessions_on(tmux, socket))
        .unwrap_or_default()
}

fn kill_exactly(tmux: &Path, socket: &Path, session: &str) {
    let _ = Command::new(tmux)
        .arg("-S")
        .arg(socket)
        .args(["kill-session", "-t"])
        .arg(format!("={session}"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output();
}

/// Run the project's own `sandbox_teardown` and report what it did.
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
        .arg("alderd-host-teardown")
        .arg(script)
        .env("REAL_TMUX", real_tmux)
        .env("SOCK", socket)
        .env("SOCKDIR", sockets)
        .env("SESSION_NAME", session)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("the shared teardown runs");
    let complaint = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "the teardown failed: {complaint}");
    let status = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert_ne!(
        status, "aborted",
        "the sandbox server held sessions this run does not own: {complaint}"
    );
    status
}

/// Whether a failure both names the call and carries what tmux said about it.
fn explains(message: &str, prefix: &str) -> bool {
    message.starts_with(prefix) && message.len() > prefix.len()
}

fn lines(text: &str) -> Vec<&str> {
    text.lines().filter(|line| !line.is_empty()).collect()
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
