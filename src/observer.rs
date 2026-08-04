use std::{
    collections::BTreeSet,
    io::Read,
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use rustix::process::{Pid, Signal, kill_process_group};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wait_timeout::ChildExt;

use crate::{
    config::ObserverConfig,
    domain::{AttemptState, ObservationKey, ProjectState, WorkState},
    error::{AlderError, Result},
};

const EXECUTION_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_EXECUTIONS: usize = 4;
const STDERR_LIMIT: usize = 4096;

#[derive(Debug, Clone, Serialize)]
pub struct ObserverRunResult {
    pub kind: String,
    pub success: bool,
    pub executions: Vec<ExecutionResult>,
    pub normalized: Vec<NormalizedObject>,
    pub observed_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecutionResult {
    pub number: usize,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stderr: String,
    pub validation_error: Option<String>,
}

/// One current level reported by an observer script. For `liveness` rows the
/// subject is the opaque handle exactly as the runner bound it; for every
/// other field the subject is stored verbatim as the observation subject.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedObject {
    pub subject: String,
    pub field: String,
    pub level: String,
}

/// One planned change to the durable observation picture: a level to report,
/// or — when `level` is `None` — a key to retire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationChange {
    pub key: ObservationKey,
    pub level: Option<String>,
}

/// The observation changes one successful observer run implies, as a pure
/// function of the folded state, so the crash harnesses apply exactly the
/// derivation production applies.
///
/// Execution-liveness observations are keyed by attempt ID. The plan reads
/// open attempts and their handles from the fold and matches each handle —
/// by equality, never by parsing — against the `liveness` subjects the script
/// listed; a matched row becomes a level about that attempt. A listed handle
/// no live attempt claims is not a statement about work and plans nothing.
/// Rows with any other field are generic observations and pass through with
/// their subject verbatim.
///
/// A successful script is a complete snapshot for its observer, so omitted
/// previously-established keys retire — with one exception: a liveness key
/// whose attempt is still active becomes an explicit `absent` level instead.
/// A dead worker is a statement the fold must carry — a reader with no
/// observer of its own can only learn the death from a level, never from
/// silence. The key retires once its attempt ends.
pub fn plan_observer_run(
    state: &ProjectState,
    kind: &str,
    normalized: &[NormalizedObject],
) -> Vec<ObservationChange> {
    let mut changes = Vec::new();
    let mut reported = BTreeSet::new();
    for object in normalized {
        let key = if object.field == "liveness" {
            let Some(attempt) = state.attempts.values().find(|attempt| {
                matches!(attempt.state, AttemptState::Starting | AttemptState::Active)
                    && attempt.handle.as_deref() == Some(object.subject.as_str())
            }) else {
                continue;
            };
            ObservationKey {
                observer: kind.to_owned(),
                subject: attempt.id.clone(),
                field: "liveness".to_owned(),
            }
        } else {
            ObservationKey {
                observer: kind.to_owned(),
                subject: object.subject.clone(),
                field: object.field.clone(),
            }
        };
        reported.insert(key.clone());
        changes.push(ObservationChange {
            key,
            level: Some(object.level.clone()),
        });
    }
    for key in state
        .observations
        .keys()
        .filter(|key| key.observer == kind && !reported.contains(*key))
    {
        let active_attempt = state.attempts.get(&key.subject).is_some_and(|attempt| {
            matches!(attempt.state, AttemptState::Starting | AttemptState::Active)
        });
        let level = (key.field == "liveness" && active_attempt).then(|| "absent".to_owned());
        changes.push(ObservationChange {
            key: key.clone(),
            level,
        });
    }
    changes
}

/// Run every configured observer. The returned normalized reports are
/// deliberately not folded here: the application append path owns newness
/// and is the sole writer of the durable observation picture.
pub fn observe(observers: &[ObserverConfig]) -> Result<Vec<ObserverRunResult>> {
    let mut runs = Vec::new();
    for observer in observers {
        let run = run_observer(observer, EXECUTION_TIMEOUT, MAX_EXECUTIONS)?;
        runs.push(run);
    }
    Ok(runs)
}

pub fn diagnose(observer: &ObserverConfig) -> Result<ObserverRunResult> {
    run_observer(observer, EXECUTION_TIMEOUT, MAX_EXECUTIONS)
}

