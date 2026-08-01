//! One-shot command parsing is observable only at the binary boundary.
//!
//! These tests deliberately use the built binary instead of duplicating its
//! argument parsing.  In particular, an option-looking word must not quietly
//! become a work ID or provider, and one-shot commands must report their data
//! rather than merely exiting successfully.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use alderd::{limits::Limits, tier::Provider};
use chrono::Utc;
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
fn global_and_command_options_reject_unknown_option_words() {
    let root = project();

    assert_failure(
        alderd(root.path(), &["--unknown"]),
        "unknown argument `--unknown`",
    );
    assert_failure(
        alderd(root.path(), &["spawn", "--unknown"]),
        "spawn needs a work ID",
    );
    assert_failure(
        alderd(root.path(), &["limit", "codex", "--unknown"]),
        "unknown argument `--unknown`",
    );
}

#[test]
fn initialized_project_is_required_before_a_one_shot_command_runs() {
    let root = TempDir::new().expect("an empty directory");
    assert_failure(
        alderd(root.path(), &["budget"]),
        "not an initialized Alder project",
    );
}

#[test]
fn help_is_successful_and_unknown_commands_fail() {
    let root = project();
    let help = alderd(root.path(), &["--help"]);
    assert!(help.status.success(), "{}", text(&help));
    assert!(String::from_utf8_lossy(&help.stdout).contains("usage: alderd"));

    assert_failure(alderd(root.path(), &["invent"]), "unknown command `invent`");
}

#[test]
fn budget_requires_a_positive_window_and_prints_its_report() {
    let root = project();
    assert_failure(
        alderd(root.path(), &["budget", "--hours", "0"]),
        "--hours must be positive",
    );

    let output = alderd(root.path(), &["budget", "--hours", "1", "--json"]);
    assert!(output.status.success(), "{}", text(&output));
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("budget prints its JSON report");
    assert_eq!(report["schema"], "alderd.budget.v0");
    assert_eq!(report["hours"], 1);
}

#[test]
fn budget_uses_the_configured_provider_homes_and_home_fallbacks() {
    let root = project();
    let codex = root.path().join("configured-codex");
    let claude = root.path().join("configured-claude");
    let output = Command::new(env!("CARGO_BIN_EXE_alderd"))
        .args(["--root", root.path().to_str().unwrap(), "budget", "--json"])
        .env("CODEX_HOME", &codex)
        .env("CLAUDE_CONFIG_DIR", &claude)
        .output()
        .expect("alderd runs with configured homes");
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

    let home = root.path().join("fallback-home");
    let output = Command::new(env!("CARGO_BIN_EXE_alderd"))
        .args(["--root", root.path().to_str().unwrap(), "budget", "--json"])
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env("HOME", &home)
        .output()
        .expect("alderd runs with fallback home");
    assert!(output.status.success(), "{}", text(&output));
    let fallback = text(&output);
    assert!(
        fallback.contains(&format!("{}/.codex/sessions", home.display())),
        "{fallback}"
    );
    assert!(
        fallback.contains(&format!("{}/.claude/projects", home.display())),
        "{fallback}"
    );
}

#[test]
fn limit_requires_positive_minutes_and_records_the_requested_future_deadline() {
    let root = project();
    assert_failure(
        alderd(root.path(), &["limit", "codex", "--minutes", "0"]),
        "--minutes must be positive",
    );

    let before = Utc::now();
    let output = alderd(root.path(), &["limit", "codex", "--minutes", "1"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("codex is rate-limited until"));
    let limits =
        Limits::load(&root.path().join(".alder/rate-limits.json")).expect("limit state is saved");
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
fn spawn_reports_parse_failures_before_it_touches_the_host() {
    let root = project();
    assert_failure(
        alderd(root.path(), &["spawn", "al-1", "unknown-tier"]),
        "unknown tier",
    );
    assert_failure(
        alderd(root.path(), &["spawn", "al-1", "terra", "extra"]),
        "spawn takes at most two arguments",
    );
}

#[test]
fn spawn_uses_the_configured_alder_binary_when_it_reaches_the_host() {
    let root = project();
    let configured = root.path().join("configured-alder");
    fs::write(
        root.path().join(".alder/driver.json"),
        format!(
            r#"{{"alder":"{}","engines":{{"claude":{{"cmd":"claude"}}}},"passDoc":"pass.md"}}"#,
            configured.display()
        ),
    )
    .expect("the driver config is written");

    assert_failure(
        alderd(root.path(), &["spawn", "al-1"]),
        &format!("cannot run `{}`", configured.display()),
    );
}

#[test]
fn limit_clear_removes_the_saved_provider_entry() {
    let root = project();
    let path: PathBuf = root.path().join(".alder/rate-limits.json");
    let mut limits = Limits::default();
    limits.set(
        Provider::Claude,
        Utc::now() + chrono::Duration::hours(1),
        Some("test".to_owned()),
    );
    limits.save(&path).expect("the initial limit is saved");

    let output = alderd(root.path(), &["limit", "claude", "--clear"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        Limits::load(&path)
            .expect("the cleared state loads")
            .limited(Provider::Claude, Utc::now())
            .is_none()
    );
}
