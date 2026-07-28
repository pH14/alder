use std::{
    fs,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;

struct TestProject {
    _temporary: TempDir,
    work: PathBuf,
}

impl TestProject {
    fn new() -> Self {
        let temporary = TempDir::new().unwrap();
        let remote = temporary.path().join("remote.git");
        let work = temporary.path().join("work");
        git(
            temporary.path(),
            &["init", "--quiet", "--bare", path(&remote)],
        );
        git(temporary.path(), &["init", "--quiet", path(&work)]);
        git(&work, &["remote", "add", "origin", path(&remote)]);
        let project = Self {
            _temporary: temporary,
            work,
        };
        let initialized = project.success(&["init", "--prefix", "hm"]);
        assert_eq!(initialized["schema"], "alder.init.v0");
        project
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("alder").unwrap();
        command.current_dir(&self.work);
        command
    }

    fn success(&self, arguments: &[&str]) -> Value {
        let output = self
            .command()
            .args(arguments)
            .arg("--json")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stderr: {}\nstdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn failure(&self, arguments: &[&str]) -> Value {
        let output = self
            .command()
            .args(arguments)
            .arg("--json")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stderr.is_empty());
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn human(&self, arguments: &[&str]) -> String {
        let output = self.command().args(arguments).output().unwrap();
        assert!(
            output.status.success(),
            "stderr: {}\nstdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn config(&self, observers: Value) {
        let path = self.work.join(".alder/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["observers"] = observers;
        let mut bytes = serde_json::to_vec_pretty(&config).unwrap();
        bytes.push(b'\n');
        fs::write(path, bytes).unwrap();
    }
}

fn git(directory: &Path, arguments: &[&str]) {
    let output = ProcessCommand::new("git")
        .args(arguments)
        .current_dir(directory)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn path(value: &Path) -> &str {
    value.to_str().unwrap()
}

fn string(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap().to_owned()
}

#[test]
fn drive_work_and_preserve_the_completion_contract() {
    let project = TestProject::new();
    let first = project.success(&[
        "work",
        "add",
        "--title",
        "Build index",
        "--priority",
        "90",
        "--check",
        "tests:index tests pass",
        "--check",
        "review:review is approved",
    ]);
    let first_id = string(&first, "work_id");
    let second = project.success(&[
        "work",
        "add",
        "--title",
        "Validate index",
        "--requires",
        &first_id,
    ]);
    let second_id = string(&second, "work_id");
    let resource_less = project.failure(&["work", "edit", &first_id]);
    assert_eq!(resource_less["code"], "validation_failed");
    assert_eq!(
        resource_less["message"],
        "work edit requires at least one field change"
    );

    let next = project.success(&["next"]);
    assert_eq!(next["work"].as_array().unwrap().len(), 1);
    assert_eq!(next["work"][0]["id"], first_id);

    let started = project.success(&["work", "start", &first_id, "--meta", "engine=opus-5"]);
    let attempt = string(&started, "attempt_id");
    assert!(attempt.ends_with("-attempt-1"));
    let in_flight = project.success(&["status"]);
    assert_eq!(in_flight["in_flight"][0]["id"], attempt);
    assert!(in_flight["ready"].as_array().unwrap().is_empty());
    let duplicate = project.failure(&["work", "start", &first_id]);
    assert_eq!(duplicate["code"], "active_attempt");
    assert_eq!(duplicate["context"]["active_attempt_id"], attempt);

    let incomplete = project.failure(&["work", "finish", &first_id, "--attempt", &attempt]);
    assert_eq!(incomplete["code"], "incomplete_checks");
    project.success(&[
        "attempt",
        "edit",
        &attempt,
        "--satisfied",
        "tests",
        "--evidence",
        "CI 42",
    ]);
    project.success(&[
        "attempt",
        "edit",
        &attempt,
        "--satisfied",
        "review",
        "--evidence",
        "review 17",
    ]);
    project.success(&["work", "finish", &first_id, "--attempt", &attempt]);

    let next = project.success(&["next"]);
    assert_eq!(next["work"][0]["id"], second_id);
    let shown = project.success(&["show", &first_id]);
    let event_types: Vec<_> = shown["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert!(event_types.contains(&"attempt.started"));
    assert!(event_types.contains(&"attempt.updated"));
    assert!(event_types.contains(&"work.finished"));

    let query = project.success(&["debug", "query", "SELECT count(*) AS n FROM work_current"]);
    assert_eq!(query["result"]["rows"][0]["n"], 2);

    let asked = project.success(&["work", "ask", &second_id, "Ship now?"]);
    let question = string(&asked, "question_id");
    let waiting = project.success(&["status"]);
    assert_eq!(waiting["waiting_on_human"][0]["id"], question);
    assert!(
        !project
            .human(&["status"])
            .contains("answered questions still blocked")
    );
    project.success(&["question", "answer", &question, "Wait"]);
    project.success(&["question", "answer", &question, "Ship"]);
    let status = project.success(&["status"]);
    assert!(status["waiting_on_human"].as_array().unwrap().is_empty());
    assert_eq!(status["questions"][0]["answer"], "Ship");
    assert_eq!(
        status["questions"][0]["answers"].as_array().unwrap().len(),
        2
    );
    assert_eq!(status["blocked"][0]["id"], second_id);
    assert!(
        project
            .human(&["status"])
            .contains("answered questions still blocked")
    );

    let handoff = project.success(&[
        "handoff",
        "add",
        "--title",
        "Side work",
        "--ref",
        "branch:side",
    ]);
    let status = project.success(&["status"]);
    assert_eq!(status["handoffs"][0]["id"], handoff["handoff_id"]);
}

#[test]
fn graph_changes_are_hypothetical_then_atomic() {
    let project = TestProject::new();
    let root = project.success(&["work", "add", "--title", "Root"]);
    let root_id = string(&root, "work_id");
    let base_head = root["head"].as_u64().unwrap();
    let document = project.work.join("change.json");
    fs::write(
        &document,
        serde_json::to_vec_pretty(&json!({
            "why": "split validation",
            "add": [
                {"local": "build", "title": "Build", "priority": 90},
                {"local": "validate", "title": "Validate", "requires": ["$build"]}
            ],
            "edit": [
                {"id": root_id, "add_requires": ["$validate"]}
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let preview = project.success(&["next", "--with", path(&document)]);
    assert_eq!(preview["hypothetical"], true);
    assert_eq!(preview["head"], base_head);
    assert_eq!(preview["work"][0]["id"], "$build");
    let head = project.success(&["debug", "log", "head"]);
    assert_eq!(head["head"], base_head);
    let query = project.success(&["debug", "query", "SELECT count(*) AS n FROM work_current"]);
    assert_eq!(query["result"]["rows"][0]["n"], 1);

    let applied = project.success(&["work", "edit", "--from", path(&document)]);
    assert_eq!(applied["head"], base_head + 1);
    assert_eq!(applied["added"].as_array().unwrap().len(), 2);
    let log = project.success(&["debug", "log", "tail"]);
    assert_eq!(log["events"].as_array().unwrap().len(), 2);
    assert_eq!(log["events"][1]["type"], "work.changed");

    let additions_only = project.work.join("additions.json");
    fs::write(
        &additions_only,
        serde_json::to_vec(&json!({"add": [{"local": "extra", "title": "Extra"}]})).unwrap(),
    )
    .unwrap();
    let added = project.success(&["work", "add", "--from", path(&additions_only)]);
    assert_eq!(added["work"].as_array().unwrap().len(), 1);
    let wrong_surface = project.failure(&["work", "edit", "--from", path(&additions_only)]);
    assert_eq!(wrong_surface["code"], "validation_failed");
}

#[test]
fn observations_distinguish_presence_outage_and_missing_configuration() {
    let project = TestProject::new();
    let work = project.success(&["work", "add", "--title", "Observed work"]);
    let work_id = string(&work, "work_id");
    let started = project.success(&["work", "start", &work_id]);
    let attempt = string(&started, "attempt_id");
    let handle = "tmux:worker";
    project.success(&["attempt", "edit", &attempt, "--handle", handle]);

    let command = format!(
        "printf '%s\\n' '[{{\"value\":\"worker\",\"attempt_id\":\"{attempt}\",\"metadata\":{{\"state\":\"running\"}}}}]'"
    );
    project.config(json!([{"observer": "tmux", "list": command}]));
    let refreshed = project.success(&["refresh"]);
    assert_eq!(refreshed["result"]["present"], 1);
    assert!(project.human(&["status"]).contains("tmux:worker  present"));
    let diagnostics = project.success(&["debug", "observations", "tmux"]);
    assert_eq!(diagnostics["kinds"][0]["configured"], true);
    assert_eq!(diagnostics["kinds"][0]["command"], command);
    assert_eq!(diagnostics["kinds"][0]["latest_run"]["kind"], "tmux");
    let reconciled = project.success(&["reconcile"]);
    assert_eq!(reconciled["refreshed"], true);
    assert!(reconciled["refresh_result"].is_object());
    let healthy = project.success(&["reconcile", "--no-refresh"]);
    assert_eq!(healthy["refreshed"], false);
    assert!(healthy["refresh_result"].is_null());
    assert!(healthy["findings"].as_array().unwrap().is_empty());

    project.config(json!([]));
    let unconfigured = project.success(&["status"]);
    assert_eq!(
        unconfigured["observations"]["handles"][0]["status"],
        "unknown"
    );

    project.config(json!([{"observer": "tmux", "list": "exit 7"}]));
    let failed = project.success(&["refresh"]);
    assert_eq!(failed["result"]["unknown"], 1);
    assert_eq!(
        failed["result"]["runs"][0]["executions"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    let outage = project.success(&["reconcile", "--no-refresh"]);
    assert_eq!(outage["findings"][0]["status"], "unknown");
    assert!(outage["findings"][0]["suggested_command"].is_null());
    assert!(
        project
            .human(&["status"])
            .contains("observation failures: tmux")
    );

    project.config(json!([]));
    let status = project.success(&["status"]);
    assert_eq!(status["attention"][0]["kind"], "unconfigured");
    assert!(status["attention"][0]["suggested_command"].is_null());
    assert_eq!(status["observations"]["handles"][0]["status"], "unknown");
}

#[test]
fn initialization_is_byte_preserving_and_conflicts_are_structured() {
    let project = TestProject::new();
    project.config(json!([{"observer": "tmux", "list": "printf '[]'"}]));
    let manifest = project.work.join(".alder/config.json");
    let before = fs::read(&manifest).unwrap();
    let repeated = project.success(&["init", "--prefix", "hm"]);
    assert_eq!(repeated["status"], "already_initialized");
    assert!(
        project
            .human(&["init", "--prefix", "hm"])
            .starts_with("already initialized ")
    );
    assert_eq!(fs::read(&manifest).unwrap(), before);

    let conflict = project.failure(&["init", "--prefix", "other"]);
    assert_eq!(conflict["code"], "config_conflict");
    assert_eq!(fs::read(&manifest).unwrap(), before);
}

#[test]
fn refresh_reports_unbound_observed_handles_in_both_outputs() {
    let project = TestProject::new();
    project.config(json!([{
        "observer": "tmux",
        "list": "printf '%s\\n' '[{\"value\":\"stray\"}]'"
    }]));
    assert!(
        project
            .human(&["status"])
            .contains("observations not refreshed")
    );
    let refreshed = project.success(&["refresh"]);
    assert_eq!(refreshed["result"]["present"], 1);
    assert_eq!(refreshed["result"]["unbound"][0]["handle"], "tmux:stray");
    let human = project.human(&["refresh"]);
    assert!(human.contains("unbound:"));
    assert!(human.contains("tmux:stray"));
}

#[test]
fn unavailable_remote_is_not_replaced_by_local_git_or_sqlite_state() {
    let project = TestProject::new();
    let added = project.success(&["work", "add", "--title", "Cached work"]);
    let revision = string(&added, "revision");
    project.success(&["status"]);

    assert!(project.work.join(".alder/state.db").is_file());
    let commit = format!("{revision}^{{commit}}");
    git(&project.work, &["cat-file", "-e", &commit]);

    let unavailable = project._temporary.path().join("unavailable.git");
    git(
        &project.work,
        &["remote", "set-url", "origin", path(&unavailable)],
    );
    let failure = project.failure(&["status"]);

    assert_eq!(failure["code"], "store_unavailable");
}

#[test]
fn debug_log_human_output_uses_one_consistent_header() {
    let project = TestProject::new();
    let added = project.success(&["work", "add", "--title", "Inspect output"]);
    let revision = string(&added, "revision");
    let event_id = string(&added, "event_id");
    let header = format!("head 1  {revision}");

    assert_eq!(
        project.human(&["debug", "log", "head"]),
        format!("{header}\n")
    );
    assert_eq!(
        project.human(&["debug", "log", "tail"]),
        format!("{header}\n#1  work.changed  {event_id}\n")
    );
}

#[test]
fn invalid_commands_and_errors_use_the_requested_output_channel() {
    let mut json_command = Command::cargo_bin("alder").unwrap();
    let json_output = json_command
        .args(["not-a-command", "--json"])
        .output()
        .unwrap();
    assert_eq!(json_output.status.code(), Some(2));
    assert!(json_output.stderr.is_empty());
    let json_error: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(json_error["code"], "invalid_command");

    let mut human_command = Command::cargo_bin("alder").unwrap();
    let human_output = human_command.args(["not-a-command"]).output().unwrap();
    assert_eq!(human_output.status.code(), Some(2));
    assert!(human_output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&human_output.stderr).contains("Usage:"));

    let project = TestProject::new();
    let validation = project
        .command()
        .args(["work", "finish", "missing", "--evidence", "proof"])
        .output()
        .unwrap();
    assert_eq!(validation.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(validation.stderr).unwrap(),
        "error [validation_failed]: --evidence is accepted only with --external\n"
    );

    let contextual = project
        .command()
        .args(["show", "missing"])
        .output()
        .unwrap();
    let stderr = String::from_utf8(contextual.stderr).unwrap();
    assert!(stderr.contains("error [not_found]"));
    assert!(stderr.contains("\"kind\": \"object\""));
    assert!(stderr.contains("\"id\": \"missing\""));
}

#[test]
fn mutually_exclusive_edit_and_finish_arguments_are_all_rejected() {
    let project = TestProject::new();
    let added = project.success(&["work", "add", "--title", "work"]);
    let work = string(&added, "work_id");

    assert_eq!(
        project.failure(&[
            "work",
            "finish",
            &work,
            "--external",
            "--attempt",
            "not-an-attempt",
            "--evidence",
            "proof",
        ])["code"],
        "validation_failed"
    );
    assert_eq!(
        project.failure(&["work", "finish", &work, "--evidence", "proof"])["code"],
        "validation_failed"
    );

    let document = project.work.join("edit.json");
    fs::write(
        &document,
        serde_json::to_vec(&json!({
            "why": "change",
            "edit": [{"id": work, "title": "changed"}]
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        project.failure(&["work", "edit", &work, "--from", path(&document)])["code"],
        "validation_failed"
    );
    assert_eq!(
        project.failure(&[
            "work",
            "edit",
            "--from",
            path(&document),
            "--title",
            "changed",
        ])["code"],
        "validation_failed"
    );
    project.success(&["work", "edit", &work, "--spec", "one", "--why", "set spec"]);
    project.success(&["work", "edit", &work, "--clear-spec", "--why", "clear spec"]);
    assert!(project.success(&["show", &work])["current"]["spec"].is_null());
    let conflicting_spec = project.failure(&[
        "work",
        "edit",
        &work,
        "--title",
        "changed",
        "--spec",
        "one",
        "--clear-spec",
        "--why",
        "reason",
    ]);
    assert_eq!(conflicting_spec["code"], "validation_failed");
    assert_eq!(
        conflicting_spec["message"],
        "--spec and --clear-spec cannot be combined"
    );
    let blank_why = project.failure(&["work", "edit", &work, "--title", "changed", "--why", " "]);
    assert_eq!(blank_why["code"], "validation_failed");
    assert_eq!(blank_why["message"], "work edit requires --why");

    // `edit` never changes state, so the state fields are gone from both the
    // flags and the graph-change document.
    let state_document = project.work.join("state.json");
    fs::write(
        &state_document,
        serde_json::to_vec(&json!({
            "why": "block it",
            "edit": [{"id": work, "block": true}]
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        project.failure(&["work", "edit", "--from", path(&state_document)])["code"],
        "invalid_json"
    );

    let started = project.success(&["work", "start", &work]);
    let attempt = string(&started, "attempt_id");
    for fields in [
        vec!["--satisfied", "test", "--evidence", "proof"],
        vec!["--failed", "test", "--evidence", "proof"],
        vec!["--evidence", "proof"],
        vec!["--note", "working"],
    ] {
        let mut args = vec!["attempt", "edit", attempt.as_str(), "--handle", "tmux:one"];
        args.extend(fields);
        assert_eq!(project.failure(&args)["code"], "validation_failed");
    }
    assert_eq!(
        project.failure(&["attempt", "edit", &attempt])["code"],
        "validation_failed"
    );
    assert_eq!(
        project.failure(&["attempt", "edit", &attempt, "--satisfied", "test"])["message"],
        "--satisfied and --failed require --evidence"
    );
    assert_eq!(
        project.failure(&["attempt", "edit", &attempt, "--evidence", "proof"])["message"],
        "--evidence is accepted only with --satisfied or --failed"
    );
    let blank_end_why = project.failure(&[
        "attempt",
        "end",
        &attempt,
        "--outcome",
        "failed",
        "--why",
        " ",
    ]);
    assert_eq!(blank_end_why["code"], "validation_failed");
    assert_eq!(blank_end_why["message"], "--why cannot be empty");
    let ended = project.success(&[
        "attempt",
        "end",
        &attempt,
        "--outcome",
        "failed",
        "--why",
        "worker exited",
    ]);
    assert_eq!(ended["schema"], "alder.attempt.end.v0");
    assert_eq!(ended["outcome"], "failed");
    assert_eq!(
        project.failure(&["attempt", "edit", &attempt, "--note", "late",])["code"],
        "attempt_ended"
    );
}

#[test]
fn work_state_verbs_replace_the_removed_edit_flags() {
    let project = TestProject::new();
    let work = string(
        &project.success(&["work", "add", "--title", "Blockable"]),
        "work_id",
    );

    let blocked = project.success(&["work", "block", &work, "--why", "credentials missing"]);
    assert_eq!(blocked["schema"], "alder.work.block.v0");
    assert_eq!(
        project.success(&["show", &work])["current"]["state"],
        "blocked"
    );
    assert!(
        project.success(&["next"])["work"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let question = string(
        &project.success(&["work", "ask", &work, "Which credentials?"]),
        "question_id",
    );
    assert_eq!(
        project.failure(&["work", "unblock", &work, "--why", "guessing"])["code"],
        "unanswered_question"
    );
    project.success(&["question", "answer", &question, "the release pair"]);
    let unblocked = project.success(&["work", "unblock", &work, "--why", "credentials installed"]);
    assert_eq!(unblocked["schema"], "alder.work.unblock.v0");
    assert_eq!(
        project.success(&["show", &work])["current"]["state"],
        "open"
    );
    assert_eq!(
        project.failure(&["work", "block", "hm-missing", "--why", "reason"])["code"],
        "not_found"
    );

    let attempt = string(&project.success(&["work", "start", &work]), "attempt_id");
    let dropped = project.success(&[
        "work",
        "drop",
        &work,
        "--attempt",
        &attempt,
        "--outcome",
        "cancelled",
        "--why",
        "approach cannot work",
    ]);
    assert_eq!(dropped["schema"], "alder.work.drop.v0");
    let reopened = project.success(&["work", "reopen", &work, "--why", "requirement stands"]);
    assert_eq!(reopened["schema"], "alder.work.reopen.v0");
    assert_eq!(
        project.success(&["show", &work])["current"]["state"],
        "open"
    );
}

#[test]
fn initialization_validates_each_store_field_and_reports_the_remote_head() {
    let project = TestProject::new();
    assert_eq!(
        project.failure(&["init", "--prefix", "hm", "--remote", ""])["code"],
        "validation_failed"
    );
    assert_eq!(
        project.failure(&["init", "--prefix", "hm", "--ref", ""])["code"],
        "validation_failed"
    );
    assert_eq!(
        project.failure(&["init", "--prefix", "hm", "--remote", "other"])["code"],
        "config_conflict"
    );
    assert_eq!(
        project.failure(&["init", "--prefix", "hm", "--ref", "refs/heads/other",])["code"],
        "config_conflict"
    );

    project.success(&["work", "add", "--title", "one"]);
    project.success(&["work", "add", "--title", "two"]);
    let initialized = project.success(&["init", "--prefix", "hm"]);
    assert_eq!(initialized["status"], "already_initialized");
    assert_eq!(initialized["head"], 2);
}

#[test]
fn actor_overrides_and_object_history_are_preserved() {
    let project = TestProject::new();
    let output = project
        .command()
        .env("ALDER_ACTOR", "side-channel")
        .args(["work", "add", "--title", "one", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let added: Value = serde_json::from_slice(&output.stdout).unwrap();
    let first = string(&added, "work_id");
    let second = project.success(&["work", "add", "--title", "two"]);
    let second = string(&second, "work_id");
    let first_question = project.success(&["work", "ask", &first, "first question"]);
    let first_question = string(&first_question, "question_id");
    project.success(&["work", "ask", &second, "second question"]);

    let shown = project.success(&["show", &first]);
    assert_eq!(shown["history"][0]["actor"], "side-channel");
    let ids: Vec<_> = shown["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    let question = project.success(&["show", &first_question]);
    assert_eq!(question["kind"], "question");
}

#[test]
fn hypothetical_ordinals_and_debug_selection_are_exact() {
    let project = TestProject::new();
    project.success(&["work", "add", "--title", "root"]);
    let document = project.work.join("anonymous.json");
    fs::write(
        &document,
        serde_json::to_vec(&json!({
            "add": [{"title": "one"}, {"title": "two"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let preview = project.success(&["next", "--with", path(&document)]);
    let hypothetical: Vec<_> = preview["work"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|work| work["id"].as_str().unwrap().starts_with('$'))
        .collect();
    assert_eq!(hypothetical[0]["id"], "$new-1");
    assert_eq!(hypothetical[1]["id"], "$new-2");
    assert_eq!(hypothetical[0]["opened_seq"], 2);

    let first = project.success(&["debug", "log", "show", "1"]);
    assert_eq!(first["event"]["seq"], 1);
    assert_eq!(
        project.failure(&["debug", "log", "show", "2"])["code"],
        "not_found"
    );

    project.config(json!([
        {"observer": "tmux", "list": "printf '[]'"},
        {"observer": "nimbus", "list": "printf '[]'"}
    ]));
    let selected = project.success(&["debug", "observations", "tmux"]);
    assert_eq!(selected["kinds"].as_array().unwrap().len(), 1);
    assert_eq!(selected["kinds"][0]["kind"], "tmux");
    let run = project.success(&["debug", "observations", "tmux", "--run"]);
    assert_eq!(run["kind"], "tmux");
    assert_eq!(run["result"]["kind"], "tmux");
    assert_eq!(
        project.failure(&["debug", "observations", "missing"])["code"],
        "not_found"
    );
    assert_eq!(
        project.failure(&["debug", "observations", "missing", "--run"])["code"],
        "observer_unconfigured"
    );
}

#[test]
fn the_loop_records_passes_and_folds_its_desired_state() {
    let project = TestProject::new();
    let empty = project.success(&["status"]);
    assert_eq!(empty["loop"]["paused"], false);
    assert_eq!(empty["loop"]["rotate_pending"], false);
    assert!(empty["loop"]["engine"].is_null());
    assert!(empty["loop"]["open_pass"].is_null());
    assert!(empty["loop"]["last_pass"].is_null());
    assert!(!project.human(&["status"]).contains("\nloop\n"));

    project.success(&["loop", "use", "claude"]);
    let paused = project.success(&["loop", "pause", "--why", "release freeze"]);
    assert_eq!(paused["schema"], "alder.loop.pause.v0");
    let status = project.success(&["status"]);
    assert_eq!(status["loop"]["paused"], true);
    assert_eq!(status["loop"]["pause_reason"], "release freeze");
    assert_eq!(status["loop"]["engine"], "claude");
    assert!(
        project
            .human(&["status"])
            .contains("paused · release freeze · engine claude")
    );
    project.success(&["loop", "resume"]);
    assert_eq!(project.success(&["status"])["loop"]["paused"], false);

    let woke = project.success(&[
        "loop",
        "wake",
        "--engine",
        "claude",
        "--handle",
        "tmux:alder-leader",
        "--trigger",
        "log",
        "--trigger",
        "due",
    ]);
    assert_eq!(woke["schema"], "alder.loop.wake.v0");
    let pass = string(&woke, "pass_id");
    assert_eq!(pass, "hm-pass-1");
    // Triggers fold into a canonical order rather than command-line order.
    assert_eq!(woke["triggers"], json!(["log", "due"]));

    let open = project.success(&["status"])["loop"]["open_pass"].clone();
    assert_eq!(open["id"], pass);
    assert_eq!(open["engine"], "claude");
    assert_eq!(open["handle"], "tmux:alder-leader");
    assert_eq!(open["at_head"], 3);

    // Passes are serialized, exactly like one active attempt per work item.
    let conflict = project.failure(&[
        "loop",
        "wake",
        "--engine",
        "claude",
        "--handle",
        "tmux:alder-leader",
    ]);
    assert_eq!(conflict["code"], "pass_open");
    assert_eq!(conflict["context"]["pass_id"], pass);

    let ended = project.success(&[
        "pass",
        "end",
        "--outcome",
        "ok",
        "--report",
        "started hm-9a1\nsecond line",
        "--wake",
        "20m",
    ]);
    assert_eq!(ended["schema"], "alder.pass.end.v0");
    assert_eq!(ended["pass_id"], pass);
    assert_eq!(ended["outcome"], "ok");
    let last = project.success(&["status"])["loop"]["last_pass"].clone();
    assert_eq!(last["id"], pass);
    assert_eq!(last["outcome"], "ok");
    assert_eq!(last["report_line"], "started hm-9a1");
    assert!(last["wake_at"].is_string());
    // The head at which the pass ended is the driver's log-trigger baseline:
    // it equals the current head until someone else appends.
    assert_eq!(last["ended_seq"], project.success(&["status"])["head"]);
    assert!(
        project
            .human(&["status"])
            .contains("last hm-pass-1  ok  started hm-9a1")
    );

    assert_eq!(
        project.failure(&["pass", "end", "--outcome", "ok"])["code"],
        "no_open_pass"
    );
    assert_eq!(
        project.failure(&["pass", "end", &pass, "--outcome", "ok"])["code"],
        "pass_ended"
    );
    assert_eq!(
        project.failure(&["pass", "end", "hm-pass-9", "--outcome", "ok"])["code"],
        "not_found"
    );
    assert_eq!(
        project.failure(&[
            "loop",
            "wake",
            "--engine",
            "claude",
            "--handle",
            "tmux:alder-leader",
            "--wake",
            "20x",
        ])["code"],
        "invalid_command"
    );

    let shown = project.success(&["show", &pass]);
    assert_eq!(shown["kind"], "pass");
    assert_eq!(shown["current"]["engine"], "claude");
    assert_eq!(shown["current"]["triggers"], json!(["log", "due"]));
    let types: Vec<_> = shown["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["pass.started", "pass.ended"]);

    let rows = project.success(&[
        "debug",
        "query",
        "SELECT id, engine, state, outcome FROM passes ORDER BY started_seq",
    ]);
    assert_eq!(rows["result"]["rows"][0]["id"], pass);
    assert_eq!(rows["result"]["rows"][0]["state"], "ended");
    let control = project.success(&["debug", "query", "SELECT * FROM loop_control"]);
    assert_eq!(control["result"]["rows"][0]["engine"], "claude");
    assert_eq!(control["result"]["rows"][0]["paused"], 0);
}

#[test]
fn a_rotation_is_pending_only_until_the_next_wake() {
    let project = TestProject::new();
    let requested = project.success(&["loop", "rotate", "--why", "engine upgraded"]);
    assert_eq!(requested["schema"], "alder.loop.rotate.v0");
    assert_eq!(project.success(&["status"])["loop"]["rotate_pending"], true);
    assert!(project.human(&["status"]).contains("rotate pending"));

    let wake = [
        "loop",
        "wake",
        "--engine",
        "codex",
        "--handle",
        "tmux:alder-leader",
    ];
    project.success(&wake);
    assert_eq!(
        project.success(&["status"])["loop"]["rotate_pending"],
        false
    );

    // `pass end --rotate` requests the next rotation without a second command.
    project.success(&["pass", "end", "--outcome", "ok", "--rotate"]);
    assert_eq!(project.success(&["status"])["loop"]["rotate_pending"], true);
    project.success(&wake);
    assert_eq!(
        project.success(&["status"])["loop"]["rotate_pending"],
        false
    );
    project.success(&[
        "pass",
        "end",
        "--outcome",
        "crashed",
        "--why",
        "engine exited",
    ]);
    assert_eq!(
        project.success(&["status"])["loop"]["rotate_pending"],
        false
    );
    assert_eq!(
        project.success(&["status"])["loop"]["last_pass"]["outcome"],
        "crashed"
    );
}

#[test]
fn refresh_reports_change_without_counting_metadata_churn() {
    let project = TestProject::new();
    let work = string(
        &project.success(&["work", "add", "--title", "Observed"]),
        "work_id",
    );
    let attempt = string(&project.success(&["work", "start", &work]), "attempt_id");
    project.success(&["attempt", "edit", &attempt, "--handle", "tmux:worker"]);

    let ticker = project.work.join("cost");
    project.config(json!([{
        "observer": "tmux",
        "list": format!(
            "n=$(cat '{path}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{path}'; \
             printf '[{{\"value\":\"worker\",\"attempt_id\":\"{attempt}\",\
             \"metadata\":{{\"estimated_cost\":%s}}}}]' \"$n\"",
            path = ticker.display(),
        )
    }]));

    let first = project.success(&["refresh"]);
    assert_eq!(first["changed"], true);
    assert_eq!(first["result"]["changed"], true);
    for _ in 0..2 {
        assert_eq!(project.success(&["refresh"])["changed"], false);
    }
    assert!(
        !project
            .human(&["refresh"])
            .contains("changed since the previous refresh")
    );

    project.config(json!([{"observer": "tmux", "list": "printf '[]'"}]));
    assert!(
        project
            .human(&["refresh"])
            .contains("changed since the previous refresh")
    );
    assert_eq!(project.success(&["refresh"])["changed"], false);
}
