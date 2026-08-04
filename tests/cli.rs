use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Output},
};

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;

struct TestProject {
    temporary: TempDir,
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
        let project = Self { temporary, work };
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

    /// Run `arguments` with a `git` that records every call, and return the
    /// output alongside those calls in order. The shim is a real program first
    /// on the child's PATH, so it sees exactly the Git processes the command
    /// spawns; it then hands off to the inherited PATH, so it counts the work
    /// without doing any.
    fn counted(&self, arguments: &[&str]) -> (Value, Vec<String>) {
        let shim = self.temporary.path().join("shim");
        let calls = self.temporary.path().join("git-calls");
        fs::create_dir_all(&shim).unwrap();
        fs::write(
            shim.join("git"),
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> \"$ALDER_GIT_CALLS\"\n\
             PATH=\"$ALDER_INHERITED_PATH\" exec git \"$@\"\n",
        )
        .unwrap();
        let mode = ProcessCommand::new("chmod")
            .args(["+x", path(&shim.join("git"))])
            .status()
            .unwrap();
        assert!(mode.success());
        let _ = fs::remove_file(&calls);

        let inherited = std::env::var("PATH").unwrap();
        let output = self
            .command()
            .args(arguments)
            .arg("--json")
            .env("PATH", format!("{}:{inherited}", path(&shim)))
            .env("ALDER_INHERITED_PATH", &inherited)
            .env("ALDER_GIT_CALLS", &calls)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stderr: {}\nstdout: {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        let recorded = fs::read_to_string(&calls)
            .unwrap_or_default()
            .lines()
            .map(ToOwned::to_owned)
            .collect();
        (serde_json::from_slice(&output.stdout).unwrap(), recorded)
    }

    /// A second working copy of the same shared log, adopted through `init`.
    /// It is a genuinely separate writer: its own repository, its own process,
    /// the same remote ref.
    fn rival(&self) -> PathBuf {
        let rival = self.temporary.path().join("rival");
        let remote = self.temporary.path().join("remote.git");
        git(self.temporary.path(), &["init", "--quiet", path(&rival)]);
        git(&rival, &["remote", "add", "origin", path(&remote)]);
        let initialized = Command::cargo_bin("alder")
            .unwrap()
            .current_dir(&rival)
            .args(["init", "--prefix", "hm", "--json"])
            .output()
            .unwrap();
        assert!(
            initialized.status.success(),
            "{}",
            String::from_utf8_lossy(&initialized.stderr)
        );
        rival
    }

    /// Run `arguments` and let a rival writer append at the exact moment this
    /// command reaches its linearization point.
    ///
    /// The shim is a real `git` first on the child's PATH. When it sees the
    /// push that publishes this command's event commit, it runs the rival's
    /// whole command to completion and only then hands the push to the real
    /// Git — which finds the ref moved and rejects it. That is the live race:
    /// a writer reads a head, validates against it, builds its commit, and
    /// loses the compare-and-append to someone who got there first.
    fn losing(&self, arguments: &[&str], rival: &Path, rival_arguments: &[&str]) -> Output {
        let shim = self.temporary.path().join("race-shim");
        fs::create_dir_all(&shim).unwrap();
        let script = self.temporary.path().join("rival.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ncd '{}' || exit 1\nexec '{}' {}\n",
                path(rival),
                path(&assert_cmd::cargo::cargo_bin("alder")),
                rival_arguments
                    .iter()
                    .map(|argument| format!("'{argument}'"))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        )
        .unwrap();
        fs::write(
            shim.join("git"),
            "#!/bin/sh\n\
             case \" $* \" in\n\
             *\" push \"*)\n\
             if [ ! -e \"$ALDER_RACE_MARKER\" ]; then\n\
             : > \"$ALDER_RACE_MARKER\"\n\
             PATH=\"$ALDER_INHERITED_PATH\" sh \"$ALDER_RACE_RIVAL\" >/dev/null 2>&1 || exit 1\n\
             fi\n\
             ;;\n\
             esac\n\
             PATH=\"$ALDER_INHERITED_PATH\" exec git \"$@\"\n",
        )
        .unwrap();
        let mode = ProcessCommand::new("chmod")
            .args(["+x", path(&shim.join("git"))])
            .status()
            .unwrap();
        assert!(mode.success());
        let marker = self.temporary.path().join("race-marker");
        let _ = fs::remove_file(&marker);

        let inherited = std::env::var("PATH").unwrap();
        let output = self
            .command()
            .args(arguments)
            .env("PATH", format!("{}:{inherited}", path(&shim)))
            .env("ALDER_INHERITED_PATH", &inherited)
            .env("ALDER_RACE_RIVAL", &script)
            .env("ALDER_RACE_MARKER", &marker)
            .output()
            .unwrap();
        assert!(
            marker.exists(),
            "the rival never ran, so nothing raced: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// Discard the record cache, so the next read has to decode event bodies
    /// again. It is derived data, so nothing else changes.
    fn forget_cached_records(&self) {
        let cache = self.work.join(".alder/cache");
        assert!(cache.is_dir(), "a read should have written {cache:?}");
        fs::remove_dir_all(cache).unwrap();
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
    assert_eq!(project.success(&["status"])["counts"]["in_flight"], 1);
    let in_flight = project.success(&["status", "--full"]);
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
    assert_eq!(
        project.success(&["status"])["counts"]["waiting_on_human"],
        1
    );
    let waiting = project.success(&["status", "--full"]);
    assert_eq!(waiting["waiting_on_human"][0]["id"], question);
    assert!(
        !project
            .human(&["status", "--full"])
            .contains("answered questions still blocked")
    );
    project.success(&["question", "answer", &question, "Wait"]);
    project.success(&["question", "answer", &question, "Ship"]);
    assert_eq!(
        project.success(&["status"])["counts"]["waiting_on_human"],
        0
    );
    assert_eq!(project.success(&["status"])["counts"]["blocked"], 1);
    let status = project.success(&["status", "--full"]);
    assert!(status["waiting_on_human"].as_array().unwrap().is_empty());
    assert_eq!(status["questions"][0]["answer"], "Ship");
    assert_eq!(
        status["questions"][0]["answers"].as_array().unwrap().len(),
        2
    );
    assert_eq!(status["blocked"][0]["id"], second_id);
    assert!(
        project
            .human(&["status", "--full"])
            .contains("answered questions still blocked")
    );
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
fn attempt_file_values_append_contents_not_local_paths() {
    let project = TestProject::new();
    let work = string(
        &project.success(&[
            "work",
            "add",
            "--title",
            "Record a reviewer finding",
            "--check",
            "review:review is recorded",
        ]),
        "work_id",
    );
    let attempt = string(&project.success(&["work", "start", &work]), "attempt_id");

    let evidence_path = project.work.join("review-findings.txt");
    let note_path = project.work.join("worker-note.txt");
    let evidence =
        "finding: literal `backtick`, $(not-a-command), and \"quotes\"\nref: work/hm-file";
    let note = "review findings recorded from the local file";
    fs::write(&evidence_path, evidence).unwrap();
    fs::write(&note_path, note).unwrap();

    let updated = project.success(&[
        "attempt",
        "edit",
        &attempt,
        "--satisfied",
        "review",
        "--evidence-file",
        path(&evidence_path),
        "--note-file",
        path(&note_path),
    ]);

    // The paths were only input to this process. Removing both files before
    // reading the event demonstrates that neither the log nor its fold
    // tries to recover their contents from a shared filesystem.
    fs::remove_file(&evidence_path).unwrap();
    fs::remove_file(&note_path).unwrap();

    let sequence = updated["head"].as_u64().unwrap().to_string();
    let event = project.success(&["debug", "log", "show", &sequence]);
    assert_eq!(event["event"]["type"], "attempt.updated");
    let payload = &event["event"]["body"];
    assert_eq!(payload["checks"][0]["evidence"], evidence);
    assert_eq!(payload["note"], note);
    assert!(payload.get("evidence_file").is_none());
    assert!(payload.get("note_file").is_none());

    let current = project.success(&["show", &attempt]);
    assert_eq!(current["current"]["checks"]["review"]["evidence"], evidence);
    assert_eq!(current["current"]["note"], note);
}

#[test]
fn blank_evidence_file_names_the_file_valued_flag() {
    let project = TestProject::new();
    let work = string(
        &project.success(&[
            "work",
            "add",
            "--title",
            "Reject blank file evidence",
            "--check",
            "review:review is recorded",
        ]),
        "work_id",
    );
    let attempt = string(&project.success(&["work", "start", &work]), "attempt_id");
    let evidence = project.work.join("blank-evidence.txt");
    fs::write(&evidence, " \n\t").unwrap();

    let failure = project.failure(&[
        "attempt",
        "edit",
        &attempt,
        "--satisfied",
        "review",
        "--evidence-file",
        path(&evidence),
    ]);
    assert_eq!(failure["code"], "validation_failed");
    assert_eq!(
        failure["message"],
        "--satisfied and --failed require --evidence-file"
    );
}

#[test]
fn refresh_keeps_levels_durable_when_configuration_changes_or_a_script_fails() {
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
    assert_eq!(refreshed["result"]["appended"], 2);
    assert!(
        project
            .human(&["status", "--full"])
            .contains("tmux:worker  present")
    );
    let diagnostics = project.success(&["debug", "observations", "tmux"]);
    assert_eq!(diagnostics["kinds"][0]["configured"], true);
    assert_eq!(diagnostics["kinds"][0]["command"], command);
    let reconciled = project.success(&["reconcile"]);
    assert_eq!(reconciled["refreshed"], true);
    assert!(reconciled["findings"].as_array().unwrap().is_empty());
    // Configuration is only how the next report is collected; removing it
    // cannot erase a belief already in the shared log.
    project.config(json!([]));
    let snapshot = project.success(&["observations"]);
    assert_eq!(
        snapshot["observations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|observation| observation["field"] == "liveness")
            .unwrap()["level"],
        "present"
    );

    project.config(json!([{"observer": "tmux", "list": "exit 7"}]));
    let failed = project.success(&["refresh"]);
    assert_eq!(failed["result"]["appended"], 0);
    assert_eq!(
        failed["result"]["runs"][0]["executions"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    let snapshot = project.success(&["observations"]);
    assert_eq!(
        snapshot["observations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|observation| observation["field"] == "liveness")
            .unwrap()["level"],
        "present"
    );
}

const STATUS_SECTIONS: [&str; 5] = [
    "attention",
    "in_flight",
    "ready",
    "waiting_on_human",
    "blocked",
];

/// One item lands in every action section. Observation configuration is not a
/// durable problem in the new model, so attention is correctly empty here.
#[test]
fn status_defaults_to_counts_and_expands_only_on_request() {
    let project = TestProject::new();

    // ready: nothing else has happened to it.
    project.success(&["work", "add", "--title", "Ready work"]);

    // in flight: a missing local observer configuration is not an attention
    // item because observations are now durable beliefs rather than local state.
    let flight_work = string(
        &project.success(&["work", "add", "--title", "In-flight work"]),
        "work_id",
    );
    let attempt = string(
        &project.success(&["work", "start", &flight_work]),
        "attempt_id",
    );
    project.success(&["attempt", "edit", &attempt, "--handle", "tmux:worker"]);

    // blocked + waiting_on_human: an unanswered question blocks its work.
    let blocked_work = string(
        &project.success(&["work", "add", "--title", "Blocked work"]),
        "work_id",
    );
    project.success(&["work", "ask", &blocked_work, "Ship now?"]);

    let counts_only = project.success(&["status"]);
    for section in STATUS_SECTIONS {
        let expected = if section == "attention" { 0 } else { 1 };
        assert_eq!(counts_only["counts"][section], expected, "{section} count");
        assert!(
            counts_only.get(section).is_none(),
            "{section} leaked into the default pack"
        );
    }
    assert!(counts_only.get("recent_events").is_none());
    assert!(counts_only.get("loop").is_some());
    assert!(counts_only.get("observations").is_some());
    assert!(counts_only.get("questions").is_some());

    let human_default = project.human(&["status"]);
    assert!(human_default.contains("\ncounts\n"), "{human_default}");
    assert!(human_default.contains("attention  0"), "{human_default}");
    assert!(human_default.contains("blocked  1"), "{human_default}");
    assert!(
        !human_default.contains(&flight_work),
        "a work id should not appear in the counts-only view"
    );

    let full = project.success(&["status", "--full"]);
    assert_eq!(full["counts"], counts_only["counts"]);
    for section in STATUS_SECTIONS {
        assert_eq!(
            full[section].as_array().unwrap().len(),
            usize::try_from(full["counts"][section].as_u64().unwrap()).unwrap(),
            "{section} count must match its expanded section length"
        );
    }
    assert!(full.get("recent_events").is_some());
    let human_full = project.human(&["status", "--full"]);
    assert!(human_full.contains(&flight_work), "{human_full}");

    for section in STATUS_SECTIONS {
        let sectioned = project.success(&["status", "--section", section]);
        assert_eq!(sectioned["counts"], counts_only["counts"]);
        assert_eq!(sectioned[section], full[section], "{section} round-trip");
        for other in STATUS_SECTIONS {
            if other != section {
                assert!(
                    sectioned.get(other).is_none(),
                    "{other} leaked under --section {section}"
                );
            }
        }
        assert!(sectioned.get("recent_events").is_none());
    }

    // Repeated sections are deduplicated and rendered in canonical order,
    // regardless of the order in which the flags were supplied.
    let selected = project.success(&[
        "status",
        "--section",
        "blocked",
        "--section",
        "ready",
        "--section",
        "blocked",
    ]);
    assert_eq!(selected["ready"], full["ready"]);
    assert_eq!(selected["blocked"], full["blocked"]);
    for other in ["attention", "in_flight", "waiting_on_human"] {
        assert!(selected.get(other).is_none(), "{other} leaked");
    }
    let selected_human = project.human(&[
        "status",
        "--section",
        "blocked",
        "--section",
        "ready",
        "--section",
        "blocked",
    ]);
    let ready_at = selected_human.find("\nready\n").unwrap();
    let blocked_at = selected_human.find("\nblocked\n").unwrap();
    assert!(ready_at < blocked_at);
    assert_eq!(selected_human.matches("\nblocked\n").count(), 1);

    // `--full` remains the all-sections view and wins when section flags are
    // also present.
    let full_with_section = project.success(&["status", "--full", "--section", "ready"]);
    assert_eq!(full_with_section, full);
}

#[test]
fn status_with_composes_with_counts() {
    let project = TestProject::new();
    project.success(&["work", "add", "--title", "Existing"]);
    let baseline = project.success(&["status"])["counts"]["ready"]
        .as_u64()
        .unwrap();

    let document = project.work.join("overlay.json");
    fs::write(
        &document,
        serde_json::to_vec(&json!({"add": [{"title": "Hypothetical"}]})).unwrap(),
    )
    .unwrap();

    let overlaid = project.success(&["status", "--with", path(&document)]);
    assert_eq!(overlaid["hypothetical"], true);
    assert_eq!(overlaid["counts"]["ready"], baseline + 1);
    assert!(overlaid.get("ready").is_none());

    let overlaid_full = project.success(&["status", "--with", path(&document), "--full"]);
    assert_eq!(overlaid_full["counts"]["ready"], baseline + 1);
    assert_eq!(
        overlaid_full["ready"].as_array().unwrap().len(),
        usize::try_from(baseline + 1).unwrap()
    );

    // Nothing was written: the plain read still sees the original count.
    assert_eq!(project.success(&["status"])["counts"]["ready"], baseline);
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
fn refresh_adapts_the_existing_handle_observer_to_durable_liveness() {
    let project = TestProject::new();
    project.config(json!([{
        "observer": "tmux",
        "list": "printf '%s\\n' '[{\"value\":\"stray\"}]'"
    }]));
    let refreshed = project.success(&["refresh"]);
    assert_eq!(refreshed["result"]["appended"], 1);
    let snapshot = project.success(&["observations"]);
    assert_eq!(snapshot["observations"][0]["observer"], "tmux");
    assert_eq!(snapshot["observations"][0]["subject"], "stray");
    assert_eq!(snapshot["observations"][0]["field"], "liveness");
    assert_eq!(snapshot["observations"][0]["level"], "present");
    let human = project.human(&["refresh"]);
    assert!(human.contains("no observation changes"));
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

    let unavailable = project.temporary.path().join("unavailable.git");
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
    // Context reaches the human channel as fields under the error line, never
    // as a JSON document — a failure must not have the shape of a result.
    assert_eq!(
        String::from_utf8(contextual.stderr).unwrap(),
        "error [not_found]: object `missing` was not found\n  id: missing\n  kind: object\n"
    );
}

#[test]
fn help_and_version_exit_successfully() {
    let mut help_command = Command::cargo_bin("alder").unwrap();
    let help_output = help_command.arg("--help").output().unwrap();
    assert_eq!(help_output.status.code(), Some(0));
    assert!(help_output.stderr.is_empty());
    assert!(String::from_utf8_lossy(&help_output.stdout).contains("Usage:"));

    let mut version_command = Command::cargo_bin("alder").unwrap();
    let version_output = version_command.arg("--version").output().unwrap();
    assert_eq!(version_output.status.code(), Some(0));
    assert!(version_output.stderr.is_empty());

    let mut json_help_command = Command::cargo_bin("alder").unwrap();
    let json_help_output = json_help_command
        .args(["--json", "--help"])
        .output()
        .unwrap();
    assert_eq!(json_help_output.status.code(), Some(0));
    assert!(json_help_output.stderr.is_empty());
    assert!(String::from_utf8_lossy(&json_help_output.stdout).contains("Usage:"));
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
    // The question was answered before the drop, so nothing is stranded.
    assert!(dropped["stranded_questions"].as_array().unwrap().is_empty());
    let reopened = project.success(&["work", "reopen", &work, "--why", "requirement stands"]);
    assert_eq!(reopened["schema"], "alder.work.reopen.v0");
    assert_eq!(
        project.success(&["show", &work])["current"]["state"],
        "open"
    );
}

#[test]
fn handoff_commands_and_work_add_flag_are_not_in_the_cli() {
    let project = TestProject::new();
    for arguments in [
        vec!["handoff", "add"],
        vec!["work", "add", "--handoff", "hm-legacy"],
        vec!["status", "--section", "handoffs"],
    ] {
        let output = project.command().args(arguments).output().unwrap();
        assert!(!output.status.success());
    }
}

#[test]
fn terminal_work_strands_its_questions_until_it_is_reopened() {
    let project = TestProject::new();
    let work = string(
        &project.success(&["work", "add", "--title", "Ship the digest"]),
        "work_id",
    );
    let question = string(
        &project.success(&["work", "ask", &work, "Masked digest, or wait for AA-6?"]),
        "question_id",
    );

    // While the work is live the question is a decision someone owes.
    assert_eq!(
        project.success(&["status"])["counts"]["waiting_on_human"],
        1
    );
    let status = project.success(&["status", "--full"]);
    assert_eq!(status["waiting_on_human"][0]["id"], question);
    assert_eq!(status["questions"][0]["stranded"], Value::Null);
    assert!(project.human(&["status", "--full"]).contains(&question));
    assert_eq!(
        project.success(&["debug", "query", "SELECT count(*) AS n FROM questions_open"])["result"]
            ["rows"][0]["n"],
        1
    );

    // Dropping the work strands it, and the drop says so at decision time.
    let dropped = project.success(&["work", "drop", &work, "--why", "requirement withdrawn"]);
    assert_eq!(dropped["stranded_questions"][0], question);
    assert_eq!(
        project.success(&["status"])["counts"]["waiting_on_human"],
        0
    );
    let status = project.success(&["status", "--full"]);
    assert!(status["waiting_on_human"].as_array().unwrap().is_empty());
    assert_eq!(status["questions"][0]["stranded"], "work dropped");
    assert!(!project.human(&["status", "--full"]).contains(&question));
    assert_eq!(
        project.success(&["debug", "query", "SELECT count(*) AS n FROM questions_open"])["result"]
            ["rows"][0]["n"],
        0
    );

    // The question itself is never hidden; `show` renders the derived state.
    let shown = project.success(&["show", &question]);
    assert_eq!(shown["kind"], "question");
    assert_eq!(shown["current"]["stranded"], "work dropped");
    assert!(project.human(&["show", &question]).contains("stranded"));

    // Reopening is the whole round trip: no repair event, and the question is
    // actionable again because visibility was never stored. The question
    // survives unanswered, so the work lands in `blocked` rather than `open`.
    project.success(&["work", "reopen", &work, "--why", "requirement stands"]);
    assert_eq!(
        project.success(&["show", &work])["current"]["state"],
        "blocked"
    );
    assert_eq!(
        project.success(&["status"])["counts"]["waiting_on_human"],
        1
    );
    let status = project.success(&["status", "--section", "waiting_on_human"]);
    assert_eq!(status["waiting_on_human"][0]["id"], question);
    assert!(status.get("blocked").is_none());
    assert_eq!(
        project.success(&["show", &question])["current"]["stranded"],
        Value::Null
    );

    // A late ruling on a stranded question is still recorded.
    project.success(&["work", "drop", &work, "--why", "withdrawn again"]);
    project.success(&["question", "answer", &question, "masked digest"]);
    let shown = project.success(&["show", &question]);
    assert_eq!(shown["current"]["answer"], "masked digest");
    assert_eq!(shown["current"]["stranded"], "work dropped");
}

#[test]
fn reopen_lands_in_blocked_when_an_unanswered_question_survives() {
    let project = TestProject::new();
    let work = string(
        &project.success(&["work", "add", "--title", "Ship the digest"]),
        "work_id",
    );
    let question = string(
        &project.success(&["work", "ask", &work, "Masked digest, or wait for AA-6?"]),
        "question_id",
    );
    project.success(&["work", "drop", &work, "--why", "requirement withdrawn"]);

    // The question survives the reopen unanswered, so the work must not come
    // back as plain `open` the way it would if nothing were pending: that
    // would let a dropped-with-a-question item lose the outstanding decision,
    // disagreeing with `work unblock`'s rejection for the identical situation.
    let reopened = project.success(&["work", "reopen", &work, "--why", "requirement stands"]);
    assert_eq!(reopened["schema"], "alder.work.reopen.v0");
    let shown = project.success(&["show", &work]);
    assert_eq!(shown["current"]["state"], "blocked");
    assert_eq!(
        shown["current"]["block_reason"],
        format!("question {question}")
    );

    // The surviving question blocks progress exactly like `work unblock` does.
    assert_eq!(
        project.failure(&["work", "unblock", &work, "--why", "guessing"])["code"],
        "unanswered_question"
    );

    // Once answered, the ordinary unblock path returns the work to `open`.
    project.success(&["question", "answer", &question, "masked digest"]);
    project.success(&["work", "unblock", &work, "--why", "decided"]);
    assert_eq!(
        project.success(&["show", &work])["current"]["state"],
        "open"
    );
}

#[test]
fn terminal_transitions_report_the_questions_they_strand() {
    let project = TestProject::new();
    let dropped_work = string(
        &project.success(&["work", "add", "--title", "Drop me"]),
        "work_id",
    );
    let dropped_question = string(
        &project.success(&["work", "ask", &dropped_work, "Which lane?"]),
        "question_id",
    );
    let human = project.human(&["work", "drop", &dropped_work, "--why", "superseded"]);
    assert!(
        human.contains(&format!("also strands {dropped_question}")),
        "{human}"
    );

    // External completion strands too: the work leaves `blocked` for `done`
    // without the question ever being answered.
    let finished_work = string(
        &project.success(&["work", "add", "--title", "Finish me"]),
        "work_id",
    );
    let finished_question = string(
        &project.success(&["work", "ask", &finished_work, "Which digest?"]),
        "question_id",
    );
    let finished = project.success(&[
        "work",
        "finish",
        &finished_work,
        "--external",
        "--evidence",
        "PR 171 merged",
    ]);
    assert_eq!(finished["stranded_questions"][0], finished_question);
    assert_eq!(
        project.success(&["show", &finished_question])["current"]["stranded"],
        "work done"
    );
    assert_eq!(
        project.success(&["status"])["counts"]["waiting_on_human"],
        0
    );

    // Work with nothing outstanding strands nothing and says nothing.
    let quiet = string(
        &project.success(&["work", "add", "--title", "Quiet"]),
        "work_id",
    );
    let human = project.human(&["work", "drop", &quiet, "--why", "not needed"]);
    assert_eq!(human.trim(), format!("{quiet}  dropped"));
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
fn the_loop_folds_desired_state_and_the_pass_noun_is_gone() {
    let project = TestProject::new();
    let empty = project.success(&["status"]);
    assert_eq!(empty["loop"]["paused"], false);
    assert!(empty["loop"]["engine"].is_null());
    assert!(empty["loop"]["rotate_requested_seq"].is_null());
    assert!(empty["loop"]["nudge_requested_seq"].is_null());
    assert!(empty["loop"]["review_at"].is_null());
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

    // The pass noun and `loop wake` are gone from the grammar entirely: the
    // log carries no statements about its own readers, so there is nothing
    // for either command to write. Clap rejects them before any code runs.
    for gone in [
        &["pass", "end", "--outcome", "ok"][..],
        &["loop", "wake"][..],
    ] {
        let output = project.command().args(gone).output().unwrap();
        assert!(!output.status.success(), "{gone:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("unrecognized subcommand"),
            "{gone:?}: {stderr}"
        );
    }
    // And no live object answers to a pass ID.
    assert_eq!(project.failure(&["show", "hm-pass-1"])["code"], "not_found");

    let control = project.success(&["debug", "query", "SELECT * FROM loop_control"]);
    assert_eq!(control["result"]["rows"][0]["engine"], "claude");
    assert_eq!(control["result"]["rows"][0]["paused"], 0);
    assert!(control["result"]["rows"][0]["rotate_requested_seq"].is_null());
}

#[test]
fn rotation_and_nudge_requests_record_the_sequence_they_were_asked_at() {
    let project = TestProject::new();
    let requested = project.success(&["loop", "rotate", "--why", "engine upgraded"]);
    assert_eq!(requested["schema"], "alder.loop.rotate.v0");
    let status = project.success(&["status"]);
    // The request records its own sequence — the head it appended at. Whether
    // any driver has acted on it is that driver's machine-local knowledge,
    // not a log fact, so the fold serves the raw sequence and nothing more.
    assert_eq!(status["loop"]["rotate_requested_seq"], status["head"]);
    assert!(status["loop"]["nudge_requested_seq"].is_null());

    let nudged = project.success(&["loop", "nudge", "--why", "an answer landed"]);
    assert_eq!(nudged["schema"], "alder.loop.nudge.v0");
    let status = project.success(&["status"]);
    assert_eq!(status["loop"]["nudge_requested_seq"], status["head"]);
    // A nudge is not a rotation; each records its own request.
    assert_eq!(status["loop"]["rotate_requested_seq"], 1);

    // A later request replaces the recorded sequence.
    project.success(&["loop", "rotate"]);
    let status = project.success(&["status"]);
    assert_eq!(status["loop"]["rotate_requested_seq"], status["head"]);
}

#[test]
fn a_deferral_is_a_statement_on_the_work_item_and_expires_into_review() {
    let project = TestProject::new();
    let early = string(
        &project.success(&["work", "add", "--title", "Deferred early"]),
        "work_id",
    );
    let late = string(
        &project.success(&["work", "add", "--title", "Deferred late"]),
        "work_id",
    );

    // A malformed instant never reaches the log.
    assert_eq!(
        project.failure(&["work", "block", &early, "--why", "wait", "--until", "3pm"])["code"],
        "validation_failed"
    );

    let blocked = project.success(&[
        "work",
        "block",
        &early,
        "--why",
        "third-party outage",
        "--until",
        "2099-01-02T15:00:00Z",
    ]);
    assert_eq!(blocked["until"], "2099-01-02T15:00:00+00:00");
    project.success(&[
        "work",
        "block",
        &late,
        "--why",
        "vendor review",
        "--until",
        "2099-06-01T09:00:00Z",
    ]);

    // The earliest deadline over all blocked work is the loop's next review
    // rendezvous — what the driver wakes the leader at.
    let status = project.success(&["status", "--section", "blocked"]);
    assert_eq!(status["loop"]["review_at"], "2099-01-02T15:00:00Z");
    let untils: Vec<_> = status["blocked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|work| work["block_until"].clone())
        .collect();
    assert!(untils.contains(&json!("2099-01-02T15:00:00Z")));
    assert!(
        project
            .human(&["status", "--section", "blocked"])
            .contains("third-party outage · until 2099-01-02T15:00:00+00:00")
    );
    // A future deadline is not yet anyone's business: no attention finding,
    // and the item is not actionable.
    assert_eq!(project.success(&["status"])["counts"]["attention"], 0);
    assert!(
        !project.success(&["next"])["work"]
            .as_array()
            .unwrap()
            .iter()
            .any(|work| work["id"] == json!(early.clone()))
    );

    // A deadline already in the past has expired: the item surfaces under
    // attention for review, and nothing unblocks by itself.
    let overdue = string(
        &project.success(&["work", "add", "--title", "Deferred and overdue"]),
        "work_id",
    );
    project.success(&[
        "work",
        "block",
        &overdue,
        "--why",
        "check again later",
        "--until",
        "2020-01-01T00:00:00Z",
    ]);
    let status = project.success(&["status", "--section", "attention"]);
    assert_eq!(status["counts"]["attention"], 1);
    let finding = status["attention"][0].clone();
    assert_eq!(finding["kind"], "block_expired");
    assert!(finding["detail"].as_str().unwrap().contains(&overdue));
    assert!(
        string(&finding, "suggested_command").starts_with(&format!("alder work unblock {overdue}"))
    );
    // Expired is still blocked: review is an explicit, reasoned act.
    assert!(
        !project.success(&["next"])["work"]
            .as_array()
            .unwrap()
            .iter()
            .any(|work| work["id"] == json!(overdue.clone()))
    );
    project.success(&["work", "unblock", &overdue, "--why", "reviewed: retry now"]);
    assert_eq!(project.success(&["status"])["counts"]["attention"], 0);

    // Re-blocking without a deadline clears it: the latest statement wins.
    // The key is present and explicitly null, not merely absent.
    project.success(&["work", "block", &overdue, "--why", "paused indefinitely"]);
    let shown = project.success(&["show", &overdue]);
    assert!(
        shown["current"]
            .as_object()
            .unwrap()
            .contains_key("block_until")
    );
    assert!(shown["current"]["block_until"].is_null());
}

#[test]
fn finish_drop_and_reopen_each_clear_the_deferral_deadline() {
    let project = TestProject::new();
    let until = "2099-01-02T15:00:00Z";
    let block_until =
        |work: &str| project.success(&["show", work])["current"]["block_until"].clone();

    // Finishing deferred work (externally: blocked work may only be finished
    // with evidence) clears its deadline with it.
    let finished = string(
        &project.success(&["work", "add", "--title", "Deferred then finished"]),
        "work_id",
    );
    project.success(&[
        "work", "block", &finished, "--why", "wait", "--until", until,
    ]);
    project.success(&[
        "work",
        "finish",
        &finished,
        "--external",
        "--evidence",
        "done upstream",
    ]);
    assert!(block_until(&finished).is_null());

    // Dropping deferred work clears it too.
    let dropped = string(
        &project.success(&["work", "add", "--title", "Deferred then dropped"]),
        "work_id",
    );
    project.success(&["work", "block", &dropped, "--why", "wait", "--until", until]);
    project.success(&["work", "drop", &dropped, "--why", "requirement withdrawn"]);
    assert!(block_until(&dropped).is_null());

    // And a reopened item carries no stale deadline from its blocked past.
    project.success(&["work", "reopen", &finished, "--why", "not actually done"]);
    assert!(block_until(&finished).is_null());
}

#[test]
fn every_confirmed_append_touches_the_local_marker() {
    let project = TestProject::new();
    let marker = project.work.join(".alder/last-append");
    let mtime = |marker: &Path| fs::metadata(marker).unwrap().modified().unwrap();
    // `init` starts no driver, so it wires no hook; the marker appears with
    // the first ordinary mutation.
    assert!(!marker.exists());
    project.success(&["work", "add", "--title", "Touch the marker"]);
    assert!(marker.exists());

    let stamped = mtime(&marker);
    // A read leaves the marker alone, and so does a failed mutation.
    project.success(&["status"]);
    assert_eq!(mtime(&marker), stamped);
    project.failure(&["work", "start", "hm-missing"]);
    assert_eq!(mtime(&marker), stamped);
    // Another confirmed append advances it.
    project.success(&["loop", "nudge"]);
    assert!(mtime(&marker) >= stamped);
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
    assert_eq!(first["result"]["appended"], 2);
    for _ in 0..2 {
        assert_eq!(project.success(&["refresh"])["changed"], false);
    }
    assert!(
        project
            .human(&["refresh"])
            .contains("no observation changes")
    );

    project.config(json!([{"observer": "tmux", "list": "printf '[]'"}]));
    assert!(
        project
            .human(&["refresh"])
            .contains("recorded 2 observation changes")
    );
    assert_eq!(project.success(&["refresh"])["changed"], false);
}

#[test]
fn observations_are_quiet_when_unchanged_and_snapshot_the_newest_level() {
    let project = TestProject::new();
    let first = project.success(&[
        "observation",
        "report",
        "github",
        "owner/repo#171",
        "ci",
        "running",
    ]);
    assert_eq!(first["appended"], true);
    assert_eq!(first["head"], 1);

    let repeated = project.success(&[
        "observation",
        "report",
        "github",
        "owner/repo#171",
        "ci",
        "running",
    ]);
    assert_eq!(repeated["appended"], false);
    assert_eq!(repeated["head"], 1);

    let changed = project.success(&[
        "observation",
        "report",
        "github",
        "owner/repo#171",
        "ci",
        "passing",
    ]);
    assert_eq!(changed["appended"], true);
    assert_eq!(changed["head"], 2);

    let snapshot = project.success(&["observations"]);
    assert_eq!(snapshot["head"], 2);
    assert_eq!(snapshot["observations"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["observations"][0]["observer"], "github");
    assert_eq!(snapshot["observations"][0]["subject"], "owner/repo#171");
    assert_eq!(snapshot["observations"][0]["field"], "ci");
    assert_eq!(snapshot["observations"][0]["level"], "passing");
    assert_eq!(snapshot["observations"][0]["reported_seq"], 2);

    let retired = project.success(&["observation", "retire", "github", "owner/repo#171", "ci"]);
    assert_eq!(retired["appended"], true);
    assert!(
        project.success(&["observations"])["observations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn refresh_applies_complete_level_snapshots_through_the_quiet_append_path() {
    let project = TestProject::new();
    project.config(json!([{
        "observer": "tmux",
        "list": "printf '%s\\n' '[{\"subject\":\"worker\",\"field\":\"liveness\",\"level\":\"present\"}]'"
    }]));

    let first = project.success(&["refresh"]);
    assert_eq!(first["changed"], true);
    assert_eq!(first["result"]["appended"], 1);
    let repeated = project.success(&["refresh"]);
    assert_eq!(repeated["changed"], false);
    assert_eq!(repeated["result"]["appended"], 0);

    project.config(json!([{
        "observer": "tmux",
        "list": "printf '%s\\n' '[{\"subject\":\"worker\",\"field\":\"liveness\",\"level\":\"absent\"}]'"
    }]));
    let changed = project.success(&["refresh"]);
    assert_eq!(changed["result"]["appended"], 1);
    assert_eq!(
        project.success(&["observations"])["observations"][0]["level"],
        "absent"
    );

    project.config(json!([{"observer": "tmux", "list": "printf '[]'"}]));
    let retired = project.success(&["refresh"]);
    assert_eq!(retired["result"]["retired"], 1);
    assert!(
        project.success(&["observations"])["observations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// A dead worker is a statement, not a silence. While its attempt is active,
/// an omitted liveness key becomes an explicit `absent` level rather than a
/// retirement, so any reader of the fold — with no observer of its own — sees
/// the death in `status` attention with the repair. The key retires only once
/// the attempt has ended.
#[test]
fn a_dead_workers_liveness_stays_absent_until_its_attempt_ends() {
    let project = TestProject::new();
    let work = string(
        &project.success(&["work", "add", "--title", "Doomed work"]),
        "work_id",
    );
    let attempt = string(&project.success(&["work", "start", &work]), "attempt_id");
    project.success(&["attempt", "edit", &attempt, "--handle", "tmux:worker"]);

    project.config(json!([{
        "observer": "tmux",
        "list": format!(
            "printf '%s\\n' '[{{\"subject\":\"worker\",\"field\":\"liveness\",\"level\":\"present\"}},{{\"subject\":\"worker\",\"field\":\"attempt-id\",\"level\":\"{attempt}\"}}]'"
        )
    }]));
    let first = project.success(&["refresh"]);
    assert_eq!(first["result"]["appended"], 2, "{first}");
    assert_eq!(project.success(&["status"])["counts"]["attention"], 0);

    // The session vanishes while the attempt is still active. The liveness
    // key flips to absent (one append); the attempt-id key retires (another).
    project.config(json!([{"observer": "tmux", "list": "printf '[]'"}]));
    let died = project.success(&["refresh"]);
    assert_eq!(died["result"]["appended"], 2);
    assert_eq!(died["result"]["retired"], 1);
    assert_eq!(
        project.success(&["observations"])["observations"][0]["level"],
        "absent"
    );

    // The fold alone carries the death: attention shows the missing worker.
    let status = project.success(&["status", "--full"]);
    assert_eq!(status["counts"]["attention"], 1);
    assert_eq!(status["attention"][0]["kind"], "missing");
    assert_eq!(status["attention"][0]["attempt_id"], attempt);

    // Saying it again changes nothing.
    let repeated = project.success(&["refresh"]);
    assert_eq!(repeated["result"]["appended"], 0);

    // Once the attempt ends, the next refresh retires the key and the
    // picture is quiet again.
    project.success(&[
        "attempt",
        "end",
        &attempt,
        "--outcome",
        "lost",
        "--why",
        "observed absent",
    ]);
    let after_end = project.success(&["refresh"]);
    assert_eq!(after_end["result"]["retired"], 1);
    assert!(
        project.success(&["observations"])["observations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(project.success(&["status"])["counts"]["attention"], 0);
}

/// The live incident of al-pass-64, reproduced over the mutation that
/// remains: a leader-side write lost the compare-and-append to a worker
/// appending its own milestones.
///
/// The loss itself is correct and stays: an ordinary mutation validated
/// against one head is never replayed against another. What is under test is
/// that losing announces the command's effect — nothing — in a form no caller
/// can read as success, and that rereading and rerunning is what settles it.
#[test]
fn a_mutation_that_loses_the_race_says_it_wrote_nothing() {
    let project = TestProject::new();
    let work = string(
        &project.success(&["work", "add", "--title", "Raced"]),
        "work_id",
    );
    let attempt = string(&project.success(&["work", "start", &work]), "attempt_id");
    let rival = project.rival();

    let lost = project.losing(
        &["work", "block", &work, "--why", "paused for the worker"],
        &rival,
        &["attempt", "edit", &attempt, "--note", "worker milestone"],
    );
    assert_eq!(lost.status.code(), Some(1));
    assert!(lost.stdout.is_empty());
    let stderr = String::from_utf8(lost.stderr).unwrap();
    let mut lines = stderr.lines();
    // The first line states the effect on the command, not on the log, and
    // names the event that was not written.
    assert_eq!(
        lines.next().unwrap(),
        "error [head_conflict]: nothing was appended: `work.changed` lost the \
         compare-and-append to another writer, which moved the shared log from 2 to 3; \
         reread and run the command again"
    );
    // Nothing under it has the shape of a result document: the receipt-shaped
    // JSON object that read as success is gone, and the context survives it.
    assert_eq!(
        lines.collect::<Vec<_>>(),
        [
            "  appended: false",
            "  current_head: 3",
            "  event: work.changed",
            "  expected_head: 2",
        ]
    );
    assert!(!stderr.contains('{'), "{stderr}");

    // Nothing was written, so the work is still open.
    assert_eq!(
        project.success(&["show", &work])["current"]["state"],
        "open"
    );

    // The same loss over the JSON channel: one document on standard output,
    // per the output contract, but one no reader can take for a mutation
    // result.
    let structured = project.losing(
        &[
            "work",
            "block",
            &work,
            "--why",
            "paused for the worker",
            "--json",
        ],
        &rival,
        &["attempt", "edit", &attempt, "--note", "another milestone"],
    );
    assert_eq!(structured.status.code(), Some(1));
    assert!(structured.stderr.is_empty());
    let document: Value = serde_json::from_slice(&structured.stdout).unwrap();
    assert_eq!(document["schema"], "alder.error.v0");
    assert_eq!(document["ok"], false);
    assert_eq!(document["code"], "head_conflict");
    assert_eq!(
        document["context"],
        json!({
            "appended": false,
            "event": "work.changed",
            "expected_head": 3,
            "current_head": 4,
        })
    );

    // A2: rereading and rerunning settles it, and records exactly one block.
    let blocked = project.success(&["work", "block", &work, "--why", "paused for the worker"]);
    assert_eq!(blocked["work_id"], json!(work.clone()));
    let shown = project.success(&["show", &work]);
    assert_eq!(shown["current"]["state"], "blocked");
    assert_eq!(shown["current"]["block_reason"], "paused for the worker");
}

/// The Git subcommand of each recorded call, which is what a cost assertion
/// is about: the revisions in the arguments differ between two logs, the
/// shape of the work does not.
fn subcommands(calls: &[String]) -> Vec<&str> {
    calls
        .iter()
        .map(|call| call.split_whitespace().next().unwrap_or_default())
        .collect()
}

#[test]
fn a_read_costs_the_same_few_git_processes_however_long_the_log_is() {
    let project = TestProject::new();
    for index in 0..2 {
        project.success(&["work", "add", "--title", &format!("Short {index}")]);
    }
    project.forget_cached_records();
    let (short, short_calls) = project.counted(&["status"]);

    for index in 0..24 {
        project.success(&["work", "add", "--title", &format!("Long {index}")]);
    }
    project.forget_cached_records();
    let (long, long_calls) = project.counted(&["status"]);

    // Twelve times the events, read the same way.
    assert!(long["head"].as_u64().unwrap() > short["head"].as_u64().unwrap() * 10);
    assert_eq!(subcommands(&short_calls), subcommands(&long_calls));
    assert!(
        long_calls.len() <= 5,
        "a read should cost at most five Git processes, not {long_calls:?}"
    );
    // One batch carries every event body, and nothing reads them one at a time.
    assert_eq!(
        long_calls
            .iter()
            .filter(|call| call.starts_with("cat-file --batch"))
            .count(),
        1,
        "{long_calls:?}"
    );
    assert!(
        !long_calls.iter().any(|call| call.starts_with("show")),
        "{long_calls:?}"
    );
}

#[test]
fn an_unchanged_head_reads_no_event_bodies() {
    let project = TestProject::new();
    project.success(&["work", "add", "--title", "Cached"]);
    let (first, first_calls) = project.counted(&["status"]);
    assert!(first_calls.iter().any(|call| call.starts_with("cat-file")));

    // The revision has not moved, so the recorded records still describe it
    // and no object is touched to prove that.
    let (repeated, repeated_calls) = project.counted(&["status"]);
    assert_eq!(repeated["head"], first["head"]);
    assert_eq!(repeated["revision"], first["revision"]);
    assert!(
        !repeated_calls
            .iter()
            .any(|call| call.starts_with("cat-file") || call.starts_with("ls-tree")),
        "{repeated_calls:?}"
    );

    // A moved head is a different revision, so the bodies are read again
    // rather than the stale ones being served.
    project.success(&["work", "add", "--title", "Moved"]);
    let (moved, moved_calls) = project.counted(&["status"]);
    assert_ne!(moved["revision"], first["revision"]);
    assert_eq!(
        moved["head"].as_u64().unwrap(),
        first["head"].as_u64().unwrap() + 1
    );
    assert!(moved_calls.iter().any(|call| call.starts_with("cat-file")));
}
