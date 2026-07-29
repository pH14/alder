use std::{
    any::Any,
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind, panic_any, resume_unwind},
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use alder::{
    domain::{
        AttemptDefinition, AttemptOutcome, AttemptState, EventDraft, EventPayload, PassDefinition,
        PassOutcome, PassState, PassTrigger, ProjectState, Snapshot, WorkDefinition, WorkOperation,
        decode_record, encode_draft,
    },
    observer::{ReconcileFinding, reconcile},
    projection::{ObservationStatus, ObservedHandle},
};
use alder_log::{Log, LogError, MemoryLog};
use alderd::{
    config::Engine,
    decide::{Decision, Poll, decide},
    driver::Driver,
    effects::Effects,
    error::{DriverError, Result},
    loop_state::LoopState,
    spawn::{Run, SpawnHost, spawn},
    tier::tier,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

const ROOT: &str = "/sim/alder";
const WORK_ID: &str = "al-sim";
const LEADER_SESSION: &str = "alder-leader";
const MAX_RECOVERY_ROUNDS: usize = 96;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentScript {
    Complete,
    DieMidPass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionKind {
    Leader,
    Worker,
}

#[derive(Debug, Clone)]
struct Session {
    kind: SessionKind,
    cwd: PathBuf,
    attempt_id: Option<String>,
    injected_pass: Option<String>,
    script: AgentScript,
}

#[derive(Debug, Clone)]
struct Worktree {
    branch: String,
}

#[derive(Debug)]
struct SimCrash {
    ordinal: usize,
    boundary: String,
}

#[derive(Debug)]
struct World {
    tick: i64,
    next_event: u64,
    boundary: usize,
    faults: VecDeque<usize>,
    trace: Vec<String>,
    sessions: BTreeMap<String, Session>,
    worktrees: BTreeMap<PathBuf, Worktree>,
    branches: BTreeSet<String>,
    directories: BTreeSet<PathBuf>,
    files: BTreeSet<PathBuf>,
    next_agent: AgentScript,
    notices: Vec<String>,
    messages: Vec<String>,
}

impl World {
    fn new(seed: u64) -> Self {
        Self {
            tick: 1_800_000_000 + (seed % 10_000) as i64,
            next_event: 1,
            boundary: 0,
            faults: VecDeque::new(),
            trace: Vec::new(),
            sessions: BTreeMap::new(),
            worktrees: BTreeMap::new(),
            branches: BTreeSet::from(["main".to_owned()]),
            directories: BTreeSet::new(),
            files: BTreeSet::new(),
            next_agent: AgentScript::Complete,
            notices: Vec::new(),
            messages: Vec::new(),
        }
    }
}

struct Shared {
    log: MemoryLog,
    world: RefCell<World>,
}

/// A deterministic, entirely in-memory implementation of both production
/// effect traits.
///
/// The domain dependency is deliberately confined to this integration-test
/// module. `alderd` itself continues to know Alder only through JSON.
#[derive(Clone)]
pub struct SimHost {
    root: PathBuf,
    shared: Rc<Shared>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub state: String,
    pub sessions: Vec<String>,
    pub worktrees: Vec<String>,
    pub trace: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Operation {
    SpawnWorker,
    RestartDaemon,
    PollDaemon,
    LeaderDiesMidPass,
    Tick(u8),
}

#[derive(Debug, Clone)]
pub struct Case {
    pub seed: u64,
    pub operations: Vec<Operation>,
    /// One-based effect ordinals. A fault kills the current process after the
    /// named effect has taken place.
    pub fault_schedule: Vec<usize>,
}

impl SimHost {
    pub fn new(seed: u64) -> Self {
        let host = Self {
            root: PathBuf::from(ROOT),
            shared: Rc::new(Shared {
                log: MemoryLog::new(),
                world: RefCell::new(World::new(seed)),
            }),
        };
        host.append_unfaulted(EventPayload::WorkChanged {
            why: None,
            operations: vec![WorkOperation::Add {
                work: WorkDefinition {
                    id: WORK_ID.to_owned(),
                    title: "simulated worker".to_owned(),
                    spec: Some("crash anywhere".to_owned()),
                    priority: 1,
                    requires: Vec::new(),
                    checks: Vec::new(),
                },
            }],
        });
        host.append_unfaulted(EventPayload::LoopEngineSelected {
            engine: "stub".to_owned(),
        });
        host.reset_boundaries(Vec::new());
        host
    }

    pub fn reset_boundaries(&self, faults: Vec<usize>) {
        assert!(
            faults.iter().all(|distance| *distance > 0),
            "fault distances are one-based"
        );
        let mut world = self.shared.world.borrow_mut();
        world.boundary = 0;
        world.faults = faults.into();
        world.trace.clear();
    }

    pub fn trace(&self) -> Vec<String> {
        self.shared.world.borrow().trace.clone()
    }

    pub fn boundary_count(&self) -> usize {
        self.shared.world.borrow().boundary
    }

    pub fn remaining_faults(&self) -> Vec<usize> {
        self.shared.world.borrow().faults.iter().copied().collect()
    }

    pub fn set_next_agent(&self, script: AgentScript) {
        self.shared.world.borrow_mut().next_agent = script;
    }

    pub fn advance(&self, ticks: u64) {
        self.shared.world.borrow_mut().tick += ticks as i64;
    }

    pub fn snapshot(&self) -> Snapshot {
        let head = self.shared.log.head().expect("the memory log has a head");
        let events = self
            .shared
            .log
            .read_all(&head)
            .expect("the memory log is readable")
            .iter()
            .map(decode_record)
            .collect::<alder::error::Result<Vec<_>>>()
            .expect("the simulated log contains Alder events");
        let state = ProjectState::fold(&events).expect("production fold accepts the simulated log");
        Snapshot {
            head,
            events,
            state,
        }
    }

    pub fn stale_cas_is_rejected(&self) -> bool {
        let expected = self.shared.log.head().expect("head");
        let one = self.draft(
            "cas-one".to_owned(),
            EventPayload::LoopNudgeRequested { why: None },
        );
        let two = self.draft(
            "cas-two".to_owned(),
            EventPayload::LoopRotationRequested { why: None },
        );
        self.shared
            .log
            .append(&expected, &encode_draft(&one).expect("draft"))
            .expect("first CAS append");
        matches!(
            self.shared
                .log
                .append(&expected, &encode_draft(&two).expect("draft")),
            Err(LogError::HeadConflict { .. })
        )
    }

    pub fn nudge(&self) {
        self.append_unfaulted(EventPayload::LoopNudgeRequested {
            why: Some("scripted interleaving".to_owned()),
        });
    }

    pub fn digest(&self) -> Digest {
        let snapshot = self.snapshot();
        let world = self.shared.world.borrow();
        let attempts = snapshot
            .state
            .attempts
            .values()
            .map(|attempt| {
                format!(
                    "{}:{:?}:{}",
                    attempt.id,
                    attempt.state,
                    attempt.handle.as_deref().unwrap_or("-")
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let passes = snapshot
            .state
            .passes
            .values()
            .map(|pass| format!("{}:{:?}:{:?}", pass.id, pass.state, pass.outcome))
            .collect::<Vec<_>>()
            .join(",");
        Digest {
            state: format!(
                "head={};attempts=[{attempts}];passes=[{passes}]",
                snapshot.head.sequence()
            ),
            sessions: world
                .sessions
                .iter()
                .map(|(name, session)| {
                    format!(
                        "{name}:{:?}:{}",
                        session.kind,
                        session.attempt_id.as_deref().unwrap_or("-")
                    )
                })
                .collect(),
            worktrees: world
                .worktrees
                .iter()
                .map(|(path, worktree)| format!("{}:{}", path.display(), worktree.branch))
                .collect(),
            trace: world.trace.clone(),
        }
    }

    pub fn decision(&self) -> Decision {
        let status = self.status_document();
        let state = LoopState::from_status(&status).expect("status is production-readable");
        decide(
            &config(),
            &state,
            &Poll {
                now: self.logical_now(),
                refresh_changed: false,
                pending_since: None,
                attached_client: false,
            },
        )
    }

    fn logical_now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp(self.shared.world.borrow().tick, 0)
            .expect("the logical tick is a timestamp")
    }

    fn draft(&self, id: String, payload: EventPayload) -> EventDraft {
        EventDraft {
            id,
            at: self.logical_now(),
            actor: "sim".to_owned(),
            payload,
            schema: "alder.event.v0".to_owned(),
        }
    }

    fn append_unfaulted(&self, payload: EventPayload) {
        self.append(payload).expect("the simulated event appends");
    }

    fn append(&self, payload: EventPayload) -> alder::error::Result<()> {
        let snapshot = self.snapshot();
        let id = {
            let mut world = self.shared.world.borrow_mut();
            let id = format!("sim-event-{:016x}", world.next_event);
            world.next_event += 1;
            id
        };
        let draft = self.draft(id, payload);
        let candidate = draft.materialize(snapshot.head.sequence() + 1);
        let mut state = snapshot.state;
        state.apply(&candidate)?;
        self.shared
            .log
            .append(&snapshot.head, &encode_draft(&draft)?)?;
        Ok(())
    }

    fn after(&self, boundary: impl Into<String>) {
        let boundary = boundary.into();
        let crash = {
            let mut world = self.shared.world.borrow_mut();
            world.boundary += 1;
            let ordinal = world.boundary;
            world.trace.push(format!("{ordinal}:{boundary}"));
            match world.faults.front_mut() {
                Some(distance) if *distance == 1 => {
                    world.faults.pop_front();
                    Some(SimCrash { ordinal, boundary })
                }
                Some(distance) => {
                    *distance -= 1;
                    None
                }
                None => None,
            }
        };
        if let Some(crash) = crash {
            panic_any(crash);
        }
    }

    fn status_document(&self) -> Value {
        let snapshot = self.snapshot();
        let state = &snapshot.state;
        let control = &state.loop_control;
        json!({
            "schema": "alder.status.v0",
            "head": snapshot.head.sequence(),
            "loop": {
                "paused": control.paused,
                "pause_reason": control.pause_reason,
                "engine": control.engine,
                "rotate_pending": control.rotate_pending(),
                "nudge_pending": control.nudge_pending(),
                "open_pass": state.open_pass().map(|pass| json!({
                    "id": pass.id,
                    "engine": pass.engine,
                    "handle": pass.handle,
                    "started_at": pass.started_at,
                })),
                "last_pass": state.last_ended_pass().map(|pass| json!({
                    "id": pass.id,
                    "outcome": pass.outcome.map(PassOutcome::as_str),
                    "wake_at": pass.wake_at,
                    "ended_at": pass.ended_at,
                    "ended_seq": pass.ended_seq,
                })),
            }
        })
    }

    fn observations(&self) -> Vec<ObservedHandle> {
        self.shared
            .world
            .borrow()
            .sessions
            .iter()
            // Production's tmux observer deliberately lists only worker
            // sessions. The leader is a pass handle, not an attempt handle.
            .filter(|(_, session)| session.kind == SessionKind::Worker)
            .map(|(name, session)| ObservedHandle {
                handle: format!("tmux:{name}"),
                attempt_id: session.attempt_id.clone(),
                status: ObservationStatus::Present,
                metadata: json!({"cwd": session.cwd}),
                observed_at: self.logical_now().to_rfc3339(),
                detail: None,
            })
            .collect()
    }

    fn reconcile(&self) -> Vec<ReconcileFinding> {
        let state = self.snapshot().state;
        let kinds = BTreeSet::from(["tmux".to_owned()]);
        reconcile(&state, &self.observations(), &kinds, &kinds)
    }

    fn anomalies(&self, want_worker: bool) -> Vec<String> {
        let snapshot = self.snapshot();
        let world = self.shared.world.borrow();
        let mut anomalies = Vec::new();
        for attempt in snapshot.state.attempts.values().filter(|attempt| {
            matches!(attempt.state, AttemptState::Starting | AttemptState::Active)
        }) {
            match attempt.handle.as_deref() {
                Some(handle) => {
                    let session = handle.strip_prefix("tmux:").and_then(|name| {
                        world
                            .sessions
                            .get(name)
                            .filter(|session| session.attempt_id.as_deref() == Some(&attempt.id))
                    });
                    if session.is_none() {
                        anomalies.push(format!("attempt:{}", attempt.id));
                    }
                }
                None => anomalies.push(format!("attempt:{}", attempt.id)),
            }
        }
        for (name, session) in &world.sessions {
            if session.kind == SessionKind::Worker {
                let described = session.attempt_id.as_deref().is_some_and(|attempt_id| {
                    snapshot
                        .state
                        .attempts
                        .get(attempt_id)
                        .is_some_and(|attempt| {
                            attempt.state == AttemptState::Active
                                && attempt.handle.as_deref() == Some(&format!("tmux:{name}"))
                        })
                });
                if !described {
                    anomalies.push(format!("session:{name}"));
                }
            }
        }
        for path in world.worktrees.keys() {
            let session = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| world.sessions.get(name));
            if session.is_none() {
                anomalies.push(format!("worktree:{}", path.display()));
            }
        }
        if want_worker
            && !snapshot.state.attempts.values().any(|attempt| {
                attempt.state == AttemptState::Active
                    && attempt.handle.as_deref() == Some("tmux:alder-work-al-sim")
            })
        {
            anomalies.push(format!("desired:{WORK_ID}"));
        }
        if snapshot
            .state
            .passes
            .values()
            .filter(|pass| pass.state == PassState::Open)
            .count()
            > 1
        {
            anomalies.push("passes:multiple-open".to_owned());
        }
        anomalies
    }

    fn assert_anomalies_named(&self, want_worker: bool, findings: &[ReconcileFinding]) {
        let anomalies = self.anomalies(want_worker);
        let local_findings = self.local_findings(want_worker);
        for anomaly in anomalies {
            let named = if let Some(id) = anomaly.strip_prefix("attempt:") {
                findings
                    .iter()
                    .any(|finding| finding.attempt_id.as_deref() == Some(id))
            } else if let Some(name) = anomaly.strip_prefix("session:") {
                findings
                    .iter()
                    .any(|finding| finding.handle.as_deref() == Some(&format!("tmux:{name}")))
            } else {
                local_findings
                    .iter()
                    .any(|(_, subject)| subject == &anomaly)
            };
            assert!(
                named,
                "world anomaly `{anomaly}` has no named finding; real={:?}; local={local_findings:?}",
                findings
                    .iter()
                    .map(|finding| (&finding.kind, &finding.attempt_id, &finding.handle))
                    .collect::<Vec<_>>()
            );
        }
    }

    fn local_findings(&self, want_worker: bool) -> Vec<(String, String)> {
        self.anomalies(want_worker)
            .into_iter()
            .filter_map(|anomaly| {
                if anomaly.starts_with("worktree:") {
                    Some(("stray_worktree".to_owned(), anomaly))
                } else if anomaly.starts_with("desired:") {
                    Some(("desired_worker_missing".to_owned(), anomaly))
                } else {
                    None
                }
            })
            .collect()
    }

    fn clean_strays(&self) -> Result<bool> {
        let stray: Vec<PathBuf> = {
            let world = self.shared.world.borrow();
            world
                .worktrees
                .keys()
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_none_or(|name| !world.sessions.contains_key(name))
                })
                .cloned()
                .collect()
        };
        for path in &stray {
            let rendered = path.display().to_string();
            let run =
                <Self as SpawnHost>::git(self, &["worktree", "remove", "--force", &rendered])?;
            if !run.ok {
                return Err(DriverError::new("simulated worktree cleanup failed"));
            }
        }
        Ok(!stray.is_empty())
    }

    fn repair_findings(&self, findings: &[ReconcileFinding]) -> Result<bool> {
        let mut changed = false;
        // External objects that cannot truthfully be bound are removed before
        // an unspawned attempt is retried on the same deterministic name.
        for finding in findings.iter().filter(|finding| {
            matches!(
                finding.kind.as_str(),
                "unclaimed" | "orphan" | "identity_mismatch"
            )
        }) {
            if let Some(session) = finding
                .handle
                .as_deref()
                .and_then(|handle| handle.strip_prefix("tmux:"))
            {
                <Self as SpawnHost>::tmux_kill_session(self, session)?;
                changed = true;
            }
        }
        changed |= self.clean_strays()?;

        for finding in findings {
            match finding.kind.as_str() {
                "bindable" => {
                    let attempt = finding.attempt_id.as_deref().expect("bindable attempt");
                    let handle = finding.handle.as_deref().expect("bindable handle");
                    self.alder_command(&["attempt", "edit", attempt, "--handle", handle])?;
                    changed = true;
                }
                "missing" => {
                    let attempt = finding.attempt_id.as_deref().expect("missing attempt");
                    self.alder_command(&[
                        "attempt",
                        "end",
                        attempt,
                        "--outcome",
                        "lost",
                        "--why",
                        "reconciler observed the worker absent",
                    ])?;
                    changed = true;
                }
                "unspawned" => {
                    spawn(
                        self,
                        WORK_ID,
                        tier("luna").expect("luna exists"),
                        Some("scripted-agent"),
                    )?;
                    changed = true;
                }
                _ => {}
            }
        }
        Ok(changed)
    }

    fn ensure_worker(&self, want_worker: bool) -> Result<bool> {
        if !want_worker {
            return Ok(false);
        }
        let state = self.snapshot().state;
        if state.attempts.values().any(|attempt| {
            attempt.state == AttemptState::Active
                && attempt.handle.as_deref() == Some("tmux:alder-work-al-sim")
        }) {
            return Ok(false);
        }
        spawn(
            self,
            WORK_ID,
            tier("luna").expect("luna exists"),
            Some("scripted-agent"),
        )?;
        Ok(true)
    }

    pub fn recover(&self, want_worker: bool) {
        let mut daemon = Driver::new(self.clone(), config());
        for _ in 0..MAX_RECOVERY_ROUNDS {
            let round = catch_sim_crash(|| {
                // observe
                let _observations = self.observations();
                // reconcile
                let findings = self.reconcile();
                self.assert_anomalies_named(want_worker, &findings);
                // decide (the production driver repeats this from the same
                // folded status immediately before applying its loop repair).
                let _decision = self.decision();
                // repair
                let mut changed = self.repair_findings(&findings)?;
                changed |= self.ensure_worker(want_worker)?;
                daemon.poll_once()?;
                Ok::<bool, DriverError>(changed)
            });
            match round {
                Some(Ok(_)) => {
                    let findings = self.reconcile();
                    let stable = findings.is_empty()
                        && self.anomalies(want_worker).is_empty()
                        && self.snapshot().state.open_pass().is_none()
                        && matches!(self.decision(), Decision::Idle(_))
                        && self.remaining_faults().is_empty();
                    if stable {
                        self.assert_invariant(want_worker);
                        return;
                    }
                }
                Some(Err(error)) => panic!("recovery failed: {error}"),
                None => {
                    // Process death forgets daemon-local session bookkeeping.
                    daemon = Driver::new(self.clone(), config());
                }
            }
        }
        panic!(
            "recovery did not reach a fixpoint; digest={:#?}, findings={:#?}",
            self.digest(),
            self.reconcile()
        );
    }

    pub fn assert_invariant(&self, want_worker: bool) {
        let snapshot = self.snapshot();
        let findings = self.reconcile();
        assert!(findings.is_empty(), "unreconciled findings: {findings:#?}");
        assert!(
            self.anomalies(want_worker).is_empty(),
            "stranded world state: {:?}",
            self.anomalies(want_worker)
        );
        assert!(
            snapshot
                .state
                .passes
                .values()
                .filter(|pass| pass.state == PassState::Open)
                .count()
                <= 1,
            "more than one pass is open"
        );
        assert!(
            snapshot.state.open_pass().is_none(),
            "the recovery fixpoint still has an open pass"
        );
        assert!(
            matches!(self.decision(), Decision::Idle(_)),
            "the recovery fixpoint still wants to fire: {:?}",
            self.decision()
        );
    }

    fn run_agent_if_ready(&self, pass_id: &str) {
        let script = {
            let world = self.shared.world.borrow();
            world.sessions.get(LEADER_SESSION).and_then(|session| {
                (session.injected_pass.as_deref() == Some(pass_id)).then_some(session.script)
            })
        };
        match script {
            Some(AgentScript::Complete) => {
                self.append_unfaulted(EventPayload::PassEnded {
                    pass_id: pass_id.to_owned(),
                    outcome: PassOutcome::Ok,
                    report: Some("scripted pass complete".to_owned()),
                    wake_at: None,
                    rotate: false,
                    why: None,
                });
                if let Some(session) = self
                    .shared
                    .world
                    .borrow_mut()
                    .sessions
                    .get_mut(LEADER_SESSION)
                {
                    session.injected_pass = None;
                }
                self.after("pass.end");
            }
            Some(AgentScript::DieMidPass) => {
                self.shared
                    .world
                    .borrow_mut()
                    .sessions
                    .remove(LEADER_SESSION);
                self.after("agent.die");
            }
            None => {}
        }
    }

    fn alder_command(&self, args: &[&str]) -> Result<Value> {
        let result = match args {
            ["show", id] if *id == WORK_ID => {
                let snapshot = self.snapshot();
                let work = snapshot
                    .state
                    .work
                    .get(*id)
                    .ok_or_else(|| DriverError::coded("not_found", "work not found"))?;
                Ok(json!({"current": {
                    "id": work.id,
                    "title": work.title,
                    "spec": work.spec,
                    "checks": work.checks,
                    "state": work.state.as_str(),
                }}))
            }
            ["show", id] if id.contains("-pass-") => {
                self.run_agent_if_ready(id);
                let snapshot = self.snapshot();
                let pass = snapshot
                    .state
                    .passes
                    .get(*id)
                    .ok_or_else(|| DriverError::coded("not_found", "pass not found"))?;
                Ok(json!({"current": {
                    "id": pass.id,
                    "state": match pass.state {
                        PassState::Open => "open",
                        PassState::Ended => "ended",
                    },
                    "outcome": pass.outcome.map(PassOutcome::as_str),
                }}))
            }
            ["status"] => Ok(self.status_document()),
            ["status", "--section", "in_flight"] => {
                let snapshot = self.snapshot();
                let in_flight: Vec<_> = snapshot
                    .state
                    .attempts
                    .values()
                    .filter(|attempt| {
                        matches!(attempt.state, AttemptState::Starting | AttemptState::Active)
                    })
                    .map(|attempt| {
                        json!({
                            "id": attempt.id,
                            "work_id": attempt.work_id,
                            "handle": attempt.handle,
                        })
                    })
                    .collect();
                Ok(json!({"in_flight": in_flight}))
            }
            ["refresh"] => Ok(json!({"changed": false})),
            ["work", "start", work_id] => {
                let snapshot = self.snapshot();
                let ordinal = snapshot
                    .state
                    .attempts
                    .values()
                    .filter(|attempt| attempt.work_id == *work_id)
                    .count()
                    + 1;
                let attempt_id = format!("{work_id}-attempt-{ordinal}");
                self.append(EventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
                        id: attempt_id.clone(),
                        work_id: (*work_id).to_owned(),
                        metadata: BTreeMap::new(),
                    },
                })
                .map_err(driver_error)?;
                Ok(json!({"attempt_id": attempt_id}))
            }
            ["attempt", "edit", attempt_id, rest @ ..] => {
                let handle = value_after(rest, "--handle")
                    .ok_or_else(|| DriverError::new("sim only supports binding attempt edits"))?;
                let metadata = rest
                    .windows(2)
                    .filter(|pair| pair[0] == "--meta")
                    .filter_map(|pair| pair[1].split_once('='))
                    .map(|(key, value)| (key.to_owned(), json!(value)))
                    .collect();
                self.append(EventPayload::AttemptBound {
                    attempt_id: (*attempt_id).to_owned(),
                    handle: handle.to_owned(),
                    metadata,
                })
                .map_err(driver_error)?;
                Ok(json!({"attempt_id": attempt_id}))
            }
            ["attempt", "end", attempt_id, rest @ ..] => {
                let outcome = match value_after(rest, "--outcome").unwrap_or("lost") {
                    "not-started" => AttemptOutcome::NotStarted,
                    "lost" => AttemptOutcome::Lost,
                    other => {
                        return Err(DriverError::new(format!(
                            "unsupported attempt outcome {other}"
                        )));
                    }
                };
                self.append(EventPayload::AttemptEnded {
                    attempt_id: (*attempt_id).to_owned(),
                    outcome,
                    why: value_after(rest, "--why")
                        .unwrap_or("simulated repair")
                        .to_owned(),
                })
                .map_err(driver_error)?;
                Ok(json!({"attempt_id": attempt_id}))
            }
            ["loop", "wake", rest @ ..] => {
                let snapshot = self.snapshot();
                if let Some(open) = snapshot.state.open_pass() {
                    return Err(DriverError::coded(
                        "pass_open",
                        format!("pass `{}` is open", open.id),
                    ));
                }
                let ordinal = snapshot.state.passes.len() + 1;
                let pass_id = format!("al-pass-{ordinal}");
                let engine = value_after(rest, "--engine").unwrap_or("stub").to_owned();
                let handle = value_after(rest, "--handle")
                    .unwrap_or("tmux:alder-leader")
                    .to_owned();
                let triggers = values_after(rest, "--trigger")
                    .into_iter()
                    .map(|trigger| match trigger {
                        "log" => PassTrigger::Log,
                        "observations" => PassTrigger::Observations,
                        "manual" => PassTrigger::Manual,
                        _ => PassTrigger::Due,
                    })
                    .collect();
                self.append(EventPayload::PassStarted {
                    pass: PassDefinition {
                        id: pass_id.clone(),
                        engine,
                        handle,
                        triggers,
                        at_head: snapshot.head.sequence(),
                    },
                })
                .map_err(driver_error)?;
                Ok(json!({"pass_id": pass_id}))
            }
            ["pass", "end", pass_id, rest @ ..] => {
                let outcome = match value_after(rest, "--outcome").unwrap_or("timeout") {
                    "crashed" => PassOutcome::Crashed,
                    "timeout" => PassOutcome::Timeout,
                    _ => PassOutcome::Ok,
                };
                self.append(EventPayload::PassEnded {
                    pass_id: (*pass_id).to_owned(),
                    outcome,
                    report: None,
                    wake_at: None,
                    rotate: false,
                    why: value_after(rest, "--why").map(str::to_owned),
                })
                .map_err(driver_error)?;
                Ok(json!({"pass_id": pass_id, "outcome": outcome.as_str()}))
            }
            other => Err(DriverError::new(format!(
                "unexpected simulated alder command: {other:?}"
            ))),
        };
        let label = match args {
            ["work", "start", ..] => "spawn.work-start",
            ["attempt", "edit", ..] => "spawn.bind",
            ["attempt", "end", ..] => "repair.attempt-end",
            ["loop", "wake", ..] => "pass.wake",
            ["pass", "end", ..] => "pass.repair-end",
            ["show", id] if id.contains("-pass-") => "pass.show",
            ["show", ..] => "spawn.show",
            ["status", "--section", ..] => "spawn.status",
            ["status"] => "daemon.status",
            ["refresh"] => "daemon.refresh",
            _ => "alder.other",
        };
        self.after(label);
        result
    }
}

