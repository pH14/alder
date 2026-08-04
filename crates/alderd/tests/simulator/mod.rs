//! A deterministic, entirely in-memory simulator of everything `alderd`
//! touches: the Alder log, the wake command, and its own notes file.
//!
//! It implements the production effect trait, so the code under test is the
//! real driver and the real `decide`. The domain dependency is deliberately
//! confined to this integration-test module: `alderd` itself continues to
//! know Alder only through JSON.
//!
//! # Crashes are modelled by footprints, not by internals
//!
//! Every mock effect declares a [`Footprint`] — the complete, ordered set of
//! world mutations it performs. A scheduled crash applies an arbitrary subset
//! of that footprint and then kills the process. The between-effects crash is
//! the subset that happens to be *everything*. Recovery must converge from
//! every subset; whatever a real interrupted step leaves behind is *some*
//! subset of its footprint, so subset coverage is a superset of reality.
//!
//! # The atomicity asymmetry is a design property, not an accident
//!
//! **A log append cannot tear.** The log is compare-and-append: a record is
//! either accepted at the head it was staged against or rejected whole, so
//! the only two subsets of an append are nothing and everything. Appends
//! land through [`Simulator::append_unfaulted`] — never through a
//! crash-schedulable footprint, which [`Footprint::tearable`] refuses to
//! hold one in.
//!
//! **The daemon's own path holds no append at all.** Since the execution
//! extraction the daemon's wake is: run the configured command, then write
//! its machine-local notes. The command is another process; its effect is
//! one indivisible mutation here (the daemon dying cannot half-run a child
//! it either spawned or did not), and the appends the *executor behind* the
//! command makes are the harness's own unfaulted CAS appends, exactly like a
//! second writer's. The crash windows left are "command ran, notes not
//! written" — a duplicate run — and "notes lost" — also a duplicate run —
//! and the invariants hold through both.

use std::{
    any::Any,
    cell::RefCell,
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind, panic_any, resume_unwind},
    path::Path,
    rc::Rc,
    time::Duration,
};

use alder::domain::{
    EventDraft, EventPayload, LoopEventPayload, ProjectState, Snapshot, WorkDefinition,
    WorkEventPayload, WorkOperation, decode_record, encode_draft,
};
use alder_log::{Head, Log, LogError, MemoryLog, RecordDraft};
use alderd::{
    decide::{Decision, Notes, Poll, decide},
    driver::Driver,
    effects::Effects,
    error::{DriverError, Result},
    loop_state::LoopState,
};
use chrono::{DateTime, Utc};
use serde_json::Value;

const NOTES_FILE: &str = ".alder/alderd-notes.json";
const MAX_RECOVERY_ROUNDS: usize = 96;

/// One indivisible change to the simulated world.
#[derive(Debug)]
enum Mutation {
    /// The one atomic mutation there is: a compare-and-append onto the log,
    /// staged against the head it was validated at. Only the harness's
    /// writers (the executor behind the command, the phone) produce it — the
    /// daemon's own footprints never contain one.
    Append {
        expected: Head,
        draft: RecordDraft,
    },
    /// The configured command ran to completion, with these triggers. One
    /// mutation on purpose: the daemon spawns the command and waits, so its
    /// own death cannot half-run the child — the child either ran or did not.
    CommandRan(String),
    /// The driver's machine-local notes file replaced whole.
    Notes(Vec<u8>),
    Notice(String),
    Message(String),
    Tick(i64),
}

impl Mutation {
    fn name(&self) -> &'static str {
        match self {
            Self::Append { .. } => "append",
            Self::CommandRan(_) => "command-ran",
            Self::Notes(_) => "notes",
            Self::Notice(_) => "notice",
            Self::Message(_) => "message",
            Self::Tick(_) => "tick",
        }
    }

    fn is_append(&self) -> bool {
        matches!(self, Self::Append { .. })
    }
}

