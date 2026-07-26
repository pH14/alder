use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub revision: Option<String>,
    pub seq: u64,
}

impl Head {
    pub fn empty() -> Self {
        Self {
            revision: None,
            seq: 0,
        }
    }
}

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
    #[serde(rename = "handoff.submitted")]
    HandoffSubmitted { handoff: HandoffDefinition },
    #[serde(rename = "handoff.integrated")]
    HandoffIntegrated {
        handoff_id: String,
        work: WorkDefinition,
    },
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
}

impl EventPayload {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::HandoffSubmitted { .. } => "handoff.submitted",
            Self::HandoffIntegrated { .. } => "handoff.integrated",
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

    pub fn references(&self, id: &str) -> bool {
        match self {
            Self::HandoffSubmitted { handoff } => handoff.id == id,
            Self::HandoffIntegrated { handoff_id, work } => handoff_id == id || work.id == id,
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
pub struct HandoffDefinition {
    pub id: String,
    pub title: String,
    #[serde(rename = "ref")]
    pub artifact_ref: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffState {
    Submitted,
    Integrated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handoff {
    pub id: String,
    pub title: String,
    #[serde(rename = "ref")]
    pub artifact_ref: String,
    pub note: Option<String>,
    pub state: HandoffState,
    pub submitted_seq: u64,
    pub work_id: Option<String>,
    pub integrated_seq: Option<u64>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkState {
    Open,
    Blocked,
    Done,
    Dropped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkStateChange {
    Block { reason: String },
    Unblock { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Work {
    pub id: String,
    pub title: String,
    pub spec: Option<String>,
    pub priority: i64,
    pub state: WorkState,
    pub block_reason: Option<String>,
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
