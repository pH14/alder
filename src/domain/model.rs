use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub actor: String,
    #[serde(flatten)]
    pub payload: EventPayload,
    pub schema: String,
}

#[derive(Debug, Clone)]
pub struct EventDraft {
    pub id: String,
    pub at: DateTime<Utc>,
    pub actor: String,
    pub payload: EventPayload,
    pub schema: String,
}

impl EventDraft {
    pub fn materialize(&self, seq: u64) -> Event {
        Event {
            id: self.id.clone(),
            seq,
            at: self.at,
            actor: self.actor.clone(),
            payload: self.payload.clone(),
            schema: self.schema.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "body")]
pub enum EventPayload {
    #[serde(rename = "observation.reported")]
    ObservationReported { observation: ObservationDefinition },
    #[serde(rename = "observation.retired")]
    ObservationRetired { key: ObservationKey },
    // Kept solely to decode events written before handoffs were removed.
    // Submission and withdrawal are inert history; integration still creates
    // its embedded work because later historical events can refer to it.
    #[serde(rename = "handoff.submitted")]
    LegacyHandoffSubmitted { handoff: LegacyHandoffDefinition },
    #[serde(rename = "handoff.integrated")]
    LegacyHandoffIntegrated {
        handoff_id: String,
        work: WorkDefinition,
    },
    #[serde(rename = "handoff.withdrawn")]
    LegacyHandoffWithdrawn { handoff_id: String, why: String },
    #[serde(rename = "work.changed")]
    WorkChanged {
        why: Option<String>,
        operations: Vec<WorkOperation>,
    },
    #[serde(rename = "work.finished")]
    WorkFinished {
        work_id: String,
        attempt_id: Option<String>,
        external: bool,
        evidence: Option<String>,
    },
    #[serde(rename = "work.dropped")]
    WorkDropped {
        work_id: String,
        attempt_id: Option<String>,
        outcome: Option<AttemptOutcome>,
        why: String,
    },
    #[serde(rename = "work.reopened")]
    WorkReopened { work_id: String, why: String },
    #[serde(rename = "attempt.started")]
    AttemptStarted { attempt: AttemptDefinition },
    #[serde(rename = "attempt.bound")]
    AttemptBound {
        attempt_id: String,
        handle: String,
        metadata: BTreeMap<String, Value>,
    },
    #[serde(rename = "attempt.updated")]
    AttemptUpdated {
        attempt_id: String,
        metadata: BTreeMap<String, Value>,
        note: Option<String>,
        checks: Vec<CheckUpdate>,
    },
    #[serde(rename = "attempt.ended")]
    AttemptEnded {
        attempt_id: String,
        outcome: AttemptOutcome,
        why: String,
    },
    #[serde(rename = "question.asked")]
    QuestionAsked { question: QuestionDefinition },
    #[serde(rename = "question.answered")]
    QuestionAnswered { question_id: String, answer: String },
    // Kept solely to decode events written before passes were removed. A pass
    // was a record of the loop reading its own log; the log carries statements
    // about work, never about its own readers, so both fold as inert history
    // with no live state and no append path. The bodies are held opaque:
    // nothing derives from them, so nothing constrains their shape.
    #[serde(rename = "pass.started")]
    LegacyPassStarted(Value),
    #[serde(rename = "pass.ended")]
    LegacyPassEnded(Value),
    #[serde(rename = "loop.paused")]
    LoopPaused { why: Option<String> },
    #[serde(rename = "loop.resumed")]
    LoopResumed {},
    #[serde(rename = "loop.engine_selected")]
    LoopEngineSelected { engine: String },
    #[serde(rename = "loop.rotation_requested")]
    LoopRotationRequested { why: Option<String> },
    #[serde(rename = "loop.nudge_requested")]
    LoopNudgeRequested { why: Option<String> },
}

impl EventPayload {
    /// A new arm here also belongs in `EVERY_EVENT` in the log tests, which
    /// sweeps every mutation for how it reports losing a compare-and-append.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::ObservationReported { .. } => "observation.reported",
            Self::ObservationRetired { .. } => "observation.retired",
            Self::LegacyHandoffSubmitted { .. } => "handoff.submitted",
            Self::LegacyHandoffIntegrated { .. } => "handoff.integrated",
            Self::LegacyHandoffWithdrawn { .. } => "handoff.withdrawn",
            Self::WorkChanged { .. } => "work.changed",
            Self::WorkFinished { .. } => "work.finished",
            Self::WorkDropped { .. } => "work.dropped",
            Self::WorkReopened { .. } => "work.reopened",
            Self::AttemptStarted { .. } => "attempt.started",
            Self::AttemptBound { .. } => "attempt.bound",
            Self::AttemptUpdated { .. } => "attempt.updated",
            Self::AttemptEnded { .. } => "attempt.ended",
            Self::QuestionAsked { .. } => "question.asked",
            Self::QuestionAnswered { .. } => "question.answered",
            Self::LegacyPassStarted(_) => "pass.started",
            Self::LegacyPassEnded(_) => "pass.ended",
            Self::LoopPaused { .. } => "loop.paused",
            Self::LoopResumed {} => "loop.resumed",
            Self::LoopEngineSelected { .. } => "loop.engine_selected",
            Self::LoopRotationRequested { .. } => "loop.rotation_requested",
            Self::LoopNudgeRequested { .. } => "loop.nudge_requested",
        }
    }

    /// Whether this variant is decode-only history. Legacy events replay from
    /// the log, but no live command may append one: the append layer refuses
    /// them before anything reaches the store.
    pub fn is_legacy(&self) -> bool {
        matches!(
            self,
            Self::LegacyHandoffSubmitted { .. }
                | Self::LegacyHandoffIntegrated { .. }
                | Self::LegacyHandoffWithdrawn { .. }
                | Self::LegacyPassStarted(_)
                | Self::LegacyPassEnded(_)
        )
    }

    pub fn references(&self, id: &str) -> bool {
        match self {
            // Observations have a composite key rather than an Alder object
            // ID, so their current picture is served by `observations`.
            Self::ObservationReported { .. } | Self::ObservationRetired { .. } => false,
            Self::LegacyHandoffSubmitted { handoff } => handoff.id == id,
            Self::LegacyHandoffIntegrated { handoff_id, work } => handoff_id == id || work.id == id,
            Self::LegacyHandoffWithdrawn { handoff_id, .. } => handoff_id == id,
            Self::WorkChanged { operations, .. } => operations.iter().any(|operation| {
                operation.id() == id
                    || operation
                        .definition()
                        .is_some_and(|work| work.requires.iter().any(|required| required == id))
            }),
            Self::WorkFinished {
                work_id,
                attempt_id,
                ..
            } => work_id == id || attempt_id.as_deref() == Some(id),
            Self::WorkDropped {
                work_id,
                attempt_id,
                ..
            } => work_id == id || attempt_id.as_deref() == Some(id),
            Self::WorkReopened { work_id, .. } => work_id == id,
            Self::AttemptStarted { attempt } => attempt.id == id || attempt.work_id == id,
            Self::AttemptBound { attempt_id, .. }
            | Self::AttemptUpdated { attempt_id, .. }
            | Self::AttemptEnded { attempt_id, .. } => attempt_id == id,
            Self::QuestionAsked { question } => question.id == id || question.work_id == id,
            Self::QuestionAnswered { question_id, .. } => question_id == id,
            // Legacy pass events named the loop's own runs, which are no
            // longer objects; they appear in no object's history.
            Self::LegacyPassStarted(_) | Self::LegacyPassEnded(_) => false,
            // Loop controls belong to the singleton loop, so they name no
            // object and appear in no object's history.
            Self::LoopPaused { .. }
            | Self::LoopResumed {}
            | Self::LoopEngineSelected { .. }
            | Self::LoopRotationRequested { .. }
            | Self::LoopNudgeRequested { .. } => false,
        }
    }
}

