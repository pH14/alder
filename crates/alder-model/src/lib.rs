//! A stateright model of Alder's protocol core.
//!
//! The model checks the wake protocol under every interleaving of a small
//! cast: the shared CAS log, the daemon's decide loop with its machine-local
//! notes, the executor engine it wakes, and an optional second writer (a phone
//! session). It deliberately reuses the real implementation wherever the
//! implementation is a pure function of a snapshot:
//!
//! - the log is a real [`MemoryLog`], so append semantics are the shipped
//!   resolution order, not a re-telling;
//! - records are encoded and decoded through the real codec, and every state
//!   is interpreted by the real [`ProjectState::fold`];
//! - the daemon reads the log through the real `loop_section` projection and
//!   the real [`LoopState`] parser, and judges with the real [`decide`],
//!   [`resolve_engine`][decide::resolve_engine], and
//!   [`session_action`][decide::session_action] over real [`Notes`];
//! - and the safety properties are the shared sentences from
//!   [`alder::domain::invariants`][invariants], which the crash simulator also
//!   asserts, so the two harnesses cannot drift into meaning different things
//!   by "correct" while both stay green.
//!
//! What is modeled by hand — the drift surface — is the *sequencing* of each
//! actor: where a process can be interrupted between reading and writing, and
//! what a crash erases. The central claim is stated by the shape of the model
//! itself: the daemon has no append step, because it appends nothing. A wake
//! is an injection plus a notes write, both outside the log, so the crash
//! windows are "delivered but not noted" (a duplicate wake) and "noted state
//! lost" (also a duplicate wake) — and every property holds through both,
//! which is what "passes are idempotent and nothing durable records them"
//! means, checked. See README.md for the bounds and the findings.

use std::hash::{Hash, Hasher};

use alder::app::loop_section;
use alder::domain::{
    Event, EventDraft, EventPayload, LoopEventPayload, ProjectState, WorkDefinition,
    WorkEventPayload, WorkOperation, decode_record, encode_draft, invariants,
};
use alder_log::{Log, MemoryLog, Record, RecordDraft};
use alderd::config::Config;
use alderd::decide::{self, Decision, EngineChoice, Notes, Poll, SessionAction, config_for};
use alderd::loop_state::LoopState;
use chrono::{DateTime, Utc};
use serde_json::json;
use stateright::{Model, Property};

/// Every event carries the same instant: time is not a modeled dimension.
/// The daemon's time-based triggers therefore reduce to their untimed cases —
/// fresh notes fire immediately, and nothing else is ever "due".
fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).expect("a valid instant")
}

const SCHEMA: &str = "alder.event.v0";
const DAEMON_ACTOR: &str = "alderd";
const EXECUTOR_ACTOR: &str = "executor";
const PHONE_ACTOR: &str = "phone";

/// Which optional actors and faults a run explores, and how far.
pub struct Scenario {
    pub config: Config,
    /// The second writer may append one `loop.rotation_requested`.
    pub phone_rotation: bool,
    /// The second writer may pause the loop with a stated reason.
    pub phone_pause: bool,
    /// The second writer may append one ordinary work statement.
    pub phone_work: bool,
    /// How many work statements the executor may append when woken.
    pub executor_appends: u8,
    /// How many daemon process crashes may be injected.
    pub daemon_crashes: u8,
    /// How many executor tmux session crashes may be injected.
    pub session_crashes: u8,
    /// How many times the daemon's notes file may be lost.
    pub notes_losses: u8,
}

impl Scenario {
    pub fn new() -> Self {
        Self {
            config: config_for(&[("claude", "claude")]),
            phone_rotation: false,
            phone_pause: false,
            phone_work: false,
            executor_appends: 1,
            daemon_crashes: 0,
            session_crashes: 0,
            notes_losses: 0,
        }
    }
}

impl Default for Scenario {
    fn default() -> Self {
        Self::new()
    }
}

