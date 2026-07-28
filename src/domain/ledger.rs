use std::collections::BTreeMap;

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use ulid::Ulid;

use crate::error::{AlderError, Result};
use alder_log::{AppendReceipt, Log, LogError};

use super::{
    AttemptDefinition, AttemptOutcome, CheckUpdate, Event, EventDraft, EventPayload,
    GraphChangeDocument, HandoffDefinition, Head, PassDefinition, PassOutcome, PassTrigger,
    PreparedChange, ProjectState, QuestionDefinition, WorkDefinition, WorkOperation, WorkState,
    WorkStateChange,
};

const ID_ALLOCATION_ATTEMPTS: usize = 16;

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub head: Head,
    pub events: Vec<Event>,
    pub state: ProjectState,
}

/// The typed work event result of an application mutation.
#[derive(Debug, Clone)]
pub struct AppendResult {
    pub head: Head,
    pub event: Event,
}

pub struct Ledger<S> {
    store: S,
    prefix: String,
    actor: String,
    /// Called once after each confirmed append, and never on failure. The CLI
    /// installs a hook that touches a local marker file so a co-located
    /// driver can notice an append without waiting out its poll interval.
    on_append: Option<Box<dyn Fn() + Send + Sync>>,
}

impl<S: Log> Ledger<S> {
    pub fn new(store: S, prefix: impl Into<String>, actor: impl Into<String>) -> Self {
        Self {
            store,
            prefix: prefix.into(),
            actor: actor.into(),
            on_append: None,
        }
    }

    pub fn with_on_append(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_append = Some(Box::new(hook));
        self
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        let head = self.store.head()?;
        let events = self
            .store
            .read_all(&head)?
            .iter()
            .map(super::decode_record)
            .collect::<Result<Vec<_>>>()?;
        let state = ProjectState::fold(&events)?;
        state.validate_prefix(&self.prefix)?;
        Ok(Snapshot {
            head,
            events,
            state,
        })
    }

    pub fn append_payload(
        &self,
        snapshot: &Snapshot,
        payload: EventPayload,
    ) -> Result<AppendResult> {
        self.append_payload_at(snapshot, Utc::now(), payload)
    }

    fn append_payload_at(
        &self,
        snapshot: &Snapshot,
        at: DateTime<Utc>,
        payload: EventPayload,
    ) -> Result<AppendResult> {
        let draft = self.draft_at(at, payload);
        let candidate = draft.materialize(snapshot.head.sequence().saturating_add(1));
        let mut state = snapshot.state.clone();
        state.apply(&candidate)?;
        state.validate_prefix(&self.prefix)?;
        let receipt = self
            .store
            .append(&snapshot.head, &super::encode_draft(&draft)?)?;
        if let Some(hook) = &self.on_append {
            hook();
        }
        append_result(receipt)
    }

    pub fn commit_change(
        &self,
        snapshot: &Snapshot,
        document: &GraphChangeDocument,
        prepared: PreparedChange,
    ) -> Result<AppendResult> {
        self.append_payload(
            snapshot,
            EventPayload::WorkChanged {
                why: document.why.clone(),
                operations: prepared.operations,
            },
        )
    }

    pub fn add_work(
        &self,
        title: String,
        spec: Option<String>,
        priority: i64,
        requires: Vec<String>,
        checks: Vec<super::CheckDefinition>,
    ) -> Result<(AppendResult, String)> {
        let snapshot = self.snapshot()?;
        let id = self.new_work_id(&snapshot.state)?;
        let operation = WorkOperation::Add {
            work: WorkDefinition {
                id: id.clone(),
                title,
                spec,
                priority,
                requires,
                checks,
            },
        };
        let result = self.append_payload(
            &snapshot,
            EventPayload::WorkChanged {
                why: None,
                operations: vec![operation],
            },
        )?;
        Ok((result, id))
    }

    pub fn integrate_handoff(
        &self,
        handoff_id: &str,
        title: Option<String>,
        spec: Option<String>,
        priority: i64,
        requires: Vec<String>,
        checks: Vec<super::CheckDefinition>,
    ) -> Result<(AppendResult, String)> {
        let snapshot = self.snapshot()?;
        let handoff = snapshot
            .state
            .handoffs
            .get(handoff_id)
            .ok_or_else(|| AlderError::not_found("handoff", handoff_id))?;
        let id = self.new_work_id(&snapshot.state)?;
        let work = WorkDefinition {
            id: id.clone(),
            title: title.unwrap_or_else(|| handoff.title.clone()),
            spec: spec.or_else(|| Some(handoff.artifact_ref.clone())),
            priority,
            requires,
            checks,
        };
        let result = self.append_payload(
            &snapshot,
            EventPayload::HandoffIntegrated {
                handoff_id: handoff_id.to_owned(),
                work,
            },
        )?;
        Ok((result, id))
    }

