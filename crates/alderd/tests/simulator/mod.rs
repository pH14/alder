//! A deterministic, entirely in-memory simulator of everything `alderd`
//! touches: the Alder log, tmux, git worktrees, and the filesystem.
//!
//! It implements both production effect traits, so the code under test is the
//! real driver, the real `spawn`, the real `decide`, and the real `reconcile`.
//! The domain dependency is deliberately confined to this integration-test
//! module: `alderd` itself continues to know Alder only through JSON.
//!
//! # Crashes are modelled by footprints, not by git internals
//!
//! Every mock effect declares a [`Footprint`] — the complete, ordered set of
//! world mutations it performs. `git worktree add` touches a branch ref, a
//! worktree admin entry, the working directory, and the files inside it;
//! `mkdir -p` touches one directory per missing component; a session create
//! touches one session entry.
//!
//! A scheduled crash applies an arbitrary subset of that footprint and then
//! kills the process. The between-effects crash this harness started with is
//! the subset that happens to be *everything*. Recovery must converge from
//! every subset, and that is what makes the model trustworthy without a
//! catalogue of git internals to drift against: whatever a real interrupted
//! command leaves behind is *some* subset of its footprint, so subset coverage
//! is a superset of reality. A subset argued genuinely impossible may be
//! excluded, but only with a comment carrying the burden of proof — and
//! nothing is excluded today, on purpose: the exclusions were where this model
//! would have started lying, so the cost of covering an impossible subset was
//! paid instead. Grep this module and `sim_crash.rs` for "excluded" before
//! believing otherwise.
//!
//! # The atomicity asymmetry is a design property, not an accident
//!
//! **A log append cannot tear.** The log is compare-and-append: a record is
//! either accepted at the head it was staged against or rejected whole, so the
//! only two subsets of an append are nothing and everything. That is enforced
//! here rather than assumed — [`Footprint::atomic`] takes exactly one append
//! and [`Footprint::tearable`] refuses to hold one, so no footprint can ever
//! offer a crash a way to land half a record.
//!
//! **World state is what tears.** Sessions, worktrees, branches, directories
//! and files are mutated by commands with no such guarantee, and they are also
//! the disposable half of the system: none of them carries a fact worth
//! keeping, so repair is free to delete anything it cannot account for. The
//! durable half — the log — never needs repairing because it never tears.
//!
//! # Residue this harness names locally, and production does not
//!
//! Torn subsets surface residue `reconcile` has no vocabulary for, because
//! `reconcile` reasons about attempts and handles and knows nothing about
//! directories. [`Simulator::stray_paths`] and [`Simulator::clean_strays`] are
//! this harness standing in for a leader-side sweep production does not have
//! yet — tracked as work `al-3pph8m` (formerly handoff al-handoff-vpzdqw). Fixing that is not this branch's job;
//! naming it is, so the convergence property below stays honest rather than
//! quietly excluding the subsets that expose it.
//!
//! Two other torn states this surfaced need no follow-up work. A session created but
//! not yet stamped with `ALDER_ATTEMPT` is already named `unclaimed` by
//! `reconcile` and killed by repair, and the unmerged adoptive-spawn branch
//! `work/al-730568` closes the window entirely by stamping the attempt as the
//! session is created. And a pane left holding text nobody submitted — the
//! injection typed, the Enter not — is real residue that nothing clears
//! eagerly, but it cannot outlive the silence: see
//! [`Simulator::assert_pending_input_is_transient`] for the rule that makes it
//! self-healing and for why demanding an empty pane at the fixpoint would have
//! been demanding something the daemon never promised.

use std::{
    any::Any,
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind, panic_any, resume_unwind},
    path::{Path, PathBuf},
    rc::Rc,
    slice,
    time::Duration,
};