/// The durable state plus every actor's volatile memory. The log is the only
/// shared truth about the project; the notes are one machine's truth about
/// itself; the rest models one process's state or the tmux reality the daemon
/// observes.
#[derive(Clone, Debug, PartialEq)]
pub struct ProtocolState {
    /// The shared record log, in append order.
    pub log: Vec<Record>,
    /// The daemon's machine-local notes file. Durable across daemon crashes,
    /// erasable only by the explicit notes-loss fault.
    pub notes: Notes,
    pub daemon: Daemon,
    pub phone: Phone,
    /// The executor tmux session generation, if one exists.
    pub tmux: Option<u8>,
    /// Whether the executor holds a submitted wake line it has not acted on.
    pub executor_pending: bool,
    /// Work statements the executor may still append.
    pub executor_appends_left: u8,
    pub ghosts: Ghosts,
}

// Record bodies are JSON produced by the model's own encoder: no floats, so
// the derived partial equality is total.
impl Eq for ProtocolState {}

impl Hash for ProtocolState {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        serde_json::to_string(&self.log)
            .expect("a log serializes")
            .hash(hasher);
        serde_json::to_string(&self.notes)
            .expect("notes serialize")
            .hash(hasher);
        self.daemon.hash(hasher);
        self.phone.hash(hasher);
        self.tmux.hash(hasher);
        self.executor_pending.hash(hasher);
        self.executor_appends_left.hash(hasher);
        self.ghosts.hash(hasher);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Daemon {
    pub ctl: DaemonCtl,
    /// Whether this daemon created the running session generation
    /// (`driver.rs`'s `self.session`); forgotten on a daemon crash.
    pub knows_session: Option<u8>,
    /// Remaining daemon process crashes the scenario may inject.
    pub crashes_left: u8,
}

/// The daemon's position between its atomic steps. The real driver runs
/// poll-decide-reconcile, then `tmux_send_keys`, then the notes write; a
/// crash can land between any two of them.
///
/// `Fired` and `Injected` carry the head the decision saw, because that is
/// what the notes write records. Recomputing it at the write would be noting
/// a fresher head than the driver acted on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DaemonCtl {
    Idle,
    /// Decided `Fire` and reconciled the session; the injection is next.
    Fired {
        head: u64,
    },
    /// The line was typed and submitted; the notes write is next. A crash
    /// here is the duplicate-wake window: the executor was handed the line, and
    /// nothing anywhere says so.
    Injected {
        head: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Phone {
    pub rotation_left: bool,
    pub pause_left: bool,
    pub work_left: bool,
}

/// Verification bookkeeping that is not part of any process's state. Latches
/// only ever go from false to true and counters only grow, which keeps the
/// state graph acyclic.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ghosts {
    /// Sessions ever created; the next generation number.
    pub sessions_created: u8,
    /// Executor session crashes injected so far.
    pub session_crashes: u8,
    /// Notes-file losses injected so far.
    pub notes_lost: u8,
    /// Wake lines ever submitted at the executor.
    pub wakes_delivered: u8,
    /// The head the most recent delivery was for, to spot a duplicate.
    pub last_delivered_head: Option<u64>,
    /// A wake was delivered twice for the same head.
    pub duplicate_wake: bool,
    /// A daemon crash landed between the injection and the notes write,
    /// stranding a delivered wake that nothing anywhere records.
    pub wake_delivered_unnoted: bool,
    /// Mirror of "a rotation request awaits its consuming act", maintained
    /// independently of the fold so the two derivations can be compared.
    pub rotation_pending: bool,
    /// A fresh session was created after the pending request.
    pub rotation_performed: bool,
    /// A notes write consumed a rotation whose restart had already happened.
    pub rotation_consumed_performed: bool,
    /// A notes write consumed a rotation that was never performed.
    pub rotation_lost: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ProtocolAction {
    /// Poll, decide `Fire`, and reconcile the session (the pre-wake step).
    DaemonPollFires,
    /// Type the wake line into the executor's terminal and submit it — the
    /// driver's `tmux_send_keys`.
    DaemonInject,
    /// Persist the notes: the head just acted on, durably enough for a
    /// restart.
    DaemonNoteWrite,
    /// The daemon process dies and restarts, forgetting its session memory.
    /// The notes file survives.
    DaemonCrash,
    /// The machine loses the notes file.
    NotesLost,
    /// The woken engine reads the fold and acts: appends one work statement,
    /// or finds nothing demanding and idles.
    ExecutorActs {
        appends: bool,
    },
    /// The executor tmux session dies.
    ExecutorCrash,
    PhoneRotationRequest,
    PhonePause,
    PhoneWorkStatement,
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

/// What the daemon observes besides the log and its notes: nothing. No client
/// is attached, refresh saw no change, and debounce has settled.
fn observation() -> Poll {
    Poll {
        now: epoch(),
        refresh_changed: false,
        pending_since: None,
        attached_client: false,
    }
}

fn typed_draft(id: &str, actor: &str, payload: impl Into<EventPayload>) -> RecordDraft {
    encode_draft(&EventDraft {
        id: id.to_owned(),
        at: epoch(),
        actor: actor.to_owned(),
        payload: payload.into(),
        schema: SCHEMA.to_owned(),
    })
    .expect("a model draft encodes")
}

/// Append at the current head. The wake path appends nothing, so no modeled
/// writer has a read-write gap worth exploring against the CAS: each appender
/// writes at the head it holds.
fn append_now(records: &[Record], draft: &RecordDraft) -> Vec<Record> {
    let store = replay(records);
    let head = store.head().expect("a memory head");
    store
        .append(&head, draft)
        .expect("an append at the head lands");
    let head = store.head().expect("a memory head");
    store.read_all(&head).expect("a complete read")
}

fn work_statement(id: &str, work_id: &str, actor: &str) -> RecordDraft {
    typed_draft(
        id,
        actor,
        WorkEventPayload::WorkChanged {
            why: None,
            operations: vec![WorkOperation::Add {
                work: WorkDefinition {
                    id: work_id.to_owned(),
                    title: "modeled statement".to_owned(),
                    spec: None,
                    priority: 0,
                    requires: Vec::new(),
                    checks: Vec::new(),
                },
            }],
        },
    )
}

/// The recovery target: the log folds, and if the loop is blocked it is
/// blocked out loud, by a pause with a stated reason. Nothing durable can be
/// "open": a wake is not a record, so there is no stranded run to rule out —
/// which is itself the design being checked.
fn recovered(state: &ProtocolState) -> bool {
    fold(&state.log).is_some_and(|project| {
        !project.loop_control.paused || project.loop_control.pause_reason.is_some()
    })
}

impl Scenario {
    /// The daemon fire path: real decide over real notes, real engine
    /// resolution, real session reconciliation, in the driver's order —
    /// session first, notes last, so a crash between them re-rotates or
    /// re-delivers instead of losing anything.
    fn daemon_poll_fires(&self, state: &mut ProtocolState) -> Option<()> {
        let view = daemon_view(&state.log)?;
        let Decision::Fire(_) = decide::decide(&self.config, &view, &state.notes, &observation())
        else {
            return None;
        };
        let EngineChoice::Run(engine) = decide::resolve_engine(&self.config, &view) else {
            return None;
        };
        let known = state.daemon.knows_session.and_then(|generation| {
            (state.tmux == Some(generation)).then(|| decide::Session {
                engine: engine.clone(),
                pass_doc_hash: 0,
                created_at: epoch(),
            })
        });
        let action = decide::session_action(
            &self.config,
            decide::rotate_pending(&view, &state.notes),
            &engine,
            0,
            state.tmux.is_some(),
            known.as_ref(),
            epoch(),
        );
        match action {
            SessionAction::Reuse => {}
            SessionAction::Restart(_) | SessionAction::Create => {
                state.ghosts.sessions_created += 1;
                let generation = state.ghosts.sessions_created;
                state.tmux = Some(generation);
                state.daemon.knows_session = Some(generation);
                // The pane died with whatever line it held.
                state.executor_pending = false;
                if state.ghosts.rotation_pending {
                    state.ghosts.rotation_performed = true;
                }
            }
        }
        state.daemon.ctl = DaemonCtl::Fired {
            head: state.log.len() as u64,
        };
        Some(())
    }

    fn daemon_inject(state: &mut ProtocolState) -> Option<()> {
        let DaemonCtl::Fired { head } = state.daemon.ctl else {
            return None;
        };
        if state.tmux.is_none() {
            // The session died between the reconcile and the send. The real
            // send fails, the poll errors, and nothing was noted: the next
            // poll starts over.
            state.daemon.ctl = DaemonCtl::Idle;
            return Some(());
        }
        state.executor_pending = true;
        state.ghosts.wakes_delivered += 1;
        if state.ghosts.last_delivered_head == Some(head) {
            state.ghosts.duplicate_wake = true;
        }
        state.ghosts.last_delivered_head = Some(head);
        state.daemon.ctl = DaemonCtl::Injected { head };
        Some(())
    }

    fn daemon_note_write(state: &mut ProtocolState) -> Option<()> {
        let DaemonCtl::Injected { head } = state.daemon.ctl else {
            return None;
        };
        state.notes = Notes {
            last_head: head,
            last_wake_at: Some(epoch()),
        };
        // Acting consumed any request at or before the noted head.
        if state.ghosts.rotation_pending {
            let consumed = fold(&state.log)
                .and_then(|project| project.loop_control.rotate_requested_seq)
                .is_some_and(|requested| requested <= head);
            if consumed {
                if state.ghosts.rotation_performed {
                    state.ghosts.rotation_consumed_performed = true;
                } else {
                    state.ghosts.rotation_lost = true;
                }
                state.ghosts.rotation_pending = false;
                state.ghosts.rotation_performed = false;
            }
        }
        state.daemon.ctl = DaemonCtl::Idle;
        Some(())
    }
}

impl Model for Scenario {
    type State = ProtocolState;
    type Action = ProtocolAction;

    fn init_states(&self) -> Vec<Self::State> {
        vec![ProtocolState {
            log: Vec::new(),
            notes: Notes::default(),
            daemon: Daemon {
                ctl: DaemonCtl::Idle,
                knows_session: None,
                crashes_left: self.daemon_crashes,
            },
            phone: Phone {
                rotation_left: self.phone_rotation,
                pause_left: self.phone_pause,
                work_left: self.phone_work,
            },
            tmux: None,
            executor_pending: false,
            executor_appends_left: self.executor_appends,
            ghosts: Ghosts::default(),
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        // A log no real reader could interpret is a dead end; the safety
        // properties flag it rather than exploring past it.
        if fold(&state.log).is_none() {
            return;
        }

        match state.daemon.ctl {
            DaemonCtl::Idle => {
                if let Some(view) = daemon_view(&state.log)
                    && matches!(
                        decide::decide(&self.config, &view, &state.notes, &observation()),
                        Decision::Fire(_)
                    )
                {
                    actions.push(ProtocolAction::DaemonPollFires);
                }
            }
            DaemonCtl::Fired { .. } => actions.push(ProtocolAction::DaemonInject),
            DaemonCtl::Injected { .. } => actions.push(ProtocolAction::DaemonNoteWrite),
        }
        if state.daemon.crashes_left > 0 {
            actions.push(ProtocolAction::DaemonCrash);
        }
        if state.ghosts.notes_lost < self.notes_losses && state.notes != Notes::default() {
            actions.push(ProtocolAction::NotesLost);
        }

        if state.executor_pending && state.tmux.is_some() {
            actions.push(ProtocolAction::ExecutorActs { appends: false });
            if state.executor_appends_left > 0 {
                actions.push(ProtocolAction::ExecutorActs { appends: true });
            }
        }
        if state.tmux.is_some() && state.ghosts.session_crashes < self.session_crashes {
            actions.push(ProtocolAction::ExecutorCrash);
        }

        if state.phone.rotation_left {
            actions.push(ProtocolAction::PhoneRotationRequest);
        }
        if state.phone.pause_left {
            actions.push(ProtocolAction::PhonePause);
        }
        if state.phone.work_left {
            actions.push(ProtocolAction::PhoneWorkStatement);
        }
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut state = last.clone();
        match action {
            ProtocolAction::DaemonPollFires => self.daemon_poll_fires(&mut state)?,
            ProtocolAction::DaemonInject => Self::daemon_inject(&mut state)?,
            ProtocolAction::DaemonNoteWrite => Self::daemon_note_write(&mut state)?,
            ProtocolAction::DaemonCrash => {
                if matches!(state.daemon.ctl, DaemonCtl::Injected { .. }) {
                    state.ghosts.wake_delivered_unnoted = true;
                }
                state.daemon.ctl = DaemonCtl::Idle;
                state.daemon.knows_session = None;
                state.daemon.crashes_left -= 1;
            }
            ProtocolAction::NotesLost => {
                state.notes = Notes::default();
                state.ghosts.notes_lost += 1;
                // With the notes gone, every recorded request looks
                // outstanding again from this machine's point of view. A
                // request that was genuinely still pending keeps its ghost
                // state untouched. One that had already been consumed comes
                // back as a *phantom*: it re-pends, and — because consumption
                // only ever follows a restart, the ordering this ghost
                // audits — the fresh session that answered it still exists,
                // so it re-pends as already performed. The daemon may rotate
                // once more for it, which is redundant and harmless; what it
                // can never do is consume a rotation nobody performed.
                if !state.ghosts.rotation_pending
                    && fold(&state.log)
                        .and_then(|project| project.loop_control.rotate_requested_seq)
                        .is_some()
                {
                    state.ghosts.rotation_pending = true;
                    state.ghosts.rotation_performed = true;
                }
            }
            ProtocolAction::ExecutorActs { appends } => {
                if !state.executor_pending || state.tmux.is_none() {
                    return None;
                }
                if appends {
                    if state.executor_appends_left == 0 {
                        return None;
                    }
                    state.executor_appends_left -= 1;
                    let ordinal = self.executor_appends - state.executor_appends_left;
                    let draft = work_statement(
                        &format!("executor-work-{ordinal}"),
                        &format!("al-executor-{ordinal}"),
                        EXECUTOR_ACTOR,
                    );
                    state.log = append_now(&state.log, &draft);
                }
                state.executor_pending = false;
            }
            ProtocolAction::ExecutorCrash => {
                state.tmux = None;
                state.ghosts.session_crashes += 1;
                // The session took its pending line with it.
                state.executor_pending = false;
            }
            ProtocolAction::PhoneRotationRequest => {
                let draft = typed_draft(
                    "phone-rotate-1",
                    PHONE_ACTOR,
                    LoopEventPayload::LoopRotationRequested { why: None },
                );
                state.log = append_now(&state.log, &draft);
                state.phone.rotation_left = false;
                state.ghosts.rotation_pending = true;
                state.ghosts.rotation_performed = false;
            }
            ProtocolAction::PhonePause => {
                let draft = typed_draft(
                    "phone-pause-1",
                    PHONE_ACTOR,
                    LoopEventPayload::LoopPaused {
                        why: Some("maintenance window".to_owned()),
                    },
                );
                state.log = append_now(&state.log, &draft);
                state.phone.pause_left = false;
            }
            ProtocolAction::PhoneWorkStatement => {
                let draft = work_statement("phone-work-1", "al-phone-1", PHONE_ACTOR);
                state.log = append_now(&state.log, &draft);
                state.phone.work_left = false;
            }
        }
        Some(state)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        // The safety properties are the shared sentences: each closure calls
        // `alder::domain::invariants` rather than restating the predicate
        // here, so this model and the crash simulator cannot drift into
        // meaning different things by "correct" while both stay green.
        // Everything below them — liveness, the ghost audit, and every
        // `sometimes` — is a claim about reachability across a state space,
        // which only a model checker has, and stays local on purpose.
        let mut properties = vec![
            Property::<Self>::always("every reachable log folds cleanly", |_, state| {
                invariants::log_folds_cleanly(&state.log)
            }),
            Property::<Self>::always("the log never mentions its own readers", |_, state| {
                interpret(&state.log).is_some_and(|(events, _)| {
                    invariants::mentions_no_readers(&events)
                        && events.iter().all(|event| event.actor != DAEMON_ACTOR)
                })
            }),
            Property::<Self>::always("the rotation request mirrors the log", |_, state| {
                interpret(&state.log).is_some_and(|(events, project)| {
                    invariants::rotation_request_mirrors_the_log(&project, &events)
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
            // bookkeeping. Several `sometimes` properties and the rotation
            // ordering claim are derived from the ghost mirror, so a mirror
            // that drifted would let them report on a rotation log nobody
            // has. Checking it against the fold keeps them honest.
            Property::<Self>::always("the rotation ghost tracks the fold", |_, state| {
                fold(&state.log).is_some_and(|project| {
                    let pending = project
                        .loop_control
                        .rotate_requested_seq
                        .is_some_and(|requested| requested > state.notes.last_head);
                    pending == state.ghosts.rotation_pending
                })
            }),
            // The rotation ordering guarantee: acting consumes a rotation
            // only after a restart performed it, whatever crashed in between.
            // There is no racing waker anymore — a wake is not an append — so
            // this holds unconditionally.
            Property::<Self>::always("a consumed rotation was performed first", |_, state| {
                !state.ghosts.rotation_lost
            }),
        ];
        if self.daemon_crashes > 0 {
            // The duplicate-wake window, and its harmlessness. Both are
            // `sometimes` because a window nobody enters proves nothing: the
            // model has to actually strand a delivered wake before "and
            // nothing broke" means anything — the `always` properties above
            // are what hold through it.
            properties.push(Property::<Self>::sometimes(
                "a crash strands a delivered wake nothing recorded",
                |_, state| state.ghosts.wake_delivered_unnoted,
            ));
            properties.push(Property::<Self>::sometimes(
                "a wake is delivered twice for the same head",
                |_, state| state.ghosts.duplicate_wake,
            ));
        }
        if self.notes_losses > 0 {
            properties.push(Property::<Self>::sometimes(
                "lost notes cost a duplicate wake and nothing else",
                |_, state| state.ghosts.notes_lost > 0 && state.ghosts.duplicate_wake,
            ));
        }
        if self.phone_rotation {
            properties.push(Property::<Self>::sometimes(
                "a rotation is performed and then consumed",
                |_, state| state.ghosts.rotation_consumed_performed,
            ));
        }
        if self.session_crashes > 0 {
            properties.push(Property::<Self>::sometimes(
                "a session crash is exercised",
                |_, state| state.ghosts.session_crashes > 0,
            ));
        }
        if self.daemon_crashes > 0 {
            properties.push(Property::<Self>::sometimes(
                "a daemon crash is exercised",
                |model, state| state.daemon.crashes_left < model.daemon_crashes,
            ));
        }
        properties
    }
}

/// The helper a property *reads through* rather than states.
///
/// Everything else in this crate is checked by exploring it. `recovered` is
/// the exception, because it is consulted by exactly one property and a wrong
/// answer makes that property vacuous rather than false — saying yes too
/// readily leaves liveness green over a silently stopped loop. A vacuous
/// property has no counterexample to find, so exploration cannot notice; the
/// sentence has to be pinned directly.
#[cfg(test)]
mod tests {
    use super::*;

    fn state_of(log: Vec<Record>) -> ProtocolState {
        let mut state = Scenario::new()
            .init_states()
            .pop()
            .expect("one initial state");
        state.log = log;
        state
    }

    fn paused(why: Option<&str>) -> RecordDraft {
        typed_draft(
            "pause-1",
            PHONE_ACTOR,
            LoopEventPayload::LoopPaused {
                why: why.map(str::to_owned),
            },
        )
    }

    /// An empty log is the initial state, and it is recovered: nothing is
    /// blocked, and nothing durable can be "open" because wakes are not
    /// records.
    #[test]
    fn an_empty_log_is_recovered() {
        assert!(recovered(&state_of(Vec::new())));
    }

    /// A paused loop is a legitimate resting place only when it says why; a
    /// pause with no reason is the loop stopping silently, which is exactly
    /// what the property refuses to call recovered.
    #[test]
    fn a_pause_is_recovered_only_when_it_states_a_reason() {
        assert!(!recovered(&state_of(append_now(&[], &paused(None)))));
        assert!(recovered(&state_of(append_now(
            &[],
            &paused(Some("maintenance window")),
        ))));
    }

    /// Ordinary statements leave the loop recovered: there is no run record
    /// to be left open.
    #[test]
    fn a_work_statement_is_recovered() {
        let log = append_now(&[], &work_statement("w-1", "al-1", EXECUTOR_ACTOR));
        assert!(recovered(&state_of(log)));
    }
}