/// Everything one effect changes, in the order it changes it.
///
/// Every daemon effect is tearable, and none may contain an append: the
/// daemon appends nothing, and the log's compare-and-append means the
/// harness's other writers land their records whole through [`Simulator::
/// append_unfaulted`] rather than through any crash-schedulable footprint.
#[derive(Debug)]
struct Footprint(Vec<Mutation>);

impl Footprint {
    /// An effect that changes nothing. Its only subset is the empty one,
    /// which is exactly the crash-between-effects the harness began with.
    fn read_only() -> Self {
        Self(Vec::new())
    }

    fn tearable(mutations: Vec<Mutation>) -> Self {
        assert!(
            !mutations.iter().any(Mutation::is_append),
            "a log append cannot appear in a daemon footprint: the daemon \
             appends nothing, and a crash could otherwise land part of a record"
        );
        Self(mutations)
    }

    fn parts(&self) -> &[Mutation] {
        &self.0
    }

    fn subsets(&self) -> u32 {
        1u32 << self.parts().len()
    }

    fn mask(&self, requested: u32) -> u32 {
        requested & (self.subsets() - 1)
    }
}

/// One scheduled crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    /// One-based number of effects to let pass — counted from the previous
    /// fault, or from the start of the case — before this one dies.
    pub after: usize,
    /// Which entries of that effect's footprint land before the process dies.
    pub torn: u32,
}

impl Fault {
    /// The whole footprint lands, then the process dies: a crash *between*
    /// effects.
    pub fn whole(after: usize) -> Self {
        Self {
            after,
            torn: u32::MAX,
        }
    }

    /// A crash *within* an effect: only the selected part of the footprint
    /// lands.
    pub fn torn(after: usize, torn: u32) -> Self {
        Self { after, torn }
    }
}

/// One effect that happened, and how it was interrupted if it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boundary {
    pub ordinal: usize,
    pub label: String,
    pub footprint: Vec<&'static str>,
    pub torn: Option<u32>,
}

impl Boundary {
    pub fn subsets(&self) -> u32 {
        1u32 << self.footprint.len()
    }

    pub fn landed(&self, mask: u32) -> Vec<&'static str> {
        self.footprint
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .map(|(_, name)| *name)
            .collect()
    }

    fn render(&self) -> String {
        match self.torn {
            None => format!("{}:{}", self.ordinal, self.label),
            Some(mask) => format!(
                "{}:{}!torn[{}]",
                self.ordinal,
                self.label,
                self.landed(mask).join("+")
            ),
        }
    }
}

#[derive(Debug)]
struct SimCrash {
    ordinal: usize,
    label: String,
}

/// What the executor behind the command does when the command runs. A
/// one-shot: running it consumes it, so one `script_executor` means one
/// scripted act, whichever run ends up delivering it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentScript {
    /// Read the fold, find nothing new to do, idle.
    Idle,
    /// Append one work statement, the way a real pass acting does.
    Append,
}

#[derive(Debug)]
struct World {
    tick: i64,
    next_event: u64,
    boundary: usize,
    faults: VecDeque<Fault>,
    trace: Vec<Boundary>,
    /// The driver's machine-local notes file, as bytes on the fake disk.
    notes: Option<Vec<u8>>,
    /// What the next command run does. Consumed by running.
    pending_script: AgentScript,
    /// How many times the command was ever run. Not process state: a
    /// witness, so a test can prove a duplicate run actually happened and was
    /// harmless.
    commands_run: usize,
    /// The triggers each run carried, in order.
    triggers_seen: Vec<String>,
    /// Whether the command exits non-zero, standing in for a wake command
    /// that crashed part-way.
    command_fails: bool,
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
            notes: None,
            pending_script: AgentScript::Idle,
            commands_run: 0,
            triggers_seen: Vec::new(),
            command_fails: false,
            notices: Vec::new(),
            messages: Vec::new(),
        }
    }
}

struct Shared {
    log: MemoryLog,
    world: RefCell<World>,
}