/// A key in the observation application. Its three parts are deliberately
/// explicit in every event and snapshot; no caller has to parse a synthetic
/// string to learn who reported what about which subject.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObservationKey {
    pub observer: String,
    pub subject: String,
    pub field: String,
}

/// The level supplied by an observer before the fold assigns its sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationDefinition {
    #[serde(flatten)]
    pub key: ObservationKey,
    pub level: String,
}

/// One current belief in the folded observation picture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    #[serde(flatten)]
    pub key: ObservationKey,
    pub level: String,
    pub reported_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyHandoffDefinition {
    pub id: String,
    pub title: String,
    #[serde(rename = "ref")]
    pub artifact_ref: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkDefinition {
    pub id: String,
    pub title: String,
    pub spec: Option<String>,
    pub priority: i64,
    pub requires: Vec<String>,
    pub checks: Vec<CheckDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WorkOperation {
    Add {
        work: WorkDefinition,
    },
    Edit {
        id: String,
        title: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "nullable_string_change"
        )]
        spec: Option<NullableString>,
        priority: Option<i64>,
        add_requires: Vec<String>,
        remove_requires: Vec<String>,
        add_checks: Vec<CheckDefinition>,
        remove_checks: Vec<String>,
        state_change: Option<WorkStateChange>,
    },
}