impl SpawnHost for SimHost {
    fn root(&self) -> &Path {
        &self.root
    }

    fn alder_binary(&self) -> PathBuf {
        self.root.join("target/debug/alder")
    }

    fn alder(&self, args: &[&str]) -> Result<Value> {
        self.alder_command(args)
    }

    fn git(&self, args: &[&str]) -> Result<Run> {
        let mut run = Run {
            ok: true,
            stdout: String::new(),
            stderr: String::new(),
        };
        let label = if args.contains(&"--git-common-dir") {
            run.stdout = format!("{ROOT}/.git\n");
            "spawn.git-common-dir"
        } else if args.first() == Some(&"rev-parse") {
            let branch = args
                .last()
                .and_then(|reference| reference.strip_prefix("refs/heads/"))
                .unwrap_or_default();
            run.ok = self.shared.world.borrow().branches.contains(branch);
            "spawn.branch-probe"
        } else if args.starts_with(&["worktree", "add"]) {
            let path = PathBuf::from(args[2]);
            let branch = if let Some(index) = args.iter().position(|arg| *arg == "-b") {
                args[index + 1].to_owned()
            } else {
                args[3].to_owned()
            };
            let mut world = self.shared.world.borrow_mut();
            if world.worktrees.contains_key(&path) {
                run.ok = false;
                run.stderr = "worktree already exists".to_owned();
            } else {
                world.branches.insert(branch.clone());
                world.worktrees.insert(path, Worktree { branch });
            }
            "spawn.worktree-add"
        } else if args.starts_with(&["worktree", "remove", "--force"]) {
            let path = PathBuf::from(args[3]);
            let mut world = self.shared.world.borrow_mut();
            world.worktrees.remove(&path);
            world
                .directories
                .retain(|candidate| !candidate.starts_with(&path));
            world
                .files
                .retain(|candidate| !candidate.starts_with(&path));
            "repair.worktree-remove"
        } else {
            run.ok = false;
            run.stderr = format!("unsupported git call {args:?}");
            "git.unsupported"
        };
        self.after(label);
        Ok(run)
    }

