use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The work application's event schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "body")]
pub enum WorkEventPayload {
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
        /// A new rung name for the attempt; omitted when unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tier: Option<String>,
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
}

impl WorkEventPayload {
    /// A new arm here also belongs in `EVERY_EVENT` in the append-layer
    /// tests, which sweep every mutation for how it reports losing a
    /// compare-and-append.
    pub fn type_name(&self) -> &'static str {
        match self {
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
        )
    }

    pub fn references(&self, id: &str) -> bool {
        match self {
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
        }
    }
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
        /// driver wakes the executor at that time.
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
    /// Serialized as an explicit null so the key is always present.
    #[serde(default)]
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
    /// The runner's rung name for this execution, opaque to Alder. Serialized
    /// as an explicit null so the key is always present; absent in events
    /// written before tiers existed.
    #[serde(default)]
    pub tier: Option<String>,
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
    /// The runner's rung name, stored so a later reader can say "retry this
    /// at a higher rung" from the log alone. Its meaning lives outside the
    /// log: any non-empty name is legal and none is validated against a
    /// table. Serialized as an explicit null so the key is always present.
    #[serde(default)]
    pub tier: Option<String>,
    /// An opaque foreign name for the execution, bound once. Alder stores it
    /// and compares it for equality; it never parses it.
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

#[cfg(test)]
mod tests {
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
