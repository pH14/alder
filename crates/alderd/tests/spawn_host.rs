//! `alderd spawn` against a real tmux server and a real git repository, both
//! this test's own.
//!
//! The unit tests in `spawn.rs` prove the ordering rules against a fake host.
//! This proves the parts only the world can answer: that a pane really does
//! receive the goal as one argv element, that it really does outlive a
//! one-shot engine, and that a worktree really is cut and carries `alder`.
//!
//! The sandbox is the one `host_tmux.rs` documents at length: a `tmux` shim
//! first on PATH that unsets `TMUX`/`TMUX_PANE` and hands the real tmux an
//! explicit `-S <socket>`, because inside a tmux pane — where this test
//! usually runs — `$TMUX` names the real server and `TMUX_TMPDIR` isolates
//! nothing. PATH cannot be changed in-process (alderd forbids unsafe code), so
//! the parent lays the sandbox out and re-runs this binary inside it; the
//! `#[ignore]`d child is what touches tmux.
//!
//! Teardown is scripts/tests/tmux-sandbox.sh, sourced rather than copied: one
//! session, by exact name, and only once the sandbox server is proved to hold
//! nothing else.

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use alderd::{effects::Host, spawn, tier};
use tempfile::{Builder, TempDir};

const SESSION_ENV: &str = "ALDERD_SPAWN_TEST_SESSION";
const SOCKET_ENV: &str = "ALDERD_SPAWN_TEST_SOCKET";
const WORK_ENV: &str = "ALDERD_SPAWN_TEST_WORK";
const CHILD: &str = "sandboxed_spawn_cuts_a_worktree_and_leaves_a_live_pane";
/// The work item the fake `alder` answers about.
const WORK: &str = "wv-1";

#[test]
fn spawn_runs_against_a_private_tmux_server_and_a_throwaway_repo() {
    let Some(real_tmux) = which("tmux") else {
        eprintln!("skipping {CHILD}: tmux is not installed");
        return;
    };
    if which("git").is_none() {
        eprintln!("skipping {CHILD}: git is not installed");
        return;
    }
    let teardown = workspace_root().join("scripts/tests/tmux-sandbox.sh");
    assert!(
        teardown.is_file(),
        "the shared sandbox teardown is missing: {}",
        teardown.display()
    );

    // /tmp, because a unix socket path is capped near 104 bytes.
    let sockets = temp_dir("alderd-spawn-sock");
    let socket = sockets.path().join("tmux.sock");
    let real_socket = real_socket();
    assert_ne!(
        Some(socket.as_path()),
        real_socket.as_deref(),
        "refusing to run: the sandbox socket is the real server's"
    );
    let bin = temp_dir("alderd-spawn-bin");
    let work = temp_dir("alderd-spawn-work");

    // Every tmux call the spawn makes lands on the sandbox server, and every
    // one is logged: "injects nothing via send-keys" is checked, not claimed.
    write_executable(
        &bin.path().join("tmux"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{log}'\nunset TMUX TMUX_PANE\nexec '{tmux}' -f /dev/null -S '{socket}' \"$@\"\n",
            log = work.path().join("tmux-calls.log").display(),
            tmux = real_tmux.display(),
            socket = socket.display()
        ),
    );
    // The pane is wrapped in `caffeinate -i`, which the sandbox answers with
    // one of its own so the real one is not needed.
    write_executable(
        &bin.path().join("caffeinate"),
        "#!/bin/sh\nshift\nexec \"$@\"\n",
    );
    // The log is reached only through this: alderd runs `alder`, never a
    // library. It records what it was asked and answers the four documents a
    // spawn reads.
    write_executable(
        &bin.path().join("alder"),
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >>'{log}'
case "$1 $2" in
  "show {WORK}")
    printf '%s' '{{"current":{{"id":"{WORK}","title":"Sandbox item","spec":"docs/S.md","checks":[{{"key":"k","description":"the check reaches the worker"}}]}}}}' ;;
  "status --section")
    printf '%s' '{{"in_flight":[]}}' ;;
  "work start")
    printf '%s' '{{"attempt_id":"{WORK}-attempt-1"}}' ;;
  "attempt edit"|"attempt end")
    printf '%s' '{{"ok":true}}' ;;
  *)
    printf '%s' '{{"code":"unknown","message":"the fake alder was asked for something a spawn does not ask for"}}'
    exit 1 ;;
esac
"#,
            log = work.path().join("alder-calls.log").display(),
        ),
    );

    let session = format!("alder-work-{WORK}");
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
        "the sandboxed spawn test failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
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
        "teardown reported `{status}`: the worker session outlived the test"
    );
}

