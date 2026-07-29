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
//! yet — handoff `al-handoff-vpzdqw`. Fixing that is not this branch's job;
//! naming it is, so the convergence property below stays honest rather than
//! quietly excluding the subsets that expose it.
//!
//! Two other torn states this surfaced need no handoff. A session created but
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
        AttemptDefinition, AttemptOutcome, AttemptState, EventDraft, EventPayload, PassDefinition,
        PassOutcome, PassState, PassTrigger, ProjectState, Snapshot, WorkDefinition, WorkOperation,
        decode_record, encode_draft,
    },
    observer::{ReconcileFinding, reconcile},
    projection::{ObservationStatus, ObservedHandle},
};
use alder_log::{Head, Log, LogError, MemoryLog, RecordDraft};
use alderd::{
    config::Engine,
    decide::{Decision, Poll, SessionAction, decide, session_action},
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

/// The envelope `src/app.rs::mutation_output` wraps around every mutating
/// `alder --json` answer. See [`MIRRORED`] for why this is spelled out here.
pub const MUTATION_ENVELOPE: [&str; 4] = ["schema", "head", "revision", "event_id"];

/// Where in `src/app.rs` the real answer to one dispatched command is built.
///
/// The region matters as much as the schema. A whole-file search for a schema
/// identifier proves almost nothing: `alder.attempt.edit.v0` is claimed by two
/// call sites — binding a handle and updating metadata — and this simulator
/// models only the binding, so a rename confined to the binding arm would slip
/// straight past a search that the *other* arm keeps satisfying.
#[derive(Debug, Clone, Copy)]
pub enum Site {
    /// The body of `fn <name>` in `src/app.rs`, for a read.
    Function(&'static str),
    /// The one `mutation_output(...)` call whose source text contains this
    /// needle. Asserted to match exactly one call, so an ambiguous needle is a
    /// test failure rather than a silently weakened check.
    MutationCall(&'static str),
}

/// One document this simulator hands back, and the CLI region that produces
/// the real one.
///
/// **DRIFT RISK, NAMED OUT LOUD.** This module hand-mirrors the real CLI's
/// output shapes. Nothing in the build makes the two move together: `alder`'s
/// documents are `json!` literals in `src/app.rs`, and the simulator's are
/// `json!` literals here. If the CLI renames `attempt_id`, drops `current`, or
/// re-wraps a mutation answer, every test in this harness keeps passing while
/// production breaks — the simulator would simply be faithfully simulating a
/// CLI that no longer exists.
///
/// The tripwire is `the_simulated_dispatcher_still_mirrors_the_cli_pack` in
/// `sim_crash.rs`. It reads `src/app.rs`, narrows to each [`Site`], and fails
/// if the schema or any mirrored field is not emitted *from that region*. What
/// it still cannot see is a field the CLI renames in a region this simulator
/// does not model at all, so a new `alder` sub-command answered here has to
/// arrive with a row in this table.
#[derive(Debug, Clone, Copy)]
pub struct Mirrored {
    /// `alder <command>`, as this simulator dispatches it.
    pub command: &'static str,
    pub site: Site,
    pub schema: &'static str,
    /// Every key the simulator's answer carries beyond the envelope.
    /// Production must still emit all of them from the same region.
    pub fields: &'static [&'static str],
}

pub const MIRRORED: [Mirrored; 9] = [
    Mirrored {
        command: "show",
        site: Site::Function("show"),
        schema: "alder.show.v0",
        fields: &["head", "id", "kind", "current", "history"],
    },
    Mirrored {
        command: "status",
        site: Site::Function("status"),
        schema: "alder.status.v0",
        fields: &["head", "revision", "loop"],
    },
    Mirrored {
        command: "status --section in_flight",
        site: Site::Function("status"),
        schema: "alder.status.v0",
        fields: &["head", "revision", "in_flight"],
    },
    Mirrored {
        command: "refresh",
        site: Site::Function("refresh"),
        schema: "alder.refresh.v0",
        fields: &["head", "changed", "result"],
    },
    Mirrored {
        command: "work start",
        site: Site::MutationCall("alder.work.start.v0"),
        schema: "alder.work.start.v0",
        fields: &["work_id", "attempt_id"],
    },
    Mirrored {
        // Narrowed to the binding arm: the updating arm claims the same schema
        // and this simulator does not model it.
        command: "attempt edit --handle",
        site: Site::MutationCall(r#""change": "bound""#),
        schema: "alder.attempt.edit.v0",
        fields: &["attempt_id", "change", "handle"],
    },
    Mirrored {
        command: "attempt end",
        site: Site::MutationCall("alder.attempt.end.v0"),
        schema: "alder.attempt.end.v0",
        fields: &["attempt_id", "outcome"],
    },
    Mirrored {
        command: "loop wake",
        site: Site::MutationCall("alder.loop.wake.v0"),
        schema: "alder.loop.wake.v0",
        fields: &["pass_id", "engine", "handle", "triggers"],
    },
    Mirrored {
        command: "pass end",
        site: Site::MutationCall("alder.pass.end.v0"),
        schema: "alder.pass.end.v0",
        fields: &["pass_id", "outcome", "rotate"],
    },
];

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
    /// Literal text typed into the pane and not yet submitted. Production's
    /// `tmux_send_keys` is two tmux invocations — `send-keys -l -- <text>`
    /// then `send-keys Enter` — so a pane holding unsubmitted text is a state
    /// a killed daemon really can leave, and the next injection would be typed
    /// on top of it.
    pending_input: Option<String>,
    /// The pass the leader was actually handed, once the text was submitted.
    injected_pass: Option<String>,
    script: AgentScript,
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
        cwd: PathBuf,
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
    /// A mutation: one append to stage, plus the fields the CLI packs around
    /// it. See [`MIRRORED`] for the drift risk this shape carries.
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

    /// Validate an append and hand it back as the atomic footprint of whatever
    /// effect is about to perform it, with the record ID its answer carries.
    ///
    /// Validation happens here, before any crash decision, so a rejected
    /// append is an ordinary error rather than a torn write.
    fn stage(&self, payload: EventPayload) -> alder::error::Result<(Footprint, String)> {
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
    fn append_unfaulted(&self, payload: EventPayload) {
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
            Mutation::SessionCreate { name, cwd } => {
                let mut world = self.shared.world.borrow_mut();
                let kind = if name == LEADER_SESSION {
                    SessionKind::Leader
                } else {
                    SessionKind::Worker
                };
                // Only the leader runs a script; a worker pane is inert here.
                let script = if kind == SessionKind::Leader {
                    std::mem::replace(&mut world.next_agent, AgentScript::Complete)
                } else {
                    AgentScript::Complete
                };
                world.sessions.insert(
                    name.clone(),
                    Session {
                        kind,
                        cwd: cwd.clone(),
                        attempt_id: None,
                        pending_input: None,
                        injected_pass: None,
                        script,
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
                if let Some(session) = self.shared.world.borrow_mut().sessions.get_mut(name) {
                    // Enter submits the line as it stands. A pass ID is read
                    // out of it here rather than at send time, so text that was
                    // corrupted by an earlier torn injection is submitted as
                    // the garbage it is.
                    session.injected_pass = session
                        .pending_input
                        .take()
                        .as_deref()
                        .and_then(injected_pass_id)
                        .map(str::to_owned);
                }
            }
            Mutation::SessionClearInjection(name) => {
                if let Some(session) = self.shared.world.borrow_mut().sessions.get_mut(name) {
                    session.injected_pass = None;
                }
            }
            Mutation::SessionRemove(name) => {
                self.shared.world.borrow_mut().sessions.remove(name);
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

    /// A mutation answer, packed the way `src/app.rs::mutation_output` packs
    /// one: the fields, plus the envelope, with the head read *after* the
    /// append landed — exactly where the real CLI reads it.
    fn pack(&self, schema: &str, event_id: &str, fields: Value) -> Value {
        let mut object = self.headed(schema, fields);
        let head = self.shared.log.head().expect("the memory log has a head");
        object.insert("revision".to_owned(), json!(head.revision()));
        object.insert("event_id".to_owned(), json!(event_id));
        Value::Object(object)
    }

    /// The two keys every `alder --json` document carries, whatever it is.
    ///
    /// Read envelopes are deliberately **not** shared beyond this. `status`
    /// carries a `revision` and `show` and `refresh` do not, and a common
    /// packer that added one to all three would be drift in the dangerous
    /// direction: daemon code could come to depend on a field production has
    /// never emitted, and this harness would keep passing while the real CLI
    /// handed back nothing. Omitting a field production does have is the safe
    /// direction — the simulator fails first — so each read says for itself.
    fn headed(&self, schema: &str, fields: Value) -> serde_json::Map<String, Value> {
        let head = self.shared.log.head().expect("the memory log has a head");
        let mut object = match fields {
            Value::Object(object) => object,
            _ => serde_json::Map::new(),
        };
        object.insert("schema".to_owned(), json!(schema));
        object.insert("head".to_owned(), json!(head.sequence()));
        object
    }

    /// `alder show --json`: schema, head, and the item. No revision.
    fn show_pack(&self, fields: Value) -> Value {
        Value::Object(self.headed("alder.show.v0", fields))
    }

    /// `alder refresh --json`: schema, head, and what the sweep saw. No
    /// revision.
    fn refresh_pack(&self, fields: Value) -> Value {
        Value::Object(self.headed("alder.refresh.v0", fields))
    }

    /// `alder status --json`, the one read that does carry a revision.
    fn status_pack(&self, fields: Value) -> Value {
        let head = self.shared.log.head().expect("the memory log has a head");
        let mut object = self.headed("alder.status.v0", fields);
        object.insert("revision".to_owned(), json!(head.revision()));
        Value::Object(object)
    }

    fn status_document(&self) -> Value {
        let snapshot = self.snapshot();
        let state = &snapshot.state;
        let control = &state.loop_control;
        self.status_pack(json!({
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
        }))
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

    /// Residue production's `reconcile` cannot name, named locally so that
    /// `assert_anomalies_named` still holds every anomaly to a finding.
    fn local_findings(&self, want_worker: bool) -> Vec<(String, String)> {
        self.anomalies(want_worker)
            .into_iter()
            .filter_map(|anomaly| {
                if anomaly.starts_with("worktree:") {
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
    /// its pre-flight forever. That gap is handoff `al-handoff-vpzdqw`.
    /// Modelling the sweep here keeps the convergence property meaningful and
    /// keeps the missing production step visible instead of silently excluded.
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
        Ok(!stray.is_empty() || !orphans.is_empty())
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
                Some(Err(error)) => {
                    panic!("recovery failed: {error}; digest={:#?}", self.digest())
                }
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
        self.assert_pending_input_is_transient();
    }

    /// What a pane holding unsubmitted text has to satisfy at the fixpoint.
    ///
    /// A torn `tmux_send_keys` — the literal text sent, the Enter not — really
    /// does leave text nobody submitted, and *nothing clears it eagerly*:
    /// `await_pass` times the abandoned pass out and the loop goes idle with
    /// the line still sitting in the pane. Requiring the fixpoint to be free of
    /// it would be requiring something the daemon never promised, and would
    /// only be satisfiable by inventing a sweep production does not perform.
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
        let status = self.status_document();
        let state = LoopState::from_status(&status).expect("status is production-readable");
        let action = session_action(&config(), &state, "stub", 0, true, None);
        assert!(
            matches!(action, SessionAction::Restart(_)),
            "panes {dirty:?} hold unsubmitted text, and the next fire would \
             {action:?} rather than restart them — the text would be typed on"
        );
    }

    /// The scripted leader agent, run when the driver reads the pass it was
    /// handed.
    ///
    /// The append comes first and is atomic; the injection is cleared after.
    /// A crash between the two replays this, which is why ending the pass is
    /// guarded on the pass still being open — the second run finds it ended
    /// and only clears the injection.
    fn run_agent_if_ready(&self, pass_id: &str) {
        let script = {
            let world = self.shared.world.borrow();
            world.sessions.get(LEADER_SESSION).and_then(|session| {
                (session.injected_pass.as_deref() == Some(pass_id)).then_some(session.script)
            })
        };
        match script {
            Some(AgentScript::Complete) => {
                let open = self
                    .snapshot()
                    .state
                    .passes
                    .get(pass_id)
                    .is_some_and(|pass| pass.state == PassState::Open);
                if open {
                    let (footprint, _) = self
                        .stage(EventPayload::PassEnded {
                            pass_id: pass_id.to_owned(),
                            outcome: PassOutcome::Ok,
                            report: Some("scripted pass complete".to_owned()),
                            wake_at: None,
                            rotate: false,
                            why: None,
                        })
                        .expect("the scripted agent ends the pass it was handed");
                    self.effect("pass.end", footprint);
                }
                self.effect(
                    "pass.clear-injection",
                    Footprint::tearable(vec![Mutation::SessionClearInjection(
                        LEADER_SESSION.to_owned(),
                    )]),
                );
            }
            Some(AgentScript::DieMidPass) => {
                self.effect(
                    "agent.die",
                    Footprint::tearable(vec![Mutation::SessionRemove(LEADER_SESSION.to_owned())]),
                );
            }
            None => {}
        }
    }

    /// The simulated `alder <args> --json`.
    ///
    /// Every shape here is hand-mirrored from `src/app.rs`; see
    /// [`MIRRORED`] for the drift risk that carries.
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
                let Some(work) = snapshot.state.work.get(*id) else {
                    return Answer::Read(Err(DriverError::coded("not_found", "work not found")));
                };
                Answer::Read(Ok(self.show_pack(json!({
                    "id": work.id,
                    "kind": "work",
                    "current": {
                        "id": work.id,
                        "title": work.title,
                        "spec": work.spec,
                        "checks": work.checks,
                        "state": work.state.as_str(),
                    },
                    "history": [],
                }))))
            }
            ["show", id] if id.contains("-pass-") => {
                self.run_agent_if_ready(id);
                let snapshot = self.snapshot();
                let Some(pass) = snapshot.state.passes.get(*id) else {
                    return Answer::Read(Err(DriverError::coded("not_found", "pass not found")));
                };
                Answer::Read(Ok(self.show_pack(json!({
                    "id": pass.id,
                    "kind": "pass",
                    "current": {
                        "id": pass.id,
                        "state": match pass.state {
                            PassState::Open => "open",
                            PassState::Ended => "ended",
                        },
                        "outcome": pass.outcome.map(PassOutcome::as_str),
                    },
                    "history": [],
                }))))
            }
            ["status"] => Answer::Read(Ok(self.status_document())),
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
                Answer::Read(Ok(self.status_pack(json!({"in_flight": in_flight}))))
            }
            ["refresh"] => Answer::Read(Ok(
                self.refresh_pack(json!({"changed": false, "result": {"changed": false}}))
            )),
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
                    payload: EventPayload::AttemptStarted {
                        attempt: AttemptDefinition {
                            id: attempt_id.clone(),
                            work_id: (*work_id).to_owned(),
                            metadata: BTreeMap::new(),
                        },
                    },
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
                    payload: EventPayload::AttemptBound {
                        attempt_id: (*attempt_id).to_owned(),
                        handle: handle.to_owned(),
                        metadata,
                    },
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
                    other => {
                        return Answer::Read(Err(DriverError::new(format!(
                            "unsupported attempt outcome {other}"
                        ))));
                    }
                };
                Answer::Mutation {
                    payload: EventPayload::AttemptEnded {
                        attempt_id: (*attempt_id).to_owned(),
                        outcome,
                        why: value_after(rest, "--why")
                            .unwrap_or("simulated repair")
                            .to_owned(),
                    },
                    schema: "alder.attempt.end.v0",
                    fields: json!({"attempt_id": attempt_id, "outcome": requested}),
                }
            }
            ["loop", "wake", rest @ ..] => {
                let snapshot = self.snapshot();
                if let Some(open) = snapshot.state.open_pass() {
                    return Answer::Read(Err(DriverError::coded(
                        "pass_open",
                        format!("pass `{}` is open", open.id),
                    )));
                }
                let ordinal = snapshot.state.passes.len() + 1;
                let pass_id = format!("al-pass-{ordinal}");
                let engine = value_after(rest, "--engine").unwrap_or("stub").to_owned();
                let handle = value_after(rest, "--handle")
                    .unwrap_or("tmux:alder-leader")
                    .to_owned();
                let names = values_after(rest, "--trigger");
                let triggers: Vec<PassTrigger> = names
                    .iter()
                    .map(|trigger| match *trigger {
                        "log" => PassTrigger::Log,
                        "observations" => PassTrigger::Observations,
                        "manual" => PassTrigger::Manual,
                        _ => PassTrigger::Due,
                    })
                    .collect();
                Answer::Mutation {
                    payload: EventPayload::PassStarted {
                        pass: PassDefinition {
                            id: pass_id.clone(),
                            engine: engine.clone(),
                            handle: handle.clone(),
                            triggers,
                            at_head: snapshot.head.sequence(),
                        },
                    },
                    schema: "alder.loop.wake.v0",
                    fields: json!({
                        "pass_id": pass_id,
                        "engine": engine,
                        "handle": handle,
                        "triggers": names,
                    }),
                }
            }
            ["pass", "end", pass_id, rest @ ..] => {
                let outcome = match value_after(rest, "--outcome").unwrap_or("timeout") {
                    "crashed" => PassOutcome::Crashed,
                    "timeout" => PassOutcome::Timeout,
                    _ => PassOutcome::Ok,
                };
                Answer::Mutation {
                    payload: EventPayload::PassEnded {
                        pass_id: (*pass_id).to_owned(),
                        outcome,
                        report: None,
                        wake_at: None,
                        rotate: false,
                        why: value_after(rest, "--why").map(str::to_owned),
                    },
                    schema: "alder.pass.end.v0",
                    fields: json!({
                        "pass_id": pass_id,
                        "outcome": outcome.as_str(),
                        "rotate": false,
                    }),
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

    fn tmux_session_exists(&self, session: &str) -> Result<bool> {
        let exists = self.shared.world.borrow().sessions.contains_key(session);
        self.effect("spawn.session-probe", Footprint::read_only());
        Ok(exists)
    }

    fn tmux_new_session(&self, session: &str, cwd: &Path, _command: &str) -> Result<()> {
        if self.shared.world.borrow().sessions.contains_key(session) {
            return Err(DriverError::new("session already exists"));
        }
        let label = if session == LEADER_SESSION {
            "pass.session-create"
        } else {
            "spawn.session-create"
        };
        self.effect(
            label,
            Footprint::tearable(vec![Mutation::SessionCreate {
                name: session.to_owned(),
                cwd: cwd.to_path_buf(),
            }]),
        );
        Ok(())
    }

    fn tmux_set_environment(&self, session: &str, name: &str, value: &str) -> Result<()> {
        if !self.shared.world.borrow().sessions.contains_key(session) {
            return Err(DriverError::new("session missing"));
        }
        // Only the attempt stamp is world state the observer can read back.
        let footprint = if name == "ALDER_ATTEMPT" {
            Footprint::tearable(vec![Mutation::SessionStamp {
                name: session.to_owned(),
                attempt_id: value.to_owned(),
            }])
        } else {
            Footprint::read_only()
        };
        self.effect("spawn.session-stamp", footprint);
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
        self.effect("pass.session-probe", Footprint::read_only());
        Ok(exists)
    }

    fn tmux_new_session(&self, session: &str, _engine: &Engine) -> Result<()> {
        <Self as SpawnHost>::tmux_new_session(self, session, &self.root, "scripted leader")
    }

    fn tmux_kill_session(&self, session: &str) -> Result<()> {
        self.effect(
            "pass.session-kill",
            Footprint::tearable(vec![Mutation::SessionRemove(session.to_owned())]),
        );
        Ok(())
    }

    fn tmux_send_keys(&self, session: &str, text: &str) -> Result<()> {
        injected_pass_id(text).ok_or_else(|| DriverError::new("injection has no pass ID"))?;
        let pending = {
            let world = self.shared.world.borrow();
            match world.sessions.get(session) {
                None => return Err(DriverError::new("leader session missing")),
                Some(session) => session.pending_input.clone(),
            }
        };
        // The invariant unsubmitted text is really held to: nothing is ever
        // typed on top of it. tmux appends, so an injection typed onto a dirty
        // pane produces one line naming two passes, and the leader would run
        // neither. This fires the moment that happens rather than leaving it to
        // be inferred from a stuck fixpoint.
        assert!(
            pending.is_none(),
            "injecting `{text}` into `{session}`, which still holds unsubmitted \
             text {pending:?} from a torn injection"
        );
        // Production types the literal text and presses Enter as two separate
        // tmux invocations, so this is genuinely two mutations: a daemon killed
        // between them leaves the pane holding text nobody submitted, and the
        // leader is never handed the pass the log already says was woken.
        self.effect(
            "pass.inject",
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
        self.effect("pass.clients", Footprint::read_only());
        Ok(false)
    }

    fn read_file(&self, _path: &Path) -> Result<Vec<u8>> {
        self.effect("pass.read-doc", Footprint::read_only());
        Ok(b"run the scripted pass".to_vec())
    }

    fn file_mtime(&self, _path: &Path) -> Option<DateTime<Utc>> {
        self.effect("clock.marker", Footprint::read_only());
        None
    }

    fn notify(&self, message: &str) {
        self.effect(
            "pass.notify",
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
            "pass.log",
            Footprint::tearable(vec![Mutation::Message(message.to_owned())]),
        );
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
            assert!(!crash.label.is_empty());
            None
        }
        Err(payload) => resume_unwind(payload),
    }
}

pub fn execute_case(case: &Case) -> Digest {
    let host = Simulator::new(case.seed);
    host.schedule_faults(case.fault_schedule.clone());
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

/// The trace label one simulated `alder` invocation is recorded under.
fn dispatch_label(args: &[&str]) -> &'static str {
    match args {
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
    }
}

/// The pass an injection line hands the leader, read out of the line itself.
fn injected_pass_id(text: &str) -> Option<&str> {
    text.split("pass-id: ")
        .nth(1)
        .and_then(|tail| tail.split([';', ')']).next())
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