/// The simulated world, and the production effect trait over it.
#[derive(Clone)]
pub struct Simulator {
    shared: Rc<Shared>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub state: String,
    pub trace: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Operation {
    RestartDaemon,
    PollDaemon,
    Nudge,
    ExecutorAppendsOnNextWake,
    Tick(u8),
}

#[derive(Debug, Clone)]
pub struct Case {
    pub seed: u64,
    pub operations: Vec<Operation>,
    pub fault_schedule: Vec<Fault>,
}

impl Simulator {
    pub fn new(seed: u64) -> Self {
        let host = Self {
            shared: Rc::new(Shared {
                log: MemoryLog::new(),
                world: RefCell::new(World::new(seed)),
            }),
        };
        host.append_unfaulted(WorkEventPayload::WorkChanged {
            why: None,
            operations: vec![WorkOperation::Add {
                work: WorkDefinition {
                    id: "al-sim".to_owned(),
                    title: "simulated item".to_owned(),
                    spec: Some("crash anywhere".to_owned()),
                    priority: 1,
                    requires: Vec::new(),
                    checks: Vec::new(),
                },
            }],
        });
        host.schedule_faults(Vec::new());
        host
    }

    /// Arm the crash schedule and start counting effects again.
    pub fn schedule_faults(&self, faults: Vec<Fault>) {
        assert!(
            faults.iter().all(|fault| fault.after > 0),
            "fault distances are one-based"
        );
        let mut world = self.shared.world.borrow_mut();
        world.boundary = 0;
        world.faults = faults.into();
        world.trace.clear();
    }

    pub fn trace(&self) -> Vec<Boundary> {
        self.shared.world.borrow().trace.clone()
    }

    pub fn remaining_faults(&self) -> Vec<Fault> {
        self.shared.world.borrow().faults.iter().copied().collect()
    }

    /// Script what the executor behind the command does on its next run.
    pub fn script_executor(&self, script: AgentScript) {
        self.shared.world.borrow_mut().pending_script = script;
    }

    /// How many times the wake command has ever run.
    pub fn commands_run(&self) -> usize {
        self.shared.world.borrow().commands_run
    }

    pub fn triggers_seen(&self) -> Vec<String> {
        self.shared.world.borrow().triggers_seen.clone()
    }

    pub fn fail_commands(&self, fails: bool) {
        self.shared.world.borrow_mut().command_fails = fails;
    }

    pub fn advance(&self, ticks: u64) {
        self.shared.world.borrow_mut().tick += ticks as i64;
    }

