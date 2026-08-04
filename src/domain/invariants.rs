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
//! # What deliberately stays out
//!
//! World-state predicates — no stranded worktrees, no phantom workers, no pane
//! left holding text nobody submitted — are claims about the machine, not about
//! the log, and only the simulator has a machine to look at. They stay there.
//!
//! Liveness and "sometimes" properties — every terminal state is progressing or
//! blocked-and-named, a duplicated wake is actually reachable — are claims
//! about reachability across a state space, not about one state, and only the
//! model checker has the state space. They stay there. A predicate that is
//! true of a single state is the only shape this module has.

use alder_log::Record;

use super::{Event, EventPayload, LoopEventPayload, ProjectState, decode_record};
use crate::error::Result;

/// The log folds cleanly: every record decodes to an Alder event, and the fold
/// accepts the whole history.
///
/// This is the predicate the others stand on. A history that no reader can
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

/// The log never mentions its own readers.
///
/// The log carries statements about work, attempts, questions, and
/// observations — never about the processes that read it. Pass events were the
/// last machinery records left, and they are decode-only history now: nothing
/// running today may append one, and the driver appends nothing at all. Any
/// history a current harness produces must therefore be free of them, which is
/// exactly what makes a missed or duplicated wake harmless — nothing durable
/// records that a wake happened, so there is nothing to be wrong about it.
///
/// A pre-existing log may hold historical pass events; this predicate is about
/// what the current system writes, so the harnesses apply it to histories they
/// produced themselves.
pub fn mentions_no_readers(events: &[Event]) -> bool {
    events.iter().all(|event| {
        !matches!(
            event.payload,
            EventPayload::Loop(
                LoopEventPayload::LegacyPassStarted(_) | LoopEventPayload::LegacyPassEnded(_)
            )
        )
    })
}

/// `rotate_requested_seq` mirrors the request log.
///
/// The fold records the sequence of the latest rotation request as it goes,
/// which is cheap and is also exactly the kind of arithmetic that can be
/// quietly wrong. So the history is read again here the other way — a straight
/// scan in log order, last writer wins — and the two derivations are compared.
/// Agreeing is the property; either alone would just be the fold agreeing with
/// itself. Whether the request has been *acted on* is deliberately absent: the
/// log never mentions its readers, so consumption is each driver's
/// machine-local knowledge and no log predicate can speak to it.
pub fn rotation_request_mirrors_the_log(state: &ProjectState, events: &[Event]) -> bool {
    state.loop_control.rotate_requested_seq == latest_rotation_request(events)
}

/// The sequence of the latest rotation request, read straight off the history.
///
/// A request is a `loop.rotation_requested`, or a historical `pass.ended` that
/// asked for one — that half of the old event was a statement about the loop
/// rather than about the pass, so it still counts.
fn latest_rotation_request(events: &[Event]) -> Option<u64> {
    events
        .iter()
        .fold(None, |latest, event| match &event.payload {
            EventPayload::Loop(LoopEventPayload::LoopRotationRequested { .. }) => Some(event.seq),
            EventPayload::Loop(LoopEventPayload::LegacyPassEnded(body))
                if body.get("rotate").and_then(serde_json::Value::as_bool) == Some(true) =>
            {
                Some(event.seq)
            }
            _ => latest,
        })
}

#[cfg(test)]
mod tests {
    use alder_log::{Log, MemoryLog};
    use chrono::{DateTime, Utc};
    use serde_json::json;

    use super::*;
    use crate::domain::{EventDraft, encode_draft};

    const SCHEMA: &str = "alder.event.v0";

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).expect("a valid instant")
    }

    fn draft(id: &str, payload: impl Into<EventPayload>) -> EventDraft {
        EventDraft {
            id: id.to_owned(),
            at: at(),
            actor: "tester".to_owned(),
            payload: payload.into(),
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

    fn rotation(id: &str) -> EventDraft {
        draft(id, LoopEventPayload::LoopRotationRequested { why: None })
    }

    fn legacy_pass_end(id: &str, rotate: bool) -> EventDraft {
        draft(
            id,
            LoopEventPayload::LegacyPassEnded(json!({
                "pass_id": "al-pass-1", "outcome": "ok", "report": null,
                "wake_at": null, "rotate": rotate, "why": null,
            })),
        )
    }

    #[test]
    fn an_ordinary_history_folds_cleanly() {
        let history = records(vec![
            draft("pause-1", LoopEventPayload::LoopPaused { why: None }),
            rotation("rotate-1"),
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
    fn a_history_free_of_pass_events_mentions_no_readers() {
        let history = events(vec![
            draft("pause-1", LoopEventPayload::LoopPaused { why: None }),
            rotation("rotate-1"),
        ]);
        assert!(mentions_no_readers(&history));
    }

    #[test]
    fn a_history_holding_a_pass_event_mentions_a_reader() {
        // Historical logs hold these; a history the current system produced
        // must not, because no append path can write one.
        let history = events(vec![legacy_pass_end("legacy-1", false)]);
        assert!(!mentions_no_readers(&history));
    }

    #[test]
    fn the_latest_rotation_request_is_the_recorded_one() {
        let (history, state) = folded(vec![rotation("rotate-1"), rotation("rotate-2")]);
        assert_eq!(state.loop_control.rotate_requested_seq, Some(2));
        assert!(rotation_request_mirrors_the_log(&state, &history));
    }

    #[test]
    fn a_history_that_never_asked_has_no_request_recorded() {
        let (history, state) = folded(vec![draft(
            "pause-1",
            LoopEventPayload::LoopPaused { why: None },
        )]);
        assert!(state.loop_control.rotate_requested_seq.is_none());
        assert!(rotation_request_mirrors_the_log(&state, &history));
    }

    #[test]
    fn a_historical_pass_end_that_asked_to_rotate_counts_as_a_request() {
        let (history, state) = folded(vec![legacy_pass_end("legacy-1", true)]);
        assert_eq!(state.loop_control.rotate_requested_seq, Some(1));
        assert!(rotation_request_mirrors_the_log(&state, &history));

        // One that did not ask counts for nothing.
        let (history, state) = folded(vec![legacy_pass_end("legacy-1", false)]);
        assert!(state.loop_control.rotate_requested_seq.is_none());
        assert!(rotation_request_mirrors_the_log(&state, &history));
    }

    #[test]
    fn a_fold_that_forgot_the_request_disagrees_with_the_history() {
        let (history, mut state) = folded(vec![rotation("rotate-1")]);
        // Stand in for a fold that dropped the request on the floor: the
        // history still holds it, so the two derivations must part company.
        state.loop_control.rotate_requested_seq = None;
        assert!(!rotation_request_mirrors_the_log(&state, &history));
    }
}
