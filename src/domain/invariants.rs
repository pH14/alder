//! The safety predicates the verification harnesses assert.
//!
//! Two harnesses check the loop protocol from opposite directions — a
//! stateright model that explores every interleaving of a small cast, and a
//! deterministic crash simulator that tears every effect every way its
//! footprint allows — and they check the same handful of things about a log
//! and the state it folds to. Stated twice, the two statements drift, and the
//! drift is invisible: both harnesses stay green while they mean different
//! things by "correct". Stated once here, both assert the same sentence, and
//! a reader of the log who was not there has one place to learn what the loop
//! promises.
//!
//! Every predicate is a pure function of a [`ProjectState`] and, where the
//! property is about the history rather than about the fold, the [`Event`]s it
//! was folded from. Nothing here reads a clock, a store, or the machine.
//!
//! # The witness a log cannot supply
//!
//! Two of these compare the log against a fact that is not in it: a crash that
//! really happened, an append whose writer really saw a receipt. Neither is
//! recordable — a log that could vouch for its own crashes would be vouching
//! for exactly the case where it was not written. So the caller passes the
//! witness in, and that parameter is the point of the predicate: without it,
//! the predicate would compare the log to itself and pass vacuously.
//!
//! # What deliberately stays out
//!
//! World-state predicates — no stranded worktrees, no phantom workers, no pane
//! left holding text nobody submitted — are claims about the machine, not about
//! the log, and only the simulator has a machine to look at. They stay there.
//!
//! Liveness and "sometimes" properties — every terminal state is progressing or
//! blocked-and-named, a lost race is eventually conceded — are claims about
//! reachability across a state space, not about one state, and only the model
//! checker has the state space. They stay there. A predicate that is true of a
//! single state is the only shape this module has.

use std::collections::BTreeSet;

use alder_log::Record;

use super::{Event, EventPayload, PassOutcome, PassState, ProjectState, decode_record};
use crate::error::Result;

/// The log folds cleanly: every record decodes to an Alder event, and the fold
/// accepts the whole history.
///
/// This is the predicate the other four stand on. A history that no reader can
/// interpret has no `ProjectState` to be right or wrong about, so a harness
/// that only ever asks the other questions of a state it managed to fold is
/// silently skipping every case where the log itself was the casualty.
///
/// It takes records rather than events because decoding is half of what can
/// fail. Both harnesses reach a state the same way — decode every record, fold
/// the result — and both failures are this predicate failing.
pub fn log_folds_cleanly(records: &[Record]) -> bool {
    let Ok(events) = records
        .iter()
        .map(decode_record)
        .collect::<Result<Vec<Event>>>()
    else {
        return false;
    };
    ProjectState::fold(&events).is_ok()
}

/// At most one pass is open.
///
/// The two harnesses spell "open" differently: the model counts passes with no
/// outcome, the simulator counts passes in [`PassState::Open`]. The fold makes
/// those the same set — `pass.started` opens a pass with no outcome and
/// `pass.ended` sets both fields together — so the two spellings are one
/// property, and this asserts it in both spellings *and* asserts that they
/// still agree. A fold that ended a pass without recording an outcome would
/// satisfy each harness's own sentence while breaking the other's, which is
/// precisely the drift a shared statement exists to catch.
///
/// The spellings are compared pass by pass, not by counting each and comparing
/// the totals. Two totals can agree while pointing at different passes — one
/// open pass that somehow carries an outcome, one ended pass that carries none,
/// and each count is one — which is the same state both harnesses would have
/// disagreed about, waved through by arithmetic.
pub fn at_most_one_open_pass(state: &ProjectState) -> bool {
    let mut open = 0;
    for pass in state.passes.values() {
        if (pass.state == PassState::Open) != pass.outcome.is_none() {
            return false;
        }
        if pass.state == PassState::Open {
            open += 1;
        }
    }
    open <= 1
}

/// A `crashed` verdict follows a real crash.
///
/// `crashed` is a claim about an observed dead session, not a way to dispose of
/// a pass nobody is watching — a driver that reached for it when the truth was
/// "the pass ran long" would be writing a fact into the log that never
/// happened, and no later reader could tell. `real_crashes` is that fact,
/// counted by whoever injected the deaths; the log may name no more crashes
/// than really occurred.
///
/// The relation is one-directional on purpose. Fewer verdicts than crashes is
/// ordinary — a session that dies between passes strands nothing to attribute.
pub fn crashed_verdicts_follow_real_crashes(state: &ProjectState, real_crashes: usize) -> bool {
    crashed_verdicts(state) <= real_crashes
}

/// How many passes the log blames on a crash.
pub fn crashed_verdicts(state: &ProjectState) -> usize {
    state
        .passes
        .values()
        .filter(|pass| pass.outcome == Some(PassOutcome::Crashed))
        .count()
}

