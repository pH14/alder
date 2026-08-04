use std::collections::BTreeMap;

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use ulid::Ulid;

use crate::error::{AlderError, Result};
use alder_log::{AppendReceipt, Log, LogError};

use super::{
    AttemptDefinition, AttemptOutcome, CheckUpdate, Event, EventDraft, EventPayload,
    GraphChangeDocument, Head, ObservationDefinition, ObservationKey, PassDefinition, PassOutcome,
    PassTrigger, PreparedChange, ProjectState, QuestionDefinition, WorkDefinition, WorkOperation,
    WorkState, WorkStateChange, validate_observation_key,
};

const ID_ALLOCATION_ATTEMPTS: usize = 16;
const OBSERVATION_APPEND_ATTEMPTS: usize = 16;

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

/// The result of recording one observation level. A repeated report is a
/// successful no-op, not an append receipt.
#[derive(Debug, Clone)]
pub enum ObservationAppend {
    Appended(Box<AppendResult>),
    Unchanged { head: Head },
}

pub struct ProjectLog<S> {
    store: S,
    prefix: String,
    actor: String,
    /// Called once after each confirmed append, and never on failure. The CLI
    /// installs a hook that touches a local marker file so a co-located
    /// driver can notice an append without waiting out its poll interval.
    on_append: Option<Box<dyn Fn() + Send + Sync>>,
}

impl<S: Log> ProjectLog<S> {
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

    /// Record a current level only when the folded picture would change.
    ///
    /// Observers are deliberately dumb and may report the same level forever.
    /// The append layer rereads after a racing writer, so two identical reports
    /// settle as one event instead of making scripts remember prior output.
    pub fn report_observation(
        &self,
        key: ObservationKey,
        level: String,
    ) -> Result<ObservationAppend> {
        self.append_observation(key, Some(level))
    }

    /// Remove a current key when an observer has established that the subject
    /// or field no longer exists. Retiring an already absent key is quiet for
    /// the same level-triggered reason as a repeated report.
    pub fn retire_observation(&self, key: ObservationKey) -> Result<ObservationAppend> {
        self.append_observation(key, None)
    }

