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
    /// The handle a probe execution was asked about; `None` for `list` runs,
    /// which take no argument.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
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

/// The observation changes one successful `list` run implies, as a pure
/// function of the folded state, so the crash harnesses apply exactly the
/// derivation production applies.
///
/// A `list` observer is generic: each row's subject passes through verbatim,
/// and a successful script is a complete snapshot for its observer, so
/// previously-established keys omitted from the snapshot retire. Execution
/// liveness is not a `list` field — it flows only through a `probe` observer
/// (see [`plan_probe_run`]) — so a `liveness` row in a list snapshot is not a
/// statement about work and plans nothing.
pub fn plan_observer_run(
    state: &ProjectState,
    kind: &str,
    normalized: &[NormalizedObject],
) -> Vec<ObservationChange> {
    let mut changes = Vec::new();
    let mut reported = BTreeSet::new();
    for object in normalized {
        if object.field == "liveness" {
            continue;
        }
        let key = ObservationKey {
            observer: kind.to_owned(),
            subject: object.subject.clone(),
            field: object.field.clone(),
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
        changes.push(ObservationChange {
            key: key.clone(),
            level: None,
        });
    }
    changes
}

/// The handles one probe sweep must ask about, as a pure function of the
/// folded state:
///
/// - every Starting/Active attempt's bound handle — so a worker that died
///   before it was ever observed still becomes an explicit `absent` level;
/// - every handle bound to an ended attempt that still has a current
///   liveness key under this observer — so an execution outliving its
///   attempt stays observed until it is gone, and `orphan` can surface.
pub fn probe_targets(state: &ProjectState, kind: &str) -> Vec<String> {
    let mut targets = BTreeSet::new();
    for attempt in state.attempts.values() {
        let Some(handle) = attempt.handle.as_deref() else {
            continue;
        };
        let active = matches!(attempt.state, AttemptState::Starting | AttemptState::Active);
        let key = ObservationKey {
            observer: kind.to_owned(),
            subject: attempt.id.clone(),
            field: "liveness".to_owned(),
        };
        if active || state.observations.contains_key(&key) {
            targets.insert(handle.to_owned());
        }
    }
    targets.into_iter().collect()
}

/// The observation changes one successful probe sweep implies, as a pure
/// function of the folded state, shared with the crash harnesses like
/// [`plan_observer_run`].
///
/// Each normalized row is one probe answer: the subject is the opaque handle
/// exactly as the runner bound it, the level one of `present`, `absent`, or
/// `unknown`. The plan matches handles against attempts by equality — never
/// by parsing — and writes levels keyed `(observer, attempt-id, liveness)`:
///
/// - active attempt, `present` or `absent`: report that level — `absent`
///   establishes the key even when none existed, because a dead worker is a
///   statement the fold must carry;
/// - active attempt, `unknown`: write nothing — the fold keeps saying
///   `observation_unknown`, which is the honest verdict;
/// - ended attempt with a current key, `present`: the level stays, so
///   `reconcile` can name the `orphan`;
/// - ended attempt with a current key, `absent`: retire the key;
/// - ended attempt with a current key, `unknown`: retire the key — the probe
///   no longer recognizes the name, and an ended attempt cannot be watched
///   forever, so the key would otherwise be unfalsifiable.
///
/// When an ended and an active attempt hold the same handle string (legal
/// sequentially — respawns reuse session names), the active attempt owns the
/// probe answer and the ended attempt's key retires. Any other key under this
/// observer's name — a liveness key naming no attempt-with-handle, or a
/// non-liveness leftover — is not a statement the probe can renew and
/// retires.
pub fn plan_probe_run(
    state: &ProjectState,
    kind: &str,
    normalized: &[NormalizedObject],
) -> Vec<ObservationChange> {
    let answers: std::collections::BTreeMap<&str, &str> = normalized
        .iter()
        .filter(|object| object.field == "liveness")
        .map(|object| (object.subject.as_str(), object.level.as_str()))
        .collect();
    let active_handles: BTreeSet<&str> = state
        .attempts
        .values()
        .filter(|attempt| matches!(attempt.state, AttemptState::Starting | AttemptState::Active))
        .filter_map(|attempt| attempt.handle.as_deref())
        .collect();
    let mut changes = Vec::new();
    let mut owned = BTreeSet::new();
    for attempt in state.attempts.values() {
        let Some(handle) = attempt.handle.as_deref() else {
            continue;
        };
        let key = ObservationKey {
            observer: kind.to_owned(),
            subject: attempt.id.clone(),
            field: "liveness".to_owned(),
        };
        owned.insert(key.clone());
        if matches!(attempt.state, AttemptState::Starting | AttemptState::Active) {
            // Anything else — `unknown`, or a handle this sweep did not
            // cover because the attempt appeared after targets were read —
            // writes nothing.
            if let Some(level @ ("present" | "absent")) = answers.get(handle).copied() {
                changes.push(ObservationChange {
                    key,
                    level: Some(level.to_owned()),
                });
            }
        } else {
            if !state.observations.contains_key(&key) {
                continue;
            }
            if active_handles.contains(handle) {
                // A live attempt holds the same handle string now, so the
                // probe answer belongs to it; this ended attempt's key is no
                // longer about anything observable.
                changes.push(ObservationChange { key, level: None });
                continue;
            }
            match answers.get(handle).copied() {
                Some("present") => changes.push(ObservationChange {
                    key,
                    level: Some("present".to_owned()),
                }),
                Some(_) => changes.push(ObservationChange { key, level: None }),
                // Not probed: the state moved since targets were read; the
                // next sweep will cover it.
                None => {}
            }
        }
    }
    for key in state
        .observations
        .keys()
        .filter(|key| key.observer == kind && !owned.contains(*key))
    {
        changes.push(ObservationChange {
            key: key.clone(),
            level: None,
        });
    }
    changes
}

/// Run every configured observer. The returned normalized reports are
/// deliberately not folded here: the application append path owns newness
/// and is the sole writer of the durable observation picture. The folded
/// state supplies each probe observer's targets — the handles worth asking
/// about — and nothing else.
pub fn observe(
    observers: &[ObserverConfig],
    state: &ProjectState,
) -> Result<Vec<ObserverRunResult>> {
    let mut runs = Vec::new();
    for observer in observers {
        let run = run_observer(observer, state, EXECUTION_TIMEOUT, MAX_EXECUTIONS)?;
        runs.push(run);
    }
    Ok(runs)
}

pub fn diagnose(observer: &ObserverConfig, state: &ProjectState) -> Result<ObserverRunResult> {
    run_observer(observer, state, EXECUTION_TIMEOUT, MAX_EXECUTIONS)
}

fn run_observer(
    observer: &ObserverConfig,
    state: &ProjectState,
    timeout: Duration,
    max_executions: usize,
) -> Result<ObserverRunResult> {
    match observer.probe.as_deref() {
        Some(probe) => {
            let targets = probe_targets(state, &observer.observer);
            run_probe_observer(observer, probe, &targets, timeout, max_executions)
        }
        None => run_list_observer(observer, timeout, max_executions),
    }
}

fn run_list_observer(
    observer: &ObserverConfig,
    timeout: Duration,
    max_executions: usize,
) -> Result<ObserverRunResult> {
    let mut executions = Vec::new();
    let mut normalized = Vec::new();
    let mut success = false;
    for number in 1..=max_executions {
        let (execution, objects) =
            execute_once(observer.command(), None, number, timeout, validate_output)?;
        executions.push(execution);
        if let Some(objects) = objects {
            normalized = objects;
            success = true;
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

/// Ask the probe about each target handle, one execution per invocation,
/// with the same retry budget per handle a `list` run has in total. The
/// sweep is all-or-nothing: a handle the probe cannot answer for after all
/// retries fails the run, so a partial sweep never masquerades as coverage.
fn run_probe_observer(
    observer: &ObserverConfig,
    probe: &str,
    targets: &[String],
    timeout: Duration,
    max_executions: usize,
) -> Result<ObserverRunResult> {
    let mut executions = Vec::new();
    let mut normalized = Vec::new();
    let mut success = true;
    let mut number = 0;
    'targets: for handle in targets {
        for _ in 0..max_executions {
            number += 1;
            let (mut execution, answer) =
                execute_once(probe, Some(handle), number, timeout, validate_probe_output)?;
            execution.subject = Some(handle.clone());
            let answered = answer.is_some();
            executions.push(execution);
            if let Some(answer) = answer {
                normalized.push(NormalizedObject {
                    subject: handle.clone(),
                    field: "liveness".to_owned(),
                    level: answer,
                });
            }
            if answered {
                continue 'targets;
            }
        }
        success = false;
        break;
    }
    Ok(ObserverRunResult {
        kind: observer.observer.clone(),
        success,
        executions,
        normalized,
        observed_at: Utc::now().to_rfc3339(),
    })
}

fn execute_once<T>(
    script: &str,
    argument: Option<&str>,
    number: usize,
    timeout: Duration,
    validate: impl Fn(&[u8]) -> Result<T>,
) -> Result<(ExecutionResult, Option<T>)> {
    let started = Instant::now();
    let mut command = Command::new("/bin/bash");
    command.args(["-o", "pipefail", "-c", script]);
    if let Some(argument) = argument {
        // The probed handle rides in as `$1` — a real argument, never spliced
        // into the command string, so no handle content is shell-interpreted.
        command.args(["alder-probe", argument]);
    }
    command
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
                subject: None,
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
                subject: None,
                duration_ms,
                exit_code: status.code(),
                timed_out: false,
                stderr,
                validation_error: Some("command exited nonzero".to_owned()),
            },
            None,
        ));
    }
    match validate(&stdout) {
        Ok(validated) => Ok((
            ExecutionResult {
                number,
                subject: None,
                duration_ms,
                exit_code: status.code(),
                timed_out: false,
                stderr,
                validation_error: None,
            },
            Some(validated),
        )),
        Err(error) => Ok((
            ExecutionResult {
                number,
                subject: None,
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

/// A probe answers with exactly one word — `present`, `absent`, or
/// `unknown` — surrounded by nothing but whitespace. Anything else is an
/// invalid execution and retries like malformed `list` output.
fn validate_probe_output(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| AlderError::new("invalid_observation", "a probe answer must be UTF-8 text"))?;
    match text.trim() {
        answer @ ("present" | "absent" | "unknown") => Ok(answer.to_owned()),
        other => Err(AlderError::with_context(
            "invalid_observation",
            "a probe must answer exactly one of `present`, `absent`, or `unknown`",
            json!({"answer": bounded(other.as_bytes(), 80)}),
        )),
    }
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
                            // The repair is a provider action Alder never
                            // performs and cannot spell without parsing the
                            // handle, so the suggestion names the execution
                            // verbatim and leaves the kill to its runner.
                            suggested_command: Some(format!(
                                "kill the runner execution named `{handle}`, then `alder refresh`"
                            )),
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
        path::{Path, PathBuf},
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

    fn list_observer(kind: &str, script: &str) -> ObserverConfig {
        ObserverConfig {
            observer: kind.to_owned(),
            list: Some(script.to_owned()),
            probe: None,
        }
    }

    fn probe_observer(kind: &str, script: &str) -> ObserverConfig {
        ObserverConfig {
            observer: kind.to_owned(),
            list: None,
            probe: Some(script.to_owned()),
        }
    }

    #[test]
    fn retries_invalid_results_and_accepts_first_valid_snapshot() {
        let temporary = TempDir::new().unwrap();
        let marker = temporary.path().join("count");
        let script = format!(
            "n=$(cat '{}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{}'; \
             if [ $n -lt 3 ]; then echo nope; else \
             echo '[{{\"subject\":\"owner/repo#171\",\"field\":\"ci\",\"level\":\"passing\"}}]'; fi",
            marker.display(),
            marker.display()
        );
        let observer = list_observer("github", &script);
        let state = ProjectState::default();
        let result = run_observer(&observer, &state, Duration::from_secs(1), 4).unwrap();
        assert!(result.success);
        assert_eq!(result.executions.len(), 3);
        assert_eq!(result.normalized[0].subject, "owner/repo#171");
        assert_eq!(result.normalized[0].level, "passing");
    }

    #[test]
    fn pipeline_failures_and_timeouts_retry_four_times() {
        let state = ProjectState::default();
        let pipeline = list_observer("tmux", "false | true");
        let failed = run_observer(&pipeline, &state, Duration::from_secs(1), 4).unwrap();
        assert!(!failed.success);
        assert_eq!(failed.executions.len(), 4);
        assert!(
            failed
                .executions
                .iter()
                .all(|execution| execution.exit_code == Some(1))
        );

        let timeout = list_observer("tmux", "sleep 10");
        let timed_out = run_observer(&timeout, &state, Duration::from_millis(30), 4).unwrap();
        assert!(!timed_out.success);
        assert_eq!(timed_out.executions.len(), 4);
        assert!(
            timed_out
                .executions
                .iter()
                .all(|execution| execution.timed_out)
        );
    }

    /// The probe is invoked once per relevant handle, with the handle riding
    /// in as `$1` rather than spliced into the command string, and each
    /// answer becomes one liveness row about that exact handle.
    #[test]
    fn a_probe_is_asked_about_each_target_handle_as_its_argument() {
        let mut state = ProjectState::default();
        state.attempts.insert(
            "active".to_owned(),
            attempt("active", AttemptState::Active, Some("tmux:worker")),
        );
        state.attempts.insert(
            "ended".to_owned(),
            attempt("ended", AttemptState::Ended, Some("tmux:done")),
        );
        with_observations(
            &mut state,
            &[observation("tmux", "ended", "liveness", "present")],
        );

        let observer = probe_observer(
            "tmux",
            "case \"$1\" in tmux:worker) echo present;; tmux:done) echo '  absent  ';; *) echo unknown;; esac",
        );
        let result = run_observer(&observer, &state, Duration::from_secs(1), 4).unwrap();
        assert!(result.success);
        assert_eq!(
            result
                .normalized
                .iter()
                .map(|row| (row.subject.as_str(), row.field.as_str(), row.level.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("tmux:done", "liveness", "absent"),
                ("tmux:worker", "liveness", "present"),
            ]
        );
        assert_eq!(
            result
                .executions
                .iter()
                .map(|execution| execution.subject.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("tmux:done"), Some("tmux:worker")]
        );
    }

    /// A handle the probe cannot answer for after every retry fails the whole
    /// run: a partial sweep must never masquerade as coverage.
    #[test]
    fn an_unanswerable_probe_fails_the_run_after_its_retries() {
        let mut state = ProjectState::default();
        state.attempts.insert(
            "active".to_owned(),
            attempt("active", AttemptState::Active, Some("tmux:worker")),
        );
        let observer = probe_observer("tmux", "echo maybe");
        let result = run_observer(&observer, &state, Duration::from_secs(1), 4).unwrap();
        assert!(!result.success);
        assert_eq!(result.executions.len(), 4);
        assert!(result.normalized.is_empty());

        // No targets means nothing to ask; the sweep is trivially complete.
        let empty = run_observer(
            &probe_observer("tmux", "echo present"),
            &ProjectState::default(),
            Duration::from_secs(1),
            4,
        )
        .unwrap();
        assert!(empty.success);
        assert!(empty.executions.is_empty());
        assert!(empty.normalized.is_empty());
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

        for (answer, expected) in [
            ("present", Some("present")),
            ("  absent\n", Some("absent")),
            ("unknown", Some("unknown")),
            ("gone", None),
            ("", None),
            ("present absent", None),
        ] {
            assert_eq!(
                validate_probe_output(answer.as_bytes()).ok().as_deref(),
                expected,
                "{answer:?}"
            );
        }
        assert!(validate_probe_output(&[0xff]).is_err());

        assert_eq!(bounded(b"", 0), "");
        assert_eq!(bounded(b"a", 0), "…");
        assert_eq!(bounded(b"abc", 3), "abc");
        assert_eq!(bounded(b"abcd", 3), "abc…");
        assert_eq!(bounded(&[0xff, b'a'], 1), "�…");

        assert!(handle_kill_result(Ok(())).is_ok());
        assert!(handle_kill_result(Err(rustix::io::Errno::SRCH)).is_ok());
        assert!(handle_kill_result(Err(rustix::io::Errno::PERM)).is_err());
    }

    fn liveness_key(kind: &str, attempt_id: &str) -> ObservationKey {
        ObservationKey {
            observer: kind.to_owned(),
            subject: attempt_id.to_owned(),
            field: "liveness".to_owned(),
        }
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

    /// Liveness belongs to the probe contract. A `list` snapshot that claims
    /// it plans nothing for those rows, and its complete-snapshot semantics
    /// retire whatever the snapshot no longer covers — including a stale
    /// liveness key such a generic observer once held.
    #[test]
    fn a_list_snapshot_plans_no_liveness_and_retires_omitted_keys() {
        let mut state = ProjectState::default();
        state.attempts.insert(
            "active".to_owned(),
            attempt("active", AttemptState::Active, Some("tmux:worker")),
        );
        with_observations(
            &mut state,
            &[
                observation("github", "owner/repo#171", "ci", "running"),
                observation("github", "active", "liveness", "present"),
            ],
        );
        let changes = plan_observer_run(
            &state,
            "github",
            &[level("tmux:worker", "liveness", "present")],
        );
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().all(|change| change.level.is_none()));

        // Another observer's keys are not this observer's to retire.
        assert!(plan_observer_run(&state, "nimbus", &[]).is_empty());
    }

    /// Probe targets are the handles worth asking about: every live
    /// attempt's, and every ended attempt's while its liveness key is still
    /// current. A handle shared across an ended and a live attempt is asked
    /// about once.
    #[test]
    fn probe_targets_cover_live_attempts_and_lingering_ended_keys() {
        let mut state = ProjectState::default();
        state.attempts.insert(
            "active".to_owned(),
            attempt("active", AttemptState::Active, Some("tmux:worker")),
        );
        state.attempts.insert(
            "never-observed".to_owned(),
            attempt("never-observed", AttemptState::Starting, Some("tmux:fresh")),
        );
        state.attempts.insert(
            "unbound".to_owned(),
            attempt("unbound", AttemptState::Starting, None),
        );
        state.attempts.insert(
            "ended-with-key".to_owned(),
            attempt("ended-with-key", AttemptState::Ended, Some("tmux:orphan")),
        );
        state.attempts.insert(
            "ended-quiet".to_owned(),
            attempt("ended-quiet", AttemptState::Ended, Some("tmux:gone")),
        );
        state.attempts.insert(
            "ended-shared".to_owned(),
            attempt("ended-shared", AttemptState::Ended, Some("tmux:worker")),
        );
        with_observations(
            &mut state,
            &[
                observation("tmux", "ended-with-key", "liveness", "present"),
                observation("tmux", "ended-shared", "liveness", "present"),
            ],
        );
        assert_eq!(
            probe_targets(&state, "tmux"),
            vec!["tmux:fresh", "tmux:orphan", "tmux:worker"]
        );
        // Another observer holds no keys here, so only live handles remain.
        assert_eq!(
            probe_targets(&state, "nimbus"),
            vec!["tmux:fresh", "tmux:worker"]
        );
    }

    /// The complete probe answer table. Levels are keyed by attempt ID; the
    /// handle is matched by equality and never parsed.
    #[test]
    fn probe_answers_map_to_attempt_keyed_levels_and_retirements() {
        let mut state = ProjectState::default();
        state.attempts.insert(
            "active".to_owned(),
            attempt("active", AttemptState::Active, Some("tmux:worker")),
        );

        // active + present → present.
        assert_eq!(
            plan_probe_run(
                &state,
                "tmux",
                &[level("tmux:worker", "liveness", "present")]
            ),
            vec![ObservationChange {
                key: liveness_key("tmux", "active"),
                level: Some("present".to_owned()),
            }]
        );
        // active + absent → absent, even with no established key: this is
        // how a worker that died before its first observation still becomes
        // a durable statement (the `missing` finding's ground truth).
        assert_eq!(
            plan_probe_run(
                &state,
                "tmux",
                &[level("tmux:worker", "liveness", "absent")]
            ),
            vec![ObservationChange {
                key: liveness_key("tmux", "active"),
                level: Some("absent".to_owned()),
            }]
        );
        // active + unknown → write nothing; reconcile keeps saying
        // observation_unknown, which is honest.
        assert!(
            plan_probe_run(
                &state,
                "tmux",
                &[level("tmux:worker", "liveness", "unknown")]
            )
            .is_empty()
        );

        // ended + present → the key stays present, so reconcile can name the
        // orphan. The unchanged level is quiet in the append path.
        let mut ended = ProjectState::default();
        ended.attempts.insert(
            "ended".to_owned(),
            attempt("ended", AttemptState::Ended, Some("tmux:orphan")),
        );
        with_observations(
            &mut ended,
            &[observation("tmux", "ended", "liveness", "present")],
        );
        assert_eq!(
            plan_probe_run(
                &ended,
                "tmux",
                &[level("tmux:orphan", "liveness", "present")]
            ),
            vec![ObservationChange {
                key: liveness_key("tmux", "ended"),
                level: Some("present".to_owned()),
            }]
        );
        // ended + absent → retire; ended + unknown → retire too, because an
        // ended attempt cannot be watched forever once the probe no longer
        // recognizes the name.
        for answer in ["absent", "unknown"] {
            assert_eq!(
                plan_probe_run(&ended, "tmux", &[level("tmux:orphan", "liveness", answer)]),
                vec![ObservationChange {
                    key: liveness_key("tmux", "ended"),
                    level: None,
                }],
                "{answer}"
            );
        }
        // An ended attempt with no current key needs nothing.
        let mut quiet = ended.clone();
        quiet.observations.clear();
        assert!(
            plan_probe_run(
                &quiet,
                "tmux",
                &[level("tmux:orphan", "liveness", "present")]
            )
            .is_empty()
        );
    }

    /// A handle string held by both an ended and a live attempt (legal
    /// sequentially — respawns reuse session names) belongs to the live one:
    /// the probe answer becomes its level and the ended attempt's key
    /// retires.
    #[test]
    fn the_active_attempt_owns_a_probe_answer_for_a_reused_handle() {
        let mut state = ProjectState::default();
        state.attempts.insert(
            "first".to_owned(),
            attempt("first", AttemptState::Ended, Some("tmux:worker")),
        );
        state.attempts.insert(
            "second".to_owned(),
            attempt("second", AttemptState::Active, Some("tmux:worker")),
        );
        with_observations(
            &mut state,
            &[observation("tmux", "first", "liveness", "present")],
        );
        let changes = plan_probe_run(
            &state,
            "tmux",
            &[level("tmux:worker", "liveness", "present")],
        );
        assert_eq!(
            changes,
            vec![
                ObservationChange {
                    key: liveness_key("tmux", "first"),
                    level: None,
                },
                ObservationChange {
                    key: liveness_key("tmux", "second"),
                    level: Some("present".to_owned()),
                },
            ]
        );
    }

    /// The real log holds observations from before the re-key: liveness and
    /// `attempt-id` levels whose subject is a session name. Neither is a
    /// statement the probe can renew, so the next successful sweep retires
    /// both while the live attempt's own key is reported.
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
        let changes = plan_probe_run(
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
        let orphan = reconcile(&orphaned, &configured, &known);
        assert_eq!(finding_kinds(&orphan), vec!["orphan"]);
        // The repair is the runner's kill; the suggestion names the
        // execution verbatim without parsing the handle.
        let suggestion = orphan[0].suggested_command.as_deref().unwrap();
        assert!(suggestion.contains("kill"), "{suggestion}");
        assert!(suggestion.contains("tmux:ended"), "{suggestion}");
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

    /// The shipped tmux observer is the probe: asked about one handle at a
    /// time, it answers `present` or `absent` for `tmux:*` names it owns and
    /// `unknown` for anything else — and when tmux itself is gone, a `tmux:*`
    /// name is `absent`, because no session can be running under it.
    #[test]
    fn the_tmux_observer_script_answers_one_probe_word_per_handle() {
        let temporary = TempDir::new().unwrap();
        let bin = temporary.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let stub = bin.join("tmux");
        fs::write(
            &stub,
            "#!/bin/sh\ncase \"$1 $2 $3\" in\n  'has-session -t =alder-work-one') exit 0 ;;\nesac\nexit 1\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&stub).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&stub, permissions).unwrap();

        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/observe-tmux.sh");
        let mut path_entries = vec![bin];
        path_entries.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
        let stubbed_path = env::join_paths(path_entries).unwrap();
        let probe = |path: &std::ffi::OsStr, handle: &str| {
            let output = Command::new("bash")
                .arg(&script)
                .arg(handle)
                .env("PATH", path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "probe failed for {handle}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            validate_probe_output(&output.stdout).unwrap()
        };
        assert_eq!(probe(&stubbed_path, "tmux:alder-work-one"), "present");
        assert_eq!(probe(&stubbed_path, "tmux:alder-work-two"), "absent");
        assert_eq!(probe(&stubbed_path, "codex:019f-rollout"), "unknown");

        // No tmux at all: a tmux: handle is absent, a foreign one unknown.
        let empty = temporary.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        let bare_path =
            env::join_paths([empty, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]).unwrap();
        assert_eq!(probe(&bare_path, "tmux:alder-work-one"), "absent");
        assert_eq!(probe(&bare_path, "codex:019f-rollout"), "unknown");
    }
}
