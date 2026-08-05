//! The `start` machine contract at the binary boundary — the shapes scripts
//! parse, pinned so drift between the runner and its callers is caught here
//! rather than in a wedged dispatch:
//!
//! - success: exit 0, stdout is the bare handle then `tier <served>`;
//! - handle already running: exit 3, stdout exactly `handle <h>`;
//! - another operation holds the lock: exit 4;
//! - a session that cannot prove its engine exited: exit 5;
//! - `--seed` lands files in the worktree before any session exists, and
//!   refuses symlinked ground;
//! - `--from` cuts a new branch at the named ref instead of `HEAD`.
//!
//! git is real (a throwaway repository); tmux is a stub on PATH that answers
//! from environment variables and records its argv, so no server is touched.

use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;

const BRANCH: &str = "work/ct-1";
const HANDLE: &str = "alder-ext-work-ct-1";

struct Sandbox {
    temporary: TempDir,
    repo: PathBuf,
    state: PathBuf,
    prompt: PathBuf,
    path: std::ffi::OsString,
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A throwaway repository plus a stub tmux first on PATH. The stub's
/// `has-session` answers absent unless `STUB_LIVE` is set; `show-environment`
/// answers from `STUB_*` variables; everything else records and succeeds.
fn sandbox() -> Sandbox {
    let temporary = TempDir::new().unwrap();
    let repo = temporary.path().join("project");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("README.md"), "contract\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "c@x"]);
    git(&repo, &["config", "user.name", "c"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "first"]);

    let state = temporary.path().join("state");
    let prompt = temporary.path().join("prompt.txt");
    fs::write(&prompt, "do the thing").unwrap();

    let tools = temporary.path().join("tools");
    fs::create_dir_all(&tools).unwrap();
    let tmux = tools.join("tmux");
    fs::write(
        &tmux,
        r#"#!/bin/sh
case "$1" in
  has-session) [ -n "$STUB_LIVE" ] && exit 0 || exit 1 ;;
  show-environment)
    case "$4" in
      ALDER_EXT_RUNNER_HANDLE)
        [ -n "$STUB_HANDLE" ] || exit 1
        printf 'ALDER_EXT_RUNNER_HANDLE=%s\n' "$STUB_HANDLE" ;;
      ALDER_EXT_RUNNER_ENGINE)
        [ -n "$STUB_ENGINE" ] || exit 1
        printf 'ALDER_EXT_RUNNER_ENGINE=%s\n' "$STUB_ENGINE" ;;
      *) exit 1 ;;
    esac ;;
  *) exit 0 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o755)).unwrap();

    let mut path = vec![tools];
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(path).unwrap();

    Sandbox {
        temporary,
        repo,
        state,
        prompt,
        path,
    }
}

fn start(sandbox: &Sandbox, live_engine: Option<&str>, extra: &[&str]) -> Output {
    let mut arguments = vec![
        "start",
        "--repo",
        sandbox.repo.to_str().unwrap(),
        "--branch",
        BRANCH,
        "--tier",
        "luna",
        "--prompt-file",
        sandbox.prompt.to_str().unwrap(),
    ];
    arguments.extend_from_slice(extra);
    Command::new(env!("CARGO_BIN_EXE_alder-ext-runner"))
        .args(&arguments)
        .env("PATH", &sandbox.path)
        .env("STUB_LIVE", if live_engine.is_some() { "1" } else { "" })
        .env("STUB_HANDLE", HANDLE)
        .env("STUB_ENGINE", live_engine.unwrap_or(""))
        .env("ALDER_EXT_RUNNER_STATE_DIR", &sandbox.state)
        .env_remove("ALDER_EXT_RUNNER_CONFIG")
        .env_remove("ALDER_EXT_RUNNER_CMD")
        .output()
        .expect("the runner runs")
}

#[test]
fn a_successful_start_prints_the_handle_then_the_served_tier() {
    let sandbox = sandbox();
    let output = start(&sandbox, None, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        vec![HANDLE, "tier luna"],
        "stdout is the parsed contract: the handle, then `tier <served>`"
    );
    // The worktree really exists, on the requested branch.
    let worktree = sandbox.temporary.path().join(HANDLE);
    assert!(worktree.is_dir(), "no worktree was cut");
    assert_eq!(
        git(&worktree, &["symbolic-ref", "--short", "HEAD"]).trim(),
        BRANCH
    );
}

#[test]
fn seeds_are_present_in_the_worktree_and_symlinked_ground_is_refused() {
    let sandbox = sandbox();
    let source = sandbox.temporary.path().join("manifest.json");
    fs::write(&source, "{\"seeded\":true}\n").unwrap();
    let seed = format!("{}:.alder/config.json", source.display());
    let output = start(&sandbox, None, &["--seed", &seed]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let seeded = sandbox
        .temporary
        .path()
        .join(HANDLE)
        .join(".alder/config.json");
    assert_eq!(
        fs::read_to_string(&seeded).expect("the seed landed"),
        "{\"seeded\":true}\n"
    );

    // A symlinked parent inside the worktree aims the copy elsewhere; the
    // next start (replacing nothing — the stub reports no session) refuses.
    let outside = sandbox.temporary.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let alder = sandbox.temporary.path().join(HANDLE).join(".alder");
    fs::remove_dir_all(&alder).unwrap();
    symlink(&outside, &alder).unwrap();
    let refused = start(&sandbox, None, &["--seed", &seed]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("symlink"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        !outside.join("config.json").exists(),
        "the seed followed a symlink out of the worktree"
    );
}

#[test]
fn from_cuts_the_new_branch_at_the_named_ref() {
    let sandbox = sandbox();
    let base = git(&sandbox.repo, &["rev-parse", "HEAD"]).trim().to_owned();
    fs::write(sandbox.repo.join("later.txt"), "after the base\n").unwrap();
    git(&sandbox.repo, &["add", "-A"]);
    git(&sandbox.repo, &["commit", "-qm", "second"]);

    let output = start(&sandbox, None, &["--from", &base]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let worktree = sandbox.temporary.path().join(HANDLE);
    assert_eq!(
        git(&worktree, &["rev-parse", "HEAD"]).trim(),
        base,
        "--from must cut the branch at the named ref, not the repo HEAD"
    );
    assert!(
        !worktree.join("later.txt").exists(),
        "the worktree carries commits past the named ref"
    );
}

#[test]
fn an_already_running_handle_is_exit_3_with_the_adoptable_stdout_line() {
    let sandbox = sandbox();
    let output = start(&sandbox, Some("running"), &[]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // Exactly `handle <h>` — scripts adopt this line without a regex over
    // prose, so its shape is the contract.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("handle {HANDLE}\n")
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already running"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn an_unproven_engine_is_exit_5_and_nothing_is_adopted() {
    let sandbox = sandbox();
    // The session exists (STUB_LIVE) but show-environment has no engine
    // marker: a session of unknown provenance.
    let output = start(&sandbox, Some(""), &[]);
    assert_eq!(
        output.status.code(),
        Some(5),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "an unproven session printed an adoptable line: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot prove its engine exited"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_held_lock_is_exit_4() {
    let sandbox = sandbox();
    fs::create_dir_all(&sandbox.state).unwrap();
    let lock_path = sandbox.state.join(format!("start-{HANDLE}.lock"));
    let lock = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    lock.lock().unwrap();

    let output = start(&sandbox, None, &[]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("holds its lock"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(lock);
}