    fn tmux_session_exists(&self, session: &str) -> Result<bool> {
        let exists = self.shared.world.borrow().sessions.contains_key(session);
        self.after("spawn.session-probe");
        Ok(exists)
    }

    fn tmux_new_session(&self, session: &str, cwd: &Path, _command: &str) -> Result<()> {
        let mut world = self.shared.world.borrow_mut();
        if world.sessions.contains_key(session) {
            return Err(DriverError::new("session already exists"));
        }
        let kind = if session == LEADER_SESSION {
            SessionKind::Leader
        } else {
            SessionKind::Worker
        };
        let script = if kind == SessionKind::Leader {
            let script = world.next_agent;
            world.next_agent = AgentScript::Complete;
            script
        } else {
            AgentScript::Complete
        };
        world.sessions.insert(
            session.to_owned(),
            Session {
                kind,
                cwd: cwd.to_path_buf(),
                attempt_id: None,
                injected_pass: None,
                script,
            },
        );
        drop(world);
        self.after(if session == LEADER_SESSION {
            "pass.session-create"
        } else {
            "spawn.session-create"
        });
        Ok(())
    }

    fn tmux_set_environment(&self, session: &str, name: &str, value: &str) -> Result<()> {
        if name == "ALDER_ATTEMPT" {
            let mut world = self.shared.world.borrow_mut();
            let session = world
                .sessions
                .get_mut(session)
                .ok_or_else(|| DriverError::new("session missing"))?;
            session.attempt_id = Some(value.to_owned());
        }
        self.after("spawn.session-stamp");
        Ok(())
    }

