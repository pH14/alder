use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use alder_observation::ObservationEventPayload;
use alder_work::WorkEventPayload;

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

/// Every event the shared log can carry: the union of the applications'
/// schemas plus the loop-control machinery records that still live here.
/// Each application owns its own half; this enum only composes them, and
/// the untagged (de)serialization leaves the wire format exactly what the
/// sub-enums' adjacent `type`/`body` tagging says it is.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EventPayload {
    Work(WorkEventPayload),
    Observation(ObservationEventPayload),
    Loop(LoopEventPayload),
}

impl From<WorkEventPayload> for EventPayload {
    fn from(payload: WorkEventPayload) -> Self {
        Self::Work(payload)
    }
}

impl From<ObservationEventPayload> for EventPayload {
    fn from(payload: ObservationEventPayload) -> Self {
        Self::Observation(payload)
    }
}

impl From<LoopEventPayload> for EventPayload {
    fn from(payload: LoopEventPayload) -> Self {
        Self::Loop(payload)
    }
}

impl EventPayload {
    /// A new arm here also belongs in `EVERY_EVENT` in the log tests, which
    /// sweeps every mutation for how it reports losing a compare-and-append.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Work(payload) => payload.type_name(),
            Self::Observation(payload) => payload.type_name(),
            Self::Loop(payload) => payload.type_name(),
        }
    }

    /// Whether this variant is decode-only history. Legacy events replay from
    /// the log, but no live command may append one: the append layer refuses
    /// them before anything reaches the store.
    pub fn is_legacy(&self) -> bool {
        match self {
            Self::Work(payload) => payload.is_legacy(),
            Self::Observation(_) => false,
            Self::Loop(payload) => payload.is_legacy(),
        }
    }

    pub fn references(&self, id: &str) -> bool {
        match self {
            Self::Work(payload) => payload.references(id),
            // Observations have a composite key rather than an Alder object
            // ID, so their current picture is served by `observations`.
            Self::Observation(_) => false,
            // Legacy pass events named the loop's own runs, which are no
            // longer objects, and loop controls belong to the singleton loop;
            // neither names an object, so neither appears in any object's
            // history.
            Self::Loop(_) => false,
        }
    }
}