/// `rotate_pending` mirrors the request log.
///
/// Rotation is derived, never stored: it is pending when the latest request is
/// later in the log than the latest wake. The fold computes that by comparing
/// two sequence numbers it accumulated along the way, which is cheap and is
/// also exactly the kind of arithmetic that can be quietly wrong. So the
/// history is read again here the other way — a straight scan in log order,
/// last writer wins — and the two derivations are compared. Agreeing is the
/// property; either alone would just be the fold agreeing with itself.
///
/// Both harnesses maintained this mirror by hand, one in ghost state and one
/// implicitly through the status document. Neither needs to now.
pub fn rotate_pending_mirrors_the_request_log(state: &ProjectState, events: &[Event]) -> bool {
    state.loop_control.rotate_pending() == rotation_is_pending(events)
}

/// An acknowledged handoff is never lost.
///
/// `acknowledged` names the submissions whose writer saw a receipt — the fact
/// no log can supply, since a writer that lost its response cannot be
/// distinguished from one whose append never landed by reading the log alone.
/// Each of those must appear in the history exactly once and must have folded
/// into a handoff the state still knows about. Withdrawn or integrated counts
/// as kept: those are things someone did to the handoff, not the log dropping
/// it.
///
/// Every other submission is held to the weaker half the same property implies.
/// A writer that retries an identical draft after a lost response is answered
/// `AlreadyPresent` rather than appended a second time, so no submission at all
/// — acknowledged or still in doubt — may appear twice.
pub fn acknowledged_handoffs_are_never_lost(
    state: &ProjectState,
    events: &[Event],
    acknowledged: &[&str],
) -> bool {
    let submissions: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::HandoffSubmitted { handoff } => {
                Some((event.id.as_str(), handoff.id.as_str()))
            }
            _ => None,
        })
        .collect();
    let mut seen = BTreeSet::new();
    if !submissions
        .iter()
        .all(|(event_id, _)| seen.insert(*event_id))
    {
        return false;
    }
    acknowledged.iter().all(|acknowledged| {
        submissions
            .iter()
            .find(|(event_id, _)| event_id == acknowledged)
            .is_some_and(|(_, handoff_id)| state.handoffs.contains_key(*handoff_id))
    })
}

/// Whether a rotation is outstanding, read straight off the history.
///
/// A request is a `loop.rotation_requested`, or a `pass.ended` that asked for
/// one; a `pass.started` consumes whatever was outstanding by being later.
/// Scanning in order and keeping the last answer is the same rule the fold
/// states as `requested > woke`, arrived at without the sequence arithmetic.
fn rotation_is_pending(events: &[Event]) -> bool {
    events
        .iter()
        .fold(false, |pending, event| match &event.payload {
            EventPayload::LoopRotationRequested { .. } => true,
            EventPayload::PassEnded { rotate, .. } => *rotate || pending,
            EventPayload::PassStarted { .. } => false,
            _ => pending,
        })
}

#[cfg(test)]
mod tests {
    use alder_log::{Log, MemoryLog};
    use chrono::{DateTime, Utc};
    use serde_json::json;

    use super::*;
    use crate::domain::{
        EventDraft, HandoffDefinition, Pass, PassDefinition, PassTrigger, encode_draft,
    };