fn run_observer(
    observer: &ObserverConfig,
    timeout: Duration,
    max_executions: usize,
) -> Result<ObserverRunResult> {
    let mut executions = Vec::new();
    let mut normalized = Vec::new();
    let mut success = false;
    for number in 1..=max_executions {
        let (execution, objects) = execute_once(&observer.list, number, timeout)?;
        let valid = objects.is_some();
        executions.push(execution);
        if let Some(objects) = objects {
            normalized = objects;
            success = true;
            break;
        }
        if valid {
            break;
        }
    }
    Ok(ObserverRunResult {
        kind: observer.observer.clone(),
        success,
        executions,
        normalized,
        observed_at: Utc::now().to_rfc3339(),
    })
}

fn execute_once(
    script: &str,
    number: usize,
    timeout: Duration,
) -> Result<(ExecutionResult, Option<Vec<NormalizedObject>>)> {
    let started = Instant::now();
    let mut command = Command::new("/bin/bash");
    command
        .args(["-o", "pipefail", "-c", script])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        AlderError::with_context(
            "observer_execution_failed",
            format!("could not start observation command: {error}"),
            json!({"shell": "/bin/bash -o pipefail -c"}),
        )
    })?;
    let process_group = Pid::from_child(&child);
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stdout_reader = thread::spawn(move || read_all(stdout));
    let stderr_reader = thread::spawn(move || read_all(stderr));
    let (status, timed_out) = match child.wait_timeout(timeout)? {
        Some(status) => (status, false),
        None => {
            // The shell is its own process-group leader, so terminate the
            // complete configured pipeline rather than only the shell.
            handle_kill_result(kill_process_group(process_group, Signal::KILL))?;
            (child.wait()?, true)
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    let duration_ms = started.elapsed().as_millis() as u64;
    let stderr = bounded(&stderr, STDERR_LIMIT);
    if timed_out {
        return Ok((
            ExecutionResult {
                number,
                duration_ms,
                exit_code: status.code(),
                timed_out: true,
                stderr,
                validation_error: Some(format!(
                    "command exceeded the fixed {} second timeout",
                    timeout.as_secs_f64()
                )),
            },
            None,
        ));
    }
    if !status.success() {
        return Ok((
            ExecutionResult {
                number,
                duration_ms,
                exit_code: status.code(),
                timed_out: false,
                stderr,
                validation_error: Some("command exited nonzero".to_owned()),
            },
            None,
        ));
    }
    match validate_output(&stdout) {
        Ok(objects) => Ok((
            ExecutionResult {
                number,
                duration_ms,
                exit_code: status.code(),
                timed_out: false,
                stderr,
                validation_error: None,
            },
            Some(objects),
        )),
        Err(error) => Ok((
            ExecutionResult {
                number,
                duration_ms,
                exit_code: status.code(),
                timed_out: false,
                stderr,
                validation_error: Some(error.message),
            },
            None,
        )),
    }
}

fn handle_kill_result(result: rustix::io::Result<()>) -> Result<()> {
    match result {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(std::io::Error::from(error).into()),
    }
}

fn validate_output(bytes: &[u8]) -> Result<Vec<NormalizedObject>> {
    let objects: Vec<NormalizedObject> = serde_json::from_slice(bytes).map_err(|error| {
        AlderError::with_context(
            "invalid_observation",
            format!("standard output is not one normalized JSON array: {error}"),
            json!({"line": error.line(), "column": error.column()}),
        )
    })?;
    let mut keys = BTreeSet::new();
    for object in &objects {
        if object.subject.trim().is_empty() {
            return Err(AlderError::new(
                "invalid_observation",
                "an observation subject cannot be empty",
            ));
        }
        if object.field.trim().is_empty() || object.level.trim().is_empty() {
            return Err(AlderError::new(
                "invalid_observation",
                "an observation field and level cannot be empty",
            ));
        }
        if !keys.insert((&object.subject, &object.field)) {
            return Err(AlderError::with_context(
                "invalid_observation",
                format!(
                    "duplicate observation key `{}` / `{}`",
                    object.subject, object.field
                ),
                json!({"subject": object.subject, "field": object.field}),
            ));
        }
    }
    Ok(objects)
}

fn read_all(mut reader: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = reader.read_to_end(&mut bytes);
    bytes
}

fn bounded(bytes: &[u8], limit: usize) -> String {
    let value = String::from_utf8_lossy(bytes);
    let mut output: String = value.chars().take(limit).collect();
    if value.chars().count() > limit {
        output.push('…');
    }
    output
}

#[derive(Debug, Clone, Serialize)]
pub struct ReconcileFinding {
    pub kind: String,
    pub attempt_id: Option<String>,
    pub handle: Option<String>,
    pub status: String,
    pub detail: String,
    pub suggested_command: Option<String>,
    pub metadata: Value,
}

/// Compare durable attempts with the folded liveness picture.
///
/// Execution-liveness observations are keyed by attempt ID, so every
/// comparison here is a direct fold lookup: no handle is parsed, and no local
/// observer state is consulted. `configured` and `known` — the observer names
/// in the manifest and the ones whose scripts just ran successfully — gate
/// only the findings about attempts that were never bound, where the honest
/// verdict depends on whether coverage was complete.
pub fn reconcile(
    state: &ProjectState,
    configured: &BTreeSet<String>,
    known: &BTreeSet<String>,
) -> Vec<ReconcileFinding> {
    let all_configured_known =
        !configured.is_empty() && configured.iter().all(|kind| known.contains(kind));
    let mut findings = Vec::new();

    for attempt in state.attempts.values() {
        let active = matches!(attempt.state, AttemptState::Starting | AttemptState::Active);
        let liveness: Vec<_> = state
            .observations
            .values()
            .filter(|observation| {
                observation.key.field == "liveness" && observation.key.subject == attempt.id
            })
            .collect();
        if let Some(handle) = attempt.handle.as_deref() {
            if active {
                if liveness.is_empty() {
                    findings.push(ReconcileFinding {
                        kind: "observation_unknown".to_owned(),
                        attempt_id: Some(attempt.id.clone()),
                        handle: Some(handle.to_owned()),
                        status: "unknown".to_owned(),
                        detail: "no current liveness observation is recorded for this attempt"
                            .to_owned(),
                        suggested_command: None,
                        metadata: json!({}),
                    });
                }
                for observation in &liveness {
                    match observation.level.as_str() {
                        "present" => {}
                        "absent" => findings.push(ReconcileFinding {
                            kind: "missing".to_owned(),
                            attempt_id: Some(attempt.id.clone()),
                            handle: Some(handle.to_owned()),
                            status: "absent".to_owned(),
                            detail: "an active attempt's bound handle is confirmed absent"
                                .to_owned(),
                            suggested_command: Some(format!(
                                "alder attempt end {} --outcome lost --why \"external handle absent\"",
                                attempt.id
                            )),
                            metadata: json!({"observer": observation.key.observer}),
                        }),
                        level => findings.push(ReconcileFinding {
                            kind: "observation_unknown".to_owned(),
                            attempt_id: Some(attempt.id.clone()),
                            handle: Some(handle.to_owned()),
                            status: "unknown".to_owned(),
                            detail: format!(
                                "the current liveness level `{level}` is neither present nor absent"
                            ),
                            suggested_command: None,
                            metadata: json!({"observer": observation.key.observer}),
                        }),
                    }
                }
            } else {
                for observation in &liveness {
                    if observation.level == "present" {
                        findings.push(ReconcileFinding {
                            kind: "orphan".to_owned(),
                            attempt_id: Some(attempt.id.clone()),
                            handle: Some(handle.to_owned()),
                            status: "present".to_owned(),
                            detail: "an ended attempt still has a live external handle".to_owned(),
                            suggested_command: None,
                            metadata: json!({"observer": observation.key.observer}),
                        });
                    }
                }
            }
        } else if active {
            if all_configured_known {
                // An attempt that has never held a handle is not a worker that
                // died; it is a worker that was never launched — a `work
                // start` from a phone, or a crash between recording the
                // attempt and spawning. While its work is still live the
                // repair is to launch one, so the suggestion is the dispatch
                // rather than a funeral. (A worker that *was* bound and then
                // vanished is the `missing` finding, and still is.)
                let spawnable = attempt.bound_seq.is_none()
                    && state.work.get(&attempt.work_id).is_some_and(|work| {
                        matches!(work.state, WorkState::Open | WorkState::Blocked)
                    });
                if spawnable {
                    findings.push(ReconcileFinding {
                        kind: "unspawned".to_owned(),
                        attempt_id: Some(attempt.id.clone()),
                        handle: None,
                        status: "absent".to_owned(),
                        detail: "an open attempt has never been bound to a handle; no worker was launched"
                            .to_owned(),
                        suggested_command: Some(format!("alderd spawn {}", attempt.work_id)),
                        metadata: json!({}),
                    });
                    continue;
                }
                findings.push(ReconcileFinding {
                    kind: "not_started".to_owned(),
                    attempt_id: Some(attempt.id.clone()),
                    handle: None,
                    status: "absent".to_owned(),
                    detail: "no handle was ever bound and every configured observer just reported"
                        .to_owned(),
                    suggested_command: Some(format!(
                        "alder attempt end {} --outcome not-started --why \"worker was not launched\"",
                        attempt.id
                    )),
                    metadata: json!({}),
                });
            } else {
                findings.push(ReconcileFinding {
                    kind: "unbound".to_owned(),
                    attempt_id: Some(attempt.id.clone()),
                    handle: None,
                    status: "unknown".to_owned(),
                    detail: "the attempt has no handle and observation coverage is incomplete"
                        .to_owned(),
                    suggested_command: None,
                    metadata: json!({}),
                });
            }
        }
    }
    findings.sort_by(|left, right| {
        left.attempt_id
            .cmp(&right.attempt_id)
            .then_with(|| left.handle.cmp(&right.handle))
            .then_with(|| left.kind.cmp(&right.kind))
    });
    findings
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        env, fs,
        os::unix::fs::PermissionsExt,
        path::Path,
        process::Command,
    };

    use tempfile::TempDir;

    use super::*;
    use crate::domain::{Attempt, AttemptState, Observation};

    fn attempt(id: &str, state: AttemptState, handle: Option<&str>) -> Attempt {
        Attempt {
            id: id.to_owned(),
            work_id: format!("{id}-work"),
            state,
            outcome: None,
            tier: None,
            handle: handle.map(ToOwned::to_owned),
            metadata: BTreeMap::new(),
            note: None,
            started_seq: 1,
            bound_seq: handle.map(|_| 2),
            updated_seq: 2,
            ended_seq: (state == AttemptState::Ended).then_some(3),
            checks: BTreeMap::new(),
        }
    }

    fn observation(observer: &str, subject: &str, field: &str, level: &str) -> Observation {
        Observation {
            key: ObservationKey {
                observer: observer.to_owned(),
                subject: subject.to_owned(),
                field: field.to_owned(),
            },
            level: level.to_owned(),
            reported_seq: 1,
        }
    }

    fn with_observations(state: &mut ProjectState, observations: &[Observation]) {
        for observation in observations {
            state
                .observations
                .insert(observation.key.clone(), observation.clone());
        }
    }

    fn level(subject: &str, field: &str, level: &str) -> NormalizedObject {
        NormalizedObject {
            subject: subject.to_owned(),
            field: field.to_owned(),
            level: level.to_owned(),
        }
    }

    fn work(id: &str, state: WorkState) -> crate::domain::Work {
        crate::domain::Work {
            id: id.to_owned(),
            title: format!("work {id}"),
            spec: None,
            priority: 0,
            state,
            block_reason: None,
            block_until: None,
            outcome: None,
            opened_seq: 1,
            changed_seq: 1,
            requires: Vec::new(),
            checks: Vec::new(),
        }
    }

    fn finding_kinds(findings: &[ReconcileFinding]) -> Vec<&str> {
        findings
            .iter()
            .map(|finding| finding.kind.as_str())
            .collect()
    }

    #[test]
    fn retries_invalid_results_and_accepts_first_valid_snapshot() {
        let temporary = TempDir::new().unwrap();
        let marker = temporary.path().join("count");
        let script = format!(
            "n=$(cat '{}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{}'; \
             if [ $n -lt 3 ]; then echo nope; else \
             echo '[{{\"subject\":\"tmux:one\",\"field\":\"liveness\",\"level\":\"present\"}}]'; fi",
            marker.display(),
            marker.display()
        );
        let observer = ObserverConfig {
            observer: "tmux".to_owned(),
            list: script,
        };
        let result = run_observer(&observer, Duration::from_secs(1), 4).unwrap();
        assert!(result.success);
        assert_eq!(result.executions.len(), 3);
        assert_eq!(result.normalized[0].subject, "tmux:one");
        assert_eq!(result.normalized[0].level, "present");
    }

    #[test]
    fn pipeline_failures_and_timeouts_retry_four_times() {
        let pipeline = ObserverConfig {
            observer: "tmux".to_owned(),
            list: "false | true".to_owned(),
        };
        let failed = run_observer(&pipeline, Duration::from_secs(1), 4).unwrap();
        assert!(!failed.success);
        assert_eq!(failed.executions.len(), 4);
        assert!(
            failed
                .executions
                .iter()
                .all(|execution| execution.exit_code == Some(1))
        );

        let timeout = ObserverConfig {
            observer: "tmux".to_owned(),
            list: "sleep 10".to_owned(),
        };
        let timed_out = run_observer(&timeout, Duration::from_millis(30), 4).unwrap();
        assert!(!timed_out.success);
        assert_eq!(timed_out.executions.len(), 4);
        assert!(
            timed_out
                .executions
                .iter()
                .all(|execution| execution.timed_out)
        );
    }

    #[test]
    fn output_validation_and_bounding_cover_exact_boundaries() {
        assert!(
            validate_output(br#"[{"subject":"tmux:one","field":"liveness","level":"present"}]"#)
                .is_ok()
        );
        for invalid in [
            br#"not json"#.as_slice(),
            br#"[{"subject":"","field":"liveness","level":"present"}]"#.as_slice(),
            br#"[{"subject":"tmux:one","field":" ","level":"present"}]"#.as_slice(),
            br#"[{"subject":"tmux:one","field":"liveness","level":" "}]"#.as_slice(),
            br#"[{"subject":"a","field":"f","level":"x"},{"subject":"a","field":"f","level":"y"}]"#
                .as_slice(),
            // The retired handle-inventory shape: `value`, `attempt_id`, and
            // `metadata` were the legacy adapter's fields and are no longer a
            // valid report.
            br#"[{"value":"one"}]"#.as_slice(),
            br#"[{"value":"one","attempt_id":"a","metadata":{}}]"#.as_slice(),
        ] {
            assert!(
                validate_output(invalid).is_err(),
                "{}",
                String::from_utf8_lossy(invalid)
            );
        }

        assert_eq!(bounded(b"", 0), "");
        assert_eq!(bounded(b"a", 0), "…");
        assert_eq!(bounded(b"abc", 3), "abc");
        assert_eq!(bounded(b"abcd", 3), "abc…");
        assert_eq!(bounded(&[0xff, b'a'], 1), "�…");

        assert!(handle_kill_result(Ok(())).is_ok());
        assert!(handle_kill_result(Err(rustix::io::Errno::SRCH)).is_ok());
        assert!(handle_kill_result(Err(rustix::io::Errno::PERM)).is_err());
    }

    #[test]
    fn a_listed_handle_becomes_a_level_keyed_by_its_attempt() {
        let mut state = ProjectState::default();
        state.attempts.insert(
            "active".to_owned(),
            attempt("active", AttemptState::Active, Some("tmux:worker")),
        );
        state.attempts.insert(
            "ended".to_owned(),
            attempt("ended", AttemptState::Ended, Some("tmux:done")),
        );

        let changes = plan_observer_run(
            &state,
            "tmux",
            &[
                level("tmux:worker", "liveness", "present"),
                // A live session no active attempt claims is not a statement
                // about work: the runner owns its sessions.
                level("tmux:stray", "liveness", "present"),
                // An ended attempt's handle no longer maps to anything.
                level("tmux:done", "liveness", "present"),
            ],
        );
        assert_eq!(
            changes,
            vec![ObservationChange {
                key: ObservationKey {
                    observer: "tmux".to_owned(),
                    subject: "active".to_owned(),
                    field: "liveness".to_owned(),
                },
                level: Some("present".to_owned()),
            }]
        );
    }

    #[test]
    fn generic_levels_pass_through_with_their_subject_verbatim() {
        let changes = plan_observer_run(
            &ProjectState::default(),
            "github",
            &[level("owner/repo#171", "ci", "passing")],
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].key.subject, "owner/repo#171");
        assert_eq!(changes[0].key.field, "ci");
        assert_eq!(changes[0].level.as_deref(), Some("passing"));
    }

    #[test]
    fn an_omitted_liveness_key_is_absent_while_active_and_retires_after() {
        let mut state = ProjectState::default();
        state.attempts.insert(
            "active".to_owned(),
            attempt("active", AttemptState::Active, Some("tmux:worker")),
        );
        with_observations(
            &mut state,
            &[observation("tmux", "active", "liveness", "present")],
        );

        // The worker vanished: the established key flips to an explicit
        // absent level while its attempt is still active.
        let died = plan_observer_run(&state, "tmux", &[]);
        assert_eq!(
            died,
            vec![ObservationChange {
                key: ObservationKey {
                    observer: "tmux".to_owned(),
                    subject: "active".to_owned(),
                    field: "liveness".to_owned(),
                },
                level: Some("absent".to_owned()),
            }]
        );

        // Another observer's keys are not this observer's to retire.
        assert!(plan_observer_run(&state, "nimbus", &[]).is_empty());

        // Once the attempt ends, the same omission retires the key.
        state.attempts.get_mut("active").unwrap().state = AttemptState::Ended;
        let ended = plan_observer_run(&state, "tmux", &[]);
        assert_eq!(ended.len(), 1);
        assert!(ended[0].level.is_none());
    }

    /// The real log holds observations from before the re-key: liveness and
    /// `attempt-id` levels whose subject is a session name. Neither subject
    /// names a live attempt, so the next successful sweep retires both.
    #[test]
    fn session_keyed_observations_from_before_the_rekey_retire() {
        let mut state = ProjectState::default();
        state.attempts.insert(
            "hm-1-attempt-1".to_owned(),
            attempt(
                "hm-1-attempt-1",
                AttemptState::Active,
                Some("tmux:alder-work-hm-1"),
            ),
        );
        with_observations(
            &mut state,
            &[
                observation("tmux", "alder-work-hm-1", "liveness", "present"),
                observation("tmux", "alder-work-hm-1", "attempt-id", "hm-1-attempt-1"),
            ],
        );
        let changes = plan_observer_run(
            &state,
            "tmux",
            &[level("tmux:alder-work-hm-1", "liveness", "present")],
        );
        let mut reports = 0;
        let mut retires = 0;
        for change in &changes {
            match &change.level {
                Some(_) => {
                    reports += 1;
                    assert_eq!(change.key.subject, "hm-1-attempt-1");
                }
                None => {
                    retires += 1;
                    assert_eq!(change.key.subject, "alder-work-hm-1");
                }
            }
        }
        assert_eq!((reports, retires), (1, 2));
    }

    #[test]
    fn reconciliation_classifies_bound_attempt_states() {
        let configured = BTreeSet::from(["tmux".to_owned()]);
        let known = configured.clone();

        let mut state = ProjectState::default();
        state.attempts.insert(
            "active".to_owned(),
            attempt("active", AttemptState::Active, Some("tmux:active")),
        );

        let mut healthy = state.clone();
        with_observations(
            &mut healthy,
            &[observation("tmux", "active", "liveness", "present")],
        );
        assert_eq!(
            finding_kinds(&reconcile(&healthy, &configured, &known)),
            Vec::<&str>::new()
        );

        let mut absent = state.clone();
        with_observations(
            &mut absent,
            &[observation("tmux", "active", "liveness", "absent")],
        );
        let missing = reconcile(&absent, &configured, &known);
        assert_eq!(finding_kinds(&missing), vec!["missing"]);
        assert_eq!(missing[0].attempt_id.as_deref(), Some("active"));
        assert_eq!(missing[0].handle.as_deref(), Some("tmux:active"));
        assert!(missing[0].suggested_command.is_some());

        let mut odd = state.clone();
        with_observations(
            &mut odd,
            &[observation("tmux", "active", "liveness", "wedged")],
        );
        let unknown = reconcile(&odd, &configured, &known);
        assert_eq!(finding_kinds(&unknown), vec!["observation_unknown"]);
        assert!(unknown[0].suggested_command.is_none());

        // Silence is not absence: with no current level there is nothing to
        // act on, and no destructive repair is suggested.
        let silent = reconcile(&state, &configured, &known);
        assert_eq!(finding_kinds(&silent), vec!["observation_unknown"]);
        assert!(silent[0].suggested_command.is_none());

        let mut ended = ProjectState::default();
        ended.attempts.insert(
            "ended".to_owned(),
            attempt("ended", AttemptState::Ended, Some("tmux:ended")),
        );
        let mut orphaned = ended.clone();
        with_observations(
            &mut orphaned,
            &[observation("tmux", "ended", "liveness", "present")],
        );
        assert_eq!(
            finding_kinds(&reconcile(&orphaned, &configured, &known)),
            vec!["orphan"]
        );
        let mut gone = ended.clone();
        with_observations(
            &mut gone,
            &[observation("tmux", "ended", "liveness", "absent")],
        );
        assert!(reconcile(&gone, &configured, &known).is_empty());
        assert!(reconcile(&ended, &configured, &known).is_empty());
    }

    #[test]
    fn reconciliation_classifies_unbound_attempts_by_coverage() {
        let configured = BTreeSet::from(["tmux".to_owned()]);
        let known = configured.clone();
        let mut state = ProjectState::default();
        state.attempts.insert(
            "active".to_owned(),
            attempt("active", AttemptState::Starting, None),
        );

        // Coverage incomplete: the honest verdict is unknown.
        assert_eq!(
            finding_kinds(&reconcile(&state, &configured, &BTreeSet::new())),
            vec!["unbound"]
        );
        assert_eq!(
            finding_kinds(&reconcile(&state, &BTreeSet::new(), &BTreeSet::new())),
            vec!["unbound"]
        );

        // Complete coverage and no live work backing the attempt: end it.
        assert_eq!(
            finding_kinds(&reconcile(&state, &configured, &known)),
            vec!["not_started"]
        );
    }

    #[test]
    fn an_attempt_that_was_never_bound_is_told_to_spawn_one() {
        let configured = BTreeSet::from(["tmux".to_owned()]);
        let known = configured.clone();
        let mut state = ProjectState::default();
        state.attempts.insert(
            "active".to_owned(),
            attempt("active", AttemptState::Starting, None),
        );
        state.work.insert(
            "active-work".to_owned(),
            work("active-work", WorkState::Open),
        );

        let findings = reconcile(&state, &configured, &known);
        assert_eq!(finding_kinds(&findings), vec!["unspawned"]);
        assert_eq!(
            findings[0].suggested_command.as_deref(),
            Some("alderd spawn active-work")
        );

        // Blocked work is spawnable too: a ruling arrives with the launch.
        state.work.insert(
            "active-work".to_owned(),
            work("active-work", WorkState::Blocked),
        );
        assert_eq!(
            finding_kinds(&reconcile(&state, &configured, &known)),
            vec!["unspawned"]
        );

        // Work that is over is not: end the attempt instead.
        state.work.insert(
            "active-work".to_owned(),
            work("active-work", WorkState::Done),
        );
        let over = reconcile(&state, &configured, &known);
        assert_eq!(finding_kinds(&over), vec!["not_started"]);
        assert!(
            over[0]
                .suggested_command
                .as_deref()
                .is_some_and(|command| command.starts_with("alder attempt end"))
        );

        // A handle that was bound and then vanished stays `missing`: the
        // worker existed, so spawning a second one is not the repair.
        let mut died = ProjectState::default();
        died.attempts.insert(
            "active".to_owned(),
            attempt("active", AttemptState::Active, Some("tmux:gone")),
        );
        died.work.insert(
            "active-work".to_owned(),
            work("active-work", WorkState::Open),
        );
        with_observations(
            &mut died,
            &[observation("tmux", "active", "liveness", "absent")],
        );
        assert_eq!(
            finding_kinds(&reconcile(&died, &configured, &known)),
            vec!["missing"]
        );
    }

    /// The shipped tmux observer emits the re-keyed shape: one liveness level
    /// per worker session, whose subject is the opaque handle the runner
    /// bound, with no attempt stamp and no session metadata.
    #[test]
    fn the_tmux_observer_script_lists_worker_handles_as_liveness_levels() {
        let temporary = TempDir::new().unwrap();
        let bin = temporary.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let stub = bin.join("tmux");
        fs::write(
            &stub,
            "#!/bin/sh\ncase \"$1\" in\n  list-sessions) printf '%s\\n' 'alder-work-one' 'alder-leader' 'alder-work-two' ;;\nesac\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&stub).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&stub, permissions).unwrap();

        let mut path_entries = vec![bin];
        path_entries.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
        let output = Command::new("bash")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/observe-tmux.sh"))
            .env("PATH", env::join_paths(path_entries).unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "observer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let objects = validate_output(&output.stdout).unwrap();
        assert_eq!(
            objects
                .iter()
                .map(|object| (
                    object.subject.as_str(),
                    object.field.as_str(),
                    object.level.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("tmux:alder-work-one", "liveness", "present"),
                ("tmux:alder-work-two", "liveness", "present"),
            ]
        );
    }
}