    fn tmux_kill_session(&self, session: &str) -> Result<()> {
        self.shared.world.borrow_mut().sessions.remove(session);
        self.after("repair.session-kill");
        Ok(())
    }

    fn path_exists(&self, path: &Path) -> bool {
        let world = self.shared.world.borrow();
        let exists = world.worktrees.contains_key(path)
            || world.directories.contains(path)
            || world.files.contains(path);
        drop(world);
        self.after("spawn.path-probe");
        exists
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        self.shared
            .world
            .borrow_mut()
            .directories
            .insert(path.to_path_buf());
        self.after("spawn.mkdir");
        Ok(())
    }

    fn copy_file(&self, _from: &Path, to: &Path) -> Result<()> {
        self.shared
            .world
            .borrow_mut()
            .files
            .insert(to.to_path_buf());
        self.after("spawn.copy");
        Ok(())
    }

    fn write_executable(&self, path: &Path, _body: &str) -> Result<()> {
        self.shared
            .world
            .borrow_mut()
            .files
            .insert(path.to_path_buf());
        self.after("spawn.resume-script");
        Ok(())
    }

    fn log(&self, message: &str) {
        self.shared
            .world
            .borrow_mut()
            .messages
            .push(message.to_owned());
        self.after("spawn.log");
    }
}