    pub fn withdraw_handoff(&self, handoff_id: &str, why: String) -> Result<AppendResult> {
        let snapshot = self.snapshot()?;
        self.append_payload(
            &snapshot,
            EventPayload::HandoffWithdrawn {
                handoff_id: handoff_id.to_owned(),
                why,
            },
        )
    }

    pub fn add_handoff(
        &self,
        title: String,
        artifact_ref: String,
        note: Option<String>,
    ) -> Result<(AppendResult, String)> {
        let initial = self.snapshot()?;
        let id = self.new_handoff_id(&initial.state)?;
        let draft = self.draft(EventPayload::HandoffSubmitted {
            handoff: HandoffDefinition {
                id: id.clone(),
                title,
                artifact_ref,
                note,
            },
        });
        let mut snapshot = initial;
        for _ in 0..16 {
            let candidate = draft.materialize(snapshot.head.sequence().saturating_add(1));
            let mut state = snapshot.state.clone();
            state.apply(&candidate)?;
            match self
                .store
                .append(&snapshot.head, &super::encode_draft(&draft)?)
            {
                Ok(result) => return Ok((append_result(result)?, id)),
                Err(LogError::HeadConflict { .. }) => {
                    snapshot = self.snapshot()?;
                    if let Some(event) = snapshot.events.iter().find(|event| event.id == draft.id) {
                        return Ok((
                            AppendResult {
                                head: snapshot.head,
                                event: event.clone(),
                            },
                            id,
                        ));
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(AlderError::with_context(
            "head_conflict",
            "handoff submission could not settle after repeated concurrent appends",
            json!({"handoff_id": id}),
        ))
    }

    pub fn start(
        &self,
        work_id: &str,
        metadata: BTreeMap<String, Value>,
    ) -> Result<(AppendResult, String)> {
        let snapshot = self.snapshot()?;
        let work = snapshot
            .state
            .work
            .get(work_id)
            .ok_or_else(|| AlderError::not_found("work", work_id))?;
        if let Some(active) = snapshot.state.active_attempt_for(work_id) {
            return Err(AlderError::with_context(
                "active_attempt",
                format!("work `{work_id}` already has an active attempt"),
                json!({"work_id": work_id, "active_attempt_id": active.id}),
            ));
        }
        if !snapshot.state.is_ready(work_id) {
            let unmet: Vec<_> = work
                .requires
                .iter()
                .filter(|required| {
                    snapshot
                        .state
                        .work
                        .get(*required)
                        .is_none_or(|dependency| dependency.state != WorkState::Done)
                })
                .cloned()
                .collect();
            return Err(AlderError::with_context(
                "work_not_ready",
                format!("work `{work_id}` is not ready"),
                json!({"work_id": work_id, "state": work.state, "unmet_dependencies": unmet}),
            ));
        }
        let ordinal = snapshot
            .state
            .attempts
            .values()
            .filter(|attempt| attempt.work_id == work_id)
            .count()
            .saturating_add(1);
        let id = format!("{work_id}-attempt-{ordinal}");
        let result = self.append_payload(
            &snapshot,
            EventPayload::AttemptStarted {
                attempt: AttemptDefinition {
                    id: id.clone(),
                    work_id: work_id.to_owned(),
                    metadata,
                },
            },
        )?;
        Ok((result, id))
    }

    pub fn bind_attempt(
        &self,
        attempt_id: &str,
        handle: String,
        metadata: BTreeMap<String, Value>,
    ) -> Result<AppendResult> {
        let snapshot = self.snapshot()?;
        self.append_payload(
            &snapshot,
            EventPayload::AttemptBound {
                attempt_id: attempt_id.to_owned(),
                handle,
                metadata,
            },
        )
    }

    pub fn update_attempt(
        &self,
        attempt_id: &str,
        metadata: BTreeMap<String, Value>,
        note: Option<String>,
        checks: Vec<CheckUpdate>,
    ) -> Result<AppendResult> {
        let snapshot = self.snapshot()?;
        self.append_payload(
            &snapshot,
            EventPayload::AttemptUpdated {
                attempt_id: attempt_id.to_owned(),
                metadata,
                note,
                checks,
            },
        )
    }

    pub fn end_attempt(
        &self,
        attempt_id: &str,
        outcome: AttemptOutcome,
        why: String,
    ) -> Result<AppendResult> {
        let snapshot = self.snapshot()?;
        self.append_payload(
            &snapshot,
            EventPayload::AttemptEnded {
                attempt_id: attempt_id.to_owned(),
                outcome,
                why,
            },
        )
    }

    pub fn finish(
        &self,
        work_id: &str,
        attempt_id: Option<String>,
        external: bool,
        evidence: Option<String>,
    ) -> Result<AppendResult> {
        let snapshot = self.snapshot()?;
        self.append_payload(
            &snapshot,
            EventPayload::WorkFinished {
                work_id: work_id.to_owned(),
                attempt_id,
                external,
                evidence,
            },
        )
    }

    pub fn drop_work(
        &self,
        work_id: &str,
        attempt_id: Option<String>,
        outcome: Option<AttemptOutcome>,
        why: String,
    ) -> Result<AppendResult> {
        let snapshot = self.snapshot()?;
        self.append_payload(
            &snapshot,
            EventPayload::WorkDropped {
                work_id: work_id.to_owned(),
                attempt_id,
                outcome,
                why,
            },
        )
    }

    pub fn reopen(&self, work_id: &str, why: String) -> Result<AppendResult> {
        let snapshot = self.snapshot()?;
        self.append_payload(
            &snapshot,
            EventPayload::WorkReopened {
                work_id: work_id.to_owned(),
                why,
            },
        )
    }

    pub fn ask(&self, work_id: &str, text: String) -> Result<(AppendResult, String)> {
        let snapshot = self.snapshot()?;
        if !snapshot.state.work.contains_key(work_id) {
            return Err(AlderError::not_found("work", work_id));
        }
        let ordinal = snapshot
            .state
            .questions
            .values()
            .filter(|question| question.work_id == work_id)
            .count()
            .saturating_add(1);
        let id = format!("{work_id}-question-{ordinal}");
        let result = self.append_payload(
            &snapshot,
            EventPayload::QuestionAsked {
                question: QuestionDefinition {
                    id: id.clone(),
                    work_id: work_id.to_owned(),
                    text,
                },
            },
        )?;
        Ok((result, id))
    }

    pub fn answer(&self, question_id: &str, answer: String) -> Result<AppendResult> {
        let snapshot = self.snapshot()?;
        self.append_payload(
            &snapshot,
            EventPayload::QuestionAnswered {
                question_id: question_id.to_owned(),
                answer,
            },
        )
    }

    /// Change one work item's state. Blocking and unblocking remain `edit`
    /// operations in `work.changed`; only the CLI spelling is a verb.
    pub fn set_work_state(&self, work_id: &str, change: WorkStateChange) -> Result<AppendResult> {
        let snapshot = self.snapshot()?;
        if !snapshot.state.work.contains_key(work_id) {
            return Err(AlderError::not_found("work", work_id));
        }
        self.append_payload(
            &snapshot,
            EventPayload::WorkChanged {
                why: Some(match &change {
                    WorkStateChange::Block { reason } | WorkStateChange::Unblock { reason } => {
                        reason.clone()
                    }
                }),
                operations: vec![WorkOperation::Edit {
                    id: work_id.to_owned(),
                    title: None,
                    spec: None,
                    priority: None,
                    add_requires: Vec::new(),
                    remove_requires: Vec::new(),
                    add_checks: Vec::new(),
                    remove_checks: Vec::new(),
                    state_change: Some(change),
                }],
            },
        )
    }

    pub fn wake_loop(
        &self,
        engine: String,
        handle: String,
        triggers: Vec<PassTrigger>,
    ) -> Result<(AppendResult, String)> {
        let snapshot = self.snapshot()?;
        if let Some(open) = snapshot.state.open_pass() {
            return Err(AlderError::with_context(
                "pass_open",
                format!("pass `{}` is still open", open.id),
                json!({
                    "pass_id": open.id,
                    "engine": open.engine,
                    "handle": open.handle,
                    "started_at": open.started_at,
                }),
            ));
        }
        let ordinal = snapshot.state.passes.len().saturating_add(1);
        let id = format!("{}-pass-{ordinal}", self.prefix);
        let result = self.append_payload(
            &snapshot,
            EventPayload::PassStarted {
                pass: PassDefinition {
                    id: id.clone(),
                    engine,
                    handle,
                    triggers,
                    at_head: snapshot.head.sequence(),
                },
            },
        )?;
        Ok((result, id))
    }

    pub fn end_pass(
        &self,
        pass_id: Option<&str>,
        outcome: PassOutcome,
        report: Option<String>,
        wake_after: Option<TimeDelta>,
        rotate: bool,
        why: Option<String>,
    ) -> Result<(AppendResult, String)> {
        let snapshot = self.snapshot()?;
        let id = match pass_id {
            Some(id) => id.to_owned(),
            None => snapshot
                .state
                .open_pass()
                .map(|pass| pass.id.clone())
                .ok_or_else(|| {
                    AlderError::new("no_open_pass", "the loop has no open pass to end")
                })?,
        };
        let at = Utc::now();
        let payload = EventPayload::PassEnded {
            pass_id: id.clone(),
            outcome,
            report,
            wake_at: wake_after.map(|after| at + after),
            rotate,
            why,
        };
        let result = self.append_payload_at(&snapshot, at, payload)?;
        Ok((result, id))
    }

    pub fn pause_loop(&self, why: Option<String>) -> Result<AppendResult> {
        let snapshot = self.snapshot()?;
        self.append_payload(&snapshot, EventPayload::LoopPaused { why })
    }

    pub fn resume_loop(&self) -> Result<AppendResult> {
        let snapshot = self.snapshot()?;
        self.append_payload(&snapshot, EventPayload::LoopResumed {})
    }

    pub fn select_engine(&self, engine: String) -> Result<AppendResult> {
        let snapshot = self.snapshot()?;
        self.append_payload(&snapshot, EventPayload::LoopEngineSelected { engine })
    }

    pub fn request_rotation(&self, why: Option<String>) -> Result<AppendResult> {
        let snapshot = self.snapshot()?;
        self.append_payload(&snapshot, EventPayload::LoopRotationRequested { why })
    }

    pub fn request_nudge(&self, why: Option<String>) -> Result<AppendResult> {
        let snapshot = self.snapshot()?;
        self.append_payload(&snapshot, EventPayload::LoopNudgeRequested { why })
    }

    fn draft(&self, payload: EventPayload) -> EventDraft {
        self.draft_at(Utc::now(), payload)
    }

    fn draft_at(&self, at: DateTime<Utc>, payload: EventPayload) -> EventDraft {
        EventDraft {
            id: Ulid::new().to_string(),
            at,
            actor: self.actor.clone(),
            payload,
            schema: "alder.event.v0".to_owned(),
        }
    }

    fn new_work_id(&self, state: &ProjectState) -> Result<String> {
        for _ in 0..ID_ALLOCATION_ATTEMPTS {
            let token = random_token();
            let id = format!("{}-{token}", self.prefix);
            if !state.work.contains_key(&id) {
                return Ok(id);
            }
        }
        Err(id_allocation_error("work"))
    }

    fn new_handoff_id(&self, state: &ProjectState) -> Result<String> {
        for _ in 0..ID_ALLOCATION_ATTEMPTS {
            let token = random_token();
            let id = format!("{}-handoff-{token}", self.prefix);
            if !state.handoffs.contains_key(&id) {
                return Ok(id);
            }
        }
        Err(id_allocation_error("handoff"))
    }

    pub fn allocate_change(
        &self,
        snapshot: &Snapshot,
        document: &GraphChangeDocument,
        mode: super::ChangeMode,
    ) -> Result<PreparedChange> {
        let mut allocated = Vec::new();
        for _ in &document.add {
            let mut candidate = None;
            for _ in 0..ID_ALLOCATION_ATTEMPTS {
                let token = random_token();
                let id = format!("{}-{token}", self.prefix);
                if work_id_available(&snapshot.state, &allocated, &id) {
                    candidate = Some(id);
                    break;
                }
            }
            allocated.push(candidate.ok_or_else(|| id_allocation_error("work"))?);
        }
        super::prepare_change(&snapshot.state, document, mode, |index, _| {
            allocated[index].clone()
        })
    }
}

fn append_result(receipt: AppendReceipt) -> Result<AppendResult> {
    Ok(AppendResult {
        head: receipt.observed_head,
        event: super::decode_record(&receipt.record)?,
    })
}

fn id_allocation_error(kind: &str) -> AlderError {
    AlderError::with_context(
        "id_allocation_failed",
        format!("could not allocate a unique {kind} ID"),
        json!({"kind": kind, "attempts": ID_ALLOCATION_ATTEMPTS}),
    )
}

fn work_id_available(state: &ProjectState, allocated: &[String], id: &str) -> bool {
    !state.work.contains_key(id) && !allocated.iter().any(|candidate| candidate == id)
}

fn random_token() -> String {
    Ulid::new()
        .to_string()
        .chars()
        .rev()
        .take(6)
        .collect::<String>()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::domain::{CheckDefinition, CheckStatus};
    use alder_log::{
        AppendReceipt, Log as Store, LogError, MemoryLog as MemoryStore, Record, RecordDraft,
    };

    #[derive(Debug, Clone, Copy)]
    enum ConflictBehavior {
        Once,
        CommitThenReport,
        Always,
        OtherError,
    }

    #[derive(Debug)]
    struct ConflictStore {
        inner: MemoryStore,
        behavior: ConflictBehavior,
        append_calls: Mutex<usize>,
    }

    impl ConflictStore {
        fn new(behavior: ConflictBehavior) -> Self {
            Self {
                inner: MemoryStore::new(),
                behavior,
                append_calls: Mutex::new(0),
            }
        }

        fn calls(&self) -> usize {
            *self.append_calls.lock().unwrap()
        }

        fn conflict() -> LogError {
            LogError::HeadConflict {
                expected: Head::empty(),
                observed: Head::empty(),
            }
        }
    }

    impl Store for ConflictStore {
        fn head(&self) -> std::result::Result<Head, LogError> {
            self.inner.head()
        }

        fn read(&self, head: &Head, after: u64) -> std::result::Result<Vec<Record>, LogError> {
            self.inner.read(head, after)
        }

        fn append(
            &self,
            expected: &Head,
            draft: &RecordDraft,
        ) -> std::result::Result<AppendReceipt, LogError> {
            let mut calls = self.append_calls.lock().unwrap();
            *calls += 1;
            let call = *calls;
            drop(calls);
            match (self.behavior, call) {
                (ConflictBehavior::Once, 1) => Err(Self::conflict()),
                (ConflictBehavior::CommitThenReport, 1) => {
                    self.inner.append(expected, draft)?;
                    Err(Self::conflict())
                }
                (ConflictBehavior::Always, _) => Err(Self::conflict()),
                (ConflictBehavior::OtherError, _) => Err(LogError::Unavailable {
                    message: "simulated outage".to_owned(),
                }),
                _ => self.inner.append(expected, draft),
            }
        }
    }

    #[test]
    fn attempts_consume_ordinals_and_late_updates_are_rejected() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "test");
        let (_, work) = ledger
            .add_work("work".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let (_, first) = ledger.start(&work, BTreeMap::new()).unwrap();
        ledger
            .end_attempt(
                &first,
                AttemptOutcome::NotStarted,
                "launch failed".to_owned(),
            )
            .unwrap();
        let (_, second) = ledger.start(&work, BTreeMap::new()).unwrap();
        assert!(second.ends_with("-attempt-2"));
        let error = ledger
            .update_attempt(&first, BTreeMap::new(), Some("late".to_owned()), vec![])
            .unwrap_err();
        assert_eq!(error.code, "attempt_ended");
    }

    #[test]
    fn asking_blocks_and_answering_does_not_unblock() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "test");
        let (_, work) = ledger
            .add_work("work".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let (_, question) = ledger.ask(&work, "which path?".to_owned()).unwrap();
        ledger.answer(&question, "path A".to_owned()).unwrap();
        let snapshot = ledger.snapshot().unwrap();
        assert_eq!(snapshot.state.work[&work].state, WorkState::Blocked);
        assert_eq!(
            snapshot.state.questions[&question].answer.as_deref(),
            Some("path A")
        );
    }

    #[test]
    fn later_attempt_gets_the_complete_check_contract() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "test");
        let (_, work) = ledger
            .add_work(
                "work".to_owned(),
                None,
                0,
                vec![],
                vec![
                    CheckDefinition {
                        key: "tests".to_owned(),
                        description: "tests pass".to_owned(),
                    },
                    CheckDefinition {
                        key: "review".to_owned(),
                        description: "review passes".to_owned(),
                    },
                ],
            )
            .unwrap();
        let (_, first) = ledger.start(&work, BTreeMap::new()).unwrap();
        ledger
            .end_attempt(&first, AttemptOutcome::Failed, "failed".to_owned())
            .unwrap();
        let (_, second) = ledger.start(&work, BTreeMap::new()).unwrap();
        let snapshot = ledger.snapshot().unwrap();
        assert_eq!(snapshot.state.attempts[&second].checks.len(), 2);
        assert!(
            snapshot.state.attempts[&second]
                .checks
                .values()
                .all(|check| check.status == CheckStatus::Pending)
        );
    }

