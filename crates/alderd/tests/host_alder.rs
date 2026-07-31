//! The `alder` shell-out, and the rest of `Host` that reaches the world
//! without tmux.
//!
//! Every case here runs a real subprocess. The `alder` binary is a stub script
//! the test writes, which records how it was called and answers with whatever
//! the case needs, so what is checked is the contract the driver leans on:
//! `--json` appended, the project root as the working directory, stdin closed,
//! and each shape of failure mapped onto a `DriverError`.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use alderd::{
    config::Config,
    effects::{Effects, Host},
};
use chrono::Utc;
use serde_json::json;
use tempfile::TempDir;

/// Write an executable stub and return its path.
fn stub(directory: &Path, name: &str, body: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, body).expect("the stub is written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("the stub is executable");
    path
}

/// A config whose only interesting fields are the two shell-outs.
fn config(alder: &Path, notify: Option<&str>) -> Config {
    serde_json::from_value(json!({
        "engines": {"claude": {"cmd": "claude"}},
        "passDoc": ".agent/skills/pass/SKILL.md",
        "alder": alder.display().to_string(),
        "notify": notify,
    }))
    .expect("the generated config is valid")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// A project root holding an `alder` that prints `body` and exits with `code`.
fn project(body: &str, code: u8) -> (TempDir, Host) {
    let root = TempDir::new().expect("a project root");
    let alder = stub(
        root.path(),
        "alder",
        &format!("#!/bin/sh\n{body}\nexit {code}\n"),
    );
    let host = Host::new(root.path().to_path_buf(), &config(&alder, None));
    (root, host)
}

#[test]
fn alder_is_run_with_json_appended_in_the_project_root_and_no_stdin() {
    let root = TempDir::new().expect("a project root");
    let argv = root.path().join("argv");
    let cwd = root.path().join("cwd");
    let stdin = root.path().join("stdin");
    let alder = stub(
        root.path(),
        "alder",
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$@" >'{argv}'
pwd -P >'{cwd}'
if IFS= read -r _; then printf 'open\n' >'{stdin}'; else printf 'closed\n' >'{stdin}'; fi
printf '{{"work_id": "al-1", "state": "open"}}\n'
"#,
            argv = argv.display(),
            cwd = cwd.display(),
            stdin = stdin.display(),
        ),
    );

    let host = Host::new(root.path().to_path_buf(), &config(&alder, None));
    let document = host.alder(&["show", "al-1"]).expect("the stub answers");

    assert_eq!(document["work_id"], "al-1");
    assert_eq!(document["state"], "open");
    // `--json` is the driver's, not the caller's: it is appended to whatever
    // was asked for.
    assert_eq!(read(&argv), "show\nal-1\n--json\n");
    assert_eq!(
        fs::canonicalize(read(&cwd).trim()).expect("the printed directory exists"),
        fs::canonicalize(root.path()).expect("the root exists"),
        "alder must run in the project root, whatever the daemon's own cwd is"
    );
    // An `alder` that asks a question must see EOF rather than block a daemon
    // nobody is watching.
    assert_eq!(read(&stdin), "closed\n");
}

#[test]
fn output_that_is_not_exactly_one_json_document_is_an_error() {
    for body in [
        "printf 'not json\\n'",
        // Two documents are as unusable as none.
        "printf '{}\\n{}\\n'",
        // So is silence, even on a clean exit.
        "true",
    ] {
        let (_root, host) = project(body, 0);
        let error = host
            .alder(&["status"])
            .expect_err("only one JSON document will do");
        assert!(
            error
                .message
                .contains("`alder status` did not print one JSON document"),
            "{error}"
        );
        assert!(error.code.is_none(), "{error}");
    }
}

#[test]
fn a_failing_alder_carries_its_code_and_message_through() {
    let (_root, host) = project(
        r#"printf '{"code": "pass_open", "message": "pass `hm-pass-1` is still open"}\n'"#,
        1,
    );
    let error = host
        .alder(&["loop", "wake"])
        .expect_err("a non-zero exit is a failure");
    assert!(error.is("pass_open"), "{error}");
    assert_eq!(error.message, "pass `hm-pass-1` is still open");
}