impl Effects for SimHost {
    fn now(&self) -> DateTime<Utc> {
        let now = self.logical_now();
        self.after("clock.read");
        now
    }

    fn alder(&self, args: &[&str]) -> Result<Value> {
        self.alder_command(args)
    }

    fn tmux_session_exists(&self, session: &str) -> Result<bool> {
        let exists = self.shared.world.borrow().sessions.contains_key(session);
        self.after("pass.session-probe");
        Ok(exists)
    }

    fn tmux_new_session(&self, session: &str, _engine: &Engine) -> Result<()> {
        <Self as SpawnHost>::tmux_new_session(self, session, &self.root, "scripted leader")
    }

    fn tmux_kill_session(&self, session: &str) -> Result<()> {
        self.shared.world.borrow_mut().sessions.remove(session);
        self.after("pass.session-kill");
        Ok(())
    }

    fn tmux_send_keys(&self, session: &str, text: &str) -> Result<()> {
        let pass_id = text
            .split("pass-id: ")
            .nth(1)
            .and_then(|tail| tail.split([';', ')']).next())
            .ok_or_else(|| DriverError::new("injection has no pass ID"))?;
        let mut world = self.shared.world.borrow_mut();
        let session = world
            .sessions
            .get_mut(session)
            .ok_or_else(|| DriverError::new("leader session missing"))?;
        session.injected_pass = Some(pass_id.to_owned());
        drop(world);
        self.after("pass.inject");
        Ok(())
    }