    pub fn snapshot(&self) -> Snapshot {
        let head = self.shared.log.head().expect("the memory log has a head");
        let events = self
            .records()
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

    fn records(&self) -> Vec<alder_log::Record> {
        let head = self.shared.log.head().expect("the memory log has a head");
        self.shared
            .log
            .read_all(&head)
            .expect("the memory log is readable")
    }

    pub fn stale_cas_is_rejected(&self) -> bool {
        let expected = self.shared.log.head().expect("head");
        let one = self.draft(
            "cas-one".to_owned(),
            LoopEventPayload::LoopNudgeRequested { why: None },
        );
        let two = self.draft(
            "cas-two".to_owned(),
            LoopEventPayload::LoopRotationRequested { why: None },
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
        self.append_unfaulted(LoopEventPayload::LoopNudgeRequested {
            why: Some("scripted interleaving".to_owned()),
        });
    }

    pub fn pause(&self, why: &str) {
        self.append_unfaulted(LoopEventPayload::LoopPaused {
            why: Some(why.to_owned()),
        });
    }

    pub fn digest(&self) -> Digest {
        let snapshot = self.snapshot();
        let world = self.shared.world.borrow();
        Digest {
            state: format!(
                "head={};commands={};triggers=[{}]",
                snapshot.head.sequence(),
                world.commands_run,
                world.triggers_seen.join(";"),
            ),
            trace: world.trace.iter().map(Boundary::render).collect(),
        }
    }

    pub fn decision(&self) -> Decision {
        let status = self.status_document();
        let state = LoopState::from_status(&status).expect("status is production-readable");
        decide(
            &config(),
            &state,
            &self.notes(),
            &Poll {
                now: self.logical_now(),
                pending_since: None,
            },
        )
    }

    /// The driver's notes as the driver itself would read them back: absent
    /// or unreadable degrades to the fresh state, which only ever costs one
    /// harmless extra run.
    fn notes(&self) -> Notes {
        self.shared
            .world
            .borrow()
            .notes
            .as_deref()
            .and_then(|bytes| serde_json::from_slice(bytes).ok())
            .unwrap_or_default()
    }

    fn logical_now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp(self.shared.world.borrow().tick, 0)
            .expect("the logical tick is a timestamp")
    }

    fn draft(&self, id: String, payload: impl Into<EventPayload>) -> EventDraft {
        EventDraft {
            id,
            at: self.logical_now(),
            actor: "sim".to_owned(),
            payload: payload.into(),
            schema: "alder.event.v0".to_owned(),
        }
    }

    /// An append that another process makes — the executor behind the
    /// command, or a phone. It crosses no daemon effect boundary and can
    /// never be interrupted by a daemon crash, which is exactly the real
    /// shape: the log's CAS makes it land whole or not at all.
    fn append_unfaulted(&self, payload: impl Into<EventPayload>) {
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
        state
            .apply(&candidate)
            .expect("the simulated event is a valid append");
        self.apply(&Mutation::Append {
            expected: snapshot.head,
            draft: encode_draft(&draft).expect("the draft encodes"),
        });
    }

    /// Perform one effect: land its footprint, or — at a scheduled fault — an
    /// arbitrary subset of it, and then die.
    fn effect(&self, label: &str, footprint: Footprint) {
        let (ordinal, mask, dying) = {
            let mut world = self.shared.world.borrow_mut();
            world.boundary += 1;
            let ordinal = world.boundary;
            let due = matches!(world.faults.front(), Some(fault) if fault.after == 1);
            if due {
                let fault = world.faults.pop_front().expect("just checked");
                (ordinal, footprint.mask(fault.torn), true)
            } else {
                if let Some(fault) = world.faults.front_mut() {
                    fault.after -= 1;
                }
                (ordinal, footprint.mask(u32::MAX), false)
            }
        };
        for (index, mutation) in footprint.parts().iter().enumerate() {
            if mask & (1 << index) != 0 {
                self.apply(mutation);
            }
        }
        let boundary = Boundary {
            ordinal,
            label: label.to_owned(),
            footprint: footprint.parts().iter().map(Mutation::name).collect(),
            torn: dying.then_some(mask),
        };
        self.shared.world.borrow_mut().trace.push(boundary);
        if dying {
            panic_any(SimCrash {
                ordinal,
                label: label.to_owned(),
            });
        }
    }

    fn apply(&self, mutation: &Mutation) {
        match mutation {
            Mutation::Append { expected, draft } => {
                self.shared
                    .log
                    .append(expected, draft)
                    .expect("a validated append lands on the head it was staged against");
            }
            Mutation::CommandRan(triggers) => {
                let script = {
                    let mut world = self.shared.world.borrow_mut();
                    world.commands_run += 1;
                    world.triggers_seen.push(triggers.clone());
                    std::mem::replace(&mut world.pending_script, AgentScript::Idle)
                };
                // The executor behind the command acts. Its append is its own
                // process's CAS append: the daemon dying cannot tear it, and
                // an executor handed a wake with nothing new to do appends
                // nothing — which is why a duplicate run is harmless.
                if script == AgentScript::Append {
                    let ordinal = self.shared.world.borrow().next_event;
                    self.append_unfaulted(WorkEventPayload::WorkChanged {
                        why: Some("the executor acted".to_owned()),
                        operations: vec![WorkOperation::Add {
                            work: WorkDefinition {
                                id: format!("al-sim-acted-{ordinal}"),
                                title: "executor statement".to_owned(),
                                spec: None,
                                priority: 0,
                                requires: Vec::new(),
                                checks: Vec::new(),
                            },
                        }],
                    });
                }
            }
            Mutation::Notes(bytes) => {
                self.shared.world.borrow_mut().notes = Some(bytes.clone());
            }
            Mutation::Notice(message) => {
                self.shared.world.borrow_mut().notices.push(message.clone());
            }
            Mutation::Message(message) => {
                self.shared
                    .world
                    .borrow_mut()
                    .messages
                    .push(message.clone());
            }
            Mutation::Tick(seconds) => {
                self.shared.world.borrow_mut().tick += seconds;
            }
        }
    }

    fn status_document(&self) -> Value {
        let snapshot = self.snapshot();
        alder::app::status_document(&snapshot.state, &snapshot.head, false, None)
    }

    /// Drive the daemon until nothing more happens: the fixpoint where the
    /// decision is idle and every scheduled fault has fired.
    pub fn recover(&self) {
        let mut daemon = Driver::new(self.clone(), config());
        for _ in 0..MAX_RECOVERY_ROUNDS {
            let round = catch_sim_crash(|| daemon.poll_once());
            match round {
                Some(Ok(())) => {
                    let stable = matches!(self.decision(), Decision::Idle(_))
                        && self.remaining_faults().is_empty();
                    if stable {
                        self.assert_invariant();
                        return;
                    }
                }
                Some(Err(error)) => {
                    // A failing command or store outage inside recovery: retry
                    // unless the failure was armed to persist.
                    if self.shared.world.borrow().command_fails {
                        panic!(
                            "recovery cannot converge around a permanently failing command: {error}"
                        );
                    }
                }
                None => {
                    // Process death forgets daemon-local state; the notes on
                    // the fake disk survive, exactly like the real `.alder/`
                    // file.
                    daemon = Driver::new(self.clone(), config());
                }
            }
        }
        panic!(
            "recovery did not reach a fixpoint; digest={:#?}",
            self.digest()
        );
    }

    pub fn assert_invariant(&self) {
        use alder::domain::invariants;

        let records = self.records();
        let snapshot = self.snapshot();
        assert!(
            invariants::log_folds_cleanly(&records),
            "the simulated log does not fold cleanly"
        );
        // The central claim: whatever crashed, however many runs were missed
        // or duplicated, the log this system produced never mentions its own
        // readers. Nothing durable records a wake, so there is nothing for a
        // crash to leave half-said about one.
        assert!(
            invariants::mentions_no_readers(&snapshot.events),
            "the simulated log mentions its own readers"
        );
        assert!(
            invariants::rotation_request_mirrors_the_log(&snapshot.state, &snapshot.events),
            "the shared rotation-log safety predicate failed"
        );
        assert!(
            matches!(self.decision(), Decision::Idle(_)),
            "the recovery fixpoint still wants to fire: {:?}",
            self.decision()
        );
    }
}

impl Effects for Simulator {
    fn now(&self) -> DateTime<Utc> {
        let now = self.logical_now();
        self.effect("clock.read", Footprint::read_only());
        now
    }

