use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Deref, DerefMut};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{AlderError, Result};

use super::{
    Event, EventPayload, LoopControl, LoopEventPayload, Observation, ObservationKey, WorkAppState,
};

/// The whole project's folded state: the composition of the work
/// application's fold, the observation application's fold, and the loop
/// controls that still live in the root. The envelope checks — contiguous
/// sequences, one schema, unique event IDs — are made here because they are
/// statements about the shared log rather than about either application.
///
/// The work application's maps are flattened in, and the struct dereferences
/// to [`WorkAppState`], so both the serialized shape and every
/// `state.work` / `state.is_ready(..)` consumer read exactly as they did
/// when the fold was one piece.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectState {
    #[serde(with = "alder_observation::observation_map")]
    pub observations: BTreeMap<ObservationKey, Observation>,
    #[serde(flatten)]
    work_app: WorkAppState,
    pub loop_control: LoopControl,
}

impl Deref for ProjectState {
    type Target = WorkAppState;

    fn deref(&self) -> &WorkAppState {
        &self.work_app
    }
}

impl DerefMut for ProjectState {
    fn deref_mut(&mut self) -> &mut WorkAppState {
        &mut self.work_app
    }
}

impl ProjectState {
    pub fn fold(events: &[Event]) -> Result<Self> {
        let mut state = Self::default();
        let mut event_ids = BTreeSet::new();
        for (index, event) in events.iter().enumerate() {
            let expected = index as u64 + 1;
            if event.seq != expected {
                return Err(AlderError::with_context(
                    "invalid_log",
                    format!(
                        "event sequence is not contiguous: expected {expected}, found {}",
                        event.seq
                    ),
                    json!({"expected_seq": expected, "actual_seq": event.seq}),
                ));
            }
            if event.schema != "alder.event.v0" {
                return Err(AlderError::with_context(
                    "invalid_log",
                    format!("unsupported event schema `{}`", event.schema),
                    json!({"event_id": event.id, "schema": event.schema}),
                ));
            }
            if !event_ids.insert(event.id.clone()) {
                return Err(AlderError::with_context(
                    "invalid_log",
                    format!("duplicate event ID `{}`", event.id),
                    json!({"event_id": event.id}),
                ));
            }
            state.apply(event)?;
        }
        Ok(state)
    }

    pub fn apply(&mut self, event: &Event) -> Result<()> {
        let mut next = self.clone();
        next.apply_in_place(event)?;
        *self = next;
        Ok(())
    }

    fn apply_in_place(&mut self, event: &Event) -> Result<()> {
        let seq = event.seq;
        match &event.payload {
            EventPayload::Observation(payload) => {
                alder_observation::apply(&mut self.observations, payload, seq)?;
            }
            EventPayload::Work(payload) => {
                self.work_app.apply(payload, seq, &event.actor)?;
            }
            EventPayload::Loop(payload) => match payload {
                // Passes were run records of the loop reading its own log.
                // They are inert history now: the fold gives them no state,
                // and no append path can produce a new one.
                LoopEventPayload::LegacyPassStarted(_) => {}
                LoopEventPayload::LegacyPassEnded(body) => {
                    // A historical `pass end --rotate` was also a rotation
                    // request, and that half of the event was a statement
                    // about the loop rather than about the pass, so it still
                    // folds.
                    if body.get("rotate").and_then(serde_json::Value::as_bool) == Some(true) {
                        self.loop_control.rotate_requested_seq = Some(seq);
                    }
                }
                LoopEventPayload::LoopPaused { why } => {
                    self.loop_control.paused = true;
                    self.loop_control.pause_reason =
                        why.clone().filter(|why| !why.trim().is_empty());
                }
                LoopEventPayload::LoopResumed {} => {
                    self.loop_control.paused = false;
                    self.loop_control.pause_reason = None;
                }
                LoopEventPayload::LoopEngineSelected { engine } => {
                    require_text("engine", engine)?;
                    self.loop_control.engine = Some(engine.clone());
                }
                LoopEventPayload::LoopRotationRequested { .. } => {
                    self.loop_control.rotate_requested_seq = Some(seq);
                }
                LoopEventPayload::LoopNudgeRequested { .. } => {
                    self.loop_control.nudge_requested_seq = Some(seq);
                }
            },
        }
        Ok(())
    }
}

