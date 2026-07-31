//! A stateright model of Alder's protocol core.
//!
//! The model checks four properties of the loop protocol under every
//! interleaving of a small cast: the shared CAS log, the daemon's decide
//! loop, the leader engine it wakes, and an optional second writer (a phone
//! session). It deliberately reuses the real implementation wherever the
//! implementation is a pure function of a snapshot:
//!
//! - the log is a real [`MemoryLog`], so compare-and-append, idempotency, and
//!   head conflicts are the shipped resolution order, not a re-telling;
//! - records are encoded and decoded through the real codec, and every state
//!   is interpreted by the real [`ProjectState::fold`];
//! - the daemon reads the log through the real `loop_section` projection and
//!   the real [`LoopState`] parser, and judges with the real [`decide`],
//!   [`resolve_engine`][decide::resolve_engine], and
//!   [`session_action`][decide::session_action];
//! - and the safety properties are the shared sentences from
//!   [`alder::domain::invariants`][invariants], which the crash simulator also
//!   asserts, so the two harnesses cannot drift into meaning different things
//!   by "correct" while both stay green.
//!
//! Liveness and the `sometimes` properties stay here, and that is deliberate:
//! they are claims about reachability across a state space, and only a model
//! checker has one.
//!
//! What is modeled by hand — the drift surface — is the *sequencing* of each
//! actor: where a process can be interrupted between reading and writing, and
//! what a crash erases. See README.md for the bounds and the findings.

use std::hash::{Hash, Hasher};

use alder::app::loop_section;
use alder::domain::{
    Event, EventDraft, EventPayload, HandoffDefinition, PassDefinition, PassOutcome, PassTrigger,
    ProjectState, decode_record, encode_draft, invariants,
};
use alder_log::{AppendDisposition, Log, LogError, MemoryLog, Record, RecordDraft};
use alderd::config::Config;
use alderd::decide::{self, Decision, EngineChoice, Poll, Session, SessionAction, config_for};
use alderd::loop_state::LoopState;
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::json;
use stateright::{Model, Property};

/// Every event carries the same instant: time is not a modeled dimension.
/// The daemon's time-based triggers therefore reduce to their untimed cases —
/// a fresh project fires immediately, and nothing else is ever "due".
fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).expect("a valid instant")
}

const SCHEMA: &str = "alder.event.v0";
const DAEMON_ACTOR: &str = "alderd";
const LEADER_ACTOR: &str = "leader";
const PHONE_ACTOR: &str = "phone";
/// The phone's pass handle. It is not `tmux:`, so the daemon can never
/// observe it and never call it crashed — exactly the real contract.
const PHONE_HANDLE: &str = "codex:phone";
/// The pass document never changes inside a run, so its hash is a constant.
const DOC_HASH: u64 = 0x5eed;

/// Which optional actors and faults a run explores, and how far.
pub struct Scenario {
    pub config: Config,
    /// A second writer may race `alder loop wake` (budget: one attempt).
    pub phone_wake: bool,
    /// The second writer may append one `loop.rotation_requested`.
    pub phone_rotation: bool,
    /// The second writer may pause the loop with a stated reason.
    pub phone_pause: bool,
    /// The second writer may submit one handoff, including a lost-response
    /// retry of the identical draft.
    pub phone_handoff: bool,
    /// A pass may end with `rotate: true` (budget: once).
    pub leader_rotate: bool,
    /// How many daemon process crashes may be injected.
    pub daemon_crashes: u8,
    /// How many leader tmux session crashes may be injected.
    pub session_crashes: u8,
    /// Total passes that may ever start; the log length bound follows.
    pub max_passes: usize,
}

impl Scenario {
    pub fn new() -> Self {
        Self {
            config: config_for(&[("claude", "claude")]),
            phone_wake: false,
            phone_rotation: false,
            phone_pause: false,
            phone_handoff: false,
            leader_rotate: false,
            daemon_crashes: 0,
            session_crashes: 0,
            max_passes: 2,
        }
    }
}

impl Default for Scenario {
    fn default() -> Self {
        Self::new()
    }
}