    fn alder(&self, args: &[&str]) -> Result<Value> {
        match args {
            ["status"] => {
                let document = self.status_document();
                self.effect("daemon.status", Footprint::read_only());
                Ok(document)
            }
            other => Err(DriverError::new(format!(
                "the driver ran `alder {}`, which is not its one read",
                other.join(" ")
            ))),
        }
    }

    fn run_command(&self, _command: &str, triggers: &str) -> Result<()> {
        if self.shared.world.borrow().command_fails {
            self.effect("wake.command", Footprint::read_only());
            return Err(DriverError::new("the command exited with exit status: 1"));
        }
        // One mutation: the daemon spawns the command and waits, so a daemon
        // crash at this boundary either follows a completed run or precedes
        // any run — it cannot half-run the child.
        self.effect(
            "wake.command",
            Footprint::tearable(vec![Mutation::CommandRan(triggers.to_owned())]),
        );
        Ok(())
    }

    fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        assert_eq!(
            path,
            Path::new(NOTES_FILE),
            "the driver reads nothing but its own notes"
        );
        // Reading one's own notes is part of process birth, not a world
        // effect: it crosses no effect boundary, so `Driver::new` cannot
        // itself be a crash site. The notes *write* is the mutation, and
        // every crash around it is scheduled there.
        self.shared
            .world
            .borrow()
            .notes
            .clone()
            .ok_or_else(|| DriverError::new("no notes yet"))
    }

