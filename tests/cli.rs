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
        "add",
        "work",
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
        "add",
        "work",
        "--title",
        "Validate index",
        "--requires",
        &first_id,
    ]);
    let second_id = string(&second, "work_id");
    let resource_less = project.failure(&["edit", &first_id]);
    assert_eq!(resource_less["code"], "invalid_command");

    let next = project.success(&["next"]);
    assert_eq!(next["work"].as_array().unwrap().len(), 1);
    assert_eq!(next["work"][0]["id"], first_id);

    let started = project.success(&["start", &first_id, "--meta", "engine=opus-5"]);
    let attempt = string(&started, "attempt_id");
    assert!(attempt.ends_with("-attempt-1"));
    let duplicate = project.failure(&["start", &first_id]);
    assert_eq!(duplicate["code"], "active_attempt");
    assert_eq!(duplicate["context"]["active_attempt_id"], attempt);

    let incomplete = project.failure(&["finish", &first_id, "--attempt", &attempt]);
    assert_eq!(incomplete["code"], "incomplete_checks");
    project.success(&[
        "edit",
        "attempt",
        &attempt,
        "--check",
        "tests=satisfied",
        "--evidence",
        "CI 42",
    ]);
    project.success(&[
        "edit",
        "attempt",
        &attempt,
        "--check",
        "review=satisfied",
        "--evidence",
        "review 17",
    ]);
    project.success(&["finish", &first_id, "--attempt", &attempt]);

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

    let asked = project.success(&["ask", &second_id, "Ship now?"]);
    let question = string(&asked, "question_id");
    project.success(&["answer", &question, "Wait"]);
    project.success(&["answer", &question, "Ship"]);
    let status = project.success(&["status"]);
    assert_eq!(status["questions"][0]["answer"], "Ship");
    assert_eq!(
        status["questions"][0]["answers"].as_array().unwrap().len(),
        2
    );
    assert_eq!(status["blocked"][0]["id"], second_id);
}

#[test]
fn graph_changes_are_hypothetical_then_atomic() {
    let project = TestProject::new();
    let root = project.success(&["add", "work", "--title", "Root"]);
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

    let applied = project.success(&["edit", "work", "--from", path(&document)]);
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
    let added = project.success(&["add", "work", "--from", path(&additions_only)]);
    assert_eq!(added["work"].as_array().unwrap().len(), 1);
    let wrong_surface = project.failure(&["edit", "work", "--from", path(&additions_only)]);
    assert_eq!(wrong_surface["code"], "validation_failed");
}

#[test]
fn observations_distinguish_presence_outage_and_missing_configuration() {
    let project = TestProject::new();
    let work = project.success(&["add", "work", "--title", "Observed work"]);
    let work_id = string(&work, "work_id");
    let started = project.success(&["start", &work_id]);
    let attempt = string(&started, "attempt_id");
    let handle = "tmux:worker";
    project.success(&["edit", "attempt", &attempt, "--handle", handle]);

    let command = format!(
        "printf '%s\\n' '[{{\"value\":\"worker\",\"attempt_id\":\"{attempt}\",\"metadata\":{{\"state\":\"running\"}}}}]'"
    );
    project.config(json!([{"observer": "tmux", "list": command}]));
    let refreshed = project.success(&["refresh"]);
    assert_eq!(refreshed["result"]["present"], 1);
    let healthy = project.success(&["reconcile", "--no-refresh"]);
    assert!(healthy["findings"].as_array().unwrap().is_empty());

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
    assert_eq!(fs::read(&manifest).unwrap(), before);

    let conflict = project.failure(&["init", "--prefix", "other"]);
    assert_eq!(conflict["code"], "config_conflict");
    assert_eq!(fs::read(&manifest).unwrap(), before);
}

#[test]
fn unavailable_remote_is_not_replaced_by_local_git_or_sqlite_state() {
    let project = TestProject::new();
    let added = project.success(&["add", "work", "--title", "Cached work"]);
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
    let added = project.success(&["add", "work", "--title", "Inspect output"]);
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
