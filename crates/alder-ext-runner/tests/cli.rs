//! Command parsing is observable only at the binary boundary.
//!
//! These tests deliberately use the built binary instead of duplicating its
//! argument parsing. In particular, an option-looking word must not quietly
//! become a handle or a provider, an unknown tier must be refused before
//! anything exists, and one-shot commands must report their data rather than
//! merely exiting successfully.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use alder_ext_runner::{limits::Limits, tier::Provider};
use chrono::Utc;
use tempfile::TempDir;

/// Run the binary with the machine-local state and config redirected into a
/// throwaway home, so no test touches the operator's real files.
fn runner(home: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_alder-ext-runner"))
        .args(arguments)
        .env("ALDER_EXT_RUNNER_STATE_DIR", home.join("state"))
        .env("ALDER_EXT_RUNNER_CONFIG", home.join("config.json"))
        .output()
        .expect("alder-ext-runner runs")
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
fn help_is_successful_and_unknown_commands_fail() {
    let home = TempDir::new().expect("a home");
    let help = runner(home.path(), &["--help"]);
    assert!(help.status.success(), "{}", text(&help));
    assert!(String::from_utf8_lossy(&help.stdout).contains("usage: alder-ext-runner"));

    assert_failure(runner(home.path(), &["invent"]), "unknown command `invent`");
}

#[test]
fn start_reports_parse_failures_before_it_touches_the_host() {
    let home = TempDir::new().expect("a home");
    let repo = TempDir::new().expect("a repo");
    let prompt = home.path().join("prompt.txt");
    fs::write(&prompt, "do the thing").unwrap();
    let repo_path = repo.path().to_str().unwrap();
    let prompt_path = prompt.to_str().unwrap();

    assert_failure(runner(home.path(), &["start"]), "start needs --repo");
    assert_failure(
        runner(home.path(), &["start", "--repo", repo_path]),
        "start needs --branch",
    );
    assert_failure(
        runner(
            home.path(),
            &["start", "--repo", repo_path, "--branch", "work/x"],
        ),
        "start needs --tier",
    );
    assert_failure(
        runner(
            home.path(),
            &[
                "start", "--repo", repo_path, "--branch", "work/x", "--tier", "terra",
            ],
        ),
        "start needs --prompt-file",
    );
    // An unknown tier is refused by name, listing the rungs, before any
    // worktree or session could exist.
    let unknown = runner(
        home.path(),
        &[
            "start",
            "--repo",
            repo_path,
            "--branch",
            "work/x",
            "--tier",
            "gpt-5.6-luna",
            "--prompt-file",
            prompt_path,
        ],
    );
    let complaint = text(&unknown);
    assert!(!unknown.status.success(), "{complaint}");
    assert!(complaint.contains("unknown tier"), "{complaint}");
    for rung in ["luna", "terra", "sol", "sonnet", "opus", "fable"] {
        assert!(complaint.contains(rung), "{complaint} omits {rung}");
    }
    assert!(
        !repo
            .path()
            .parent()
            .unwrap()
            .join("alder-ext-work-x")
            .exists(),
        "a rejected tier still cut a worktree"
    );
}

#[test]
fn a_config_file_replaces_the_tier_table_the_binary_serves() {
    let home = TempDir::new().expect("a home");
    let repo = TempDir::new().expect("a repo");
    let prompt = home.path().join("prompt.txt");
    fs::write(&prompt, "do the thing").unwrap();
    fs::write(
        home.path().join("config.json"),
        r#"{"tiers": {
            "fast": {"provider": "codex", "model": "m", "effort": "low", "counterpart": "slow"},
            "slow": {"provider": "claude", "model": "n", "effort": "high", "counterpart": "fast"}
        }}"#,
    )
    .unwrap();

    let unknown = runner(
        home.path(),
        &[
            "start",
            "--repo",
            repo.path().to_str().unwrap(),
            "--branch",
            "work/x",
            "--tier",
            "terra",
            "--prompt-file",
            prompt.to_str().unwrap(),
        ],
    );
    let complaint = text(&unknown);
    assert!(!unknown.status.success(), "{complaint}");
    assert!(
        complaint.contains("the rungs are fast, slow"),
        "the config table did not replace the built-in one: {complaint}"
    );

    // A broken config is an error, not a silent fallback to the defaults.
    fs::write(home.path().join("config.json"), "{\"tiers\": {}}").unwrap();
    assert_failure(
        runner(
            home.path(),
            &[
                "start",
                "--repo",
                repo.path().to_str().unwrap(),
                "--branch",
                "work/x",
                "--tier",
                "terra",
                "--prompt-file",
                prompt.to_str().unwrap(),
            ],
        ),
        "cannot be empty",
    );
}