    fn write_file(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        assert_eq!(
            path,
            Path::new(NOTES_FILE),
            "the driver writes nothing but its own notes"
        );
        self.effect(
            "notes.write",
            Footprint::tearable(vec![Mutation::Notes(bytes.to_vec())]),
        );
        Ok(())
    }

    fn file_mtime(&self, _path: &Path) -> Option<DateTime<Utc>> {
        self.effect("clock.marker", Footprint::read_only());
        None
    }

    fn notify(&self, message: &str) {
        self.effect(
            "wake.notify",
            Footprint::tearable(vec![Mutation::Notice(message.to_owned())]),
        );
    }

    fn sleep(&self, duration: Duration) {
        self.effect(
            "clock.tick",
            Footprint::tearable(vec![Mutation::Tick(duration.as_secs() as i64)]),
        );
    }

    fn log(&self, message: &str) {
        self.effect(
            "wake.log",
            Footprint::tearable(vec![Mutation::Message(message.to_owned())]),
        );
    }
}

pub fn config() -> alderd::config::Config {
    alderd::decide::config_for("scripted-executor")
}

pub fn catch_sim_crash<T>(action: impl FnOnce() -> T) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(action)) {
        Ok(value) => Some(value),
        Err(payload) if payload.is::<SimCrash>() => {
            let crash = payload
                .downcast_ref::<SimCrash>()
                .expect("the payload type was checked");
            assert!(crash.ordinal > 0);
            assert!(!crash.label.is_empty());
            None
        }
        Err(payload) => resume_unwind(payload),
    }
}

/// Execute a generated schedule and assert the recovered fixed point.
///
/// [`Operation::Tick`] may legitimately leave an ordinary live state between
/// operations: it advances only the logical clock, and no daemon poll has yet
/// observed a newly due timeout. Convergence is therefore not a valid claim
/// after every operation; it is valid after `recover`, which polls until the
/// clock drives no unfinished work.
pub fn assert_case_converges(case: &Case) -> Digest {
    let host = Simulator::new(case.seed);
    host.schedule_faults(case.fault_schedule.clone());
    let mut daemon = Driver::new(host.clone(), config());
    for operation in &case.operations {
        // A scheduled fault is a process death, so whatever the daemon
        // remembered — how long a fire condition has been pending — is gone
        // with it. Keeping the same `Driver` across a crash would let
        // process-local state outlive the process.
        let survived = match operation {
            Operation::RestartDaemon => {
                daemon = Driver::new(host.clone(), config());
                true
            }
            Operation::PollDaemon => catch_sim_crash(|| daemon.poll_once()).is_some(),
            Operation::Nudge => {
                host.nudge();
                true
            }
            Operation::ExecutorAppendsOnNextWake => {
                host.script_executor(AgentScript::Append);
                true
            }
            Operation::Tick(ticks) => {
                host.advance(u64::from(*ticks));
                true
            }
        };
        if !survived {
            daemon = Driver::new(host.clone(), config());
        }
    }
    host.recover();
    host.assert_invariant();
    assert!(
        host.remaining_faults().is_empty(),
        "case ended before scheduled crashes fired: {case:#?}; remaining={:?}",
        host.remaining_faults()
    );
    host.digest()
}

/// Run a case through the same convergence assertion that generated cases
/// exercise, then return its replay digest.
pub fn execute_case(case: &Case) -> Digest {
    assert_case_converges(case)
}

#[allow(dead_code)]
fn _panic_payload_is_send_for_std(payload: Box<dyn Any + Send>) -> Box<dyn Any + Send> {
    payload
}