/// The loop-control and legacy pass records.
///
/// These are neither work nor observation: they are surviving machinery
/// records — statements about how the project is driven, and about the
/// loop's retired run bookkeeping — that a later work item removes from the
/// log entirely. Until then their schema and fold stay here in the root,
/// deliberately outside both applications.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "body")]
pub enum LoopEventPayload {
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

impl LoopEventPayload {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::LegacyPassStarted(_) => "pass.started",
            Self::LegacyPassEnded(_) => "pass.ended",
            Self::LoopPaused { .. } => "loop.paused",
            Self::LoopResumed {} => "loop.resumed",
            Self::LoopEngineSelected { .. } => "loop.engine_selected",
            Self::LoopRotationRequested { .. } => "loop.rotation_requested",
            Self::LoopNudgeRequested { .. } => "loop.nudge_requested",
        }
    }

    pub fn is_legacy(&self) -> bool {
        matches!(self, Self::LegacyPassStarted(_) | Self::LegacyPassEnded(_))
    }
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
    use std::collections::BTreeMap;

    use alder_log::Head;
    use chrono::Utc;

    use crate::domain::{
        AttemptDefinition, AttemptOutcome, LegacyHandoffDefinition, QuestionDefinition,
        WorkDefinition, WorkOperation,
    };

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
            payload: EventPayload::Work(WorkEventPayload::WorkReopened {
                work_id: "hm-one".to_owned(),
                why: "reason".to_owned(),
            }),
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
                EventPayload::Work(WorkEventPayload::LegacyHandoffSubmitted {
                    handoff: LegacyHandoffDefinition {
                        id: "handoff".to_owned(),
                        title: "handoff".to_owned(),
                        artifact_ref: "ref".to_owned(),
                        note: None,
                    },
                }),
                "handoff.submitted",
                vec!["handoff"],
            ),
            (
                EventPayload::Work(WorkEventPayload::LegacyHandoffIntegrated {
                    handoff_id: "handoff".to_owned(),
                    work: work("work", Vec::new()),
                }),
                "handoff.integrated",
                vec!["handoff", "work"],
            ),
            (
                EventPayload::Work(WorkEventPayload::LegacyHandoffWithdrawn {
                    handoff_id: "handoff".to_owned(),
                    why: "reason".to_owned(),
                }),
                "handoff.withdrawn",
                vec!["handoff"],
            ),
            (
                EventPayload::Work(WorkEventPayload::WorkChanged {
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
                }),
                "work.changed",
                vec!["added", "required", "edited"],
            ),
            (
                EventPayload::Work(WorkEventPayload::WorkFinished {
                    work_id: "work".to_owned(),
                    attempt_id: Some("attempt".to_owned()),
                    external: false,
                    evidence: None,
                }),
                "work.finished",
                vec!["work", "attempt"],
            ),
            (
                EventPayload::Work(WorkEventPayload::WorkDropped {
                    work_id: "work".to_owned(),
                    attempt_id: Some("attempt".to_owned()),
                    outcome: Some(AttemptOutcome::Failed),
                    why: "reason".to_owned(),
                }),
                "work.dropped",
                vec!["work", "attempt"],
            ),
            (
                EventPayload::Work(WorkEventPayload::WorkReopened {
                    work_id: "work".to_owned(),
                    why: "reason".to_owned(),
                }),
                "work.reopened",
                vec!["work"],
            ),
            (
                EventPayload::Work(WorkEventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
                        id: "attempt".to_owned(),
                        work_id: "work".to_owned(),
                        tier: Some("terra".to_owned()),
                        metadata: BTreeMap::new(),
                    },
                }),
                "attempt.started",
                vec!["attempt", "work"],
            ),
            (
                EventPayload::Work(WorkEventPayload::AttemptBound {
                    attempt_id: "attempt".to_owned(),
                    handle: "tmux:one".to_owned(),
                    metadata: BTreeMap::new(),
                }),
                "attempt.bound",
                vec!["attempt"],
            ),
            (
                EventPayload::Work(WorkEventPayload::AttemptUpdated {
                    attempt_id: "attempt".to_owned(),
                    tier: None,
                    metadata: BTreeMap::new(),
                    note: None,
                    checks: Vec::new(),
                }),
                "attempt.updated",
                vec!["attempt"],
            ),
            (
                EventPayload::Work(WorkEventPayload::AttemptEnded {
                    attempt_id: "attempt".to_owned(),
                    outcome: AttemptOutcome::Failed,
                    why: "reason".to_owned(),
                }),
                "attempt.ended",
                vec!["attempt"],
            ),
            (
                EventPayload::Work(WorkEventPayload::QuestionAsked {
                    question: QuestionDefinition {
                        id: "question".to_owned(),
                        work_id: "work".to_owned(),
                        text: "question".to_owned(),
                    },
                }),
                "question.asked",
                vec!["question", "work"],
            ),
            (
                EventPayload::Work(WorkEventPayload::QuestionAnswered {
                    question_id: "question".to_owned(),
                    answer: "answer".to_owned(),
                }),
                "question.answered",
                vec!["question"],
            ),
            (
                EventPayload::Loop(LoopEventPayload::LegacyPassStarted(serde_json::json!({
                    "pass": {"id": "pass", "engine": "claude"},
                }))),
                "pass.started",
                vec![],
            ),
            (
                EventPayload::Loop(LoopEventPayload::LegacyPassEnded(serde_json::json!({
                    "pass_id": "pass", "outcome": "ok",
                }))),
                "pass.ended",
                vec![],
            ),
            (
                EventPayload::Loop(LoopEventPayload::LoopPaused { why: None }),
                "loop.paused",
                vec![],
            ),
            (
                EventPayload::Loop(LoopEventPayload::LoopResumed {}),
                "loop.resumed",
                vec![],
            ),
            (
                EventPayload::Loop(LoopEventPayload::LoopEngineSelected {
                    engine: "codex".to_owned(),
                }),
                "loop.engine_selected",
                vec![],
            ),
            (
                EventPayload::Loop(LoopEventPayload::LoopRotationRequested { why: None }),
                "loop.rotation_requested",
                vec![],
            ),
            (
                EventPayload::Loop(LoopEventPayload::LoopNudgeRequested { why: None }),
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
    fn exactly_the_handoff_and_pass_variants_are_legacy() {
        let legacy = [
            EventPayload::Work(WorkEventPayload::LegacyHandoffSubmitted {
                handoff: LegacyHandoffDefinition {
                    id: "handoff".to_owned(),
                    title: "handoff".to_owned(),
                    artifact_ref: "ref".to_owned(),
                    note: None,
                },
            }),
            EventPayload::Work(WorkEventPayload::LegacyHandoffIntegrated {
                handoff_id: "handoff".to_owned(),
                work: work("work", Vec::new()),
            }),
            EventPayload::Work(WorkEventPayload::LegacyHandoffWithdrawn {
                handoff_id: "handoff".to_owned(),
                why: "reason".to_owned(),
            }),
            EventPayload::Loop(LoopEventPayload::LegacyPassStarted(serde_json::json!({}))),
            EventPayload::Loop(LoopEventPayload::LegacyPassEnded(serde_json::json!({}))),
        ];
        for payload in &legacy {
            assert!(payload.is_legacy(), "{}", payload.type_name());
        }
        let live = [
            EventPayload::Work(WorkEventPayload::WorkReopened {
                work_id: "work".to_owned(),
                why: "reason".to_owned(),
            }),
            EventPayload::Loop(LoopEventPayload::LoopPaused { why: None }),
            EventPayload::Loop(LoopEventPayload::LoopRotationRequested { why: None }),
            EventPayload::Loop(LoopEventPayload::LoopNudgeRequested { why: None }),
        ];
        for payload in &live {
            assert!(!payload.is_legacy(), "{}", payload.type_name());
        }
    }
}