/// The durable log plus every actor's volatile memory. The log is the only
/// shared truth; the rest models one process's state or the tmux reality the
/// daemon observes.
#[derive(Clone, Debug, PartialEq)]
pub struct ProtocolState {
    /// The shared record log, in append order.
    pub log: Vec<Record>,
    pub daemon: Daemon,
    pub phone: Phone,
    /// The leader tmux session generation, if one exists. `None` means never
    /// created, killed, or crashed — a dead pane ends the tmux session, which
    /// is what `tmux has-session` reports either way.
    pub tmux: Option<u8>,
    pub ghosts: Ghosts,
}

// Record bodies are JSON produced by the model's own encoder: no floats, so
// the derived partial equality is total.
impl Eq for ProtocolState {}

impl Hash for ProtocolState {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        // Records serialize deterministically for the model's fixed
        // construction paths, so the JSON text is a faithful digest.
        serde_json::to_string(&self.log)
            .expect("a log serializes")
            .hash(hasher);
        self.daemon.hash(hasher);
        self.phone.hash(hasher);
        self.tmux.hash(hasher);
        self.ghosts.hash(hasher);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Daemon {
    pub ctl: DaemonCtl,
    /// What the daemon remembers about the session it launched
    /// (`driver.rs`'s `self.session`); forgotten on a daemon crash.
    pub known: Option<KnownSession>,
    /// Wake attempts so far, for deterministic record IDs.
    pub wakes: u8,
    /// Remaining daemon process crashes the scenario may inject.
    pub crashes_left: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KnownSession {
    pub generation: u8,
    pub passes: u32,
}

/// The daemon's position between its atomic steps. The real driver runs
/// poll-decide-reconcile, then `alder loop wake` (whose own snapshot is
/// fresh), then the CAS push; a crash or a rival append can land between any
/// two of them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DaemonCtl {
    Idle,
    /// Decided `Fire` and reconciled the session; the wake call is next.
    Armed,
    /// The wake's own snapshot saw no open pass; the append is in flight
    /// against the head it pinned.
    Appending {
        expected: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Phone {
    pub wake: PhoneWake,
    pub handoff: PhoneHandoff,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PhoneWake {
    NotYet,
    /// `alder loop wake` snapshotted at this head and saw no open pass.
    Armed {
        expected: u64,
    },
    /// The one attempt was spent, appended or conceded.
    Spent,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PhoneHandoff {
    NotYet,
    /// The submission pinned this head; a conflict re-arms at the new head,
    /// which is the real retry loop.
    Armed {
        expected: u64,
    },
    /// The append landed but the response was lost; the writer must retry
    /// the identical draft and be answered `AlreadyPresent`.
    Unsure,
    Done,
}

/// Verification bookkeeping that is not part of any process's state. Latches
/// only ever go from false to true and counters only grow, which keeps the
/// state graph acyclic.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ghosts {
    /// Sessions ever created; the next generation number.
    pub sessions_created: u8,
    /// Leader session crashes injected so far.
    pub session_crashes: u8,
    /// Mirror of "a rotation request awaits its consuming wake", maintained
    /// independently of the fold so the two derivations can be compared.
    pub rotation_pending: bool,
    /// A fresh session was created after the pending request.
    pub rotation_performed: bool,
    /// A wake consumed a rotation whose restart had already happened.
    pub rotation_consumed_performed: bool,
    /// The daemon's own wake consumed a rotation it never performed.
    pub rotation_lost_by_daemon: bool,
    /// The phone's wake consumed a rotation nobody performed.
    pub rotation_swallowed_by_phone: bool,
    /// The daemon's wake call saw a rival's open pass and conceded.
    pub conceded: bool,
    /// A `pass.started` append lost the CAS race.
    pub wake_conflicted: bool,
    /// A handoff append lost the CAS race and re-armed.
    pub handoff_conflicted: bool,
    /// An identical retried draft was absorbed as `AlreadyPresent`.
    pub already_present: bool,
}

#[derive(Clone, Copy)]
enum Consumer {
    Daemon,
    Phone,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ProtocolAction {
    /// Poll, decide `Fire`, and reconcile the session (the pre-wake step).
    DaemonPollFires,
    /// The same poll after the max-interval ceiling has elapsed — the `Due`
    /// trigger that time, left running, always produces.
    DaemonCeilingFires,
    /// The wake CLI's own snapshot: concede to an open pass, or pin a head.
    DaemonWakeSnapshot,
    /// Push the pinned `pass.started` through the CAS append.
    DaemonWakeAppend,
    /// Observe the recorded session dead and end the open pass `crashed`.
    DaemonResolveCrashed,
    /// The daemon process dies and restarts, forgetting its session memory.
    DaemonCrash,
    /// The engine finishes its pass, optionally asking for rotation.
    LeaderEndPass {
        rotate: bool,
    },
    /// The leader tmux session dies.
    LeaderCrash,
    PhoneRotationRequest,
    PhonePause,
    PhoneWakeArm,
    PhoneWakeAppend,
    PhoneEndPass,
    PhoneHandoffArm,
    /// Append and see the receipt.
    PhoneHandoffAppendAcked,
    /// Append, but the response is lost before the writer sees it.
    PhoneHandoffAppendLost,
    /// Retry the identical draft after a lost response.
    PhoneHandoffRetry,
}

/// Rebuild a real in-process log from the recorded history. Replay goes
/// through the public append path, so the records in a state are exactly the
/// records a real store would hold.
fn replay(records: &[Record]) -> MemoryLog {
    let log = MemoryLog::new();
    for record in records {
        let head = log.head().expect("a memory head");
        let draft = RecordDraft::new(
            record.id().clone(),
            record.at(),
            record.actor(),
            record.kind().clone(),
            record.body().clone(),
            record.schema().clone(),
        )
        .expect("a persisted record re-drafts");
        log.append(&head, &draft).expect("a valid log replays");
    }
    log
}

/// Interpret a log with the real codec and the real fold, keeping both halves.
/// `None` marks a log no real reader could interpret, which the safety
/// properties flag.
///
/// The events come back alongside the state because two of the shared
/// predicates are claims about the history rather than about the fold, and
/// re-decoding for them would be asking the same question twice.
fn interpret(records: &[Record]) -> Option<(Vec<Event>, ProjectState)> {
    let events = records
        .iter()
        .map(decode_record)
        .collect::<Result<Vec<Event>, _>>()
        .ok()?;
    let state = ProjectState::fold(&events).ok()?;
    Some((events, state))
}

/// The fold alone, for the callers that only need the state.
fn fold(records: &[Record]) -> Option<ProjectState> {
    interpret(records).map(|(_, state)| state)
}

/// The daemon's complete read: the real status projection parsed by the real
/// `LoopState` reader.
fn daemon_view(records: &[Record]) -> Option<LoopState> {
    let state = fold(records)?;
    let status = json!({"head": records.len() as u64, "loop": loop_section(&state)});
    LoopState::from_status(&status).ok()
}

/// What the daemon observes besides the log: nothing. No client is attached,
/// refresh saw no change, and debounce has settled.
fn observation() -> Poll {
    Poll {
        now: epoch(),
        refresh_changed: false,
        pending_since: None,
        attached_client: false,
    }
}

/// The same observation, made after the configured ceiling has elapsed. Time
/// is not a modeled dimension, but "the max interval eventually passes" is a
/// fairness fact, so the model offers it as an explicit step.
fn late_observation(config: &Config) -> Poll {
    Poll {
        now: epoch() + TimeDelta::seconds(config.max_interval_seconds as i64 + 1),
        ..observation()
    }
}

fn typed_draft(id: &str, actor: &str, payload: EventPayload) -> RecordDraft {
    encode_draft(&EventDraft {
        id: id.to_owned(),
        at: epoch(),
        actor: actor.to_owned(),
        payload,
        schema: SCHEMA.to_owned(),
    })
    .expect("a model draft encodes")
}

enum AppendOutcome {
    Appended(Vec<Record>),
    AlreadyPresent(Vec<Record>),
    Conflicted,
}

/// Attempt a compare-and-append against the head pinned at `expected_seq`
/// records, resolved by the real store. The pinned head is recovered by
/// replaying the prefix the writer actually saw — the log is linear, so a
/// length names exactly one prefix.
fn append_at(records: &[Record], expected_seq: u64, draft: &RecordDraft) -> AppendOutcome {
    let expected = replay(&records[..expected_seq as usize])
        .head()
        .expect("a replayed head");
    let store = replay(records);
    match store.append(&expected, draft) {
        Ok(receipt) => {
            let head = store.head().expect("a memory head");
            let complete = store.read_all(&head).expect("a complete read");
            match receipt.disposition {
                AppendDisposition::Appended => AppendOutcome::Appended(complete),
                AppendDisposition::AlreadyPresent => AppendOutcome::AlreadyPresent(complete),
            }
        }
        Err(LogError::HeadConflict { .. }) => AppendOutcome::Conflicted,
        Err(error) => panic!("the model expects only head conflicts: {error}"),
    }
}

/// Append at the current head. For appends whose read-write gap the model
/// does not explore; the interesting gaps all sit in the wake and handoff
/// paths, which go through [`append_at`] with a pinned head.
fn append_now(records: &[Record], draft: &RecordDraft) -> Vec<Record> {
    match append_at(records, records.len() as u64, draft) {
        AppendOutcome::Appended(complete) => complete,
        _ => panic!("an append at the current head lands"),
    }
}

fn handoff_draft() -> RecordDraft {
    typed_draft(
        "phone-handoff-1",
        PHONE_ACTOR,
        EventPayload::HandoffSubmitted {
            handoff: HandoffDefinition {
                id: "hm-handoff-model".to_owned(),
                title: "a handoff from the phone".to_owned(),
                artifact_ref: "branch:phone-session".to_owned(),
                note: None,
            },
        },
    )
}

fn consume_rotation(ghosts: &mut Ghosts, consumer: Consumer) {
    if !ghosts.rotation_pending {
        return;
    }
    if ghosts.rotation_performed {
        ghosts.rotation_consumed_performed = true;
    } else {
        match consumer {
            Consumer::Daemon => ghosts.rotation_lost_by_daemon = true,
            Consumer::Phone => ghosts.rotation_swallowed_by_phone = true,
        }
    }
    ghosts.rotation_pending = false;
    ghosts.rotation_performed = false;
}

fn daemon_handle(config: &Config) -> String {
    format!("tmux:{}", config.tmux_session)
}

/// How many passes this log blames on a crash, counted by the shared module.
/// A log no reader can interpret blames nobody; [`log_folds_cleanly`] is the
/// property that fires on it.
///
/// [`log_folds_cleanly`]: invariants::log_folds_cleanly
fn crashed_verdicts(state: &ProtocolState) -> usize {
    fold(&state.log).map_or(0, |project| invariants::crashed_verdicts(&project))
}

/// The recovery target: no pass is open — in particular none stranded by a
/// crash — and if the loop is blocked it is blocked out loud, by a pause
/// with a stated reason. From here the next trigger simply runs.
fn recovered(state: &ProtocolState) -> bool {
    fold(&state.log).is_some_and(|project| {
        project.open_pass().is_none()
            && (!project.loop_control.paused || project.loop_control.pause_reason.is_some())
    })
}

impl Scenario {
    /// The daemon fire path: real decide, real engine resolution, real
    /// session reconciliation, in the driver's order — session first, wake
    /// second, so a crash between them re-rotates instead of losing one.
    fn daemon_poll_fires(&self, state: &mut ProtocolState, poll: &Poll) -> Option<()> {
        let view = daemon_view(&state.log)?;
        let Decision::Fire(_) = decide::decide(&self.config, &view, poll) else {
            return None;
        };
        let EngineChoice::Run(engine) = decide::resolve_engine(&self.config, &view) else {
            return None;
        };
        let known = state.daemon.known.as_ref().map(|session| Session {
            engine: engine.clone(),
            pass_doc_hash: DOC_HASH,
            passes: session.passes,
        });
        let action = decide::session_action(
            &self.config,
            &view,
            &engine,
            DOC_HASH,
            state.tmux.is_some(),
            known.as_ref(),
        );
        match action {
            SessionAction::Reuse => {}
            SessionAction::Restart(_) | SessionAction::Create => {
                state.ghosts.sessions_created += 1;
                let generation = state.ghosts.sessions_created;
                state.tmux = Some(generation);
                state.daemon.known = Some(KnownSession {
                    generation,
                    passes: 0,
                });
                if state.ghosts.rotation_pending {
                    state.ghosts.rotation_performed = true;
                }
            }
        }
        state.daemon.wakes += 1;
        state.daemon.ctl = DaemonCtl::Armed;
        Some(())
    }

    fn daemon_wake_append(&self, state: &mut ProtocolState) -> Option<()> {
        let DaemonCtl::Appending { expected } = state.daemon.ctl else {
            return None;
        };
        let prefix = fold(&state.log[..expected as usize])?;
        let pass_id = format!("hm-pass-{}", prefix.passes.len() + 1);
        let draft = typed_draft(
            &format!("daemon-wake-{}", state.daemon.wakes),
            DAEMON_ACTOR,
            EventPayload::PassStarted {
                pass: PassDefinition {
                    id: pass_id,
                    engine: "claude".to_owned(),
                    handle: daemon_handle(&self.config),
                    triggers: vec![PassTrigger::Due],
                    at_head: expected,
                },
            },
        );
        match append_at(&state.log, expected, &draft) {
            AppendOutcome::Appended(log) => {
                state.log = log;
                if let Some(session) = state.daemon.known.as_mut() {
                    session.passes += 1;
                }
                consume_rotation(&mut state.ghosts, Consumer::Daemon);
            }
            AppendOutcome::Conflicted => state.ghosts.wake_conflicted = true,
            AppendOutcome::AlreadyPresent(_) => return None,
        }
        state.daemon.ctl = DaemonCtl::Idle;
        Some(())
    }

    fn daemon_resolve_crashed(&self, state: &mut ProtocolState) -> Option<()> {
        let project = fold(&state.log)?;
        let pass = project.open_pass()?;
        let own = pass.handle == daemon_handle(&self.config);
        let draft = typed_draft(
            &format!("daemon-end-{}", pass.id),
            DAEMON_ACTOR,
            EventPayload::PassEnded {
                pass_id: pass.id.clone(),
                outcome: PassOutcome::Crashed,
                report: None,
                wake_at: None,
                rotate: false,
                why: Some("the tmux session is gone".to_owned()),
            },
        );
        state.log = append_now(&state.log, &draft);
        if own {
            state.daemon.known = None;
        }
        Some(())
    }

    fn phone_wake_append(&self, state: &mut ProtocolState) -> Option<()> {
        let PhoneWake::Armed { expected } = state.phone.wake else {
            return None;
        };
        let prefix = fold(&state.log[..expected as usize])?;
        let pass_id = format!("hm-pass-{}", prefix.passes.len() + 1);
        let draft = typed_draft(
            "phone-wake-1",
            PHONE_ACTOR,
            EventPayload::PassStarted {
                pass: PassDefinition {
                    id: pass_id,
                    engine: "codex".to_owned(),
                    handle: PHONE_HANDLE.to_owned(),
                    triggers: vec![PassTrigger::Manual],
                    at_head: expected,
                },
            },
        );
        match append_at(&state.log, expected, &draft) {
            AppendOutcome::Appended(log) => {
                state.log = log;
                consume_rotation(&mut state.ghosts, Consumer::Phone);
            }
            AppendOutcome::Conflicted => state.ghosts.wake_conflicted = true,
            AppendOutcome::AlreadyPresent(_) => return None,
        }
        state.phone.wake = PhoneWake::Spent;
        Some(())
    }

    fn end_pass_ok(
        state: &mut ProtocolState,
        actor: &str,
        id_prefix: &str,
        rotate: bool,
    ) -> Option<()> {
        let project = fold(&state.log)?;
        let pass = project.open_pass()?;
        let draft = typed_draft(
            &format!("{id_prefix}-{}", pass.id),
            actor,
            EventPayload::PassEnded {
                pass_id: pass.id.clone(),
                outcome: PassOutcome::Ok,
                report: None,
                wake_at: None,
                rotate,
                why: None,
            },
        );
        state.log = append_now(&state.log, &draft);
        if rotate {
            state.ghosts.rotation_pending = true;
            state.ghosts.rotation_performed = false;
        }
        Some(())
    }
}

impl Model for Scenario {
    type State = ProtocolState;
    type Action = ProtocolAction;

    fn init_states(&self) -> Vec<Self::State> {
        vec![ProtocolState {
            log: Vec::new(),
            daemon: Daemon {
                ctl: DaemonCtl::Idle,
                known: None,
                wakes: 0,
                crashes_left: self.daemon_crashes,
            },
            phone: Phone {
                wake: PhoneWake::NotYet,
                handoff: PhoneHandoff::NotYet,
            },
            tmux: None,
            ghosts: Ghosts::default(),
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        // A log no real reader could interpret is a dead end; the safety
        // properties flag it rather than exploring past it.
        let Some(project) = fold(&state.log) else {
            return;
        };
        let open = project.open_pass();

        match state.daemon.ctl {
            DaemonCtl::Idle => {
                if project.passes.len() < self.max_passes
                    && let Some(view) = daemon_view(&state.log)
                {
                    let prompt = decide::decide(&self.config, &view, &observation());
                    if matches!(prompt, Decision::Fire(_)) {
                        actions.push(ProtocolAction::DaemonPollFires);
                    } else if matches!(
                        decide::decide(&self.config, &view, &late_observation(&self.config)),
                        Decision::Fire(_)
                    ) {
                        actions.push(ProtocolAction::DaemonCeilingFires);
                    }
                }
                if let Some(pass) = open
                    && decide::observable_session(&pass.handle).is_some()
                    && state.tmux.is_none()
                {
                    actions.push(ProtocolAction::DaemonResolveCrashed);
                }
            }
            DaemonCtl::Armed => actions.push(ProtocolAction::DaemonWakeSnapshot),
            DaemonCtl::Appending { .. } => actions.push(ProtocolAction::DaemonWakeAppend),
        }
        // A crash during the CAS push itself is the ambiguous-response case;
        // the driver resolves that era by timeout, which needs time, so the
        // model stops the crash injection short of it.
        if state.daemon.crashes_left > 0 && !matches!(state.daemon.ctl, DaemonCtl::Appending { .. })
        {
            actions.push(ProtocolAction::DaemonCrash);
        }

        if let Some(pass) = open
            && decide::observable_session(&pass.handle).is_some()
            && state.tmux.is_some()
        {
            actions.push(ProtocolAction::LeaderEndPass { rotate: false });
            if self.leader_rotate && !project.passes.values().any(|pass| pass.rotate) {
                actions.push(ProtocolAction::LeaderEndPass { rotate: true });
            }
        }
        if state.tmux.is_some() && state.ghosts.session_crashes < self.session_crashes {
            actions.push(ProtocolAction::LeaderCrash);
        }

        if self.phone_rotation
            && !state
                .log
                .iter()
                .any(|record| record.kind().as_str() == "loop.rotation_requested")
        {
            actions.push(ProtocolAction::PhoneRotationRequest);
        }
        if self.phone_pause
            && !state
                .log
                .iter()
                .any(|record| record.kind().as_str() == "loop.paused")
        {
            actions.push(ProtocolAction::PhonePause);
        }
        match state.phone.wake {
            PhoneWake::NotYet
                if self.phone_wake && open.is_none() && project.passes.len() < self.max_passes =>
            {
                actions.push(ProtocolAction::PhoneWakeArm);
            }
            PhoneWake::Armed { .. } => actions.push(ProtocolAction::PhoneWakeAppend),
            _ => {}
        }
        if open.is_some_and(|pass| pass.handle == PHONE_HANDLE) {
            actions.push(ProtocolAction::PhoneEndPass);
        }
        match state.phone.handoff {
            PhoneHandoff::NotYet if self.phone_handoff => {
                actions.push(ProtocolAction::PhoneHandoffArm);
            }
            PhoneHandoff::Armed { .. } => {
                actions.push(ProtocolAction::PhoneHandoffAppendAcked);
                actions.push(ProtocolAction::PhoneHandoffAppendLost);
            }
            PhoneHandoff::Unsure => actions.push(ProtocolAction::PhoneHandoffRetry),
            _ => {}
        }
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut state = last.clone();
        match action {
            ProtocolAction::DaemonPollFires => {
                self.daemon_poll_fires(&mut state, &observation())?
            }
            ProtocolAction::DaemonCeilingFires => {
                self.daemon_poll_fires(&mut state, &late_observation(&self.config))?;
            }
            ProtocolAction::DaemonWakeSnapshot => {
                let project = fold(&state.log)?;
                if project.open_pass().is_some() {
                    state.ghosts.conceded = true;
                    state.daemon.ctl = DaemonCtl::Idle;
                } else {
                    state.daemon.ctl = DaemonCtl::Appending {
                        expected: state.log.len() as u64,
                    };
                }
            }
            ProtocolAction::DaemonWakeAppend => self.daemon_wake_append(&mut state)?,
            ProtocolAction::DaemonResolveCrashed => self.daemon_resolve_crashed(&mut state)?,
            ProtocolAction::DaemonCrash => {
                state.daemon.ctl = DaemonCtl::Idle;
                state.daemon.known = None;
                state.daemon.crashes_left -= 1;
            }
            ProtocolAction::LeaderEndPass { rotate } => {
                Self::end_pass_ok(&mut state, LEADER_ACTOR, "leader-end", rotate)?;
            }
            ProtocolAction::LeaderCrash => {
                state.tmux = None;
                state.ghosts.session_crashes += 1;
            }
            ProtocolAction::PhoneRotationRequest => {
                let draft = typed_draft(
                    "phone-rotate-1",
                    PHONE_ACTOR,
                    EventPayload::LoopRotationRequested { why: None },
                );
                state.log = append_now(&state.log, &draft);
                state.ghosts.rotation_pending = true;
                state.ghosts.rotation_performed = false;
            }
            ProtocolAction::PhonePause => {
                let draft = typed_draft(
                    "phone-pause-1",
                    PHONE_ACTOR,
                    EventPayload::LoopPaused {
                        why: Some("maintenance window".to_owned()),
                    },
                );
                state.log = append_now(&state.log, &draft);
            }
            ProtocolAction::PhoneWakeArm => {
                state.phone.wake = PhoneWake::Armed {
                    expected: state.log.len() as u64,
                };
            }
            ProtocolAction::PhoneWakeAppend => self.phone_wake_append(&mut state)?,
            ProtocolAction::PhoneEndPass => {
                Self::end_pass_ok(&mut state, PHONE_ACTOR, "phone-end", false)?;
            }
            ProtocolAction::PhoneHandoffArm => {
                state.phone.handoff = PhoneHandoff::Armed {
                    expected: state.log.len() as u64,
                };
            }
            ProtocolAction::PhoneHandoffAppendAcked => {
                let PhoneHandoff::Armed { expected } = state.phone.handoff else {
                    return None;
                };
                match append_at(&state.log, expected, &handoff_draft()) {
                    AppendOutcome::Appended(log) | AppendOutcome::AlreadyPresent(log) => {
                        state.log = log;
                        state.phone.handoff = PhoneHandoff::Done;
                    }
                    AppendOutcome::Conflicted => {
                        state.ghosts.handoff_conflicted = true;
                        state.phone.handoff = PhoneHandoff::Armed {
                            expected: state.log.len() as u64,
                        };
                    }
                }
            }
            ProtocolAction::PhoneHandoffAppendLost => {
                let PhoneHandoff::Armed { expected } = state.phone.handoff else {
                    return None;
                };
                match append_at(&state.log, expected, &handoff_draft()) {
                    AppendOutcome::Appended(log) => {
                        state.log = log;
                        state.phone.handoff = PhoneHandoff::Unsure;
                    }
                    // The acked variant already covers the conflict re-arm.
                    AppendOutcome::Conflicted | AppendOutcome::AlreadyPresent(_) => return None,
                }
            }
            ProtocolAction::PhoneHandoffRetry => {
                let PhoneHandoff::Unsure = state.phone.handoff else {
                    return None;
                };
                match append_at(&state.log, state.log.len() as u64, &handoff_draft()) {
                    AppendOutcome::AlreadyPresent(log) => {
                        state.log = log;
                        state.ghosts.already_present = true;
                        state.phone.handoff = PhoneHandoff::Done;
                    }
                    _ => return None,
                }
            }
        }
        Some(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        // The five safety properties are the shared sentences: each closure
        // calls `alder::domain::invariants` rather than restating the
        // predicate here, so this model and the crash simulator cannot drift
        // into meaning different things by "correct" while both stay green.
        // Everything below them — liveness, the ghost audit, and every
        // `sometimes` — is a claim about reachability across a state space,
        // which only a model checker has, and stays local on purpose.
        let mut properties = vec![
            Property::<Self>::always("every reachable log folds cleanly", |_, state| {
                invariants::log_folds_cleanly(&state.log)
            }),
            Property::<Self>::always("at most one pass is ever open", |_, state| {
                fold(&state.log).is_some_and(|project| invariants::at_most_one_open_pass(&project))
            }),
            Property::<Self>::always("a crashed verdict follows a real crash", |_, state| {
                // The witness the log cannot supply: the deaths this run
                // actually injected, counted by the injector.
                fold(&state.log).is_some_and(|project| {
                    invariants::crashed_verdicts_follow_real_crashes(
                        &project,
                        usize::from(state.ghosts.session_crashes),
                    )
                })
            }),
            Property::<Self>::always("rotate_pending mirrors the request ledger", |_, state| {
                interpret(&state.log).is_some_and(|(events, project)| {
                    invariants::rotate_pending_mirrors_the_request_ledger(&project, &events)
                })
            }),
            Property::<Self>::always("an acknowledged handoff is never lost", |_, state| {
                // The other witness. `Done` is the shared module's literal
                // case — the writer saw its receipt. `Unsure` is the stronger
                // one: the model injected the lost response, so it knows the
                // append landed even though the writer does not, and the log
                // owes that submission just as much. Before either, the phone
                // is entitled to nothing, and the predicate holds the log only
                // to the no-duplicates half.
                let landed: &[&str] = match state.phone.handoff {
                    PhoneHandoff::Unsure | PhoneHandoff::Done => &["phone-handoff-1"],
                    PhoneHandoff::NotYet | PhoneHandoff::Armed { .. } => &[],
                };
                interpret(&state.log).is_some_and(|(events, project)| {
                    invariants::acknowledged_handoffs_are_never_lost(&project, &events, landed)
                })
            }),
            Property::<Self>::always(
                "every terminal state is progressing or blocked-and-named",
                |model, state| {
                    let mut enabled = Vec::new();
                    model.actions(state, &mut enabled);
                    !enabled.is_empty() || recovered(state)
                },
            ),
            // Not a protocol predicate: an audit of this model's own
            // bookkeeping. The shared statement above compares the fold's
            // arithmetic against a scan of the history, and no longer needs
            // the ghost mirror to do it — but three `sometimes` properties and
            // the daemon-ordering `always` are all derived from that mirror,
            // so a mirror that drifted would let them report on a rotation
            // ledger nobody has. Checking it against the fold keeps them
            // honest, and costs one comparison.
            Property::<Self>::always("the rotation ghost tracks the fold", |_, state| {
                fold(&state.log).is_some_and(|project| {
                    project.loop_control.rotate_pending() == state.ghosts.rotation_pending
                })
            }),
        ];
        if self.leader_rotate || self.phone_rotation {
            properties.push(Property::<Self>::sometimes(
                "a rotation is performed and then consumed",
                |_, state| state.ghosts.rotation_consumed_performed,
            ));
        }
        if self.phone_wake {
            properties.push(Property::<Self>::sometimes(
                "a lost wake race is conceded",
                |_, state| state.ghosts.conceded,
            ));
            properties.push(Property::<Self>::sometimes(
                "a wake append loses the CAS race",
                |_, state| state.ghosts.wake_conflicted,
            ));
        } else {
            // With one waker there is no race, so the driver's ordering
            // guarantee is checkable: a rotation the daemon consumes was
            // always performed first, whatever crashed in between.
            properties.push(Property::<Self>::always(
                "a rotation consumed by the daemon was performed first",
                |_, state| !state.ghosts.rotation_lost_by_daemon,
            ));
        }
        if self.phone_wake && self.phone_rotation {
            // The concurrent-writer wart, kept visible on purpose: a wake can
            // consume a rotation request nobody performed. See README.md.
            properties.push(Property::<Self>::sometimes(
                "a racing wake consumes a rotation nobody performed",
                |_, state| {
                    state.ghosts.rotation_swallowed_by_phone || state.ghosts.rotation_lost_by_daemon
                },
            ));
        }
        if self.session_crashes > 0 {
            properties.push(Property::<Self>::sometimes(
                "a crashed pass is attributed in the log",
                |_, state| crashed_verdicts(state) > 0,
            ));
        }
        if self.daemon_crashes > 0 {
            properties.push(Property::<Self>::sometimes(
                "a daemon crash is exercised",
                |model, state| state.daemon.crashes_left < model.daemon_crashes,
            ));
        }
        if self.phone_handoff {
            properties.push(Property::<Self>::sometimes(
                "a lost response is absorbed idempotently",
                |_, state| state.ghosts.already_present,
            ));
            properties.push(Property::<Self>::sometimes(
                "a handoff append loses the CAS race and retries",
                |_, state| state.ghosts.handoff_conflicted,
            ));
        }
        properties
    }
}
