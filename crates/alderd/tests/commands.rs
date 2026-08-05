//! Argument parsing is observable only at the binary boundary.
//!
//! These tests deliberately use the built binary instead of duplicating its
//! parsing. The daemon is the whole binary now — the one-shot dispatch,
//! budget and limit commands moved to `alder-ext-runner` with the execution
//! extraction — so what is checked here is that it refuses everything but
//! the loop, and refuses to start without its project and its config.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use tempfile::TempDir;

fn project() -> TempDir {
    let root = TempDir::new().expect("a project root");
    fs::create_dir(root.path().join(".alder")).expect("the Alder directory is created");
    fs::write(root.path().join(".alder/config.json"), "{}\n").expect("the project is initialized");
    root
}

fn alderd(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_alderd"))
        .arg("--root")
        .arg(root)
        .args(arguments)
        .output()
        .expect("alderd runs")
}

fn text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn assert_failure(output: Output, expected: &str) {
    assert!(
        !output.status.success(),
        "the command unexpectedly succeeded: {}",
        text(&output)
    );
    assert!(text(&output).contains(expected), "{}", text(&output));
}

#[test]
fn unknown_arguments_and_stale_subcommands_are_rejected() {
    let root = project();
    assert_failure(
        alderd(root.path(), &["--unknown"]),
        "unknown argument `--unknown`",
    );
    // The dispatch verbs left with the execution extraction; a caller still
    // typing them must hear that they are gone, not watch a daemon hang.
    for gone in ["spawn", "budget", "limit"] {
        assert_failure(
            alderd(root.path(), &[gone]),
            &format!("unknown argument `{gone}`"),
        );
    }
}

#[test]
fn initialized_project_is_required_before_the_daemon_starts() {
    let root = TempDir::new().expect("an empty directory");
    assert_failure(alderd(root.path(), &[]), "not an initialized Alder project");
}

#[test]
fn the_daemon_refuses_to_start_without_its_driver_config() {
    let root = project();
    assert_failure(alderd(root.path(), &[]), "cannot read driver config");

    // A config from before the extraction names fields that left; the daemon
    // must refuse it loudly rather than half-work.
    fs::write(
        root.path().join(".alder/driver.json"),
        r#"{"command": "true", "engines": {"claude": {"cmd": "claude"}}, "passDoc": "p"}"#,
    )
    .expect("the stale config is written");
    assert_failure(alderd(root.path(), &[]), "invalid driver config");
}

#[test]
fn help_is_successful_and_names_the_command_contract() {
    let root = project();
    let help = alderd(root.path(), &["--help"]);
    assert!(help.status.success(), "{}", text(&help));
    let printed = String::from_utf8_lossy(&help.stdout).into_owned();
    assert!(printed.contains("usage: alderd"), "{printed}");
    assert!(printed.contains("ALDERD_TRIGGERS"), "{printed}");
}