#[test]
fn handle_verbs_reject_option_words_and_missing_arguments() {
    let home = TempDir::new().expect("a home");
    assert_failure(
        runner(home.path(), &["status"]),
        "status takes exactly one handle",
    );
    assert_failure(
        runner(home.path(), &["status", "--verbose"]),
        "status takes exactly one handle",
    );
    assert_failure(
        runner(home.path(), &["kill"]),
        "kill takes exactly one handle",
    );
    assert_failure(
        runner(home.path(), &["send", "some-handle"]),
        "send needs --file",
    );
    assert_failure(
        runner(home.path(), &["send", "--file", "x"]),
        "send needs a handle",
    );
    assert_failure(
        runner(home.path(), &["send", "a", "b", "--file", "x"]),
        "send takes exactly one handle",
    );
}

#[test]
fn budget_requires_a_positive_window_and_prints_its_report() {
    let home = TempDir::new().expect("a home");
    assert_failure(
        runner(home.path(), &["budget", "--hours", "0"]),
        "--hours must be positive",
    );

    let output = runner(home.path(), &["budget", "--hours", "1", "--json"]);
    assert!(output.status.success(), "{}", text(&output));
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("budget prints its JSON report");
    assert_eq!(report["schema"], "alder-ext-runner.budget.v0");
    assert_eq!(report["hours"], 1);
}

#[test]
fn budget_uses_the_configured_provider_homes_and_home_fallbacks() {
    let home = TempDir::new().expect("a home");
    let codex = home.path().join("configured-codex");
    let claude = home.path().join("configured-claude");
    let output = Command::new(env!("CARGO_BIN_EXE_alder-ext-runner"))
        .args(["budget", "--json"])
        .env("ALDER_EXT_RUNNER_STATE_DIR", home.path().join("state"))
        .env("CODEX_HOME", &codex)
        .env("CLAUDE_CONFIG_DIR", &claude)
        .output()
        .expect("the runner runs with configured homes");
    assert!(output.status.success(), "{}", text(&output));
    let configured = text(&output);
    assert!(
        configured.contains(&format!("{}/sessions", codex.display())),
        "{configured}"
    );
    assert!(
        configured.contains(&format!("{}/projects", claude.display())),
        "{configured}"
    );

    let fallback_home = home.path().join("fallback-home");
    let output = Command::new(env!("CARGO_BIN_EXE_alder-ext-runner"))
        .args(["budget", "--json"])
        .env("ALDER_EXT_RUNNER_STATE_DIR", home.path().join("state"))
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env("HOME", &fallback_home)
        .output()
        .expect("the runner runs with fallback home");
    assert!(output.status.success(), "{}", text(&output));
    let fallback = text(&output);
    assert!(
        fallback.contains(&format!("{}/.codex/sessions", fallback_home.display())),
        "{fallback}"
    );
    assert!(
        fallback.contains(&format!("{}/.claude/projects", fallback_home.display())),
        "{fallback}"
    );
}

#[test]
fn limit_requires_positive_minutes_and_records_the_requested_future_deadline() {
    let home = TempDir::new().expect("a home");
    assert_failure(
        runner(home.path(), &["limit", "codex", "--minutes", "0"]),
        "--minutes must be positive",
    );
    assert_failure(
        runner(home.path(), &["limit", "codex", "--unknown"]),
        "unknown argument `--unknown`",
    );

    let before = Utc::now();
    let output = runner(home.path(), &["limit", "codex", "--minutes", "1"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("codex is rate-limited until"));
    let limits = Limits::load(&home.path().join("state/rate-limits.json"))
        .expect("limit state is saved under the state directory");
    let until = limits
        .limited(Provider::Codex, before)
        .expect("the freshly recorded limit is still live")
        .until;
    assert!(
        until > before,
        "the deadline must be in the future: {until}"
    );
    assert!(
        until < before + chrono::Duration::minutes(2),
        "unexpected deadline: {until}"
    );
}

#[test]
fn limit_clear_removes_the_saved_provider_entry() {
    let home = TempDir::new().expect("a home");
    let path = home.path().join("state/rate-limits.json");
    let mut limits = Limits::default();
    limits.set(
        Provider::Claude,
        Utc::now() + chrono::Duration::hours(1),
        Some("test".to_owned()),
    );
    limits.save(&path).expect("the initial limit is saved");

    let output = runner(home.path(), &["limit", "claude", "--clear"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        Limits::load(&path)
            .expect("the cleared state loads")
            .limited(Provider::Claude, Utc::now())
            .is_none()
    );
}