/// The half that touches tmux and git.
#[test]
#[ignore = "runs inside the sandbox spawn_runs_against_a_private_tmux_server_and_a_throwaway_repo lays out"]
fn sandboxed_spawn_cuts_a_worktree_and_leaves_a_live_pane() {
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

    // A throwaway project: a git repo on main with one commit, and the
    // machine-local `.alder` a worker is given a copy of.
    let root = work.join("project");
    fs::create_dir_all(root.join(".alder")).expect("the project is created");
    fs::write(
        root.join(".alder/config.json"),
        "{\"schema\":\"alder.config.v0\"}",
    )
    .expect("the config is written");
    fs::write(root.join("README.md"), "sandbox\n").expect("a file to commit");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "v@x"]);
    git(&root, &["config", "user.name", "v"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "sandbox"]);

    // The engine stub records the argv it was handed, one line per argument,
    // and then exits. What happens after it exits is the point.
    let argv = work.join("engine-argv");
    let argc = work.join("engine-argc");
    let stub = write_executable(
        &work.join("engine.sh"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$#\" >'{argc}'\nprintf '%s\\n' \"$@\" >'{argv}.part' && mv '{argv}.part' '{argv}'\n",
            argc = argc.display(),
            argv = argv.display()
        ),
    );

    let host = Host::for_command(root.clone(), "alder".to_owned());
    let started = Instant::now();
    let spawned = spawn::spawn(
        &host,
        WORK,
        tier::tier("luna").expect("luna is a rung"),
        Some(&stub.display().to_string()),
    )
    .expect("the spawn succeeds");
    let elapsed = started.elapsed();

    assert_eq!(spawned.session, session);
    assert_eq!(spawned.branch, format!("work/{WORK}"));
    assert_eq!(spawned.worktree, work.join(format!("alder-work-{WORK}")));
    assert!(!spawned.adopted);

    // Nothing on this path sleeps or waits for an engine to boot.
    assert!(
        elapsed < Duration::from_secs(3),
        "the spawn took {elapsed:?}: something on the path is waiting"
    );

    // The goal arrives as exactly one argument, and it is the whole brief.
    let goal = await_file(&argv);
    assert_eq!(
        await_file(&argc).trim(),
        "1",
        "the goal was split into arguments"
    );
    for part in [
        WORK,
        "attempt wv-1-attempt-1",
        "Goal: Sandbox item.",
        "Spec: docs/S.md.",
        "k — the check reaches the worker",
        "cargo clippy --workspace --all-targets",
        "Read WORKER.md",
    ] {
        assert!(goal.contains(part), "the goal omits `{part}`: {goal}");
    }

    // The pane outlives the engine, so the handle stays observable and a
    // ruling can be relayed into it afterwards. The engine has written its
    // argv and exited; a pane that did not end in a shell takes the session
    // with it, and tmux is not slow about that, so settling for half a second
    // is what makes this an assertion rather than a race.
    thread::sleep(Duration::from_millis(500));
    assert!(
        session_exists(&session),
        "the session died with the engine; the pane does not end `; exec bash`"
    );

    // The worktree is real, on its own branch, carrying alder and nothing that
    // would let a worker dispatch.
    let worktree = &spawned.worktree;
    assert!(
        worktree.join("README.md").is_file(),
        "the worktree is empty"
    );
    assert!(
        worktree.join(".alder/bin/alder").is_file(),
        "the worker has no alder"
    );
    assert!(
        worktree.join(".alder/config.json").is_file(),
        "the worker has no config"
    );
    assert!(
        !worktree.join(".alder/bin/alderd").exists(),
        "the worker was given alderd: workers cannot dispatch"
    );

    // The relay back into a one-shot worker, written where its shell sits and
    // runnable as it stands.
    let resume = worktree.join(".alder/resume");
    assert!(resume.is_file(), "a codex worker has nothing to resume it");
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
        run(
            &root,
            "git",
            &["rev-parse", "--abbrev-ref", &format!("work/{WORK}")]
        )
        .trim(),
        format!("work/{WORK}")
    );

    // What alderd asked the log for, in order, and what it typed at a
    // terminal, which is nothing.
    let alder_calls = fs::read_to_string(work.join("alder-calls.log")).unwrap_or_default();
    let asked: Vec<_> = alder_calls.lines().collect();
    assert!(asked[0].starts_with(&format!("show {WORK}")), "{asked:?}");
    assert!(
        asked
            .iter()
            .any(|call| call.starts_with("status --section in_flight")),
        "{asked:?}"
    );
    assert!(
        asked
            .iter()
            .any(|call| call.starts_with(&format!("work start {WORK}"))),
        "{asked:?}"
    );
    assert!(
        asked.iter().any(|call| call.contains("attempt edit")
            && call.contains(&format!("--handle tmux:{session}"))
            && call.contains("--meta engine=gpt-5.6-luna")
            && call.contains("--meta effort=high")
            && call.contains("--meta tier=luna")),
        "the attempt was not bound with its whole tier: {asked:?}"
    );
    assert!(
        !asked.iter().any(|call| call.contains("attempt end")),
        "a successful spawn ended its own attempt: {asked:?}"
    );
    let tmux_calls = fs::read_to_string(work.join("tmux-calls.log")).unwrap_or_default();
    assert!(
        !tmux_calls.contains("send-keys"),
        "the spawn typed into the pane: {tmux_calls}"
    );
    assert!(
        tmux_calls.contains("; exec bash"),
        "the pane command does not end in a shell: {tmux_calls}"
    );

    // A second spawn at a live session is refused before it touches anything.
    let refused = spawn::spawn(
        &host,
        WORK,
        tier::tier("luna").expect("luna is a rung"),
        Some(&stub.display().to_string()),
    )
    .expect_err("a second worker is refused");
    assert!(refused.message.contains("already exists"), "{refused}");

    kill_session(&session);
    await_true(|| !session_exists(&session), "the session goes away");
    fs::write(work.join("done"), "ok").expect("the marker is written");
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
        .arg("alderd-spawn-teardown")
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