#[test]
fn a_failing_alder_that_names_nothing_gets_placeholders() {
    let (_root, host) = project("printf '{}\\n'", 3);
    let error = host.alder(&["status"]).expect_err("exit 3 is a failure");
    assert!(error.is("unknown"), "{error}");
    assert_eq!(error.message, "the command failed");
}

#[test]
fn a_missing_alder_binary_is_an_error_not_a_panic() {
    let root = TempDir::new().expect("a project root");
    let absent = root.path().join("no-such-alder");
    let host = Host::new(root.path().to_path_buf(), &config(&absent, None));
    let error = host
        .alder(&["status"])
        .expect_err("there is no binary to run");
    assert!(
        error
            .message
            .starts_with(&format!("cannot run `{}`", absent.display())),
        "{error}"
    );
    assert!(error.code.is_none(), "{error}");
}

#[test]
fn notify_hands_the_message_to_the_configured_command() {
    let root = TempDir::new().expect("a project root");
    let notice = root.path().join("notice");
    // The command is run by /bin/sh with `alderd` as $0 and the message as $1.
    let command = format!(r#"printf '%s|%s\n' "$0" "$1" >'{}'"#, notice.display());
    let alder = root.path().join("alder");
    let host = Host::new(root.path().to_path_buf(), &config(&alder, Some(&command)));

    host.notify("the leader is not answering");
    assert_eq!(read(&notice), "alderd|the leader is not answering\n");

    // With nothing configured the message is only logged, and a notifier that
    // cannot be run is still not fatal — a broken notifier must not take the
    // daemon down with it.
    let quiet = Host::new(root.path().to_path_buf(), &config(&alder, None));
    quiet.notify("nobody is listening");
    let broken = Host::new(
        root.path().to_path_buf(),
        &config(&alder, Some("/no/such/notifier")),
    );
    broken.notify("still nobody");
}

#[test]
fn files_are_read_against_the_project_root() {
    let root = TempDir::new().expect("a project root");
    let elsewhere = TempDir::new().expect("a directory outside the project");
    fs::write(root.path().join("marker"), b"appended\n").expect("the marker is written");
    fs::write(elsewhere.path().join("outside"), b"elsewhere\n").expect("the file is written");
    let host = Host::new(root.path().to_path_buf(), &config(Path::new("alder"), None));

    assert_eq!(
        host.read_file(Path::new("marker"))
            .expect("the marker reads"),
        b"appended\n"
    );
    assert_eq!(
        host.read_file(&elsewhere.path().join("outside"))
            .expect("an absolute path is taken as it is"),
        b"elsewhere\n"
    );
    let error = host
        .read_file(Path::new("missing"))
        .expect_err("a missing file is an error");
    assert!(error.message.contains("cannot read"), "{error}");

    // The marker's mtime is a hint, so its absence is not an error.
    let mtime = host
        .file_mtime(Path::new("marker"))
        .expect("the marker has an mtime");
    assert!(
        (host.now() - mtime).num_seconds().abs() < 60,
        "the marker was written just now, but its mtime is {mtime}"
    );
    assert!(host.file_mtime(Path::new("missing")).is_none());
}

#[test]
fn the_clock_and_the_wait_are_the_real_ones() {
    let root = TempDir::new().expect("a project root");
    let host = Host::new(root.path().to_path_buf(), &config(Path::new("alder"), None));

    assert!((host.now() - Utc::now()).num_seconds().abs() < 60);
    let start = Instant::now();
    host.sleep(Duration::from_millis(50));
    assert!(start.elapsed() >= Duration::from_millis(50));
    // Logging goes to stderr and returns nothing; that it cannot panic is the
    // whole of its contract.
    host.log("the integration test was here");
}