    fn append_observation(
        &self,
        key: ObservationKey,
        level: Option<String>,
    ) -> Result<ObservationAppend> {
        validate_observation_key(&key)?;
        if let Some(level) = level.as_deref()
            && level.trim().is_empty()
        {
            return Err(AlderError::validation("observation level cannot be empty"));
        }
        for _ in 0..OBSERVATION_APPEND_ATTEMPTS {
            let snapshot = self.snapshot()?;
            let current = snapshot.state.observations.get(&key);
            let payload = match (current, level.as_deref()) {
                (Some(observation), Some(level)) if observation.level == level => {
                    return Ok(ObservationAppend::Unchanged {
                        head: snapshot.head,
                    });
                }
                (None, None) => {
                    return Ok(ObservationAppend::Unchanged {
                        head: snapshot.head,
                    });
                }
                (_, Some(level)) => EventPayload::ObservationReported {
                    observation: ObservationDefinition {
                        key: key.clone(),
                        level: level.to_owned(),
                    },
                },
                (Some(_), None) => EventPayload::ObservationRetired { key: key.clone() },
            };
            match self.append_payload(&snapshot, payload) {
                Ok(result) => return Ok(ObservationAppend::Appended(Box::new(result))),
                Err(error) if error.code == "head_conflict" => continue,
                Err(error) => return Err(error),
            }
        }
        let event = if level.is_some() {
            "observation.reported"
        } else {
            "observation.retired"
        };
        Err(AlderError::with_context(
            "head_conflict",
            format!(
                "nothing was appended: `{event}` could not settle after repeated concurrent appends"
            ),
            json!({"appended": false, "event": event, "key": key}),
        ))
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
            .append(&snapshot.head, &super::encode_draft(&draft)?)
            .map_err(|error| lost_append(draft.payload.type_name(), error))?;
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

/// Name the event a losing writer did not append.
///
/// Every mutation reaches the store through one call, so
/// this is the single place a lost compare-and-append is described — and it
/// describes the command's effect, not the log's. Retrying here is deliberately
/// not an option: the draft was validated against one projection and
/// materialized at one sequence, so replaying it against a log that has moved
/// would append a decision nobody made. Reconsideration belongs to the caller.
fn lost_append(event: &'static str, error: LogError) -> AlderError {
    match error {
        LogError::HeadConflict { expected, observed } => {
            AlderError::lost_append(Some(event), expected.sequence(), observed.sequence())
        }
        other => other.into(),
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
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    use super::*;
    use crate::domain::{ChangeMode, CheckDefinition, CheckStatus};
    use alder_log::{
        AppendReceipt, Log as Store, LogError, MemoryLog as MemoryStore, Record, RecordDraft,
    };

    #[derive(Debug)]
    struct ConflictStore {
        inner: MemoryStore,
        append_calls: Mutex<usize>,
        armed: Mutex<bool>,
    }

    impl ConflictStore {
        fn new() -> Self {
            Self {
                inner: MemoryStore::new(),
                append_calls: Mutex::new(0),
                armed: Mutex::new(false),
            }
        }

        fn arm(&self, armed: bool) {
            *self.armed.lock().unwrap() = armed;
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
            drop(calls);
            let armed = *self.armed.lock().unwrap();
            if armed {
                Err(Self::conflict())
            } else {
                self.inner.append(expected, draft)
            }
        }
    }

    #[test]
    fn attempts_consume_ordinals_and_late_updates_are_rejected() {
        let log = ProjectLog::new(MemoryStore::new(), "hm", "test");
        let (_, work) = log
            .add_work("work".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let (_, first) = log.start(&work, BTreeMap::new()).unwrap();
        log.end_attempt(
            &first,
            AttemptOutcome::NotStarted,
            "launch failed".to_owned(),
        )
        .unwrap();
        let (_, second) = log.start(&work, BTreeMap::new()).unwrap();
        assert!(second.ends_with("-attempt-2"));
        let error = log
            .update_attempt(&first, BTreeMap::new(), Some("late".to_owned()), vec![])
            .unwrap_err();
        assert_eq!(error.code, "attempt_ended");
    }

    #[test]
    fn asking_blocks_and_answering_does_not_unblock() {
        let log = ProjectLog::new(MemoryStore::new(), "hm", "test");
        let (_, work) = log
            .add_work("work".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let (_, question) = log.ask(&work, "which path?".to_owned()).unwrap();
        log.answer(&question, "path A".to_owned()).unwrap();
        let snapshot = log.snapshot().unwrap();
        assert_eq!(snapshot.state.work[&work].state, WorkState::Blocked);
        assert_eq!(
            snapshot.state.questions[&question].answer.as_deref(),
            Some("path A")
        );
    }

    #[test]
    fn later_attempt_gets_the_complete_check_contract() {
        let log = ProjectLog::new(MemoryStore::new(), "hm", "test");
        let (_, work) = log
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
        let (_, first) = log.start(&work, BTreeMap::new()).unwrap();
        log.end_attempt(&first, AttemptOutcome::Failed, "failed".to_owned())
            .unwrap();
        let (_, second) = log.start(&work, BTreeMap::new()).unwrap();
        let snapshot = log.snapshot().unwrap();
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
        let log = ProjectLog::new(MemoryStore::new(), "hm", "test");
        let (_, prerequisite) = log
            .add_work("A".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let (_, first_attempt) = log.start(&prerequisite, BTreeMap::new()).unwrap();
        log.finish(&prerequisite, Some(first_attempt), false, None)
            .unwrap();
        let (_, downstream) = log
            .add_work("B".to_owned(), None, 0, vec![prerequisite.clone()], vec![])
            .unwrap();
        let (_, downstream_attempt) = log.start(&downstream, BTreeMap::new()).unwrap();
        let error = log
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
        let log = ProjectLog::new(MemoryStore::new(), "hm", "test");
        let (_, work) = log
            .add_work("work".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let (_, attempt) = log.start(&work, BTreeMap::new()).unwrap();
        log.drop_work(
            &work,
            Some(attempt.clone()),
            Some(AttemptOutcome::Cancelled),
            "no longer useful".to_owned(),
        )
        .unwrap();
        let snapshot = log.snapshot().unwrap();
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
        let log = ProjectLog::new(MemoryStore::new(), "hm", "test");
        let (_, work) = log
            .add_work("work".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let (_, question) = log.ask(&work, "which?".to_owned()).unwrap();
        log.answer(&question, "A".to_owned()).unwrap();
        log.answer(&question, "B".to_owned()).unwrap();
        let snapshot = log.snapshot().unwrap();
        assert_eq!(snapshot.state.questions[&question].answers.len(), 2);
        assert_eq!(snapshot.state.work[&work].state, WorkState::Blocked);

        log.set_work_state(
            &work,
            WorkStateChange::Unblock {
                reason: "decision incorporated".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(
            log.snapshot().unwrap().state.work[&work].state,
            WorkState::Open
        );
    }

    #[test]
    fn external_completion_is_distinct_and_needs_evidence() {
        let log = ProjectLog::new(MemoryStore::new(), "hm", "test");
        let (_, work) = log
            .add_work("work".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let missing = log.finish(&work, None, true, None).unwrap_err();
        assert_eq!(missing.code, "validation_failed");
        log.finish(&work, None, true, Some("merged outside Alder".to_owned()))
            .unwrap();
        let snapshot = log.snapshot().unwrap();
        match &snapshot.events.last().unwrap().payload {
            EventPayload::WorkFinished { external, .. } => assert!(*external),
            _ => panic!("expected external work finish"),
        }
    }

    #[test]
    fn starts_report_every_unmet_dependency_and_questions_are_per_work() {
        let log = ProjectLog::new(MemoryStore::new(), "hm", "test");
        let (_, prerequisite) = log
            .add_work("prerequisite".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let (_, dependent) = log
            .add_work(
                "dependent".to_owned(),
                None,
                0,
                vec![prerequisite.clone()],
                vec![],
            )
            .unwrap();

        let error = log.start(&dependent, BTreeMap::new()).unwrap_err();
        assert_eq!(error.code, "work_not_ready");
        assert_eq!(
            error.context["unmet_dependencies"],
            json!([prerequisite.clone()])
        );

        let (_, first) = log.ask(&dependent, "first?".to_owned()).unwrap();
        let (_, second) = log.ask(&dependent, "second?".to_owned()).unwrap();
        let (_, other) = log.ask(&prerequisite, "other?".to_owned()).unwrap();
        assert_eq!(first, format!("{dependent}-question-1"));
        assert_eq!(second, format!("{dependent}-question-2"));
        assert_eq!(other, format!("{prerequisite}-question-1"));
        assert_eq!(
            log.ask("hm-missing", "missing?".to_owned())
                .unwrap_err()
                .code,
            "not_found"
        );
    }

    #[test]
    fn passes_take_serial_ordinals_and_carry_the_head_they_saw() {
        let log = ProjectLog::new(MemoryStore::new(), "hm", "alderd");
        log.add_work("work".to_owned(), None, 0, vec![], vec![])
            .unwrap();

        let (_, first) = log
            .wake_loop(
                "claude".to_owned(),
                "tmux:alder-leader".to_owned(),
                vec![PassTrigger::Log],
            )
            .unwrap();
        assert_eq!(first, "hm-pass-1");
        assert_eq!(log.snapshot().unwrap().state.passes[&first].at_head, 1);

        let conflict = log
            .wake_loop(
                "claude".to_owned(),
                "tmux:alder-leader".to_owned(),
                vec![PassTrigger::Manual],
            )
            .unwrap_err();
        assert_eq!(conflict.code, "pass_open");
        assert_eq!(conflict.context["pass_id"], "hm-pass-1");

        let (_, ended) = log
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
        let state = log.snapshot().unwrap().state;
        let wake_at = state.passes[&first].wake_at.expect("a wake time");
        assert!(wake_at > Utc::now() + TimeDelta::minutes(19));

        assert_eq!(
            log.end_pass(None, PassOutcome::Ok, None, None, false, None)
                .unwrap_err()
                .code,
            "no_open_pass"
        );

        let (_, second) = log
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
        let log = ProjectLog::new(MemoryStore::new(), "hm", "operator");
        let (_, work) = log
            .add_work("work".to_owned(), None, 0, vec![], vec![])
            .unwrap();

        log.pause_loop(Some("release freeze".to_owned())).unwrap();
        log.select_engine("codex".to_owned()).unwrap();
        log.request_rotation(None).unwrap();
        log.request_nudge(None).unwrap();
        let state = log.snapshot().unwrap().state;
        assert!(state.loop_control.paused);
        assert_eq!(state.loop_control.engine.as_deref(), Some("codex"));
        assert!(state.loop_control.rotate_pending());
        assert!(state.loop_control.nudge_pending());

        log.resume_loop().unwrap();
        assert!(!log.snapshot().unwrap().state.loop_control.paused);

        log.set_work_state(
            &work,
            WorkStateChange::Block {
                reason: "credentials missing".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(
            log.snapshot().unwrap().state.work[&work].state,
            WorkState::Blocked
        );
        log.set_work_state(
            &work,
            WorkStateChange::Unblock {
                reason: "credentials installed".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(
            log.snapshot().unwrap().state.work[&work].state,
            WorkState::Open
        );
        assert_eq!(
            log.set_work_state(
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

    /// One leader-side mutation, ready to be raced.
    type Mutation<'a> = Box<dyn Fn() -> Result<AppendResult> + 'a>;

    /// Every event a mutation can append, so the sweep below can prove it
    /// reached all of them. `EventPayload::type_name` is the compiler-checked
    /// list; this is the copy the sweep is measured against.
    const EVERY_EVENT: [&str; 19] = [
        "observation.reported",
        "observation.retired",
        "work.changed",
        "work.finished",
        "work.dropped",
        "work.reopened",
        "attempt.started",
        "attempt.bound",
        "attempt.updated",
        "attempt.ended",
        "question.asked",
        "question.answered",
        "pass.started",
        "pass.ended",
        "loop.paused",
        "loop.resumed",
        "loop.engine_selected",
        "loop.rotation_requested",
        "loop.nudge_requested",
    ];

    /// `pass end` is only the mutation that happened to collide. Every one of
    /// them reaches the store through the same call, so every one of them has
    /// to lose the same way: nothing appended, said so first, and named.
    #[test]
    fn every_mutation_that_loses_the_race_says_it_appended_nothing() {
        let log = ProjectLog::new(ConflictStore::new(), "hm", "leader");
        // A fixture rich enough that every mutation gets past its own
        // preconditions and fails only on the append.
        let add = |title: &str| {
            log.add_work(title.to_owned(), None, 0, vec![], vec![])
                .unwrap()
                .1
        };
        let work = add("work");
        let idle = add("idle");
        let asked = add("asked");
        let finished = add("finished");
        let (_, attempt) = log.start(&work, BTreeMap::new()).unwrap();
        let (_, done_attempt) = log.start(&finished, BTreeMap::new()).unwrap();
        log.finish(&finished, Some(done_attempt), false, None)
            .unwrap();
        let (_, question) = log.ask(&asked, "which path?".to_owned()).unwrap();
        let observation = ObservationKey {
            observer: "tmux".to_owned(),
            subject: "worker".to_owned(),
            field: "liveness".to_owned(),
        };
        log.report_observation(observation.clone(), "present".to_owned())
            .unwrap();
        let settled = log.snapshot().unwrap();
        let addition = GraphChangeDocument {
            why: Some("replan".to_owned()),
            add: vec![serde_json::from_value(json!({"title": "added"})).unwrap()],
            edit: vec![],
        };
        let prepared = log
            .allocate_change(&settled, &addition, ChangeMode::AddOnly)
            .unwrap();

        let losing: Vec<(&str, Mutation<'_>)> = vec![
            (
                "observation.reported",
                Box::new(|| {
                    log.report_observation(observation.clone(), "absent".to_owned())
                        .map(|result| match result {
                            ObservationAppend::Appended(result) => *result,
                            ObservationAppend::Unchanged { .. } => {
                                panic!("a changed observation report was unexpectedly quiet")
                            }
                        })
                }),
            ),
            (
                "observation.retired",
                Box::new(|| {
                    log.retire_observation(observation.clone())
                        .map(|result| match result {
                            ObservationAppend::Appended(result) => *result,
                            ObservationAppend::Unchanged { .. } => {
                                panic!("a current observation retirement was unexpectedly quiet")
                            }
                        })
                }),
            ),
            (
                "work.changed",
                Box::new(|| {
                    log.add_work("raced".to_owned(), None, 0, vec![], vec![])
                        .map(|(result, _)| result)
                }),
            ),
            (
                "work.changed",
                Box::new(|| log.commit_change(&settled, &addition, prepared.clone())),
            ),
            (
                "work.changed",
                Box::new(|| {
                    log.set_work_state(
                        &idle,
                        WorkStateChange::Block {
                            reason: "raced".to_owned(),
                        },
                    )
                }),
            ),
            (
                "attempt.started",
                Box::new(|| log.start(&idle, BTreeMap::new()).map(|(result, _)| result)),
            ),
            (
                "attempt.bound",
                Box::new(|| log.bind_attempt(&attempt, "tmux:worker".to_owned(), BTreeMap::new())),
            ),
            (
                "attempt.updated",
                Box::new(|| {
                    log.update_attempt(&attempt, BTreeMap::new(), Some("raced".to_owned()), vec![])
                }),
            ),
            (
                "attempt.ended",
                Box::new(|| {
                    log.end_attempt(&attempt, AttemptOutcome::Cancelled, "raced".to_owned())
                }),
            ),
            (
                "work.finished",
                Box::new(|| log.finish(&work, Some(attempt.clone()), false, None)),
            ),
            (
                "work.dropped",
                Box::new(|| {
                    log.drop_work(
                        &work,
                        Some(attempt.clone()),
                        Some(AttemptOutcome::Cancelled),
                        "raced".to_owned(),
                    )
                }),
            ),
            (
                "work.reopened",
                Box::new(|| log.reopen(&finished, "raced".to_owned())),
            ),
            (
                "question.asked",
                Box::new(|| {
                    log.ask(&work, "another?".to_owned())
                        .map(|(result, _)| result)
                }),
            ),
            (
                "question.answered",
                Box::new(|| log.answer(&question, "path A".to_owned())),
            ),
            (
                "pass.started",
                Box::new(|| {
                    log.wake_loop(
                        "claude".to_owned(),
                        "tmux:alder-leader".to_owned(),
                        vec![PassTrigger::Log],
                    )
                    .map(|(result, _)| result)
                }),
            ),
            (
                "loop.paused",
                Box::new(|| log.pause_loop(Some("raced".to_owned()))),
            ),
            ("loop.resumed", Box::new(|| log.resume_loop())),
            (
                "loop.engine_selected",
                Box::new(|| log.select_engine("codex".to_owned())),
            ),
            (
                "loop.rotation_requested",
                Box::new(|| log.request_rotation(None)),
            ),
            ("loop.nudge_requested", Box::new(|| log.request_nudge(None))),
        ];

        let mut covered = BTreeSet::new();
        log.store().arm(true);
        for (event, mutation) in &losing {
            let error = mutation().unwrap_err();
            assert_eq!(error.code, "head_conflict", "{event}");
            assert_eq!(error.context["appended"], json!(false), "{event}");
            assert_eq!(error.context["event"], json!(event), "{event}");
            assert!(
                error.message.starts_with("nothing was appended: "),
                "{event}: {}",
                error.message
            );
            assert!(error.message.contains(event), "{event}: {}", error.message);
            covered.insert(*event);
        }

        // A pass has to be open for `pass end` to reach its append, and no
        // pass may be open for `loop wake` to reach its own, so this one is
        // raced after the fixture opens a pass for real.
        log.store().arm(false);
        log.wake_loop(
            "claude".to_owned(),
            "tmux:alder-leader".to_owned(),
            vec![PassTrigger::Log],
        )
        .unwrap();
        log.store().arm(true);
        let error = log
            .end_pass(None, PassOutcome::Ok, None, None, false, None)
            .unwrap_err();
        assert_eq!(error.code, "head_conflict");
        assert_eq!(error.context["appended"], json!(false));
        assert_eq!(error.context["event"], json!("pass.ended"));
        covered.insert("pass.ended");

        assert_eq!(
            covered,
            EVERY_EVENT.into_iter().collect::<BTreeSet<_>>(),
            "every event a mutation can append has to be swept"
        );
        // Losing changed nothing: only the one pass the sweep opened for real
        // is on the log, and every raced object is as the fixture left it.
        let after = log.snapshot().unwrap();
        assert_eq!(after.head.sequence(), settled.head.sequence() + 1);
        assert_eq!(
            after.state.work[&work].state,
            settled.state.work[&work].state
        );
        assert_eq!(after.state.work[&idle].state, WorkState::Open);
        assert_eq!(after.state.work[&asked].state, WorkState::Blocked);
        assert_eq!(after.state.work[&finished].state, WorkState::Done);
        assert!(after.state.attempts[&attempt].outcome.is_none());
        assert!(after.state.questions[&question].answer.is_none());
    }

    #[test]
    fn change_allocation_rejects_both_persisted_and_batch_collisions() {
        let log = ProjectLog::new(MemoryStore::new(), "hm", "test");
        let (_, persisted) = log
            .add_work("persisted".to_owned(), None, 0, vec![], vec![])
            .unwrap();
        let snapshot = log.snapshot().unwrap();
        let allocated = vec!["hm-batch".to_owned()];

        assert!(!work_id_available(&snapshot.state, &allocated, &persisted));
        assert!(!work_id_available(&snapshot.state, &allocated, "hm-batch"));
        assert!(work_id_available(&snapshot.state, &allocated, "hm-unused"));
    }
}