    fn tmux_has_clients(&self, _session: &str) -> Result<bool> {
        self.after("pass.clients");
        Ok(false)
    }

    fn read_file(&self, _path: &Path) -> Result<Vec<u8>> {
        self.after("pass.read-doc");
        Ok(b"run the scripted pass".to_vec())
    }

    fn file_mtime(&self, _path: &Path) -> Option<DateTime<Utc>> {
        self.after("clock.marker");
        None
    }

    fn notify(&self, message: &str) {
        self.shared
            .world
            .borrow_mut()
            .notices
            .push(message.to_owned());
        self.after("pass.notify");
    }

    fn sleep(&self, duration: Duration) {
        self.shared.world.borrow_mut().tick += duration.as_secs() as i64;
        self.after("clock.tick");
    }

    fn log(&self, message: &str) {
        self.shared
            .world
            .borrow_mut()
            .messages
            .push(message.to_owned());
        self.after("pass.log");
    }
}

pub fn config() -> alderd::config::Config {
    alderd::decide::config_for(&[("stub", "scripted-leader")])
}

pub fn catch_sim_crash<T>(action: impl FnOnce() -> T) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(action)) {
        Ok(value) => Some(value),
        Err(payload) if payload.is::<SimCrash>() => {
            let crash = payload
                .downcast_ref::<SimCrash>()
                .expect("the payload type was checked");
            assert!(crash.ordinal > 0);
            assert!(!crash.boundary.is_empty());
            None
        }
        Err(payload) => resume_unwind(payload),
    }
}