    const SCHEMA: &str = "alder.event.v0";

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).expect("a valid instant")
    }

    fn draft(id: &str, payload: EventPayload) -> EventDraft {
        EventDraft {
            id: id.to_owned(),
            at: at(),
            actor: "tester".to_owned(),
            payload,
            schema: SCHEMA.to_owned(),
        }
    }

    /// A record history built through the real log and the real codec, so a
    /// predicate that reads records is reading what a store would hold.
    fn records(drafts: Vec<EventDraft>) -> Vec<Record> {
        let log = MemoryLog::new();
        for draft in &drafts {
            let head = log.head().expect("a memory head");
            log.append(&head, &encode_draft(draft).expect("a draft encodes"))
                .expect("an append at the head lands");
        }
        let head = log.head().expect("a memory head");
        log.read_all(&head).expect("a complete read")
    }

    fn events(drafts: Vec<EventDraft>) -> Vec<Event> {
        records(drafts)
            .iter()
            .map(decode_record)
            .collect::<Result<Vec<Event>>>()
            .expect("the drafts decode")
    }

    fn folded(drafts: Vec<EventDraft>) -> (Vec<Event>, ProjectState) {
        let events = events(drafts);
        let state = ProjectState::fold(&events).expect("the history folds");
        (events, state)
    }

    fn wake(id: &str, pass_id: &str) -> EventDraft {
        draft(
            id,
            EventPayload::PassStarted {
                pass: PassDefinition {
                    id: pass_id.to_owned(),
                    engine: "claude".to_owned(),
                    handle: "tmux:alder-leader".to_owned(),
                    triggers: vec![PassTrigger::Due],
                    at_head: 0,
                },
            },
        )
    }

    fn end(id: &str, pass_id: &str, outcome: PassOutcome, rotate: bool) -> EventDraft {
        draft(
            id,
            EventPayload::PassEnded {
                pass_id: pass_id.to_owned(),
                outcome,
                report: None,
                wake_at: None,
                rotate,
                why: None,
            },
        )
    }

    fn submission(id: &str, handoff_id: &str) -> EventDraft {
        draft(
            id,
            EventPayload::HandoffSubmitted {
                handoff: HandoffDefinition {
                    id: handoff_id.to_owned(),
                    title: "a handoff".to_owned(),
                    artifact_ref: "branch:somewhere".to_owned(),
                    note: None,
                },
            },
        )
    }

    fn pass(id: &str, state: PassState, outcome: Option<PassOutcome>) -> Pass {
        Pass {
            id: id.to_owned(),
            engine: "claude".to_owned(),
            handle: "tmux:alder-leader".to_owned(),
            triggers: vec![PassTrigger::Due],
            state,
            outcome,
            report: None,
            wake_at: None,
            rotate: false,
            why: None,
            at_head: 0,
            started_at: at(),
            started_seq: 1,
            ended_at: None,
            ended_seq: None,
        }
    }

    /// A state assembled by hand rather than folded, so a predicate can be
    /// shown failing on a shape the current fold refuses to produce.
    fn state_with(passes: Vec<Pass>) -> ProjectState {
        let mut state = ProjectState::default();
        for pass in passes {
            state.passes.insert(pass.id.clone(), pass);
        }
        state
    }

    #[test]
    fn an_ordinary_history_folds_cleanly() {
        let history = records(vec![
            wake("wake-1", "al-pass-1"),
            end("end-1", "al-pass-1", PassOutcome::Ok, false),
        ]);
        assert!(log_folds_cleanly(&history));
    }

    #[test]
    fn a_record_no_reader_can_decode_does_not_fold() {
        // The log is opaque, so a record carrying an event type Alder has never
        // heard of is a record a store really can hold.
        let alien: Record = serde_json::from_value(json!({
            "id": "alien-1",
            "seq": 1,
            "at": "2026-07-27T12:00:00Z",
            "actor": "tester",
            "type": "not.an.alder.event",
            "body": {},
            "schema": SCHEMA,
        }))
        .expect("the log accepts an opaque record");
        assert!(!log_folds_cleanly(&[alien]));
    }

    #[test]
    fn a_second_wake_over_an_open_pass_does_not_fold() {
        // Every record decodes; it is the fold that refuses, which is the other
        // half of what this predicate covers.
        let history = records(vec![
            wake("wake-1", "al-pass-1"),
            wake("wake-2", "al-pass-2"),
        ]);
        assert!(!log_folds_cleanly(&history));
    }

    #[test]
    fn one_open_pass_is_within_the_limit() {
        assert!(at_most_one_open_pass(&state_with(vec![
            pass("al-pass-1", PassState::Ended, Some(PassOutcome::Ok)),
            pass("al-pass-2", PassState::Open, None),
        ])));
    }

    #[test]
    fn two_open_passes_are_not() {
        assert!(!at_most_one_open_pass(&state_with(vec![
            pass("al-pass-1", PassState::Open, None),
            pass("al-pass-2", PassState::Open, None),
        ])));
    }

    #[test]
    fn a_pass_ended_without_an_outcome_breaks_the_two_spellings_apart() {
        // Neither harness alone would notice: the simulator counts no open pass
        // at all, the model counts one pass still unfinished, and each of those
        // is within its own "at most one".
        assert!(!at_most_one_open_pass(&state_with(vec![pass(
            "al-pass-1",
            PassState::Ended,
            None
        )])));
    }

    #[test]
    fn two_passes_can_disagree_while_the_two_counts_match() {
        // The reason this is checked pass by pass. One open pass carrying an
        // outcome and one ended pass carrying none put the totals at one each,
        // so comparing the counts would wave through a state where the two
        // spellings name different passes.
        assert!(!at_most_one_open_pass(&state_with(vec![
            pass("al-pass-1", PassState::Open, Some(PassOutcome::Ok)),
            pass("al-pass-2", PassState::Ended, None),
        ])));
    }

    #[test]
    fn a_crashed_verdict_needs_a_crash_to_point_at() {
        let state = state_with(vec![pass(
            "al-pass-1",
            PassState::Ended,
            Some(PassOutcome::Crashed),
        )]);
        assert_eq!(crashed_verdicts(&state), 1);
        assert!(crashed_verdicts_follow_real_crashes(&state, 1));
        assert!(!crashed_verdicts_follow_real_crashes(&state, 0));
    }

    #[test]
    fn a_crash_nobody_blamed_on_a_pass_is_not_a_violation() {
        let state = state_with(vec![pass(
            "al-pass-1",
            PassState::Ended,
            Some(PassOutcome::Ok),
        )]);
        assert!(crashed_verdicts_follow_real_crashes(&state, 3));
    }

    #[test]
    fn a_timeout_is_not_a_crashed_verdict() {
        let state = state_with(vec![pass(
            "al-pass-1",
            PassState::Ended,
            Some(PassOutcome::Timeout),
        )]);
        assert!(crashed_verdicts_follow_real_crashes(&state, 0));
    }

    #[test]
    fn a_rotation_request_is_pending_until_the_next_wake_consumes_it() {
        let requested = vec![
            wake("wake-1", "al-pass-1"),
            end("end-1", "al-pass-1", PassOutcome::Ok, false),
            draft(
                "rotate-1",
                EventPayload::LoopRotationRequested { why: None },
            ),
        ];
        let (history, state) = folded(requested.clone());
        assert!(state.loop_control.rotate_pending());
        assert!(rotate_pending_mirrors_the_request_log(&state, &history));

        let mut consumed = requested;
        consumed.push(wake("wake-2", "al-pass-2"));
        let (history, state) = folded(consumed);
        assert!(!state.loop_control.rotate_pending());
        assert!(rotate_pending_mirrors_the_request_log(&state, &history));
    }

    #[test]
    fn a_pass_that_asked_to_rotate_is_itself_a_request() {
        let (history, state) = folded(vec![
            wake("wake-1", "al-pass-1"),
            end("end-1", "al-pass-1", PassOutcome::Ok, true),
        ]);
        assert!(state.loop_control.rotate_pending());
        assert!(rotate_pending_mirrors_the_request_log(&state, &history));
    }

    #[test]
    fn a_history_that_never_asked_has_nothing_pending() {
        let (history, state) = folded(vec![
            wake("wake-1", "al-pass-1"),
            end("end-1", "al-pass-1", PassOutcome::Ok, false),
        ]);
        assert!(rotate_pending_mirrors_the_request_log(&state, &history));
    }

    #[test]
    fn a_fold_that_forgot_the_request_disagrees_with_the_history() {
        let (history, mut state) = folded(vec![draft(
            "rotate-1",
            EventPayload::LoopRotationRequested { why: None },
        )]);
        // Stand in for a fold that dropped the request on the floor: the
        // history still holds it, so the two derivations must part company.
        state.loop_control.rotate_requested_seq = None;
        assert!(!rotate_pending_mirrors_the_request_log(&state, &history));
    }

    #[test]
    fn an_acknowledged_submission_present_once_survives() {
        let (history, state) = folded(vec![submission("submit-1", "al-handoff-one")]);
        assert!(acknowledged_handoffs_are_never_lost(
            &state,
            &history,
            &["submit-1"]
        ));
    }

    #[test]
    fn an_acknowledged_submission_that_left_the_history_is_lost() {
        let (history, state) = folded(vec![wake("wake-1", "al-pass-1")]);
        assert!(!acknowledged_handoffs_are_never_lost(
            &state,
            &history,
            &["submit-1"]
        ));
    }

    #[test]
    fn a_submission_nobody_acknowledged_may_simply_be_absent() {
        let (history, state) = folded(vec![wake("wake-1", "al-pass-1")]);
        assert!(acknowledged_handoffs_are_never_lost(&state, &history, &[]));
    }

    #[test]
    fn a_submission_appended_twice_is_a_lost_update() {
        // A real store answers the retried draft `AlreadyPresent`, so this
        // history is forged — which is the point: the predicate has to fail on
        // the shape a store that stopped being idempotent would produce.
        let (history, state) = folded(vec![submission("submit-1", "al-handoff-one")]);
        let mut duplicated = history.clone();
        duplicated.push(Event {
            seq: 2,
            ..history[0].clone()
        });
        assert!(acknowledged_handoffs_are_never_lost(
            &state,
            &history,
            &["submit-1"]
        ));
        assert!(!acknowledged_handoffs_are_never_lost(
            &state,
            &duplicated,
            &["submit-1"]
        ));
    }

    #[test]
    fn a_submission_the_fold_never_took_is_lost_even_when_the_record_is_there() {
        // The record is in the history and the writer was told so, but no
        // handoff came out of the fold. Counting records alone would call that
        // kept; it is not.
        let history = events(vec![submission("submit-1", "al-handoff-one")]);
        let empty = ProjectState::default();
        assert!(!acknowledged_handoffs_are_never_lost(
            &empty,
            &history,
            &["submit-1"]
        ));
    }
}