use alder::{
    domain::{
        AttemptDefinition, AttemptOutcome, AttemptState, CheckDefinition, EventDraft, EventPayload,
        LoopEventPayload, ObservationDefinition, ObservationEventPayload, ObservationKey,
        ProjectState, Snapshot, WorkDefinition, WorkEventPayload, WorkOperation, decode_record,
        encode_draft,
    },
    observer::{NormalizedObject, ReconcileFinding, plan_probe_run, probe_targets, reconcile},
};
use alder_log::{Head, Log, LogError, MemoryLog, RecordDraft};
use alderd::{
    config::Engine,
    decide::{Decision, Notes, Poll, SessionAction, decide, session_action},
    driver::Driver,
    effects::Effects,
    error::{DriverError, Result},
    loop_state::LoopState,
    spawn::{ObservedSession, Run, SpawnHost, spawn},
    tier::tier,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

const ROOT: &str = "/sim/alder";
const WORK_ID: &str = "al-sim";
const LEADER_SESSION: &str = "alder-leader";
const NOTES_FILE: &str = ".alder/alderd-notes.json";
const MAX_RECOVERY_ROUNDS: usize = 96;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentScript {
    Complete,
    DieMidAct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionKind {
    Leader,
    Worker,
}

#[derive(Debug, Clone)]
struct Session {
    kind: SessionKind,
    attempt_id: Option<String>,
    /// Literal text typed into the pane and not yet submitted. Production's
    /// `tmux_send_keys` is two tmux invocations — `send-keys -l -- <text>`
    /// then `send-keys Enter` — so a pane holding unsubmitted text is a state
    /// a killed daemon really can leave, and the next injection would be typed
    /// on top of it.
    pending_input: Option<String>,
    /// The wake line the leader was actually handed, once the text was
    /// submitted and not yet acted on.
    injected_line: Option<String>,
}

#[derive(Debug, Clone)]
struct Worktree {
    branch: String,
}

/// One indivisible change to the simulated world.
///
/// These variants are the whole vocabulary a footprint is written in, so
/// adding a resource to the world means adding it here first.
#[derive(Debug)]
enum Mutation {
    /// The one atomic mutation there is: a compare-and-append onto the log,
    /// staged against the head it was validated at.
    Append {
        expected: Head,
        draft: RecordDraft,
    },
    Branch(String),
    WorktreeEntry {
        path: PathBuf,
        branch: String,
    },
    Directory(PathBuf),
    File(PathBuf),
    SessionCreate {
        name: String,
    },
    SessionStamp {
        name: String,
        attempt_id: String,
    },
    /// `tmux send-keys -l -- <text>`: literal text lands in the pane's input,
    /// submitted by nothing.
    SessionType {
        name: String,
        text: String,
    },
    /// `tmux send-keys Enter`: whatever the pane is holding is submitted.
    SessionSubmit(String),
    SessionClearInjection(String),
    SessionRemove(String),
    /// The driver's machine-local notes file replaced whole.
    Notes(Vec<u8>),
    WorktreeEntryRemoved(PathBuf),
    FilesRemovedUnder(PathBuf),
    DirectoriesRemovedUnder(PathBuf),
    DirectoryRemoved(PathBuf),
    FileRemoved(PathBuf),
    Notice(String),
    Message(String),
    Tick(i64),
}

impl Mutation {
    /// The name this mutation carries in a trace, so a failing case says which
    /// parts of a footprint landed.
    fn name(&self) -> &'static str {
        match self {
            Self::Append { .. } => "append",
            Self::Branch(_) => "branch",
            Self::WorktreeEntry { .. } => "worktree-entry",
            Self::Directory(_) => "directory",
            Self::File(_) => "file",
            Self::SessionCreate { .. } => "session",
            Self::SessionStamp { .. } => "stamp",
            Self::SessionType { .. } => "typed",
            Self::SessionSubmit(_) => "submitted",
            Self::SessionClearInjection(_) => "injection-cleared",
            Self::SessionRemove(_) => "session-removed",
            Self::Notes(_) => "notes",
            Self::WorktreeEntryRemoved(_) => "worktree-entry-removed",
            Self::FilesRemovedUnder(_) => "files-removed",
            Self::DirectoriesRemovedUnder(_) => "directories-removed",
            Self::DirectoryRemoved(_) => "directory-removed",
            Self::FileRemoved(_) => "file-removed",
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
/// This is also the complete statement of what an interrupted effect can leave
/// behind: a crash inside the effect applies an arbitrary subset and dies.
#[derive(Debug)]
enum Footprint {
    /// A compare-and-append onto the log. Indivisible: the only subsets are
    /// nothing and everything. See the module docs on the atomicity asymmetry.
    Atomic(Mutation),
    /// World state — sessions, worktrees, branches, directories, files. No
    /// atomicity is claimed for any of it, so any subset can land.
    Tearable(Vec<Mutation>),
}

impl Footprint {
    /// An effect that changes nothing. Its only subset is the empty one, which
    /// is exactly the crash-between-effects the harness began with.
    fn read_only() -> Self {
        Self::Tearable(Vec::new())
    }

    fn atomic(mutation: Mutation) -> Self {
        assert!(
            mutation.is_append(),
            "only a compare-and-append is atomic; every world mutation tears"
        );
        Self::Atomic(mutation)
    }

    fn tearable(mutations: Vec<Mutation>) -> Self {
        assert!(
            !mutations.iter().any(Mutation::is_append),
            "a log append cannot be bundled into a tearable footprint: \
             a crash could then land part of a record"
        );
        Self::Tearable(mutations)
    }

    fn parts(&self) -> &[Mutation] {
        match self {
            Self::Atomic(mutation) => slice::from_ref(mutation),
            Self::Tearable(mutations) => mutations,
        }
    }

    /// How many distinct subsets a crash inside this effect chooses between.
    /// An atomic footprint holds exactly one part, so this is 2: nothing, or
    /// everything.
    fn subsets(&self) -> u32 {
        1u32 << self.parts().len()
    }

    /// The subset a crash actually applies. Bits beyond the footprint are
    /// ignored, so `u32::MAX` always means "all of it".
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
    /// Bit `i` selects footprint entry `i`; bits beyond the footprint are
    /// ignored, so [`Fault::whole`] is the crash-between-effects case.
    pub torn: u32,
}

impl Fault {
    /// The whole footprint lands, then the process dies: a crash *between*
    /// effects, which is the subset that happens to be everything.
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
    /// One-based position in this run's effect sequence.
    pub ordinal: usize,
    pub label: String,
    /// The declared footprint, named part by part in the order it is applied.
    pub footprint: Vec<&'static str>,
    /// The subset that landed, when this boundary is where the process died.
    pub torn: Option<u32>,
}

impl Boundary {
    /// How many subsets a fault at this boundary chooses between.
    pub fn subsets(&self) -> u32 {
        1u32 << self.footprint.len()
    }

    /// The parts of the footprint a given mask lands.
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

#[derive(Debug)]
struct World {
    tick: i64,
    next_event: u64,
    boundary: usize,
    faults: VecDeque<Fault>,
    trace: Vec<Boundary>,
    sessions: BTreeMap<String, Session>,
    worktrees: BTreeMap<PathBuf, Worktree>,
    branches: BTreeSet<String>,
    directories: BTreeSet<PathBuf>,
    files: BTreeSet<PathBuf>,
    /// What the next leader agent to be handed a wake will do. A one-shot:
    /// running it consumes it, so one `script_leader` means one scripted act,
    /// whichever session ends up handling it.
    pending_script: AgentScript,
    /// The driver's machine-local notes file, as bytes on the fake disk.
    notes: Option<Vec<u8>>,
    /// How many wake lines were ever submitted into the leader's pane. Not
    /// process state: a witness, so a test can prove a duplicate delivery
    /// actually happened and was harmless.
    wakes_delivered: usize,
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
            pending_script: AgentScript::Complete,
            notes: None,
            wakes_delivered: 0,
            notices: Vec::new(),
            messages: Vec::new(),
        }
    }
}

struct Shared {
    log: MemoryLog,
    world: RefCell<World>,
}

/// The simulated world, and both production effect traits over it.
#[derive(Clone)]
pub struct Simulator {
    root: PathBuf,
    shared: Rc<Shared>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub state: String,
    pub sessions: Vec<String>,
    pub worktrees: Vec<String>,
    pub paths: Vec<String>,
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
    pub fault_schedule: Vec<Fault>,
}

/// What one simulated `alder` invocation answers with.
enum Answer {
    /// A read: no footprint, and the document as it stands.
    Read(Result<Value>),
    /// A mutation: one append to stage, plus the command-specific fields that
    /// the shared CLI document builder packs around its receipt.
    Mutation {
        payload: EventPayload,
        schema: &'static str,
        fields: Value,
    },
}

impl Simulator {
    pub fn new(seed: u64) -> Self {
        let host = Self {
            root: PathBuf::from(ROOT),
            shared: Rc::new(Shared {
                log: MemoryLog::new(),
                world: RefCell::new(World::new(seed)),
            }),
        };
        host.append_unfaulted(WorkEventPayload::WorkChanged {
            why: None,
            operations: vec![WorkOperation::Add {
                work: WorkDefinition {
                    id: WORK_ID.to_owned(),
                    title: "simulated worker".to_owned(),
                    spec: Some("crash anywhere".to_owned()),
                    priority: 1,
                    requires: Vec::new(),
                    // Not decoration: `Brief::from_show` reads `key` and
                    // `description` off each check, two levels down inside
                    // `current`, and an empty list would leave that shape
                    // untested.
                    checks: vec![CheckDefinition {
                        key: "converges".to_owned(),
                        description: "recovery reaches a fixpoint".to_owned(),
                    }],
                },
            }],
        });
        host.append_unfaulted(LoopEventPayload::LoopEngineSelected {
            engine: "stub".to_owned(),
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

    /// Every effect this run has performed, in order.
    pub fn trace(&self) -> Vec<Boundary> {
        self.shared.world.borrow().trace.clone()
    }

    pub fn remaining_faults(&self) -> Vec<Fault> {
        self.shared.world.borrow().faults.iter().copied().collect()
    }

    /// Script what the leader agent does on the next wake it is handed.
    ///
    /// The script belongs to the *wake*, not to a session, and that is what
    /// makes one call mean exactly one scripted act. Scripting a session
    /// instead has a failure on each side and the harness has had both: a
    /// script left on the next *creation* never reaches a leader the daemon
    /// reuses, and a script written to both places fires on the live session
    /// and then stays armed for the replacement, so one call meant two deaths.
    /// Whichever session ends up handed the wake consumes it — see
    /// [`Simulator::run_leader_if_injected`] — so reuse and restart behave
    /// alike.
    pub fn script_leader(&self, script: AgentScript) {
        self.shared.world.borrow_mut().pending_script = script;
    }

    /// How many wake lines have ever been submitted at the leader.
    pub fn wakes_delivered(&self) -> usize {
        self.shared.world.borrow().wakes_delivered
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
        Digest {
            state: format!(
                "head={};attempts=[{attempts}];wakes={}",
                snapshot.head.sequence(),
                world.wakes_delivered,
            ),
            sessions: world
                .sessions
                .iter()
                .map(|(name, session)| {
                    format!(
                        "{name}:{:?}:{}{}",
                        session.kind,
                        session.attempt_id.as_deref().unwrap_or("-"),
                        if session.pending_input.is_some() {
                            ":typed"
                        } else {
                            ""
                        }
                    )
                })
                .collect(),
            worktrees: world
                .worktrees
                .iter()
                .map(|(path, worktree)| format!("{}:{}", path.display(), worktree.branch))
                .collect(),
            paths: world
                .directories
                .iter()
                .map(|path| format!("d {}", path.display()))
                .chain(
                    world
                        .files
                        .iter()
                        .map(|path| format!("f {}", path.display())),
                )
                .collect(),
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
                refresh_changed: false,
                pending_since: None,
                attached_client: false,
            },
        )
    }

    /// The driver's notes as the driver itself would read them back: absent or
    /// unreadable degrades to the fresh state, which only ever costs one
    /// harmless extra wake.
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

    /// Validate an append and hand it back as the atomic footprint of whatever
    /// effect is about to perform it, with the record ID its answer carries.
    ///
    /// Validation happens here, before any crash decision, so a rejected
    /// append is an ordinary error rather than a torn write.
    fn stage(&self, payload: impl Into<EventPayload>) -> alder::error::Result<(Footprint, String)> {
        let snapshot = self.snapshot();
        let id = {
            let mut world = self.shared.world.borrow_mut();
            let id = format!("sim-event-{:016x}", world.next_event);
            world.next_event += 1;
            id
        };
        let draft = self.draft(id.clone(), payload);
        let candidate = draft.materialize(snapshot.head.sequence() + 1);
        let mut state = snapshot.state;
        state.apply(&candidate)?;
        Ok((
            Footprint::atomic(Mutation::Append {
                expected: snapshot.head,
                draft: encode_draft(&draft)?,
            }),
            id,
        ))
    }

    /// An append that sets a scenario up rather than exercising the system
    /// under test: it crosses no effect boundary and can never be interrupted.
    fn append_unfaulted(&self, payload: impl Into<EventPayload>) {
        let (footprint, _) = self
            .stage(payload)
            .expect("the simulated event is a valid append");
        for mutation in footprint.parts() {
            self.apply(mutation);
        }
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
            Mutation::Branch(name) => {
                self.shared.world.borrow_mut().branches.insert(name.clone());
            }
            Mutation::WorktreeEntry { path, branch } => {
                self.shared.world.borrow_mut().worktrees.insert(
                    path.clone(),
                    Worktree {
                        branch: branch.clone(),
                    },
                );
            }
            Mutation::Directory(path) => {
                self.shared
                    .world
                    .borrow_mut()
                    .directories
                    .insert(path.clone());
            }
            Mutation::File(path) => {
                self.shared.world.borrow_mut().files.insert(path.clone());
            }
            Mutation::SessionCreate { name } => {
                let mut world = self.shared.world.borrow_mut();
                let kind = if name == LEADER_SESSION {
                    SessionKind::Leader
                } else {
                    SessionKind::Worker
                };
                world.sessions.insert(
                    name.clone(),
                    Session {
                        kind,
                        attempt_id: None,
                        pending_input: None,
                        injected_line: None,
                    },
                );
            }
            Mutation::SessionStamp { name, attempt_id } => {
                if let Some(session) = self.shared.world.borrow_mut().sessions.get_mut(name) {
                    session.attempt_id = Some(attempt_id.clone());
                }
            }
            Mutation::SessionType { name, text } => {
                if let Some(session) = self.shared.world.borrow_mut().sessions.get_mut(name) {
                    // tmux appends to whatever the pane is already holding, so
                    // typing onto unsubmitted text really does concatenate.
                    session
                        .pending_input
                        .get_or_insert_with(String::new)
                        .push_str(text);
                }
            }
            Mutation::SessionSubmit(name) => {
                let mut world = self.shared.world.borrow_mut();
                let mut delivered = false;
                if let Some(session) = world.sessions.get_mut(name) {
                    // Enter submits the line as it stands, garbage included if
                    // an earlier torn injection corrupted it.
                    session.injected_line = session.pending_input.take();
                    delivered = name == LEADER_SESSION && session.injected_line.is_some();
                }
                if delivered {
                    world.wakes_delivered += 1;
                }
            }
            Mutation::SessionClearInjection(name) => {
                if let Some(session) = self.shared.world.borrow_mut().sessions.get_mut(name) {
                    session.injected_line = None;
                }
            }
            Mutation::SessionRemove(name) => {
                self.shared.world.borrow_mut().sessions.remove(name);
            }
            Mutation::Notes(bytes) => {
                self.shared.world.borrow_mut().notes = Some(bytes.clone());
            }
            Mutation::WorktreeEntryRemoved(path) => {
                self.shared.world.borrow_mut().worktrees.remove(path);
            }
            Mutation::FilesRemovedUnder(path) => {
                self.shared
                    .world
                    .borrow_mut()
                    .files
                    .retain(|candidate| !candidate.starts_with(path));
            }
            Mutation::DirectoriesRemovedUnder(path) => {
                self.shared
                    .world
                    .borrow_mut()
                    .directories
                    .retain(|candidate| !candidate.starts_with(path));
            }
            Mutation::DirectoryRemoved(path) => {
                self.shared.world.borrow_mut().directories.remove(path);
            }
            Mutation::FileRemoved(path) => {
                self.shared.world.borrow_mut().files.remove(path);
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

    /// A mutation answer built by the same packer the CLI uses, with the head
    /// read after the simulated append landed.
    fn pack(&self, schema: &str, event_id: &str, fields: Value) -> Value {
        let head = self.shared.log.head().expect("the memory log has a head");
        alder::app::mutation_document(&head, schema, event_id, &fields)
    }

    fn status_document(&self) -> Value {
        let snapshot = self.snapshot();
        alder::app::status_document(&snapshot.state, &snapshot.head, false, None)
    }

    /// The observation sweep, applied through production's own plan. The
    /// probe targets come from production's `probe_targets` over the fold —
    /// every live attempt's handle plus every ended attempt's handle whose
    /// liveness key is still current — and each handle is answered exactly
    /// as `scripts/observe-tmux.sh` would answer it: `present` or `absent`
    /// for a `tmux:*` session name, `unknown` for anything else.
    /// `plan_probe_run` turns the answers into attempt-keyed levels and
    /// retirements, exactly as `alder refresh` does. Like the CLI's quiet
    /// append path, an unchanged level appends nothing.
    fn observe(&self) {
        let state = self.snapshot().state;
        let rows: Vec<NormalizedObject> = probe_targets(&state, "tmux")
            .into_iter()
            .map(|handle| {
                let level = match handle.strip_prefix("tmux:") {
                    Some(name) => {
                        if self.shared.world.borrow().sessions.contains_key(name) {
                            "present"
                        } else {
                            "absent"
                        }
                    }
                    None => "unknown",
                };
                NormalizedObject {
                    subject: handle,
                    field: "liveness".to_owned(),
                    level: level.to_owned(),
                }
            })
            .collect();
        for change in plan_probe_run(&state, "tmux", &rows) {
            let current = state.observations.get(&change.key);
            match change.level {
                Some(level) => {
                    if current.is_some_and(|observation| observation.level == level) {
                        continue;
                    }
                    self.append_unfaulted(ObservationEventPayload::ObservationReported {
                        observation: ObservationDefinition {
                            key: change.key,
                            level,
                        },
                    });
                }
                None => {
                    if current.is_none() {
                        continue;
                    }
                    self.append_unfaulted(ObservationEventPayload::ObservationRetired {
                        key: change.key,
                    });
                }
            }
        }
    }

    fn reconcile(&self) -> Vec<ReconcileFinding> {
        let state = self.snapshot().state;
        let kinds = BTreeSet::from(["tmux".to_owned()]);
        reconcile(&state, &kinds, &kinds)
    }

    /// One observation sweep followed by production's reconcile, exposed for
    /// directed scenarios that assert on the findings themselves rather than
    /// only on convergence.
    pub fn observe_and_reconcile(&self) -> Vec<ReconcileFinding> {
        self.observe();
        self.reconcile()
    }

    /// Act on findings exactly as one recovery round would, exposed for the
    /// same directed scenarios.
    pub fn repair(&self, findings: &[ReconcileFinding]) {
        self.repair_findings(findings).expect("repair succeeds");
    }

    /// End an attempt through the simulated CLI, as a leader running
    /// `alder attempt end` does.
    pub fn end_attempt(&self, attempt_id: &str, outcome: &str, why: &str) {
        self.alder_command(&[
            "attempt",
            "end",
            attempt_id,
            "--outcome",
            outcome,
            "--why",
            why,
        ])
        .expect("the simulated attempt end succeeds");
    }

    /// Whether the named worker session still exists in the world.
    pub fn session_exists(&self, name: &str) -> bool {
        self.shared.world.borrow().sessions.contains_key(name)
    }

    /// Directories and files belonging to no registered worktree.
    ///
    /// A torn `git worktree add` that made the directory before the admin
    /// entry, or a torn removal that took the entry first, leaves exactly
    /// this. `reconcile` has no word for it — it reasons about attempts and
    /// handles, not paths — so the harness names it here. See the module docs.
    fn stray_paths(&self) -> Vec<PathBuf> {
        let world = self.shared.world.borrow();
        let roots: Vec<&PathBuf> = world.worktrees.keys().collect();
        world
            .directories
            .iter()
            .chain(world.files.iter())
            .filter(|path| !roots.iter().any(|root| path.starts_with(root)))
            .cloned()
            .collect()
    }

    fn anomalies(&self, want_worker: bool) -> Vec<String> {
        let snapshot = self.snapshot();
        let strays = self.stray_paths();
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
        for path in strays {
            anomalies.push(format!("path:{}", path.display()));
        }
        if want_worker
            && !snapshot.state.attempts.values().any(|attempt| {
                attempt.state == AttemptState::Active
                    && attempt.handle.as_deref() == Some("tmux:alder-work-al-sim")
            })
        {
            anomalies.push(format!("desired:{WORK_ID}"));
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
                    || local_findings
                        .iter()
                        .any(|(_, subject)| subject == &anomaly)
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

    /// Residue production's `reconcile` cannot name, named locally so that
    /// `assert_anomalies_named` still holds every anomaly to a finding.
    ///
    /// A session an ended attempt accounts for through a current liveness
    /// key is deliberately NOT named here: production's `orphan` finding
    /// must name it, and `assert_anomalies_named` fails when it does not.
    fn local_findings(&self, want_worker: bool) -> Vec<(String, String)> {
        let state = self.snapshot().state;
        self.anomalies(want_worker)
            .into_iter()
            .filter_map(|anomaly| {
                if let Some(name) = anomaly.strip_prefix("session:") {
                    (!orphan_accounted(&state, name))
                        .then(|| ("stray_session".to_owned(), anomaly.clone()))
                } else if anomaly.starts_with("worktree:") {
                    Some(("stray_worktree".to_owned(), anomaly))
                } else if anomaly.starts_with("path:") {
                    Some(("stray_path".to_owned(), anomaly))
                } else if anomaly.starts_with("desired:") {
                    Some(("desired_worker_missing".to_owned(), anomaly))
                } else {
                    None
                }
            })
            .collect()
    }

    /// The leader-side sweep production does not have yet.
    ///
    /// A worktree nobody is working in is removed through git, which takes the
    /// admin entry and the checkout with it. What can be left after a torn
    /// `worktree add` or a torn removal — a directory or a file under no
    /// registered worktree at all — is residue git will not clean up and no
    /// production code path removes today, and a respawn onto that path fails
    /// its pre-flight forever. That gap is work `al-3pph8m` (formerly handoff al-handoff-vpzdqw).
    /// Modelling the sweep here keeps the convergence property meaningful and
    /// keeps the missing production step visible instead of silently excluded.
    fn clean_strays(&self) -> Result<bool> {
        // Worker sessions NO attempt accounts for. Reconciliation cannot see
        // them: an unclaimed session is the runner's residue, not a statement
        // about work, so no observation names it. Sweeping them is
        // runner-side housekeeping, modelled here like the worktree sweep
        // below until the runner extraction gives it a production home.
        //
        // Deliberately narrow: a session an ENDED attempt still accounts for
        // through a current liveness key is production's `orphan` finding to
        // surface and `repair_findings` to kill. Sweeping it here would let
        // convergence succeed even if `orphan` silently stopped surfacing —
        // exactly the masking this harness must not provide.
        let stray_sessions: Vec<String> = {
            let world = self.shared.world.borrow();
            let state = self.snapshot().state;
            world
                .sessions
                .iter()
                .filter(|(_, session)| session.kind == SessionKind::Worker)
                .filter(|(name, session)| {
                    let described = session.attempt_id.as_deref().is_some_and(|attempt_id| {
                        state.attempts.get(attempt_id).is_some_and(|attempt| {
                            attempt.state == AttemptState::Active
                                && attempt.handle.as_deref() == Some(&format!("tmux:{name}"))
                        })
                    });
                    !described && !orphan_accounted(&state, name)
                })
                .map(|(name, _)| name.clone())
                .collect()
        };
        for session in &stray_sessions {
            <Self as SpawnHost>::tmux_kill_session(self, session)?;
        }
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
        let orphans = self.stray_paths();
        if !orphans.is_empty() {
            let footprint = {
                let world = self.shared.world.borrow();
                orphans
                    .iter()
                    .map(|path| {
                        if world.directories.contains(path) {
                            Mutation::DirectoryRemoved(path.clone())
                        } else {
                            Mutation::FileRemoved(path.clone())
                        }
                    })
                    .collect()
            };
            self.effect("repair.path-sweep", Footprint::tearable(footprint));
        }
        Ok(!stray_sessions.is_empty() || !stray.is_empty() || !orphans.is_empty())
    }

    fn repair_findings(&self, findings: &[ReconcileFinding]) -> Result<bool> {
        let mut changed = false;
        // A session an ended attempt still holds cannot truthfully serve any
        // other attempt; it is removed before an unspawned attempt is retried
        // on the same deterministic name. This is the ONLY path that kills an
        // orphan session — `clean_strays` deliberately leaves it alone — so
        // recovery converges only when `reconcile` actually surfaces the
        // finding.
        for finding in findings
            .iter()
            .filter(|finding| finding.kind.as_str() == "orphan")
        {
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
                // The leader acts on any wake it was handed. It reads the fold
                // and finds this loop already doing the repairs, so it idles —
                // which is exactly why a duplicated wake is harmless.
                self.run_leader_if_injected();
                // observe
                self.observe();
                // reconcile
                let findings = self.reconcile();
                self.assert_anomalies_named(want_worker, &findings);
                // decide (the production driver repeats this from the same
                // folded status immediately before acting).
                let _decision = self.decision();
                // repair
                let mut changed = self.repair_findings(&findings)?;
                changed |= self.ensure_worker(want_worker)?;
                daemon.poll_once()?;
                Ok::<bool, DriverError>(changed)
            });
            match round {
                Some(Ok(_)) => {
                    // The leader consuming a just-injected wake is an effect
                    // boundary like any other: a scheduled fault may land on
                    // it, and that death is a case to recover from, not a
                    // harness failure.
                    if catch_sim_crash(|| self.run_leader_if_injected()).is_none() {
                        daemon = Driver::new(self.clone(), config());
                        continue;
                    }
                    let findings = self.reconcile();
                    let stable = findings.is_empty()
                        && self.anomalies(want_worker).is_empty()
                        && matches!(self.decision(), Decision::Idle(_))
                        && self.remaining_faults().is_empty();
                    if stable {
                        self.assert_invariant(want_worker);
                        return;
                    }
                }
                Some(Err(error)) => {
                    panic!("recovery failed: {error}; digest={:#?}", self.digest())
                }
                None => {
                    // Process death forgets daemon-local session bookkeeping;
                    // the notes file on the fake disk survives, exactly like
                    // the real `.alder/` file.
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
        use alder::domain::invariants;

        let records = self.records();
        let snapshot = self.snapshot();
        let findings = self.reconcile();
        assert!(
            invariants::log_folds_cleanly(&records),
            "the simulated log does not fold cleanly"
        );
        // The new central claim: whatever crashed, however many wakes were
        // missed or duplicated, the log this system produced never mentions
        // its own readers. Nothing durable records a wake, so there is nothing
        // for a crash to leave half-said about one.
        assert!(
            invariants::mentions_no_readers(&snapshot.events),
            "the simulated log mentions its own readers"
        );
        assert!(
            invariants::rotation_request_mirrors_the_log(&snapshot.state, &snapshot.events),
            "the shared rotation-log safety predicate failed"
        );
        assert!(findings.is_empty(), "unreconciled findings: {findings:#?}");
        assert!(
            self.anomalies(want_worker).is_empty(),
            "stranded world state: {:?}",
            self.anomalies(want_worker)
        );
        assert!(
            matches!(self.decision(), Decision::Idle(_)),
            "the recovery fixpoint still wants to fire: {:?}",
            self.decision()
        );
        self.assert_pending_input_is_transient();
    }

    /// What a pane holding unsubmitted text has to satisfy at the fixpoint.
    ///
    /// A torn `tmux_send_keys` — the literal text sent, the Enter not — really
    /// does leave text nobody submitted, and *nothing clears it eagerly*: the
    /// loop goes idle with the line still sitting in the pane. Requiring the
    /// fixpoint to be free of it would be requiring something the daemon never
    /// promised, and would only be satisfiable by inventing a sweep production
    /// does not perform.
    ///
    /// What production does promise is that the residue cannot outlive the
    /// silence: the daemon that comes back has forgotten a session it never
    /// created, so its next fire reconciles the session before it types
    /// anything, and `session_action` restarts a pane it does not know. That is
    /// asserted here through production's own rule rather than assumed, and the
    /// stronger half of the property — that no injection is ever typed on top
    /// of pending text — is asserted at the seam in `tmux_send_keys`.
    fn assert_pending_input_is_transient(&self) {
        let dirty: Vec<String> = self
            .shared
            .world
            .borrow()
            .sessions
            .iter()
            .filter(|(_, session)| session.pending_input.is_some())
            .map(|(name, _)| name.clone())
            .collect();
        if dirty.is_empty() {
            return;
        }
        let action = session_action(&config(), false, "stub", 0, true, None, self.logical_now());
        assert!(
            matches!(action, SessionAction::Restart(_)),
            "panes {dirty:?} hold unsubmitted text, and the next fire would \
             {action:?} rather than restart them — the text would be typed on"
        );
    }

    /// The scripted leader agent, run when a submitted wake line is waiting.
    ///
    /// The ordinary script reads the fold and finds nothing this harness has
    /// not already handled, so it acts by doing nothing and clears the line.
    /// That is the load-bearing half of the protocol's crash story: a wake
    /// delivered twice — or to a leader that already acted — changes nothing,
    /// because the wake carries no work of its own and nothing durable records
    /// it.
    pub fn run_leader_if_injected(&self) {
        let script = {
            let mut world = self.shared.world.borrow_mut();
            let handed = world
                .sessions
                .get(LEADER_SESSION)
                .is_some_and(|session| session.injected_line.is_some());
            // Consumed by whichever session was handed the wake, so the
            // replacement created after a scripted death is an ordinary
            // leader again.
            handed.then(|| std::mem::replace(&mut world.pending_script, AgentScript::Complete))
        };
        match script {
            Some(AgentScript::Complete) => {
                // The leader's read of the current state.
                self.effect("leader.read-state", Footprint::read_only());
                self.effect(
                    "leader.idle",
                    Footprint::tearable(vec![Mutation::SessionClearInjection(
                        LEADER_SESSION.to_owned(),
                    )]),
                );
            }
            Some(AgentScript::DieMidAct) => {
                self.effect(
                    "agent.die",
                    Footprint::tearable(vec![Mutation::SessionRemove(LEADER_SESSION.to_owned())]),
                );
            }
            None => {}
        }
    }

    /// The simulated `alder <args> --json`, built with production's JSON
    /// builders over the simulator's current state and append receipts.
    fn alder_command(&self, args: &[&str]) -> Result<Value> {
        let label = dispatch_label(args);
        match self.answer(args) {
            Answer::Read(result) => {
                self.effect(label, Footprint::read_only());
                result
            }
            Answer::Mutation {
                payload,
                schema,
                fields,
            } => match self.stage(payload) {
                Ok((footprint, event_id)) => {
                    self.effect(label, footprint);
                    Ok(self.pack(schema, &event_id, fields))
                }
                Err(error) => {
                    self.effect(label, Footprint::read_only());
                    Err(driver_error(error))
                }
            },
        }
    }

    fn answer(&self, args: &[&str]) -> Answer {
        match args {
            ["show", id] if *id == WORK_ID => {
                let snapshot = self.snapshot();
                Answer::Read(
                    alder::app::show_document(
                        &snapshot.state,
                        &snapshot.events,
                        &snapshot.head,
                        id,
                    )
                    .map_err(|error| DriverError::new(error.to_string())),
                )
            }
            ["status"] => Answer::Read(Ok(self.status_document())),
            ["status", "--section", "in_flight"] => {
                let snapshot = self.snapshot();
                let mut document =
                    alder::app::status_document(&snapshot.state, &snapshot.head, false, None);
                document
                    .as_object_mut()
                    .expect("status document is an object")
                    .insert(
                        "in_flight".to_owned(),
                        alder::app::in_flight_section(&snapshot.state),
                    );
                Answer::Read(Ok(document))
            }
            ["refresh"] => {
                let head = self.shared.log.head().expect("the memory log has a head");
                let result = json!({"changed": false});
                Answer::Read(Ok(alder::app::refresh_document(&head, false, &result)))
            }
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
                Answer::Mutation {
                    payload: WorkEventPayload::AttemptStarted {
                        attempt: AttemptDefinition {
                            id: attempt_id.clone(),
                            work_id: (*work_id).to_owned(),
                            tier: None,
                            metadata: BTreeMap::new(),
                        },
                    }
                    .into(),
                    schema: "alder.work.start.v0",
                    fields: json!({"work_id": work_id, "attempt_id": attempt_id}),
                }
            }
            ["attempt", "edit", attempt_id, rest @ ..] => {
                let Some(handle) = value_after(rest, "--handle") else {
                    return Answer::Read(Err(DriverError::new(
                        "the simulator only supports binding attempt edits",
                    )));
                };
                let metadata = rest
                    .windows(2)
                    .filter(|pair| pair[0] == "--meta")
                    .filter_map(|pair| pair[1].split_once('='))
                    .map(|(key, value)| (key.to_owned(), json!(value)))
                    .collect();
                Answer::Mutation {
                    payload: WorkEventPayload::AttemptBound {
                        attempt_id: (*attempt_id).to_owned(),
                        handle: handle.to_owned(),
                        metadata,
                    }
                    .into(),
                    schema: "alder.attempt.edit.v0",
                    fields: json!({
                        "attempt_id": attempt_id,
                        "change": "bound",
                        "handle": handle,
                    }),
                }
            }
            ["attempt", "end", attempt_id, rest @ ..] => {
                let requested = value_after(rest, "--outcome").unwrap_or("lost");
                let outcome = match requested {
                    "not-started" => AttemptOutcome::NotStarted,
                    "lost" => AttemptOutcome::Lost,
                    "cancelled" => AttemptOutcome::Cancelled,
                    other => {
                        return Answer::Read(Err(DriverError::new(format!(
                            "unsupported attempt outcome {other}"
                        ))));
                    }
                };
                Answer::Mutation {
                    payload: WorkEventPayload::AttemptEnded {
                        attempt_id: (*attempt_id).to_owned(),
                        outcome,
                        why: value_after(rest, "--why")
                            .unwrap_or("simulated repair")
                            .to_owned(),
                    }
                    .into(),
                    schema: "alder.attempt.end.v0",
                    fields: json!({"attempt_id": attempt_id, "outcome": requested}),
                }
            }
            other => Answer::Read(Err(DriverError::new(format!(
                "unexpected simulated alder command: {other:?}"
            )))),
        }
    }
}

impl SpawnHost for Simulator {
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
        let (label, footprint) = if args.contains(&"--git-common-dir") {
            run.stdout = format!("{ROOT}/.git\n");
            ("spawn.git-common-dir", Footprint::read_only())
        } else if args.first() == Some(&"rev-parse") {
            let branch = args
                .last()
                .and_then(|reference| reference.strip_prefix("refs/heads/"))
                .unwrap_or_default();
            run.ok = self.shared.world.borrow().branches.contains(branch);
            ("spawn.branch-probe", Footprint::read_only())
        } else if args.starts_with(&["worktree", "list", "--porcelain", "-z"]) {
            run.stdout = self
                .shared
                .world
                .borrow()
                .worktrees
                .keys()
                .map(|path| format!("worktree {}\\0", path.display()))
                .collect();
            ("repair.worktree-list", Footprint::read_only())
        } else if args.first() == Some(&"-C")
            && args.get(2) == Some(&"symbolic-ref")
            && args.last() == Some(&"HEAD")
        {
            let path = PathBuf::from(args.get(1).copied().unwrap_or_default());
            let branch = self
                .shared
                .world
                .borrow()
                .worktrees
                .get(&path)
                .map(|worktree| worktree.branch.clone());
            match branch {
                Some(branch) => run.stdout = format!("{branch}\\n"),
                None => {
                    run.ok = false;
                    run.stderr = "worktree is not registered".to_owned();
                }
            }
            ("spawn.worktree-probe", Footprint::read_only())
        } else if args.starts_with(&["worktree", "add"]) {
            let path = PathBuf::from(args[2]);
            let branch = if let Some(index) = args.iter().position(|arg| *arg == "-b") {
                args[index + 1].to_owned()
            } else {
                args[3].to_owned()
            };
            let footprint = if self.shared.world.borrow().worktrees.contains_key(&path) {
                run.ok = false;
                run.stderr = "worktree already exists".to_owned();
                Footprint::read_only()
            } else {
                // What `git worktree add` touches, in the order it touches it:
                // the branch ref, the worktree admin entry, the working
                // directory, and the `.git` file pointing back at the
                // repository. Any of them can be the last thing a killed git
                // managed to do.
                Footprint::tearable(vec![
                    Mutation::Branch(branch.clone()),
                    Mutation::WorktreeEntry {
                        path: path.clone(),
                        branch,
                    },
                    Mutation::Directory(path.clone()),
                    Mutation::File(path.join(".git")),
                ])
            };
            ("spawn.worktree-add", footprint)
        } else if args.starts_with(&["worktree", "remove", "--force"]) {
            let path = PathBuf::from(args[3]);
            // The inverse order: the checkout goes first, then the directory,
            // then the admin entry. A killed removal leaves any prefix — and,
            // because nothing here is atomic, any subset at all.
            (
                "repair.worktree-remove",
                Footprint::tearable(vec![
                    Mutation::FilesRemovedUnder(path.clone()),
                    Mutation::DirectoriesRemovedUnder(path.clone()),
                    Mutation::WorktreeEntryRemoved(path),
                ]),
            )
        } else {
            run.ok = false;
            run.stderr = format!("unsupported git call {args:?}");
            ("git.unsupported", Footprint::read_only())
        };
        self.effect(label, footprint);
        Ok(run)
    }

    fn tmux_session(&self, session: &str) -> Result<Option<ObservedSession>> {
        let observed = self
            .shared
            .world
            .borrow()
            .sessions
            .get(session)
            .map(|session| {
                ObservedSession {
                    attempt_id: session.attempt_id.clone(),
                    // Simulated worker panes stay alive after their scripted
                    // command, just as the production pane ends in `exec bash`.
                    engine_live: true,
                }
            });
        self.effect("spawn.session-probe", Footprint::read_only());
        Ok(observed)
    }

    fn tmux_new_session(
        &self,
        session: &str,
        _cwd: &Path,
        _command: &str,
        attempt_id: &str,
    ) -> Result<()> {
        if self.shared.world.borrow().sessions.contains_key(session) {
            return Err(DriverError::new("session already exists"));
        }
        let label = if session == LEADER_SESSION {
            "wake.session-create"
        } else {
            "spawn.session-create"
        };
        self.effect(
            label,
            Footprint::tearable(vec![
                Mutation::SessionCreate {
                    name: session.to_owned(),
                },
                Mutation::SessionStamp {
                    name: session.to_owned(),
                    attempt_id: attempt_id.to_owned(),
                },
            ]),
        );
        Ok(())
    }

    fn tmux_kill_session(&self, session: &str) -> Result<()> {
        self.effect(
            "repair.session-kill",
            Footprint::tearable(vec![Mutation::SessionRemove(session.to_owned())]),
        );
        Ok(())
    }

    fn path_exists(&self, path: &Path) -> bool {
        let world = self.shared.world.borrow();
        let exists = world.worktrees.contains_key(path)
            || world.directories.contains(path)
            || world.files.contains(path);
        drop(world);
        self.effect("spawn.path-probe", Footprint::read_only());
        exists
    }

    fn canonical_path(&self, path: &Path) -> Result<PathBuf> {
        self.effect("repair.path-canonicalize", Footprint::read_only());
        Ok(path.to_path_buf())
    }

    fn remove_path(&self, path: &Path) -> Result<()> {
        self.effect(
            "repair.path-sweep",
            Footprint::tearable(vec![
                Mutation::FilesRemovedUnder(path.to_path_buf()),
                Mutation::DirectoriesRemovedUnder(path.to_path_buf()),
            ]),
        );
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        // `mkdir -p` creates one directory per missing component, so it tears
        // component by component. Only the components below the directory
        // worktrees are cut beside are modelled: everything above that belongs
        // to the machine, not to this project.
        let base = self.root.parent().unwrap_or(&self.root);
        let mut components: Vec<PathBuf> = path
            .ancestors()
            .take_while(|ancestor| ancestor.starts_with(base) && *ancestor != base)
            .map(Path::to_path_buf)
            .collect();
        components.reverse();
        self.effect(
            "spawn.mkdir",
            Footprint::tearable(components.into_iter().map(Mutation::Directory).collect()),
        );
        Ok(())
    }

    fn copy_file(&self, _from: &Path, to: &Path) -> Result<()> {
        self.effect(
            "spawn.copy",
            Footprint::tearable(vec![Mutation::File(to.to_path_buf())]),
        );
        Ok(())
    }

    fn write_executable(&self, path: &Path, _body: &str) -> Result<()> {
        self.effect(
            "spawn.resume-script",
            Footprint::tearable(vec![Mutation::File(path.to_path_buf())]),
        );
        Ok(())
    }

    fn log(&self, message: &str) {
        self.effect(
            "spawn.log",
            Footprint::tearable(vec![Mutation::Message(message.to_owned())]),
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
        self.alder_command(args)
    }

    fn tmux_session_exists(&self, session: &str) -> Result<bool> {
        let exists = self.shared.world.borrow().sessions.contains_key(session);
        self.effect("wake.session-probe", Footprint::read_only());
        Ok(exists)
    }

    fn tmux_new_session(&self, session: &str, _engine: &Engine) -> Result<()> {
        if self.shared.world.borrow().sessions.contains_key(session) {
            return Err(DriverError::new("session already exists"));
        }
        self.effect(
            "wake.session-create",
            Footprint::tearable(vec![Mutation::SessionCreate {
                name: session.to_owned(),
            }]),
        );
        Ok(())
    }

    fn tmux_kill_session(&self, session: &str) -> Result<()> {
        self.effect(
            "wake.session-kill",
            Footprint::tearable(vec![Mutation::SessionRemove(session.to_owned())]),
        );
        Ok(())
    }

    fn tmux_send_keys(&self, session: &str, text: &str) -> Result<()> {
        let pending = {
            let world = self.shared.world.borrow();
            match world.sessions.get(session) {
                None => return Err(DriverError::new("leader session missing")),
                Some(session) => session.pending_input.clone(),
            }
        };
        // The invariant unsubmitted text is really held to: nothing is ever
        // typed on top of it. tmux appends, so an injection typed onto a dirty
        // pane produces one garbled line the leader cannot act on. This fires
        // the moment that happens rather than leaving it to be inferred from a
        // stuck fixpoint.
        assert!(
            pending.is_none(),
            "injecting `{text}` into `{session}`, which still holds unsubmitted \
             text {pending:?} from a torn injection"
        );
        // Production types the literal text and presses Enter as two separate
        // tmux invocations, so this is genuinely two mutations: a daemon killed
        // between them leaves the pane holding text nobody submitted, and the
        // leader is never handed the wake.
        self.effect(
            "wake.inject",
            Footprint::tearable(vec![
                Mutation::SessionType {
                    name: session.to_owned(),
                    text: text.to_owned(),
                },
                Mutation::SessionSubmit(session.to_owned()),
            ]),
        );
        Ok(())
    }

    fn tmux_has_clients(&self, _session: &str) -> Result<bool> {
        self.effect("wake.clients", Footprint::read_only());
        Ok(false)
    }

    fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        if path == Path::new(NOTES_FILE) {
            // Reading one's own notes is part of process birth, not a world
            // effect: it crosses no effect boundary, so `Driver::new` cannot
            // itself be a crash site. The notes *write* is the mutation, and
            // every crash around it is scheduled there.
            return self
                .shared
                .world
                .borrow()
                .notes
                .clone()
                .ok_or_else(|| DriverError::new("no notes yet"));
        }
        self.effect("wake.read-doc", Footprint::read_only());
        Ok(b"run one bounded iteration".to_vec())
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

/// Whether an ended attempt still accounts for the named worker session
/// through a current liveness key — the state in which production's `orphan`
/// finding, not the harness's stray sweep, owns the repair.
fn orphan_accounted(state: &ProjectState, session: &str) -> bool {
    let handle = format!("tmux:{session}");
    state.attempts.values().any(|attempt| {
        attempt.state == AttemptState::Ended
            && attempt.handle.as_deref() == Some(handle.as_str())
            && state.observations.contains_key(&ObservationKey {
                observer: "tmux".to_owned(),
                subject: attempt.id.clone(),
                field: "liveness".to_owned(),
            })
    })
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
/// after every operation. It is valid after `recover`, which repeats observe,
/// reconcile, decide, and repair until the clock drives no unfinished work;
/// that is where this function asserts both shared log safety and SimHost's
/// local world-state predicates.
pub fn assert_case_converges(case: &Case) -> Digest {
    let host = Simulator::new(case.seed);
    host.schedule_faults(case.fault_schedule.clone());
    let mut daemon = Driver::new(host.clone(), config());
    let mut want_worker = false;
    for operation in &case.operations {
        // A scheduled fault is a process death, so whatever the daemon
        // remembered — the session it launched, whether the next injection
        // must bootstrap, how long a fire condition has been pending — is gone
        // with it. Keeping the same `Driver` across a crash would let
        // process-local state outlive the process, and the next operation
        // would reuse a session a restarted daemon would have replaced. This
        // is the same reset `recover` performs between rounds.
        let survived = match operation {
            Operation::SpawnWorker => {
                want_worker = true;
                catch_sim_crash(|| {
                    spawn(
                        &host,
                        WORK_ID,
                        tier("luna").expect("luna exists"),
                        Some("scripted-agent"),
                    )
                })
                .is_some()
            }
            Operation::RestartDaemon => {
                daemon = Driver::new(host.clone(), config());
                true
            }
            Operation::PollDaemon => {
                let survived = catch_sim_crash(|| daemon.poll_once()).is_some();
                // The leader acts on whatever the poll delivered.
                catch_sim_crash(|| host.run_leader_if_injected());
                survived
            }
            Operation::LeaderDiesMidPass => {
                host.nudge();
                host.script_leader(AgentScript::DieMidAct);
                let survived = catch_sim_crash(|| daemon.poll_once()).is_some();
                catch_sim_crash(|| host.run_leader_if_injected());
                survived
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
    host.recover(want_worker);
    host.assert_invariant(want_worker);
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

/// The trace label one simulated `alder` invocation is recorded under.
fn dispatch_label(args: &[&str]) -> &'static str {
    match args {
        ["work", "start", ..] => "spawn.work-start",
        ["attempt", "edit", ..] => "spawn.bind",
        ["attempt", "end", ..] => "repair.attempt-end",
        ["show", ..] => "spawn.show",
        ["status", "--section", ..] => "spawn.status",
        ["status"] => "daemon.status",
        ["refresh"] => "daemon.refresh",
        _ => "alder.other",
    }
}

fn value_after<'a>(args: &'a [&str], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1])
}

fn driver_error(error: alder::error::AlderError) -> DriverError {
    DriverError::coded(error.code, error.message)
}

#[allow(dead_code)]
fn _panic_payload_is_send_for_std(payload: Box<dyn Any + Send>) -> Box<dyn Any + Send> {
    payload
}