pub fn execute_case(case: &Case) -> Digest {
    let host = SimHost::new(case.seed);
    host.reset_boundaries(case.fault_schedule.clone());
    let mut daemon = Driver::new(host.clone(), config());
    let mut want_worker = false;
    for operation in &case.operations {
        match operation {
            Operation::SpawnWorker => {
                want_worker = true;
                let _ = catch_sim_crash(|| {
                    spawn(
                        &host,
                        WORK_ID,
                        tier("luna").expect("luna exists"),
                        Some("scripted-agent"),
                    )
                });
            }
            Operation::RestartDaemon => {
                daemon = Driver::new(host.clone(), config());
            }
            Operation::PollDaemon => {
                let _ = catch_sim_crash(|| daemon.poll_once());
            }
            Operation::LeaderDiesMidPass => {
                host.nudge();
                host.set_next_agent(AgentScript::DieMidPass);
                let _ = catch_sim_crash(|| daemon.poll_once());
            }
            Operation::Tick(ticks) => host.advance(u64::from(*ticks)),
        }
    }
    host.recover(want_worker);
    assert!(
        host.remaining_faults().is_empty(),
        "case ended before scheduled crashes fired: {case:#?}; remaining={:?}",
        host.remaining_faults()
    );
    host.digest()
}

fn value_after<'a>(args: &'a [&str], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1])
}

fn values_after<'a>(args: &'a [&str], flag: &str) -> Vec<&'a str> {
    args.windows(2)
        .filter(|pair| pair[0] == flag)
        .map(|pair| pair[1])
        .collect()
}

fn driver_error(error: alder::error::AlderError) -> DriverError {
    DriverError::coded(error.code, error.message)
}

#[allow(dead_code)]
fn _panic_payload_is_send_for_std(payload: Box<dyn Any + Send>) -> Box<dyn Any + Send> {
    payload
}