    #[test]
    fn reopening_a_prerequisite_rejects_active_downstream_work() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "test");
        let (_, prerequisite) = ledger
            .add_work("A".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let (_, first_attempt) = ledger.start(&prerequisite, BTreeMap::new()).unwrap();
        ledger
            .finish(&prerequisite, Some(first_attempt), false, None)
            .unwrap();
        let (_, downstream) = ledger
            .add_work("B".to_owned(), None, 0, vec![prerequisite.clone()], vec![])
            .unwrap();
        let (_, downstream_attempt) = ledger.start(&downstream, BTreeMap::new()).unwrap();
        let error = ledger
            .reopen(&prerequisite, "regressed".to_owned())
            .unwrap_err();
        assert_eq!(error.code, "active_downstream");
        assert_eq!(
            error.context["active_attempts"][0],
            downstream_attempt.as_str()
        );
    }

    #[test]
    fn dropping_active_work_ends_the_named_attempt_atomically() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "test");
        let (_, work) = ledger
            .add_work("work".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let (_, attempt) = ledger.start(&work, BTreeMap::new()).unwrap();
        ledger
            .drop_work(
                &work,
                Some(attempt.clone()),
                Some(AttemptOutcome::Cancelled),
                "no longer useful".to_owned(),
            )
            .unwrap();
        let snapshot = ledger.snapshot().unwrap();
        assert_eq!(snapshot.state.work[&work].state, WorkState::Dropped);
        assert_eq!(
            snapshot.state.attempts[&attempt].outcome,
            Some(AttemptOutcome::Cancelled)
        );
        assert_eq!(
            snapshot.events.last().unwrap().payload.type_name(),
            "work.dropped"
        );
    }

    #[test]
    fn revised_answers_keep_history_and_still_require_explicit_unblock() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "test");
        let (_, work) = ledger
            .add_work("work".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let (_, question) = ledger.ask(&work, "which?".to_owned()).unwrap();
        ledger.answer(&question, "A".to_owned()).unwrap();
        ledger.answer(&question, "B".to_owned()).unwrap();
        let snapshot = ledger.snapshot().unwrap();
        assert_eq!(snapshot.state.questions[&question].answers.len(), 2);
        assert_eq!(snapshot.state.work[&work].state, WorkState::Blocked);

        ledger
            .set_work_state(
                &work,
                WorkStateChange::Unblock {
                    reason: "decision incorporated".to_owned(),
                },
            )
            .unwrap();
        assert_eq!(
            ledger.snapshot().unwrap().state.work[&work].state,
            WorkState::Open
        );
    }

    #[test]
    fn external_completion_is_distinct_and_needs_evidence() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "test");
        let (_, work) = ledger
            .add_work("work".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let missing = ledger.finish(&work, None, true, None).unwrap_err();
        assert_eq!(missing.code, "validation_failed");
        ledger
            .finish(&work, None, true, Some("merged outside Alder".to_owned()))
            .unwrap();
        let snapshot = ledger.snapshot().unwrap();
        match &snapshot.events.last().unwrap().payload {
            EventPayload::WorkFinished { external, .. } => assert!(*external),
            _ => panic!("expected external work finish"),
        }
    }

    #[test]
    fn starts_report_every_unmet_dependency_and_questions_are_per_work() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "test");
        let (_, prerequisite) = ledger
            .add_work("prerequisite".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let (_, dependent) = ledger
            .add_work(
                "dependent".to_owned(),
                None,
                0,
                vec![prerequisite.clone()],
                vec![],
            )
            .unwrap();

        let error = ledger.start(&dependent, BTreeMap::new()).unwrap_err();
        assert_eq!(error.code, "work_not_ready");
        assert_eq!(
            error.context["unmet_dependencies"],
            json!([prerequisite.clone()])
        );

        let (_, first) = ledger.ask(&dependent, "first?".to_owned()).unwrap();
        let (_, second) = ledger.ask(&dependent, "second?".to_owned()).unwrap();
        let (_, other) = ledger.ask(&prerequisite, "other?".to_owned()).unwrap();
        assert_eq!(first, format!("{dependent}-question-1"));
        assert_eq!(second, format!("{dependent}-question-2"));
        assert_eq!(other, format!("{prerequisite}-question-1"));
        assert_eq!(
            ledger
                .ask("hm-missing", "missing?".to_owned())
                .unwrap_err()
                .code,
            "not_found"
        );
    }

    #[test]
    fn handoff_submission_retries_only_head_conflicts_and_recovers_ambiguity() {
        let retry = Ledger::new(
            ConflictStore::new(ConflictBehavior::Once),
            "hm",
            "side-channel",
        );
        let (result, id) = retry
            .add_handoff(
                "candidate".to_owned(),
                "branch:topic".to_owned(),
                Some("ready".to_owned()),
            )
            .unwrap();
        assert_eq!(retry.store().calls(), 2);
        assert_eq!(result.head.sequence(), 1);
        assert!(id.starts_with("hm-handoff-"));
        assert_eq!(id.len(), "hm-handoff-".len() + 6);
        assert_eq!(result.event.actor, "side-channel");
        assert_eq!(result.event.schema, "alder.event.v0");

        let ambiguous = Ledger::new(
            ConflictStore::new(ConflictBehavior::CommitThenReport),
            "hm",
            "side-channel",
        );
        let (result, id) = ambiguous
            .add_handoff("candidate".to_owned(), "branch:topic".to_owned(), None)
            .unwrap();
        assert_eq!(ambiguous.store().calls(), 1);
        assert_eq!(result.event.id, ambiguous.snapshot().unwrap().events[0].id);
        assert_eq!(
            ambiguous.snapshot().unwrap().state.handoffs[&id].artifact_ref,
            "branch:topic"
        );

        let unavailable = Ledger::new(
            ConflictStore::new(ConflictBehavior::OtherError),
            "hm",
            "side-channel",
        );
        assert_eq!(
            unavailable
                .add_handoff("candidate".to_owned(), "branch:topic".to_owned(), None)
                .unwrap_err()
                .code,
            "store_unavailable"
        );

        let exhausted = Ledger::new(
            ConflictStore::new(ConflictBehavior::Always),
            "hm",
            "side-channel",
        );
        let error = exhausted
            .add_handoff("candidate".to_owned(), "branch:topic".to_owned(), None)
            .unwrap_err();
        assert_eq!(error.code, "head_conflict");
        assert_eq!(exhausted.store().calls(), 16);
    }

    #[test]
    fn integration_uses_handoff_defaults_and_is_single_use() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "test");
        let (_, handoff) = ledger
            .add_handoff(
                "candidate".to_owned(),
                "branch:topic".to_owned(),
                Some("ready".to_owned()),
            )
            .unwrap();
        let (_, work) = ledger
            .integrate_handoff(&handoff, None, None, 7, vec![], vec![])
            .unwrap();
        let snapshot = ledger.snapshot().unwrap();
        assert_eq!(snapshot.state.work[&work].title, "candidate");
        assert_eq!(
            snapshot.state.work[&work].spec.as_deref(),
            Some("branch:topic")
        );
        assert_eq!(snapshot.state.work[&work].priority, 7);

        let error = ledger
            .integrate_handoff(
                &handoff,
                Some("replacement".to_owned()),
                Some("new spec".to_owned()),
                0,
                vec![],
                vec![],
            )
            .unwrap_err();
        assert_eq!(error.code, "invalid_transition");
    }

    #[test]
    fn withdrawing_a_handoff_retires_it_and_frees_nothing_to_integrate() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "test");
        let (_, handoff) = ledger
            .add_handoff("candidate".to_owned(), "branch:topic".to_owned(), None)
            .unwrap();

        let result = ledger
            .withdraw_handoff(&handoff, "superseded by a follow-up".to_owned())
            .unwrap();
        let snapshot = ledger.snapshot().unwrap();
        assert_eq!(
            snapshot.state.handoffs[&handoff].state,
            crate::domain::HandoffState::Withdrawn
        );
        assert_eq!(result.event.payload.type_name(), "handoff.withdrawn");

        // Rejection: an already-withdrawn handoff cannot be integrated.
        let error = ledger
            .integrate_handoff(&handoff, None, None, 0, vec![], vec![])
            .unwrap_err();
        assert_eq!(error.code, "invalid_transition");

        // Rejection: an already-withdrawn handoff cannot be withdrawn again.
        let error = ledger
            .withdraw_handoff(&handoff, "again".to_owned())
            .unwrap_err();
        assert_eq!(error.code, "invalid_transition");
    }

    #[test]
    fn passes_take_serial_ordinals_and_carry_the_head_they_saw() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "alderd");
        ledger
            .add_work("work".to_owned(), None, 0, vec![], vec![])
            .unwrap();

        let (_, first) = ledger
            .wake_loop(
                "claude".to_owned(),
                "tmux:alder-leader".to_owned(),
                vec![PassTrigger::Log],
            )
            .unwrap();
        assert_eq!(first, "hm-pass-1");
        assert_eq!(ledger.snapshot().unwrap().state.passes[&first].at_head, 1);

        let conflict = ledger
            .wake_loop(
                "claude".to_owned(),
                "tmux:alder-leader".to_owned(),
                vec![PassTrigger::Manual],
            )
            .unwrap_err();
        assert_eq!(conflict.code, "pass_open");
        assert_eq!(conflict.context["pass_id"], "hm-pass-1");

        let (_, ended) = ledger
            .end_pass(
                None,
                PassOutcome::Ok,
                Some("swept the frontier".to_owned()),
                Some(TimeDelta::minutes(20)),
                false,
                None,
            )
            .unwrap();
        assert_eq!(ended, first);
        let state = ledger.snapshot().unwrap().state;
        let wake_at = state.passes[&first].wake_at.expect("a wake time");
        assert!(wake_at > Utc::now() + TimeDelta::minutes(19));

        assert_eq!(
            ledger
                .end_pass(None, PassOutcome::Ok, None, None, false, None)
                .unwrap_err()
                .code,
            "no_open_pass"
        );

        let (_, second) = ledger
            .wake_loop(
                "codex".to_owned(),
                "tmux:alder-leader".to_owned(),
                vec![PassTrigger::Due],
            )
            .unwrap();
        assert_eq!(second, "hm-pass-2");
    }

    #[test]
    fn loop_controls_and_work_state_verbs_append_their_own_events() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "operator");
        let (_, work) = ledger
            .add_work("work".to_owned(), None, 0, vec![], vec![])
            .unwrap();

        ledger
            .pause_loop(Some("release freeze".to_owned()))
            .unwrap();
        ledger.select_engine("codex".to_owned()).unwrap();
        ledger.request_rotation(None).unwrap();
        ledger.request_nudge(None).unwrap();
        let state = ledger.snapshot().unwrap().state;
        assert!(state.loop_control.paused);
        assert_eq!(state.loop_control.engine.as_deref(), Some("codex"));
        assert!(state.loop_control.rotate_pending());
        assert!(state.loop_control.nudge_pending());

        ledger.resume_loop().unwrap();
        assert!(!ledger.snapshot().unwrap().state.loop_control.paused);

        ledger
            .set_work_state(
                &work,
                WorkStateChange::Block {
                    reason: "credentials missing".to_owned(),
                },
            )
            .unwrap();
        assert_eq!(
            ledger.snapshot().unwrap().state.work[&work].state,
            WorkState::Blocked
        );
        ledger
            .set_work_state(
                &work,
                WorkStateChange::Unblock {
                    reason: "credentials installed".to_owned(),
                },
            )
            .unwrap();
        assert_eq!(
            ledger.snapshot().unwrap().state.work[&work].state,
            WorkState::Open
        );
        assert_eq!(
            ledger
                .set_work_state(
                    "hm-missing",
                    WorkStateChange::Block {
                        reason: "reason".to_owned(),
                    },
                )
                .unwrap_err()
                .code,
            "not_found"
        );
    }

    #[test]
    fn change_allocation_rejects_both_persisted_and_batch_collisions() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "test");
        let (_, persisted) = ledger
            .add_work("persisted".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let snapshot = ledger.snapshot().unwrap();
        let allocated = vec!["hm-batch".to_owned()];

        assert!(!work_id_available(&snapshot.state, &allocated, &persisted));
        assert!(!work_id_available(&snapshot.state, &allocated, "hm-batch"));
        assert!(work_id_available(&snapshot.state, &allocated, "hm-unused"));
    }
}