fn require_text(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(AlderError::validation(format!("{field} cannot be empty")))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{DateTime, Utc};

    use super::*;
    use crate::domain::{
        AttemptDefinition, AttemptOutcome, AttemptState, CheckDefinition, CheckStatus, CheckUpdate,
        EventPayload, LegacyHandoffDefinition, LoopEventPayload, ObservationEventPayload,
        QuestionDefinition, WorkDefinition, WorkEventPayload, WorkOperation, WorkState,
        WorkStateChange, validate_check,
    };

    fn event(seq: u64, payload: impl Into<EventPayload>) -> Event {
        Event {
            id: format!("event-{seq}"),
            seq,
            at: Utc::now(),
            actor: "test".to_owned(),
            payload: payload.into(),
            schema: "alder.event.v0".to_owned(),
        }
    }

    fn add(id: &str, requires: &[&str], checks: &[&str]) -> WorkOperation {
        WorkOperation::Add {
            work: WorkDefinition {
                id: id.to_owned(),
                title: id.to_owned(),
                spec: None,
                priority: 0,
                requires: requires.iter().map(|value| (*value).to_owned()).collect(),
                checks: checks
                    .iter()
                    .map(|key| CheckDefinition {
                        key: (*key).to_owned(),
                        description: format!("{key} passes"),
                    })
                    .collect(),
            },
        }
    }

    fn edit(id: &str) -> WorkOperation {
        WorkOperation::Edit {
            id: id.to_owned(),
            title: None,
            spec: None,
            priority: None,
            add_requires: Vec::new(),
            remove_requires: Vec::new(),
            add_checks: Vec::new(),
            remove_checks: Vec::new(),
            state_change: None,
        }
    }

    #[test]
    fn readiness_and_attempt_checks_are_folded() {
        let events = vec![
            event(
                1,
                WorkEventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &["tests"]), add("hm-b", &["hm-a"], &[])],
                },
            ),
            event(
                2,
                WorkEventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
                        tier: None,
                        id: "hm-a-attempt-1".to_owned(),
                        work_id: "hm-a".to_owned(),
                        metadata: BTreeMap::new(),
                    },
                },
            ),
        ];
        let before_attempt = ProjectState::fold(&events[..1]).unwrap();
        assert_eq!(
            before_attempt
                .ready()
                .iter()
                .map(|work| work.id.as_str())
                .collect::<Vec<_>>(),
            ["hm-a"]
        );
        let state = ProjectState::fold(&events).unwrap();
        assert!(state.ready().is_empty());
        assert_eq!(
            state.attempts["hm-a-attempt-1"].checks["tests"].status,
            CheckStatus::Pending
        );
    }

    #[test]
    fn cycle_rejects_the_whole_change() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                WorkEventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &["hm-b"], &[]), add("hm-b", &["hm-a"], &[])],
                },
            ))
            .unwrap_err();
        assert!(state.work.is_empty());
    }

    #[test]
    fn dropped_dependency_is_not_ready() {
        let events = vec![
            event(
                1,
                WorkEventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &[]), add("hm-b", &["hm-a"], &[])],
                },
            ),
            event(
                2,
                WorkEventPayload::WorkDropped {
                    work_id: "hm-a".to_owned(),
                    attempt_id: None,
                    outcome: None,
                    why: "no longer needed".to_owned(),
                },
            ),
        ];
        let state = ProjectState::fold(&events).unwrap();
        assert!(!state.is_ready("hm-b"));
    }

    #[test]
    fn fold_rejects_malformed_envelopes_before_changing_state() {
        let payload = WorkEventPayload::WorkChanged {
            why: None,
            operations: vec![add("hm-a", &[], &[])],
        };
        let mut wrong_sequence = event(2, payload.clone());
        assert_eq!(
            ProjectState::fold(&[wrong_sequence.clone()])
                .unwrap_err()
                .code,
            "invalid_log"
        );
        wrong_sequence.seq = 1;
        wrong_sequence.schema = "alder.event.v1".to_owned();
        assert_eq!(
            ProjectState::fold(&[wrong_sequence]).unwrap_err().code,
            "invalid_log"
        );
        let first = event(1, payload.clone());
        let mut duplicate = event(2, payload);
        duplicate.id = first.id.clone();
        assert_eq!(
            ProjectState::fold(&[first, duplicate]).unwrap_err().code,
            "invalid_log"
        );
    }

    #[test]
    fn legacy_handoff_submission_and_withdrawal_are_inert_history() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                WorkEventPayload::LegacyHandoffSubmitted {
                    handoff: LegacyHandoffDefinition {
                        id: "hm-handoff-one".to_owned(),
                        title: "handoff".to_owned(),
                        artifact_ref: "branch".to_owned(),
                        note: None,
                    },
                },
            ))
            .unwrap();
        state
            .apply(&event(
                2,
                WorkEventPayload::LegacyHandoffWithdrawn {
                    handoff_id: "hm-handoff-one".to_owned(),
                    why: "superseded".to_owned(),
                },
            ))
            .unwrap();
        assert!(state.work.is_empty());
    }

    #[test]
    fn active_attempt_freezes_every_dependency_and_check_edit() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                WorkEventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &["old"]), add("hm-b", &[], &[])],
                },
            ))
            .unwrap();
        state
            .apply(&event(
                2,
                WorkEventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
                        tier: None,
                        id: "hm-a-attempt-1".to_owned(),
                        work_id: "hm-a".to_owned(),
                        metadata: BTreeMap::new(),
                    },
                },
            ))
            .unwrap();

        let mut edits = Vec::new();
        let mut operation = edit("hm-a");
        if let WorkOperation::Edit { add_requires, .. } = &mut operation {
            add_requires.push("hm-b".to_owned());
        }
        edits.push(operation);
        let mut operation = edit("hm-a");
        if let WorkOperation::Edit {
            remove_requires, ..
        } = &mut operation
        {
            remove_requires.push("hm-b".to_owned());
        }
        edits.push(operation);
        let mut operation = edit("hm-a");
        if let WorkOperation::Edit { add_checks, .. } = &mut operation {
            add_checks.push(CheckDefinition {
                key: "new".to_owned(),
                description: "new check".to_owned(),
            });
        }
        edits.push(operation);
        let mut operation = edit("hm-a");
        if let WorkOperation::Edit { remove_checks, .. } = &mut operation {
            remove_checks.push("old".to_owned());
        }
        edits.push(operation);

        for operation in edits {
            let mut candidate = state.clone();
            let error = candidate
                .apply(&event(
                    3,
                    WorkEventPayload::WorkChanged {
                        why: Some("change contract".to_owned()),
                        operations: vec![operation],
                    },
                ))
                .unwrap_err();
            assert_eq!(error.code, "active_attempt");
        }
    }

    #[test]
    fn work_edits_apply_each_collection_change_without_cross_talk() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                WorkEventPayload::WorkChanged {
                    why: None,
                    operations: vec![
                        add("hm-a", &[], &[]),
                        add("hm-b", &["hm-a"], &["old"]),
                        add("hm-c", &[], &[]),
                    ],
                },
            ))
            .unwrap();
        let mut operation = edit("hm-b");
        if let WorkOperation::Edit {
            add_requires,
            remove_requires,
            add_checks,
            remove_checks,
            ..
        } = &mut operation
        {
            add_requires.push("hm-c".to_owned());
            remove_requires.push("hm-a".to_owned());
            add_checks.push(CheckDefinition {
                key: "new".to_owned(),
                description: "new passes".to_owned(),
            });
            remove_checks.push("old".to_owned());
        }
        state
            .apply(&event(
                2,
                WorkEventPayload::WorkChanged {
                    why: Some("update contract".to_owned()),
                    operations: vec![operation],
                },
            ))
            .unwrap();
        assert_eq!(state.work["hm-b"].requires, ["hm-c"]);
        assert_eq!(
            state.work["hm-b"]
                .checks
                .iter()
                .map(|check| check.key.as_str())
                .collect::<Vec<_>>(),
            ["new"]
        );

        let mut duplicate = edit("hm-b");
        if let WorkOperation::Edit { add_checks, .. } = &mut duplicate {
            add_checks.push(CheckDefinition {
                key: "new".to_owned(),
                description: "duplicate".to_owned(),
            });
        }
        assert!(
            state
                .apply(&event(
                    3,
                    WorkEventPayload::WorkChanged {
                        why: Some("duplicate".to_owned()),
                        operations: vec![duplicate],
                    },
                ))
                .is_err()
        );
    }

    #[test]
    fn questions_block_only_their_work_until_answered() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                WorkEventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &[]), add("hm-b", &[], &[])],
                },
            ))
            .unwrap();
        state
            .apply(&event(
                2,
                WorkEventPayload::QuestionAsked {
                    question: QuestionDefinition {
                        id: "hm-a-question-1".to_owned(),
                        work_id: "hm-a".to_owned(),
                        text: "which?".to_owned(),
                    },
                },
            ))
            .unwrap();

        let mut unblock_a = edit("hm-a");
        if let WorkOperation::Edit { state_change, .. } = &mut unblock_a {
            *state_change = Some(WorkStateChange::Unblock {
                reason: "try".to_owned(),
            });
        }
        assert_eq!(
            state
                .apply(&event(
                    3,
                    WorkEventPayload::WorkChanged {
                        why: Some("try".to_owned()),
                        operations: vec![unblock_a.clone()],
                    },
                ))
                .unwrap_err()
                .code,
            "unanswered_question"
        );

        let mut block_b = edit("hm-b");
        if let WorkOperation::Edit { state_change, .. } = &mut block_b {
            *state_change = Some(WorkStateChange::Block {
                reason: "pause".to_owned(),
                until: None,
            });
        }
        state
            .apply(&event(
                4,
                WorkEventPayload::WorkChanged {
                    why: Some("pause".to_owned()),
                    operations: vec![block_b],
                },
            ))
            .unwrap();
        let mut unblock_b = edit("hm-b");
        if let WorkOperation::Edit { state_change, .. } = &mut unblock_b {
            *state_change = Some(WorkStateChange::Unblock {
                reason: "resume".to_owned(),
            });
        }
        state
            .apply(&event(
                5,
                WorkEventPayload::WorkChanged {
                    why: Some("resume".to_owned()),
                    operations: vec![unblock_b],
                },
            ))
            .unwrap();
        assert_eq!(state.work["hm-b"].state, WorkState::Open);

        state
            .apply(&event(
                6,
                WorkEventPayload::QuestionAnswered {
                    question_id: "hm-a-question-1".to_owned(),
                    answer: "A".to_owned(),
                },
            ))
            .unwrap();
        state
            .apply(&event(
                7,
                WorkEventPayload::WorkChanged {
                    why: Some("resolved".to_owned()),
                    operations: vec![unblock_a],
                },
            ))
            .unwrap();
        assert_eq!(state.work["hm-a"].state, WorkState::Open);
    }

    #[test]
    fn an_attempt_update_activates_a_starting_attempt() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                WorkEventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &["test"])],
                },
            ))
            .unwrap();
        state
            .apply(&event(
                2,
                WorkEventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
                        tier: None,
                        id: "hm-a-attempt-1".to_owned(),
                        work_id: "hm-a".to_owned(),
                        metadata: BTreeMap::new(),
                    },
                },
            ))
            .unwrap();
        state
            .apply(&event(
                3,
                WorkEventPayload::AttemptUpdated {
                    tier: None,
                    attempt_id: "hm-a-attempt-1".to_owned(),
                    metadata: BTreeMap::new(),
                    note: Some("working".to_owned()),
                    checks: vec![CheckUpdate {
                        key: "test".to_owned(),
                        status: CheckStatus::Satisfied,
                        evidence: "CI".to_owned(),
                    }],
                },
            ))
            .unwrap();
        assert_eq!(state.attempts["hm-a-attempt-1"].state, AttemptState::Active);
        assert_eq!(
            state.attempts["hm-a-attempt-1"].checks["test"].status,
            CheckStatus::Satisfied
        );

        let started = {
            let mut state = ProjectState::default();
            state
                .apply(&event(
                    1,
                    WorkEventPayload::WorkChanged {
                        why: None,
                        operations: vec![add("hm-a", &[], &["test"])],
                    },
                ))
                .unwrap();
            state
                .apply(&event(
                    2,
                    WorkEventPayload::AttemptStarted {
                        attempt: AttemptDefinition {
                            tier: None,
                            id: "hm-a-attempt-1".to_owned(),
                            work_id: "hm-a".to_owned(),
                            metadata: BTreeMap::new(),
                        },
                    },
                ))
                .unwrap();
            state
        };

        let mut metadata_only = started.clone();
        metadata_only
            .apply(&event(
                3,
                WorkEventPayload::AttemptUpdated {
                    tier: None,
                    attempt_id: "hm-a-attempt-1".to_owned(),
                    metadata: BTreeMap::from([("engine".to_owned(), json!("opus"))]),
                    note: None,
                    checks: vec![],
                },
            ))
            .unwrap();
        assert_eq!(
            metadata_only.attempts["hm-a-attempt-1"].metadata["engine"],
            "opus"
        );

        let mut note_only = started.clone();
        note_only
            .apply(&event(
                3,
                WorkEventPayload::AttemptUpdated {
                    tier: None,
                    attempt_id: "hm-a-attempt-1".to_owned(),
                    metadata: BTreeMap::new(),
                    note: Some("working".to_owned()),
                    checks: vec![],
                },
            ))
            .unwrap();
        assert_eq!(
            note_only.attempts["hm-a-attempt-1"].note.as_deref(),
            Some("working")
        );

        let mut check_only = started.clone();
        check_only
            .apply(&event(
                3,
                WorkEventPayload::AttemptUpdated {
                    tier: None,
                    attempt_id: "hm-a-attempt-1".to_owned(),
                    metadata: BTreeMap::new(),
                    note: None,
                    checks: vec![CheckUpdate {
                        key: "test".to_owned(),
                        status: CheckStatus::Failed,
                        evidence: "CI".to_owned(),
                    }],
                },
            ))
            .unwrap();
        assert_eq!(
            check_only.attempts["hm-a-attempt-1"].checks["test"].status,
            CheckStatus::Failed
        );

        let mut empty = started;
        assert_eq!(
            empty
                .apply(&event(
                    3,
                    WorkEventPayload::AttemptUpdated {
                        tier: None,
                        attempt_id: "hm-a-attempt-1".to_owned(),
                        metadata: BTreeMap::new(),
                        note: None,
                        checks: vec![],
                    },
                ))
                .unwrap_err()
                .message,
            "an attempt update must change a tier, metadata, a note, or a check"
        );
    }

    #[test]
    fn a_handle_held_only_by_ended_attempts_may_be_rebound() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                WorkEventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &[]), add("hm-b", &[], &[])],
                },
            ))
            .unwrap();
        state
            .apply(&event(
                2,
                WorkEventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
                        tier: None,
                        id: "hm-a-attempt-1".to_owned(),
                        work_id: "hm-a".to_owned(),
                        metadata: BTreeMap::new(),
                    },
                },
            ))
            .unwrap();
        state
            .apply(&event(
                3,
                WorkEventPayload::AttemptBound {
                    attempt_id: "hm-a-attempt-1".to_owned(),
                    handle: "tmux:worker".to_owned(),
                    metadata: BTreeMap::new(),
                },
            ))
            .unwrap();
        state
            .apply(&event(
                4,
                WorkEventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
                        tier: None,
                        id: "hm-b-attempt-1".to_owned(),
                        work_id: "hm-b".to_owned(),
                        metadata: BTreeMap::new(),
                    },
                },
            ))
            .unwrap();

        // A live attempt still holds the handle: a second attempt cannot
        // bind the same one.
        let rejected = state
            .clone()
            .apply(&event(
                5,
                WorkEventPayload::AttemptBound {
                    attempt_id: "hm-b-attempt-1".to_owned(),
                    handle: "tmux:worker".to_owned(),
                    metadata: BTreeMap::new(),
                },
            ))
            .unwrap_err();
        assert_eq!(rejected.message, "handle `tmux:worker` is already attached");

        // Once the holding attempt ends, the same handle is free to reuse.
        state
            .apply(&event(
                5,
                WorkEventPayload::AttemptEnded {
                    attempt_id: "hm-a-attempt-1".to_owned(),
                    outcome: AttemptOutcome::Failed,
                    why: "worker crashed".to_owned(),
                },
            ))
            .unwrap();
        state
            .apply(&event(
                6,
                WorkEventPayload::AttemptBound {
                    attempt_id: "hm-b-attempt-1".to_owned(),
                    handle: "tmux:worker".to_owned(),
                    metadata: BTreeMap::new(),
                },
            ))
            .unwrap();
        assert_eq!(
            state.attempts["hm-b-attempt-1"].handle.as_deref(),
            Some("tmux:worker")
        );
    }

    #[test]
    fn ordinary_finish_requires_every_check_to_be_satisfied() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                WorkEventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &["test"])],
                },
            ))
            .unwrap();
        state
            .apply(&event(
                2,
                WorkEventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
                        tier: None,
                        id: "hm-a-attempt-1".to_owned(),
                        work_id: "hm-a".to_owned(),
                        metadata: BTreeMap::new(),
                    },
                },
            ))
            .unwrap();
        let finish = WorkEventPayload::WorkFinished {
            work_id: "hm-a".to_owned(),
            attempt_id: Some("hm-a-attempt-1".to_owned()),
            external: false,
            evidence: None,
        };
        assert_eq!(
            state.apply(&event(3, finish.clone())).unwrap_err().code,
            "incomplete_checks"
        );
        state
            .apply(&event(
                3,
                WorkEventPayload::AttemptUpdated {
                    tier: None,
                    attempt_id: "hm-a-attempt-1".to_owned(),
                    metadata: BTreeMap::new(),
                    note: None,
                    checks: vec![CheckUpdate {
                        key: "test".to_owned(),
                        status: CheckStatus::Satisfied,
                        evidence: "CI".to_owned(),
                    }],
                },
            ))
            .unwrap();
        state.apply(&event(4, finish)).unwrap();
        assert_eq!(state.work["hm-a"].state, WorkState::Done);
    }

    #[test]
    fn external_finish_rejects_either_form_of_attempt_association() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                WorkEventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &[]), add("hm-b", &[], &[])],
                },
            ))
            .unwrap();
        assert!(
            state
                .apply(&event(
                    2,
                    WorkEventPayload::WorkFinished {
                        work_id: "hm-a".to_owned(),
                        attempt_id: Some("not-active".to_owned()),
                        external: true,
                        evidence: Some("proof".to_owned()),
                    },
                ))
                .is_err()
        );

        state
            .apply(&event(
                3,
                WorkEventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
                        tier: None,
                        id: "hm-b-attempt-1".to_owned(),
                        work_id: "hm-b".to_owned(),
                        metadata: BTreeMap::new(),
                    },
                },
            ))
            .unwrap();
        assert!(
            state
                .apply(&event(
                    4,
                    WorkEventPayload::WorkFinished {
                        work_id: "hm-b".to_owned(),
                        attempt_id: None,
                        external: true,
                        evidence: Some("proof".to_owned()),
                    },
                ))
                .is_err()
        );
    }

    #[test]
    fn graph_and_identity_helpers_cover_exact_boundaries() {
        let mut state = ProjectState::default();
        assert!(
            state
                .apply(&event(
                    1,
                    WorkEventPayload::WorkChanged {
                        why: None,
                        operations: vec![
                            add("hm-a", &[], &[]),
                            add("hm-b", &["hm-a", "hm-a"], &[])
                        ],
                    },
                ))
                .is_err()
        );

        state
            .apply(&event(
                2,
                WorkEventPayload::WorkChanged {
                    why: None,
                    operations: vec![
                        add("hm-a", &[], &[]),
                        add("hm-b", &["hm-a"], &[]),
                        add("hm-c", &["hm-b"], &[]),
                        add("hm-unrelated", &[], &[]),
                    ],
                },
            ))
            .unwrap();
        assert_eq!(
            state.downstream("hm-a"),
            vec!["hm-b".to_owned(), "hm-c".to_owned()]
        );
        assert!(state.validate_prefix("hm").is_ok());
        assert!(state.validate_prefix("other").is_err());
        for invalid in [
            CheckDefinition {
                key: String::new(),
                description: "description".to_owned(),
            },
            CheckDefinition {
                key: "key".to_owned(),
                description: String::new(),
            },
            CheckDefinition {
                key: "has space".to_owned(),
                description: "description".to_owned(),
            },
            CheckDefinition {
                key: "has:colon".to_owned(),
                description: "description".to_owned(),
            },
        ] {
            assert!(validate_check(&invalid).is_err());
        }
        assert!(
            validate_check(&CheckDefinition {
                key: "tests".to_owned(),
                description: "tests pass".to_owned(),
            })
            .is_ok()
        );
    }

    /// A historical pass event decoded off the wire, complete with the body
    /// shapes the old schema wrote.
    fn legacy_pass(seq: u64, kind: &str, body: serde_json::Value) -> Event {
        let payload = serde_json::from_value(json!({"type": kind, "body": body}))
            .expect("a historical pass event decodes");
        Event {
            id: format!("legacy-{seq}"),
            seq,
            at: Utc::now(),
            actor: "alderd".to_owned(),
            payload,
            schema: "alder.event.v0".to_owned(),
        }
    }

    #[test]
    fn legacy_pass_events_fold_as_inert_history() {
        let mut state = ProjectState::default();
        state
            .apply(&legacy_pass(
                1,
                "pass.started",
                json!({"pass": {"id": "hm-pass-1", "engine": "claude",
                        "handle": "tmux:alder-leader", "triggers": ["log"], "at_head": 0}}),
            ))
            .unwrap();
        state
            .apply(&legacy_pass(
                2,
                "pass.ended",
                json!({"pass_id": "hm-pass-1", "outcome": "ok",
                        "report": "swept", "wake_at": null, "rotate": false, "why": null}),
            ))
            .unwrap();
        // Nothing folds: no object, no loop state, no constraint on order.
        assert!(state.work.is_empty());
        assert!(state.loop_control.rotate_requested_seq.is_none());
        // Two historical opens in a row were once rejected; as history they
        // are inert and both replay.
        state
            .apply(&legacy_pass(
                3,
                "pass.started",
                json!({"pass": {"id": "hm-pass-2"}}),
            ))
            .unwrap();
        state
            .apply(&legacy_pass(
                4,
                "pass.started",
                json!({"pass": {"id": "hm-pass-3"}}),
            ))
            .unwrap();
    }

    #[test]
    fn a_legacy_pass_end_that_asked_to_rotate_still_reads_as_a_request() {
        let mut state = ProjectState::default();
        state
            .apply(&legacy_pass(
                1,
                "pass.ended",
                json!({"pass_id": "hm-pass-1", "outcome": "ok", "report": null,
                        "wake_at": null, "rotate": true, "why": null}),
            ))
            .unwrap();
        assert_eq!(state.loop_control.rotate_requested_seq, Some(1));
    }

    #[test]
    fn rotation_and_nudge_requests_record_the_sequence_they_were_asked_at() {
        let mut state = ProjectState::default();
        assert!(state.loop_control.rotate_requested_seq.is_none());
        assert!(state.loop_control.nudge_requested_seq.is_none());

        state
            .apply(&event(
                1,
                LoopEventPayload::LoopRotationRequested { why: None },
            ))
            .unwrap();
        state
            .apply(&event(
                2,
                LoopEventPayload::LoopNudgeRequested { why: None },
            ))
            .unwrap();
        assert_eq!(state.loop_control.rotate_requested_seq, Some(1));
        assert_eq!(state.loop_control.nudge_requested_seq, Some(2));

        // A later request replaces the recorded sequence; whether either has
        // been acted on is each driver's machine-local knowledge, not a fold
        // fact.
        state
            .apply(&event(
                3,
                LoopEventPayload::LoopRotationRequested { why: None },
            ))
            .unwrap();
        assert_eq!(state.loop_control.rotate_requested_seq, Some(3));
        assert_eq!(state.loop_control.nudge_requested_seq, Some(2));
    }

    fn block_until(id: &str, until: Option<&str>) -> WorkEventPayload {
        let mut operation = edit(id);
        if let WorkOperation::Edit { state_change, .. } = &mut operation {
            *state_change = Some(WorkStateChange::Block {
                reason: "deferred".to_owned(),
                until: until.map(|value| {
                    value
                        .parse::<DateTime<Utc>>()
                        .expect("a test instant parses")
                }),
            });
        }
        WorkEventPayload::WorkChanged {
            why: Some("deferred".to_owned()),
            operations: vec![operation],
        }
    }

    #[test]
    fn a_block_may_carry_a_review_deadline_and_the_latest_block_wins_whole() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                WorkEventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &[]), add("hm-b", &[], &[])],
                },
            ))
            .unwrap();
        assert!(state.next_review_at().is_none());

        state
            .apply(&event(2, block_until("hm-a", Some("2026-08-04T15:00:00Z"))))
            .unwrap();
        state
            .apply(&event(3, block_until("hm-b", Some("2026-08-04T12:00:00Z"))))
            .unwrap();
        assert_eq!(
            state.work["hm-a"].block_until.unwrap().to_rfc3339(),
            "2026-08-04T15:00:00+00:00"
        );
        // The earliest deadline over all blocked work is the loop's next
        // review rendezvous.
        assert_eq!(
            state.next_review_at().unwrap().to_rfc3339(),
            "2026-08-04T12:00:00+00:00"
        );

        // Re-blocking without a deadline clears it: the latest statement wins.
        state.apply(&event(4, block_until("hm-b", None))).unwrap();
        assert!(state.work["hm-b"].block_until.is_none());

        // Unblocking clears the deadline with the reason.
        let mut unblock = edit("hm-a");
        if let WorkOperation::Edit { state_change, .. } = &mut unblock {
            *state_change = Some(WorkStateChange::Unblock {
                reason: "reviewed".to_owned(),
            });
        }
        state
            .apply(&event(
                5,
                WorkEventPayload::WorkChanged {
                    why: Some("reviewed".to_owned()),
                    operations: vec![unblock],
                },
            ))
            .unwrap();
        assert!(state.work["hm-a"].block_until.is_none());
        assert!(state.next_review_at().is_none());
    }

    #[test]
    fn pause_resume_and_engine_selection_are_last_writer_wins() {
        let mut state = ProjectState::default();
        assert!(!state.loop_control.paused);
        assert_eq!(state.loop_control.engine, None);

        state
            .apply(&event(
                1,
                LoopEventPayload::LoopPaused {
                    why: Some("release freeze".to_owned()),
                },
            ))
            .unwrap();
        assert!(state.loop_control.paused);
        assert_eq!(
            state.loop_control.pause_reason.as_deref(),
            Some("release freeze")
        );

        state
            .apply(&event(
                2,
                LoopEventPayload::LoopPaused {
                    why: Some(" ".to_owned()),
                },
            ))
            .unwrap();
        assert!(state.loop_control.paused);
        assert_eq!(state.loop_control.pause_reason, None);

        state
            .apply(&event(3, LoopEventPayload::LoopResumed {}))
            .unwrap();
        assert!(!state.loop_control.paused);
        assert_eq!(state.loop_control.pause_reason, None);

        for engine in ["claude", "codex"] {
            state
                .apply(&event(
                    4,
                    LoopEventPayload::LoopEngineSelected {
                        engine: engine.to_owned(),
                    },
                ))
                .unwrap();
            assert_eq!(state.loop_control.engine.as_deref(), Some(engine));
        }
        assert_eq!(
            state
                .apply(&event(
                    5,
                    LoopEventPayload::LoopEngineSelected {
                        engine: " ".to_owned(),
                    },
                ))
                .unwrap_err()
                .code,
            "validation_failed"
        );
    }

    #[test]
    fn cycle_errors_report_the_actual_cycle() {
        let mut state = ProjectState::default();
        let error = state
            .apply(&event(
                1,
                WorkEventPayload::WorkChanged {
                    why: None,
                    operations: vec![
                        add("hm-a", &["hm-c"], &[]),
                        add("hm-b", &["hm-a"], &[]),
                        add("hm-c", &["hm-b"], &[]),
                    ],
                },
            ))
            .unwrap_err();
        assert_eq!(
            error.context["cycle"],
            json!(["hm-a", "hm-c", "hm-b", "hm-a"])
        );
    }

    #[test]
    fn observation_picture_serializes_as_an_ordered_list_not_a_synthetic_key() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                ObservationEventPayload::ObservationReported {
                    observation: super::super::ObservationDefinition {
                        key: ObservationKey {
                            observer: "github".to_owned(),
                            subject: "owner/repo#171".to_owned(),
                            field: "ci".to_owned(),
                        },
                        level: "passing".to_owned(),
                    },
                },
            ))
            .unwrap();

        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["observations"][0]["level"], "passing");
        let round_trip: ProjectState = serde_json::from_value(value).unwrap();
        assert_eq!(round_trip.observations, state.observations);
    }
}