impl WorkOperation {
    pub fn id(&self) -> &str {
        match self {
            Self::Add { work } => &work.id,
            Self::Edit { id, .. } => id,
        }
    }

    pub fn definition(&self) -> Option<&WorkDefinition> {
        match self {
            Self::Add { work } => Some(work),
            Self::Edit { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NullableString(pub Option<String>);

pub(crate) mod nullable_string_change {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::NullableString;

    pub fn serialize<S>(
        value: &Option<NullableString>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(value) => value.0.serialize(serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> std::result::Result<Option<NullableString>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer).map(|value| Some(NullableString(value)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkState {
    Open,
    Blocked,
    Done,
    Dropped,
}

impl WorkState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Dropped => "dropped",
        }
    }

    /// `done` and `dropped` are the states a work item leaves only by being
    /// reopened. Nothing subordinate to the work is actionable while it is in
    /// one of them.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Dropped)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkStateChange {
    Block {
        reason: String,
        /// An optional review deadline: "come back to this at …". Stored on
        /// the work item; passing the instant changes nothing in the fold,
        /// but `status` surfaces the expired deferral for review and the
        /// driver wakes the leader at that time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<DateTime<Utc>>,
    },
    Unblock {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Work {
    pub id: String,
    pub title: String,
    pub spec: Option<String>,
    pub priority: i64,
    pub state: WorkState,
    pub block_reason: Option<String>,
    /// The review deadline carried by the latest block, if it stated one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_until: Option<DateTime<Utc>>,
    pub outcome: Option<String>,
    pub opened_seq: u64,
    pub changed_seq: u64,
    pub requires: Vec<String>,
    pub checks: Vec<CheckDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckDefinition {
    pub key: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptDefinition {
    pub id: String,
    pub work_id: String,
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Starting,
    Active,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Lost,
    NotStarted,
}

impl AttemptOutcome {
    pub fn is_non_success(self) -> bool {
        self != Self::Succeeded
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub id: String,
    pub work_id: String,
    pub state: AttemptState,
    pub outcome: Option<AttemptOutcome>,
    pub handle: Option<String>,
    pub metadata: BTreeMap<String, Value>,
    pub note: Option<String>,
    pub started_seq: u64,
    pub bound_seq: Option<u64>,
    pub updated_seq: u64,
    pub ended_seq: Option<u64>,
    pub checks: BTreeMap<String, AttemptCheck>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pending,
    Satisfied,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckUpdate {
    pub key: String,
    pub status: CheckStatus,
    pub evidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptCheck {
    pub key: String,
    pub status: CheckStatus,
    pub evidence: Option<String>,
    pub updated_seq: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionDefinition {
    pub id: String,
    pub work_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub work_id: String,
    pub text: String,
    pub answer: Option<String>,
    pub asked_seq: u64,
    pub answered_seq: Option<u64>,
    pub answered_by: Option<String>,
    pub answers: Vec<QuestionAnswer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionAnswer {
    pub answer: String,
    pub seq: u64,
    pub actor: String,
}

/// The desired state of the singleton loop. Pause and engine are
/// last-writer-wins folds; rotation and nudge requests are recorded as the
/// sequence they were asked at. The log carries no record of its readers, so
/// "has this request been acted on" is not a log fact: each driver compares
/// the request sequence with the last head it acted on, which lives in that
/// driver's machine-local notes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoopControl {
    pub paused: bool,
    pub pause_reason: Option<String>,
    pub engine: Option<String>,
    pub rotate_requested_seq: Option<u64>,
    pub nudge_requested_seq: Option<u64>,
}

#[cfg(test)]
mod tests {
    use alder_log::Head;
    use chrono::Utc;

    use super::*;

    fn work(id: &str, requires: Vec<String>) -> WorkDefinition {
        WorkDefinition {
            id: id.to_owned(),
            title: "work".to_owned(),
            spec: None,
            priority: 0,
            requires,
            checks: Vec::new(),
        }
    }

    #[test]
    fn heads_and_drafts_materialize_exact_event_data() {
        assert!(Head::empty().is_empty());
        let at = Utc::now();
        let draft = EventDraft {
            id: "event".to_owned(),
            at,
            actor: "actor".to_owned(),
            payload: EventPayload::WorkReopened {
                work_id: "hm-one".to_owned(),
                why: "reason".to_owned(),
            },
            schema: "alder.event.v0".to_owned(),
        };
        let event = draft.materialize(7);
        assert_eq!(event.id, "event");
        assert_eq!(event.seq, 7);
        assert_eq!(event.at, at);
        assert_eq!(event.actor, "actor");
        assert_eq!(event.payload.type_name(), "work.reopened");
        assert_eq!(event.schema, "alder.event.v0");
    }

    #[test]
    fn payload_names_and_references_cover_every_variant() {
        let cases = vec![
            (
                EventPayload::LegacyHandoffSubmitted {
                    handoff: LegacyHandoffDefinition {
                        id: "handoff".to_owned(),
                        title: "handoff".to_owned(),
                        artifact_ref: "ref".to_owned(),
                        note: None,
                    },
                },
                "handoff.submitted",
                vec!["handoff"],
            ),
            (
                EventPayload::LegacyHandoffIntegrated {
                    handoff_id: "handoff".to_owned(),
                    work: work("work", Vec::new()),
                },
                "handoff.integrated",
                vec!["handoff", "work"],
            ),
            (
                EventPayload::LegacyHandoffWithdrawn {
                    handoff_id: "handoff".to_owned(),
                    why: "reason".to_owned(),
                },
                "handoff.withdrawn",
                vec!["handoff"],
            ),
            (
                EventPayload::WorkChanged {
                    why: Some("reason".to_owned()),
                    operations: vec![
                        WorkOperation::Add {
                            work: work("added", vec!["required".to_owned()]),
                        },
                        WorkOperation::Edit {
                            id: "edited".to_owned(),
                            title: None,
                            spec: None,
                            priority: None,
                            add_requires: Vec::new(),
                            remove_requires: Vec::new(),
                            add_checks: Vec::new(),
                            remove_checks: Vec::new(),
                            state_change: None,
                        },
                    ],
                },
                "work.changed",
                vec!["added", "required", "edited"],
            ),
            (
                EventPayload::WorkFinished {
                    work_id: "work".to_owned(),
                    attempt_id: Some("attempt".to_owned()),
                    external: false,
                    evidence: None,
                },
                "work.finished",
                vec!["work", "attempt"],
            ),
            (
                EventPayload::WorkDropped {
                    work_id: "work".to_owned(),
                    attempt_id: Some("attempt".to_owned()),
                    outcome: Some(AttemptOutcome::Failed),
                    why: "reason".to_owned(),
                },
                "work.dropped",
                vec!["work", "attempt"],
            ),
            (
                EventPayload::WorkReopened {
                    work_id: "work".to_owned(),
                    why: "reason".to_owned(),
                },
                "work.reopened",
                vec!["work"],
            ),
            (
                EventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
                        id: "attempt".to_owned(),
                        work_id: "work".to_owned(),
                        metadata: BTreeMap::new(),
                    },
                },
                "attempt.started",
                vec!["attempt", "work"],
            ),
            (
                EventPayload::AttemptBound {
                    attempt_id: "attempt".to_owned(),
                    handle: "tmux:one".to_owned(),
                    metadata: BTreeMap::new(),
                },
                "attempt.bound",
                vec!["attempt"],
            ),
            (
                EventPayload::AttemptUpdated {
                    attempt_id: "attempt".to_owned(),
                    metadata: BTreeMap::new(),
                    note: None,
                    checks: Vec::new(),
                },
                "attempt.updated",
                vec!["attempt"],
            ),
            (
                EventPayload::AttemptEnded {
                    attempt_id: "attempt".to_owned(),
                    outcome: AttemptOutcome::Failed,
                    why: "reason".to_owned(),
                },
                "attempt.ended",
                vec!["attempt"],
            ),
            (
                EventPayload::QuestionAsked {
                    question: QuestionDefinition {
                        id: "question".to_owned(),
                        work_id: "work".to_owned(),
                        text: "question".to_owned(),
                    },
                },
                "question.asked",
                vec!["question", "work"],
            ),
            (
                EventPayload::QuestionAnswered {
                    question_id: "question".to_owned(),
                    answer: "answer".to_owned(),
                },
                "question.answered",
                vec!["question"],
            ),
            (
                EventPayload::LegacyPassStarted(serde_json::json!({
                    "pass": {"id": "pass", "engine": "claude"},
                })),
                "pass.started",
                vec![],
            ),
            (
                EventPayload::LegacyPassEnded(serde_json::json!({
                    "pass_id": "pass", "outcome": "ok",
                })),
                "pass.ended",
                vec![],
            ),
            (
                EventPayload::LoopPaused { why: None },
                "loop.paused",
                vec![],
            ),
            (EventPayload::LoopResumed {}, "loop.resumed", vec![]),
            (
                EventPayload::LoopEngineSelected {
                    engine: "codex".to_owned(),
                },
                "loop.engine_selected",
                vec![],
            ),
            (
                EventPayload::LoopRotationRequested { why: None },
                "loop.rotation_requested",
                vec![],
            ),
            (
                EventPayload::LoopNudgeRequested { why: None },
                "loop.nudge_requested",
                vec![],
            ),
        ];

        for (payload, type_name, references) in cases {
            assert_eq!(payload.type_name(), type_name);
            assert!(!payload.references("unrelated"), "{type_name}");
            for reference in references {
                assert!(payload.references(reference), "{type_name}: {reference}");
            }
        }
    }

    #[test]
    fn operation_accessors_and_outcomes_have_exact_semantics() {
        let added = WorkOperation::Add {
            work: work("added", Vec::new()),
        };
        assert_eq!(added.id(), "added");
        assert_eq!(added.definition().unwrap().id, "added");

        let edited = WorkOperation::Edit {
            id: "edited".to_owned(),
            title: None,
            spec: None,
            priority: None,
            add_requires: Vec::new(),
            remove_requires: Vec::new(),
            add_checks: Vec::new(),
            remove_checks: Vec::new(),
            state_change: None,
        };
        assert_eq!(edited.id(), "edited");
        assert!(edited.definition().is_none());

        assert!(!AttemptOutcome::Succeeded.is_non_success());
        for outcome in [
            AttemptOutcome::Failed,
            AttemptOutcome::Cancelled,
            AttemptOutcome::Lost,
            AttemptOutcome::NotStarted,
        ] {
            assert!(outcome.is_non_success());
        }
    }

    #[test]
    fn exactly_the_handoff_and_pass_variants_are_legacy() {
        let legacy = [
            EventPayload::LegacyHandoffSubmitted {
                handoff: LegacyHandoffDefinition {
                    id: "handoff".to_owned(),
                    title: "handoff".to_owned(),
                    artifact_ref: "ref".to_owned(),
                    note: None,
                },
            },
            EventPayload::LegacyHandoffIntegrated {
                handoff_id: "handoff".to_owned(),
                work: work("work", Vec::new()),
            },
            EventPayload::LegacyHandoffWithdrawn {
                handoff_id: "handoff".to_owned(),
                why: "reason".to_owned(),
            },
            EventPayload::LegacyPassStarted(serde_json::json!({})),
            EventPayload::LegacyPassEnded(serde_json::json!({})),
        ];
        for payload in &legacy {
            assert!(payload.is_legacy(), "{}", payload.type_name());
        }
        let live = [
            EventPayload::WorkReopened {
                work_id: "work".to_owned(),
                why: "reason".to_owned(),
            },
            EventPayload::LoopPaused { why: None },
            EventPayload::LoopRotationRequested { why: None },
            EventPayload::LoopNudgeRequested { why: None },
        ];
        for payload in &live {
            assert!(!payload.is_legacy(), "{}", payload.type_name());
        }
    }

    #[test]
    fn work_edit_json_distinguishes_omitted_set_and_cleared_specs() {
        let operation = |spec| WorkOperation::Edit {
            id: "edited".to_owned(),
            title: None,
            spec,
            priority: None,
            add_requires: Vec::new(),
            remove_requires: Vec::new(),
            add_checks: Vec::new(),
            remove_checks: Vec::new(),
            state_change: None,
        };

        let omitted = serde_json::to_value(operation(None)).unwrap();
        assert!(omitted.get("spec").is_none());

        let cleared = serde_json::to_value(operation(Some(NullableString(None)))).unwrap();
        assert!(cleared.get("spec").unwrap().is_null());
        let decoded: WorkOperation = serde_json::from_value(cleared).unwrap();
        match decoded {
            WorkOperation::Edit {
                spec: Some(NullableString(None)),
                ..
            } => {}
            _ => panic!("explicit null must survive as a spec clear"),
        }

        let set =
            serde_json::to_value(operation(Some(NullableString(Some("new spec".to_owned())))))
                .unwrap();
        assert_eq!(set["spec"], "new spec");
    }
}
