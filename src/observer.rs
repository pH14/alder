use std::{
    collections::{BTreeMap, BTreeSet},
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
    domain::{AttemptState, ProjectState, validate_handle},
    error::{AlderError, Result},
    projection::{
        ObservationRun, ObservationStatus, ObservedHandle, Projection, replace_observation_kind,
    },
};

const EXECUTION_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_EXECUTIONS: usize = 4;
const STDERR_LIMIT: usize = 4096;

#[derive(Debug, Clone, Serialize)]
pub struct RefreshResult {
    pub runs: Vec<ObserverRunResult>,
    pub present: usize,
    pub absent: usize,
    pub unknown: usize,
    pub unbound: Vec<ObservedHandle>,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedObject {
    pub value: String,
    #[serde(default)]
    pub attempt_id: Option<String>,
    #[serde(default = "empty_object")]
    pub metadata: Value,
}

pub fn refresh(
    projection: &Projection,
    observers: &[ObserverConfig],
    state: &ProjectState,
) -> Result<RefreshResult> {
    let existing = projection.observations()?;
    let durable: Vec<_> = state
        .attempts
        .values()
        .filter_map(|attempt| {
            attempt
                .handle
                .as_ref()
                .map(|handle| (handle.clone(), attempt.id.clone()))
        })
        .collect();
    let mut run_results = Vec::new();
    for observer in observers {
        let run = run_observer(observer, EXECUTION_TIMEOUT, MAX_EXECUTIONS)?;
        let handles = materialize_handles(observer, &run, &durable, &existing);
        let last = run.executions.last();
        let summary = ObservationRun {
            kind: observer.observer.clone(),
            success: run.success,
            executions: run.executions.len() as u32,
            duration_ms: run
                .executions
                .iter()
                .map(|execution| execution.duration_ms)
                .sum(),
            stderr: last
                .map(|execution| execution.stderr.clone())
                .unwrap_or_default(),
            validation_error: last.and_then(|execution| execution.validation_error.clone()),
            observed_at: run.observed_at.clone(),
            object_count: run.normalized.len(),
        };
        replace_observation_kind(projection, &observer.observer, &handles, &summary)?;
        run_results.push(run);
    }
    let observations = projection.observations()?;
    let bound: BTreeSet<_> = durable.iter().map(|(handle, _)| handle.as_str()).collect();
    Ok(RefreshResult {
        present: observations
            .iter()
            .filter(|handle| handle.status == ObservationStatus::Present)
            .count(),
        absent: observations
            .iter()
            .filter(|handle| handle.status == ObservationStatus::Absent)
            .count(),
        unknown: observations
            .iter()
            .filter(|handle| handle.status == ObservationStatus::Unknown)
            .count(),
        unbound: observations
            .iter()
            .filter(|handle| !bound.contains(handle.handle.as_str()))
            .cloned()
            .collect(),
        runs: run_results,
    })
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
    let mut values = BTreeSet::new();
    let mut attempts = BTreeSet::new();
    for object in &objects {
        if object.value.is_empty() {
            return Err(AlderError::new(
                "invalid_observation",
                "an observed value cannot be empty",
            ));
        }
        if !object.metadata.is_object() {
            return Err(AlderError::new(
                "invalid_observation",
                "observation metadata must be a JSON object",
            ));
        }
        if !values.insert(&object.value) {
            return Err(AlderError::with_context(
                "invalid_observation",
                format!("duplicate observed value `{}`", object.value),
                json!({"value": object.value}),
            ));
        }
        if let Some(attempt_id) = object.attempt_id.as_deref()
            && !attempts.insert(attempt_id)
        {
            return Err(AlderError::with_context(
                "invalid_observation",
                format!("attempt `{attempt_id}` appears on multiple observed values"),
                json!({"attempt_id": attempt_id}),
            ));
        }
    }
    Ok(objects)
}

fn materialize_handles(
    observer: &ObserverConfig,
    run: &ObserverRunResult,
    durable: &[(String, String)],
    existing: &[ObservedHandle],
) -> Vec<ObservedHandle> {
    let at = run.observed_at.clone();
    if run.success {
        let mut handles: BTreeMap<String, ObservedHandle> = run
            .normalized
            .iter()
            .map(|object| {
                let handle = format!("{}:{}", observer.observer, object.value);
                (
                    handle.clone(),
                    ObservedHandle {
                        handle,
                        attempt_id: object.attempt_id.clone(),
                        status: ObservationStatus::Present,
                        metadata: object.metadata.clone(),
                        observed_at: at.clone(),
                        detail: None,
                    },
                )
            })
            .collect();
        for (handle, _) in durable.iter().filter(|(handle, _)| {
            validate_handle(handle).is_ok_and(|(kind, _)| kind == observer.observer)
        }) {
            handles
                .entry(handle.clone())
                .or_insert_with(|| ObservedHandle {
                    handle: handle.clone(),
                    attempt_id: None,
                    status: ObservationStatus::Absent,
                    metadata: json!({}),
                    observed_at: at.clone(),
                    detail: None,
                });
        }
        handles.into_values().collect()
    } else {
        let detail = run
            .executions
            .last()
            .and_then(|execution| execution.validation_error.clone());
        let mut handles: BTreeMap<String, ObservedHandle> = existing
            .iter()
            .filter(|handle| {
                validate_handle(&handle.handle).is_ok_and(|(kind, _)| kind == observer.observer)
            })
            .map(|handle| {
                let mut handle = handle.clone();
                handle.status = ObservationStatus::Unknown;
                handle.observed_at = at.clone();
                handle.detail = detail.clone();
                (handle.handle.clone(), handle)
            })
            .collect();
        for (handle, _) in durable.iter().filter(|(handle, _)| {
            validate_handle(handle).is_ok_and(|(kind, _)| kind == observer.observer)
        }) {
            handles
                .entry(handle.clone())
                .or_insert_with(|| ObservedHandle {
                    handle: handle.clone(),
                    attempt_id: None,
                    status: ObservationStatus::Unknown,
                    metadata: json!({}),
                    observed_at: at.clone(),
                    detail: detail.clone(),
                });
        }
        handles.into_values().collect()
    }
}

fn empty_object() -> Value {
    json!({})
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

pub fn reconcile(
    state: &ProjectState,
    observations: &[ObservedHandle],
    configured: &BTreeSet<String>,
    known: &BTreeSet<String>,
) -> Vec<ReconcileFinding> {
    let by_handle: BTreeMap<_, _> = observations
        .iter()
        .map(|observation| (observation.handle.as_str(), observation))
        .collect();
    let attached: BTreeSet<_> = state
        .attempts
        .values()
        .filter_map(|attempt| attempt.handle.as_deref())
        .collect();
    let all_configured_known =
        !configured.is_empty() && configured.iter().all(|kind| known.contains(kind));
    let mut findings = Vec::new();

    for attempt in state.attempts.values() {
        let active = matches!(attempt.state, AttemptState::Starting | AttemptState::Active);
        if let Some(handle) = attempt.handle.as_deref() {
            let kind = validate_handle(handle).map(|(kind, _)| kind).unwrap_or("");
            if !configured.contains(kind) {
                if active {
                    findings.push(ReconcileFinding {
                        kind: "unconfigured".to_owned(),
                        attempt_id: Some(attempt.id.clone()),
                        handle: Some(handle.to_owned()),
                        status: "unknown".to_owned(),
                        detail: "no observation command is configured for this handle kind"
                            .to_owned(),
                        suggested_command: None,
                        metadata: json!({}),
                    });
                }
                continue;
            }
            match by_handle.get(handle).copied() {
                Some(observation) if observation.status == ObservationStatus::Present => {
                    if observation.attempt_id.as_deref() == Some(&attempt.id) {
                        if !active {
                            findings.push(ReconcileFinding {
                                kind: "orphan".to_owned(),
                                attempt_id: Some(attempt.id.clone()),
                                handle: Some(handle.to_owned()),
                                status: "present".to_owned(),
                                detail: "an ended attempt still has a live external handle"
                                    .to_owned(),
                                suggested_command: None,
                                metadata: observation.metadata.clone(),
                            });
                        }
                    } else {
                        findings.push(ReconcileFinding {
                            kind: "identity_mismatch".to_owned(),
                            attempt_id: Some(attempt.id.clone()),
                            handle: Some(handle.to_owned()),
                            status: "present".to_owned(),
                            detail: "the external object does not present this attempt ID"
                                .to_owned(),
                            suggested_command: None,
                            metadata: observation.metadata.clone(),
                        });
                    }
                }
                Some(observation) if observation.status == ObservationStatus::Absent && active => {
                    findings.push(ReconcileFinding {
                        kind: "missing".to_owned(),
                        attempt_id: Some(attempt.id.clone()),
                        handle: Some(handle.to_owned()),
                        status: "absent".to_owned(),
                        detail: "an active attempt's bound handle is confirmed absent".to_owned(),
                        suggested_command: Some(format!(
                            "alder edit attempt {} --end lost --why \"external handle absent\"",
                            attempt.id
                        )),
                        metadata: observation.metadata.clone(),
                    });
                }
                Some(observation) if observation.status == ObservationStatus::Unknown && active => {
                    findings.push(ReconcileFinding {
                        kind: "observation_unknown".to_owned(),
                        attempt_id: Some(attempt.id.clone()),
                        handle: Some(handle.to_owned()),
                        status: "unknown".to_owned(),
                        detail: "observation failed; no destructive repair is suggested".to_owned(),
                        suggested_command: None,
                        metadata: observation.metadata.clone(),
                    });
                }
                None if active => findings.push(ReconcileFinding {
                    kind: "observation_unknown".to_owned(),
                    attempt_id: Some(attempt.id.clone()),
                    handle: Some(handle.to_owned()),
                    status: "unknown".to_owned(),
                    detail: "no fresh observation is available".to_owned(),
                    suggested_command: None,
                    metadata: json!({}),
                }),
                _ => {}
            }
        } else if active {
            let matching: Vec<_> = observations
                .iter()
                .filter(|observation| {
                    observation.status == ObservationStatus::Present
                        && observation.attempt_id.as_deref() == Some(&attempt.id)
                })
                .collect();
            if matching.len() == 1 {
                let observation = matching[0];
                findings.push(ReconcileFinding {
                    kind: "bindable".to_owned(),
                    attempt_id: Some(attempt.id.clone()),
                    handle: Some(observation.handle.clone()),
                    status: "present".to_owned(),
                    detail: "a stamped external object matches the unbound attempt".to_owned(),
                    suggested_command: Some(format!(
                        "alder edit attempt {} --handle {}",
                        attempt.id, observation.handle
                    )),
                    metadata: observation.metadata.clone(),
                });
            } else if all_configured_known {
                findings.push(ReconcileFinding {
                    kind: "not_started".to_owned(),
                    attempt_id: Some(attempt.id.clone()),
                    handle: None,
                    status: "absent".to_owned(),
                    detail: "no configured observer found a stamped external object".to_owned(),
                    suggested_command: Some(format!(
                        "alder edit attempt {} --end not-started --why \"worker was not launched\"",
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
    for observation in observations.iter().filter(|observation| {
        observation.status == ObservationStatus::Present
            && !attached.contains(observation.handle.as_str())
    }) {
        if findings.iter().any(|finding| {
            finding.kind == "bindable" && finding.handle.as_deref() == Some(&observation.handle)
        }) {
            continue;
        }
        let collision = observation
            .attempt_id
            .as_deref()
            .and_then(|id| state.attempts.get(id))
            .is_some_and(|attempt| attempt.state == AttemptState::Ended);
        findings.push(ReconcileFinding {
            kind: if collision { "orphan" } else { "unclaimed" }.to_owned(),
            attempt_id: observation.attempt_id.clone(),
            handle: Some(observation.handle.clone()),
            status: "present".to_owned(),
            detail: if collision {
                "the observed object names an ended attempt"
            } else {
                "the observed object is not attached to an active attempt"
            }
            .to_owned(),
            suggested_command: None,
            metadata: observation.metadata.clone(),
        });
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
    use std::collections::{BTreeMap, BTreeSet};

    use tempfile::TempDir;

    use super::*;
    use crate::{
        domain::{Attempt, AttemptState},
        projection::Projection,
    };

    fn attempt(id: &str, state: AttemptState, handle: Option<&str>) -> Attempt {
        Attempt {
            id: id.to_owned(),
            work_id: format!("{id}-work"),
            state,
            outcome: None,
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

    fn observation(
        handle: &str,
        attempt_id: Option<&str>,
        status: ObservationStatus,
    ) -> ObservedHandle {
        ObservedHandle {
            handle: handle.to_owned(),
            attempt_id: attempt_id.map(ToOwned::to_owned),
            status,
            metadata: json!({"source": "test"}),
            observed_at: "2026-01-01T00:00:00Z".to_owned(),
            detail: None,
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
             if [ $n -lt 3 ]; then echo nope; else echo '[{{\"value\":\"one\"}}]'; fi",
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
        assert_eq!(result.normalized[0].value, "one");
    }

    #[test]
    fn failed_refresh_marks_old_inventory_unknown() {
        let temporary = TempDir::new().unwrap();
        let projection = Projection::new(temporary.path().join("state.db"));
        let success = ObserverConfig {
            observer: "tmux".to_owned(),
            list: "echo '[{\"value\":\"one\"}]'".to_owned(),
        };
        refresh(&projection, &[success], &ProjectState::default()).unwrap();
        let failure = ObserverConfig {
            observer: "tmux".to_owned(),
            list: "exit 1".to_owned(),
        };
        refresh(&projection, &[failure], &ProjectState::default()).unwrap();
        assert_eq!(
            projection.observations().unwrap()[0].status,
            ObservationStatus::Unknown
        );
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
    fn refresh_counts_bound_unbound_absent_and_unknown_handles() {
        let temporary = TempDir::new().unwrap();
        let projection = Projection::new(temporary.path().join("state.db"));
        let mut state = ProjectState::default();
        state.attempts.insert(
            "attempt-one".to_owned(),
            attempt("attempt-one", AttemptState::Active, Some("tmux:missing")),
        );
        state.attempts.insert(
            "attempt-two".to_owned(),
            attempt("attempt-two", AttemptState::Active, Some("nimbus:other")),
        );
        let success = ObserverConfig {
            observer: "tmux".to_owned(),
            list: "printf '[{\"value\":\"extra\"}]'".to_owned(),
        };
        let refreshed = refresh(&projection, &[success], &state).unwrap();
        assert_eq!(refreshed.present, 1);
        assert_eq!(refreshed.absent, 1);
        assert_eq!(refreshed.unknown, 0);
        assert_eq!(refreshed.unbound.len(), 1);
        assert_eq!(refreshed.unbound[0].handle, "tmux:extra");
        let handles = projection.observations().unwrap();
        assert!(handles.iter().any(|handle| {
            handle.handle == "tmux:missing" && handle.status == ObservationStatus::Absent
        }));
        assert!(!handles.iter().any(|handle| handle.handle == "nimbus:other"));

        let nimbus = ObserverConfig {
            observer: "nimbus".to_owned(),
            list: "printf '[{\"value\":\"other\"}]'".to_owned(),
        };
        refresh(&projection, &[nimbus], &state).unwrap();
        let failed = ObserverConfig {
            observer: "tmux".to_owned(),
            list: "exit 1".to_owned(),
        };
        let refreshed = refresh(&projection, &[failed], &state).unwrap();
        assert_eq!(refreshed.present, 1);
        assert_eq!(refreshed.absent, 0);
        assert_eq!(refreshed.unknown, 2);
        let handles = projection.observations().unwrap();
        assert!(handles.iter().any(|handle| {
            handle.handle == "nimbus:other" && handle.status == ObservationStatus::Present
        }));
    }

    #[test]
    fn output_validation_and_bounding_cover_exact_boundaries() {
        assert!(validate_output(br#"[{"value":"one","metadata":{}}]"#).is_ok());
        let stamped = validate_output(br#"[{"value":"one","attempt_id":"attempt-one"}]"#).unwrap();
        assert_eq!(stamped[0].attempt_id.as_deref(), Some("attempt-one"));
        for invalid in [
            br#"not json"#.as_slice(),
            br#"[{"value":""}]"#.as_slice(),
            br#"[{"value":"one","metadata":[]}]"#.as_slice(),
            br#"[{"value":"one"},{"value":"one"}]"#.as_slice(),
            br#"[{"value":"one","attempt_id":"a"},{"value":"two","attempt_id":"a"}]"#.as_slice(),
        ] {
            assert!(validate_output(invalid).is_err());
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
    fn reconciliation_classifies_bound_attempt_states() {
        let configured = BTreeSet::from(["tmux".to_owned()]);
        let known = configured.clone();

        let mut state = ProjectState::default();
        state.attempts.insert(
            "active".to_owned(),
            attempt("active", AttemptState::Active, Some("tmux:active")),
        );
        assert_eq!(
            finding_kinds(&reconcile(
                &state,
                &[observation(
                    "tmux:active",
                    Some("active"),
                    ObservationStatus::Present
                )],
                &configured,
                &known,
            )),
            Vec::<&str>::new()
        );
        assert_eq!(
            finding_kinds(&reconcile(
                &state,
                &[observation(
                    "tmux:active",
                    Some("other"),
                    ObservationStatus::Present
                )],
                &configured,
                &known,
            )),
            vec!["identity_mismatch"]
        );
        let missing = reconcile(
            &state,
            &[observation(
                "tmux:active",
                Some("active"),
                ObservationStatus::Absent,
            )],
            &configured,
            &known,
        );
        assert_eq!(finding_kinds(&missing), vec!["missing"]);
        assert!(missing[0].suggested_command.is_some());
        let unknown = reconcile(
            &state,
            &[observation(
                "tmux:active",
                Some("active"),
                ObservationStatus::Unknown,
            )],
            &configured,
            &known,
        );
        assert_eq!(finding_kinds(&unknown), vec!["observation_unknown"]);
        assert!(unknown[0].suggested_command.is_none());
        assert_eq!(
            finding_kinds(&reconcile(&state, &[], &configured, &known)),
            vec!["observation_unknown"]
        );

        let mut ended = ProjectState::default();
        ended.attempts.insert(
            "ended".to_owned(),
            attempt("ended", AttemptState::Ended, Some("tmux:ended")),
        );
        assert_eq!(
            finding_kinds(&reconcile(
                &ended,
                &[observation(
                    "tmux:ended",
                    Some("ended"),
                    ObservationStatus::Present
                )],
                &configured,
                &known,
            )),
            vec!["orphan"]
        );
        for status in [ObservationStatus::Absent, ObservationStatus::Unknown] {
            assert!(
                reconcile(
                    &ended,
                    &[observation("tmux:ended", Some("ended"), status)],
                    &configured,
                    &known,
                )
                .is_empty()
            );
        }
        assert!(reconcile(&ended, &[], &configured, &known).is_empty());

        assert_eq!(
            finding_kinds(&reconcile(&state, &[], &BTreeSet::new(), &BTreeSet::new(),)),
            vec!["unconfigured"]
        );
        assert!(reconcile(&ended, &[], &BTreeSet::new(), &BTreeSet::new(),).is_empty());
    }

    #[test]
    fn reconciliation_classifies_unbound_and_unclaimed_objects() {
        let configured = BTreeSet::from(["tmux".to_owned()]);
        let known = configured.clone();
        let mut state = ProjectState::default();
        state.attempts.insert(
            "active".to_owned(),
            attempt("active", AttemptState::Starting, None),
        );

        let bindable = reconcile(
            &state,
            &[
                observation("tmux:worker", Some("active"), ObservationStatus::Present),
                observation("tmux:extra", None, ObservationStatus::Present),
            ],
            &configured,
            &known,
        );
        assert_eq!(finding_kinds(&bindable), vec!["unclaimed", "bindable"]);
        assert!(
            bindable
                .iter()
                .find(|finding| finding.kind == "bindable")
                .unwrap()
                .suggested_command
                .is_some()
        );
        assert_eq!(
            finding_kinds(&reconcile(
                &state,
                &[observation(
                    "tmux:absent",
                    Some("active"),
                    ObservationStatus::Absent,
                )],
                &configured,
                &known,
            )),
            vec!["not_started"]
        );

        assert_eq!(
            finding_kinds(&reconcile(&state, &[], &configured, &known)),
            vec!["not_started"]
        );
        assert_eq!(
            finding_kinds(&reconcile(&state, &[], &configured, &BTreeSet::new(),)),
            vec!["unbound"]
        );
        assert_eq!(
            finding_kinds(&reconcile(
                &ProjectState::default(),
                &[observation("tmux:extra", None, ObservationStatus::Present)],
                &configured,
                &known,
            )),
            vec!["unclaimed"]
        );

        let mut ended = ProjectState::default();
        ended.attempts.insert(
            "ended".to_owned(),
            attempt("ended", AttemptState::Ended, None),
        );
        assert_eq!(
            finding_kinds(&reconcile(
                &ended,
                &[observation(
                    "tmux:orphan",
                    Some("ended"),
                    ObservationStatus::Present
                )],
                &configured,
                &known,
            )),
            vec!["orphan"]
        );
        assert!(
            reconcile(
                &ProjectState::default(),
                &[observation("tmux:absent", None, ObservationStatus::Absent)],
                &configured,
                &known,
            )
            .is_empty()
        );
    }
}
