use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{AlderError, Result};

use super::{
    Attempt, AttemptCheck, AttemptOutcome, AttemptState, CheckStatus, Event, EventPayload, Handoff,
    HandoffState, Question, QuestionAnswer, Work, WorkOperation, WorkState, WorkStateChange,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectState {
    pub handoffs: BTreeMap<String, Handoff>,
    pub work: BTreeMap<String, Work>,
    pub attempts: BTreeMap<String, Attempt>,
    pub questions: BTreeMap<String, Question>,
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
            EventPayload::HandoffSubmitted { handoff } => {
                if self.handoffs.contains_key(&handoff.id) {
                    return Err(AlderError::validation(format!(
                        "handoff `{}` already exists",
                        handoff.id
                    )));
                }
                require_text("handoff title", &handoff.title)?;
                require_text("handoff ref", &handoff.artifact_ref)?;
                self.handoffs.insert(
                    handoff.id.clone(),
                    Handoff {
                        id: handoff.id.clone(),
                        title: handoff.title.clone(),
                        artifact_ref: handoff.artifact_ref.clone(),
                        note: handoff.note.clone(),
                        state: HandoffState::Submitted,
                        submitted_seq: seq,
                        work_id: None,
                        integrated_seq: None,
                    },
                );
            }
            EventPayload::HandoffIntegrated { handoff_id, work } => {
                let handoff = self
                    .handoffs
                    .get(handoff_id)
                    .ok_or_else(|| AlderError::not_found("handoff", handoff_id))?;
                if handoff.state != HandoffState::Submitted {
                    return Err(AlderError::with_context(
                        "invalid_transition",
                        format!("handoff `{handoff_id}` is already integrated"),
                        json!({"handoff_id": handoff_id, "state": handoff.state}),
                    ));
                }
                self.add_work(work, seq)?;
                let handoff = self.handoffs.get_mut(handoff_id).expect("checked above");
                handoff.state = HandoffState::Integrated;
                handoff.work_id = Some(work.id.clone());
                handoff.integrated_seq = Some(seq);
                self.validate_graph()?;
            }
            EventPayload::WorkChanged { operations, .. } => {
                if operations.is_empty() {
                    return Err(AlderError::validation(
                        "a work change must contain at least one operation",
                    ));
                }
                let mut targets = BTreeSet::new();
                for operation in operations {
                    if !targets.insert(operation.id().to_owned()) {
                        return Err(AlderError::validation(format!(
                            "work `{}` is targeted more than once",
                            operation.id()
                        )));
                    }
                }
                let mut next = self.clone();
                for operation in operations {
                    match operation {
                        WorkOperation::Add { work } => next.add_work(work, seq)?,
                        WorkOperation::Edit {
                            id,
                            title,
                            spec,
                            priority,
                            add_requires,
                            remove_requires,
                            add_checks,
                            remove_checks,
                            state_change,
                        } => {
                            let has_active = next.active_attempt_for(id).is_some();
                            if has_active
                                && (!add_requires.is_empty()
                                    || !remove_requires.is_empty()
                                    || !add_checks.is_empty()
                                    || !remove_checks.is_empty())
                            {
                                let attempt = next.active_attempt_for(id).expect("checked");
                                return Err(AlderError::with_context(
                                    "active_attempt",
                                    format!(
                                        "dependencies and checks cannot change while `{}` is active",
                                        attempt.id
                                    ),
                                    json!({"work_id": id, "attempt_id": attempt.id}),
                                ));
                            }
                            let has_unanswered = next.questions.values().any(|question| {
                                question.work_id == *id && question.answer.is_none()
                            });
                            let work = next
                                .work
                                .get_mut(id)
                                .ok_or_else(|| AlderError::not_found("work", id))?;
                            if matches!(work.state, WorkState::Done | WorkState::Dropped) {
                                return Err(AlderError::with_context(
                                    "invalid_transition",
                                    format!(
                                        "terminal work `{id}` must be reopened before it is edited"
                                    ),
                                    json!({"work_id": id, "state": work.state}),
                                ));
                            }
                            if let Some(title) = title {
                                require_text("work title", title)?;
                                work.title = title.clone();
                            }
                            if let Some(spec) = spec {
                                work.spec = spec.0.clone();
                            }
                            if let Some(priority) = priority {
                                work.priority = *priority;
                            }
                            for required in remove_requires {
                                work.requires.retain(|candidate| candidate != required);
                            }
                            for required in add_requires {
                                if !work.requires.contains(required) {
                                    work.requires.push(required.clone());
                                }
                            }
                            for key in remove_checks {
                                work.checks.retain(|check| check.key != *key);
                            }
                            for check in add_checks {
                                validate_check(check)?;
                                if work.checks.iter().any(|existing| existing.key == check.key) {
                                    return Err(AlderError::validation(format!(
                                        "check `{}` already exists on `{id}`",
                                        check.key
                                    )));
                                }
                                work.checks.push(check.clone());
                            }
                            if let Some(state_change) = state_change {
                                match state_change {
                                    WorkStateChange::Block { reason } => {
                                        require_text("block reason", reason)?;
                                        if !matches!(
                                            work.state,
                                            WorkState::Open | WorkState::Blocked
                                        ) {
                                            return Err(AlderError::validation(format!(
                                                "work `{id}` cannot be blocked from {:?}",
                                                work.state
                                            )));
                                        }
                                        work.state = WorkState::Blocked;
                                        work.block_reason = Some(reason.clone());
                                    }
                                    WorkStateChange::Unblock { reason } => {
                                        require_text("unblock reason", reason)?;
                                        if work.state != WorkState::Blocked {
                                            return Err(AlderError::validation(format!(
                                                "work `{id}` is not blocked"
                                            )));
                                        }
                                        if has_unanswered {
                                            return Err(AlderError::with_context(
                                                "unanswered_question",
                                                format!("work `{id}` has an unanswered question"),
                                                json!({"work_id": id}),
                                            ));
                                        }
                                        work.state = WorkState::Open;
                                        work.block_reason = None;
                                    }
                                }
                            }
                            work.requires.sort();
                            work.checks.sort_by(|left, right| left.key.cmp(&right.key));
                            work.changed_seq = seq;
                        }
                    }
                }
                next.validate_graph()?;
                *self = next;
            }
            EventPayload::AttemptStarted { attempt } => {
                if self.attempts.contains_key(&attempt.id) {
                    return Err(AlderError::validation(format!(
                        "attempt `{}` already exists",
                        attempt.id
                    )));
                }
                let work = self
                    .work
                    .get(&attempt.work_id)
                    .ok_or_else(|| AlderError::not_found("work", &attempt.work_id))?;
                if !self.is_ready(&attempt.work_id) {
                    return Err(AlderError::with_context(
                        "work_not_ready",
                        format!("work `{}` is not ready", attempt.work_id),
                        json!({"work_id": attempt.work_id}),
                    ));
                }
                let checks = work
                    .checks
                    .iter()
                    .map(|check| {
                        (
                            check.key.clone(),
                            AttemptCheck {
                                key: check.key.clone(),
                                status: CheckStatus::Pending,
                                evidence: None,
                                updated_seq: None,
                            },
                        )
                    })
                    .collect();
                self.attempts.insert(
                    attempt.id.clone(),
                    Attempt {
                        id: attempt.id.clone(),
                        work_id: attempt.work_id.clone(),
                        state: AttemptState::Starting,
                        outcome: None,
                        handle: None,
                        metadata: attempt.metadata.clone(),
                        note: None,
                        started_seq: seq,
                        bound_seq: None,
                        updated_seq: seq,
                        ended_seq: None,
                        checks,
                    },
                );
            }
            EventPayload::AttemptBound {
                attempt_id,
                handle,
                metadata,
            } => {
                validate_handle(handle)?;
                if self
                    .attempts
                    .values()
                    .any(|attempt| attempt.handle.as_deref() == Some(handle))
                {
                    return Err(AlderError::validation(format!(
                        "handle `{handle}` is already attached"
                    )));
                }
                let attempt = self.active_attempt_mut(attempt_id)?;
                if attempt.handle.is_some() {
                    return Err(AlderError::with_context(
                        "handle_already_bound",
                        format!("attempt `{attempt_id}` already has a handle"),
                        json!({"attempt_id": attempt_id, "handle": attempt.handle}),
                    ));
                }
                attempt.handle = Some(handle.clone());
                attempt.state = AttemptState::Active;
                attempt.bound_seq = Some(seq);
                attempt.updated_seq = seq;
                attempt.metadata.extend(metadata.clone());
            }
            EventPayload::AttemptUpdated {
                attempt_id,
                metadata,
                note,
                checks,
            } => {
                let attempt = self.active_attempt_mut(attempt_id)?;
                if metadata.is_empty() && note.is_none() && checks.is_empty() {
                    return Err(AlderError::validation(
                        "an attempt update must change metadata, a note, or a check",
                    ));
                }
                for update in checks {
                    require_text("check evidence", &update.evidence)?;
                    let check = attempt.checks.get_mut(&update.key).ok_or_else(|| {
                        AlderError::with_context(
                            "unknown_check",
                            format!("attempt `{attempt_id}` has no check named `{}`", update.key),
                            json!({"attempt_id": attempt_id, "check": update.key}),
                        )
                    })?;
                    check.status = update.status;
                    check.evidence = Some(update.evidence.clone());
                    check.updated_seq = Some(seq);
                }
                attempt.metadata.extend(metadata.clone());
                if let Some(note) = note {
                    require_text("attempt note", note)?;
                    attempt.note = Some(note.clone());
                }
                attempt.updated_seq = seq;
                if attempt.state == AttemptState::Starting {
                    attempt.state = AttemptState::Active;
                }
            }
            EventPayload::AttemptEnded {
                attempt_id,
                outcome,
                why,
            } => {
                require_text("attempt end reason", why)?;
                if !outcome.is_non_success() {
                    return Err(AlderError::validation(
                        "successful attempts are ended by finishing their work",
                    ));
                }
                let attempt = self.active_attempt_mut(attempt_id)?;
                attempt.state = AttemptState::Ended;
                attempt.outcome = Some(*outcome);
                attempt.ended_seq = Some(seq);
                attempt.updated_seq = seq;
            }
            EventPayload::WorkFinished {
                work_id,
                attempt_id,
                external,
                evidence,
            } => {
                let work_state = self
                    .work
                    .get(work_id)
                    .ok_or_else(|| AlderError::not_found("work", work_id))?
                    .state;
                if !matches!(work_state, WorkState::Open | WorkState::Blocked) {
                    return Err(AlderError::with_context(
                        "invalid_transition",
                        format!("work `{work_id}` cannot be finished from {work_state:?}"),
                        json!({"work_id": work_id, "state": work_state}),
                    ));
                }
                if *external {
                    require_text(
                        "external completion evidence",
                        evidence.as_deref().unwrap_or(""),
                    )?;
                    if attempt_id.is_some() || self.active_attempt_for(work_id).is_some() {
                        return Err(AlderError::validation(
                            "external completion requires no active attempt",
                        ));
                    }
                } else {
                    if work_state != WorkState::Open {
                        return Err(AlderError::validation(
                            "blocked work may only be finished with external evidence",
                        ));
                    }
                    let attempt_id = attempt_id.as_deref().ok_or_else(|| {
                        AlderError::validation("ordinary completion requires --attempt")
                    })?;
                    let active = self.active_attempt_for(work_id).ok_or_else(|| {
                        AlderError::validation(format!("work `{work_id}` has no active attempt"))
                    })?;
                    if active.id != attempt_id {
                        return Err(AlderError::with_context(
                            "attempt_mismatch",
                            format!("`{attempt_id}` is not the active attempt for `{work_id}`"),
                            json!({"work_id": work_id, "attempt_id": attempt_id, "active_attempt_id": active.id}),
                        ));
                    }
                    let incomplete: Vec<_> = active
                        .checks
                        .values()
                        .filter(|check| check.status != CheckStatus::Satisfied)
                        .map(|check| check.key.clone())
                        .collect();
                    if !incomplete.is_empty() {
                        return Err(AlderError::with_context(
                            "incomplete_checks",
                            format!("work `{work_id}` has incomplete checks"),
                            json!({"work_id": work_id, "attempt_id": attempt_id, "checks": incomplete}),
                        ));
                    }
                    let attempt = self.active_attempt_mut(attempt_id)?;
                    attempt.state = AttemptState::Ended;
                    attempt.outcome = Some(AttemptOutcome::Succeeded);
                    attempt.ended_seq = Some(seq);
                    attempt.updated_seq = seq;
                }
                let work = self.work.get_mut(work_id).expect("checked above");
                work.state = WorkState::Done;
                work.block_reason = None;
                work.outcome = Some(if *external {
                    format!("external: {}", evidence.as_deref().unwrap_or_default())
                } else {
                    "succeeded".to_owned()
                });
                work.changed_seq = seq;
            }
            EventPayload::WorkDropped {
                work_id,
                attempt_id,
                outcome,
                why,
            } => {
                require_text("drop reason", why)?;
                let work = self
                    .work
                    .get(work_id)
                    .ok_or_else(|| AlderError::not_found("work", work_id))?;
                if !matches!(work.state, WorkState::Open | WorkState::Blocked) {
                    return Err(AlderError::with_context(
                        "invalid_transition",
                        format!("work `{work_id}` cannot be dropped from {:?}", work.state),
                        json!({"work_id": work_id, "state": work.state}),
                    ));
                }
                let active_id = self
                    .active_attempt_for(work_id)
                    .map(|attempt| attempt.id.clone());
                match (active_id.as_deref(), attempt_id.as_deref(), outcome) {
                    (Some(active), Some(provided), Some(outcome)) => {
                        if active != provided {
                            return Err(AlderError::with_context(
                                "attempt_mismatch",
                                format!("`{provided}` is not the active attempt for `{work_id}`"),
                                json!({"work_id": work_id, "attempt_id": provided, "active_attempt_id": active}),
                            ));
                        }
                        if !outcome.is_non_success() {
                            return Err(AlderError::validation(
                                "drop requires a non-success attempt outcome",
                            ));
                        }
                        let attempt = self.active_attempt_mut(provided)?;
                        attempt.state = AttemptState::Ended;
                        attempt.outcome = Some(*outcome);
                        attempt.ended_seq = Some(seq);
                        attempt.updated_seq = seq;
                    }
                    (Some(active), _, _) => {
                        return Err(AlderError::with_context(
                            "active_attempt",
                            format!("dropping `{work_id}` requires its active attempt and outcome"),
                            json!({"work_id": work_id, "active_attempt_id": active}),
                        ));
                    }
                    (None, None, None) => {}
                    (None, _, _) => {
                        return Err(AlderError::validation(
                            "attempt fields are not accepted when work has no active attempt",
                        ));
                    }
                }
                self.reject_active_downstream(work_id)?;
                let work = self.work.get_mut(work_id).expect("checked above");
                work.state = WorkState::Dropped;
                work.block_reason = None;
                work.outcome = Some(why.clone());
                work.changed_seq = seq;
            }
            EventPayload::WorkReopened { work_id, why } => {
                require_text("reopen reason", why)?;
                let state = self
                    .work
                    .get(work_id)
                    .ok_or_else(|| AlderError::not_found("work", work_id))?
                    .state;
                if !matches!(state, WorkState::Done | WorkState::Dropped) {
                    return Err(AlderError::with_context(
                        "invalid_transition",
                        format!("work `{work_id}` is not terminal"),
                        json!({"work_id": work_id, "state": state}),
                    ));
                }
                self.reject_active_downstream(work_id)?;
                let work = self.work.get_mut(work_id).expect("checked above");
                work.state = WorkState::Open;
                work.outcome = None;
                work.block_reason = None;
                work.changed_seq = seq;
            }
            EventPayload::QuestionAsked { question } => {
                require_text("question", &question.text)?;
                if self.questions.contains_key(&question.id) {
                    return Err(AlderError::validation(format!(
                        "question `{}` already exists",
                        question.id
                    )));
                }
                let work = self
                    .work
                    .get_mut(&question.work_id)
                    .ok_or_else(|| AlderError::not_found("work", &question.work_id))?;
                if !matches!(work.state, WorkState::Open | WorkState::Blocked) {
                    return Err(AlderError::validation(format!(
                        "questions cannot be asked against terminal work `{}`",
                        question.work_id
                    )));
                }
                if work.state == WorkState::Open {
                    work.state = WorkState::Blocked;
                    work.block_reason = Some(format!("question {}", question.id));
                }
                work.changed_seq = seq;
                self.questions.insert(
                    question.id.clone(),
                    Question {
                        id: question.id.clone(),
                        work_id: question.work_id.clone(),
                        text: question.text.clone(),
                        answer: None,
                        asked_seq: seq,
                        answered_seq: None,
                        answered_by: None,
                        answers: Vec::new(),
                    },
                );
            }
            EventPayload::QuestionAnswered {
                question_id,
                answer,
            } => {
                require_text("answer", answer)?;
                let question = self
                    .questions
                    .get_mut(question_id)
                    .ok_or_else(|| AlderError::not_found("question", question_id))?;
                question.answer = Some(answer.clone());
                question.answered_seq = Some(seq);
                question.answered_by = Some(event.actor.clone());
                question.answers.push(QuestionAnswer {
                    answer: answer.clone(),
                    seq,
                    actor: event.actor.clone(),
                });
            }
        }
        Ok(())
    }

    fn add_work(&mut self, definition: &super::WorkDefinition, seq: u64) -> Result<()> {
        if self.work.contains_key(&definition.id) {
            return Err(AlderError::validation(format!(
                "work `{}` already exists",
                definition.id
            )));
        }
        require_text("work title", &definition.title)?;
        let mut requires = definition.requires.clone();
        requires.sort();
        if requires.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(AlderError::validation(format!(
                "work `{}` contains duplicate dependencies",
                definition.id
            )));
        }
        let mut checks = definition.checks.clone();
        checks.sort_by(|left, right| left.key.cmp(&right.key));
        for check in &checks {
            validate_check(check)?;
        }
        if checks.windows(2).any(|pair| pair[0].key == pair[1].key) {
            return Err(AlderError::validation(format!(
                "work `{}` contains duplicate check keys",
                definition.id
            )));
        }
        self.work.insert(
            definition.id.clone(),
            Work {
                id: definition.id.clone(),
                title: definition.title.clone(),
                spec: definition.spec.clone(),
                priority: definition.priority,
                state: WorkState::Open,
                block_reason: None,
                outcome: None,
                opened_seq: seq,
                changed_seq: seq,
                requires,
                checks,
            },
        );
        Ok(())
    }

    pub fn validate_graph(&self) -> Result<()> {
        for work in self.work.values() {
            for required in &work.requires {
                if !self.work.contains_key(required) {
                    return Err(AlderError::with_context(
                        "missing_dependency",
                        format!("work `{}` requires missing work `{required}`", work.id),
                        json!({"work_id": work.id, "required_id": required}),
                    ));
                }
                if required == &work.id {
                    return Err(AlderError::with_context(
                        "dependency_cycle",
                        format!("work `{}` cannot depend on itself", work.id),
                        json!({"cycle": [work.id.clone(), work.id.clone()]}),
                    ));
                }
            }
        }
        let mut color: BTreeMap<&str, u8> = BTreeMap::new();
        let mut stack = Vec::new();
        for id in self.work.keys() {
            if color.get(id.as_str()).copied().unwrap_or_default() == 0 {
                self.visit(id, &mut color, &mut stack)?;
            }
        }
        Ok(())
    }

    fn visit<'a>(
        &'a self,
        id: &'a str,
        color: &mut BTreeMap<&'a str, u8>,
        stack: &mut Vec<&'a str>,
    ) -> Result<()> {
        color.insert(id, 1);
        stack.push(id);
        let work = self.work.get(id).expect("graph keys originate from work");
        for required in &work.requires {
            match color.get(required.as_str()).copied().unwrap_or_default() {
                0 => self.visit(required, color, stack)?,
                1 => {
                    let start = stack
                        .iter()
                        .position(|candidate| *candidate == required)
                        .unwrap_or_default();
                    let mut cycle: Vec<_> = stack[start..]
                        .iter()
                        .map(|value| value.to_string())
                        .collect();
                    cycle.push(required.clone());
                    return Err(AlderError::with_context(
                        "dependency_cycle",
                        "the work graph contains a dependency cycle",
                        json!({"cycle": cycle}),
                    ));
                }
                _ => {}
            }
        }
        stack.pop();
        color.insert(id, 2);
        Ok(())
    }

    pub fn active_attempt_for(&self, work_id: &str) -> Option<&Attempt> {
        self.attempts.values().find(|attempt| {
            attempt.work_id == work_id
                && matches!(attempt.state, AttemptState::Starting | AttemptState::Active)
        })
    }

    fn active_attempt_mut(&mut self, attempt_id: &str) -> Result<&mut Attempt> {
        let attempt = self
            .attempts
            .get_mut(attempt_id)
            .ok_or_else(|| AlderError::not_found("attempt", attempt_id))?;
        if attempt.state == AttemptState::Ended {
            return Err(AlderError::with_context(
                "attempt_ended",
                format!("attempt `{attempt_id}` has ended"),
                json!({"attempt_id": attempt_id, "outcome": attempt.outcome}),
            ));
        }
        Ok(attempt)
    }

    pub fn is_ready(&self, work_id: &str) -> bool {
        let Some(work) = self.work.get(work_id) else {
            return false;
        };
        work.state == WorkState::Open
            && self.active_attempt_for(work_id).is_none()
            && work.requires.iter().all(|required| {
                self.work
                    .get(required)
                    .is_some_and(|dependency| dependency.state == WorkState::Done)
            })
    }

    pub fn ready(&self) -> Vec<&Work> {
        let mut ready: Vec<_> = self
            .work
            .values()
            .filter(|work| self.is_ready(&work.id))
            .collect();
        ready.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.opened_seq.cmp(&right.opened_seq))
                .then_with(|| left.id.cmp(&right.id))
        });
        ready
    }

    pub fn downstream(&self, work_id: &str) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([work_id.to_owned()]);
        while let Some(required) = queue.pop_front() {
            for work in self.work.values() {
                if work.requires.contains(&required) && seen.insert(work.id.clone()) {
                    queue.push_back(work.id.clone());
                }
            }
        }
        seen.into_iter().collect()
    }

    fn reject_active_downstream(&self, work_id: &str) -> Result<()> {
        let attempts: Vec<_> = self
            .downstream(work_id)
            .iter()
            .filter_map(|id| self.active_attempt_for(id))
            .map(|attempt| attempt.id.clone())
            .collect();
        if attempts.is_empty() {
            Ok(())
        } else {
            Err(AlderError::with_context(
                "active_downstream",
                format!("changing `{work_id}` would invalidate active downstream work"),
                json!({"work_id": work_id, "active_attempts": attempts}),
            ))
        }
    }

    pub fn validate_prefix(&self, prefix: &str) -> Result<()> {
        let work_prefix = format!("{prefix}-");
        let handoff_prefix = format!("{prefix}-handoff-");
        if let Some(id) = self.work.keys().find(|id| !id.starts_with(&work_prefix)) {
            return Err(AlderError::with_context(
                "config_conflict",
                format!("configured prefix `{prefix}` does not match work `{id}`"),
                json!({"prefix": prefix, "id": id}),
            ));
        }
        if let Some(id) = self
            .handoffs
            .keys()
            .find(|id| !id.starts_with(&handoff_prefix))
        {
            return Err(AlderError::with_context(
                "config_conflict",
                format!("configured prefix `{prefix}` does not match handoff `{id}`"),
                json!({"prefix": prefix, "id": id}),
            ));
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

fn validate_check(check: &super::CheckDefinition) -> Result<()> {
    require_text("check key", &check.key)?;
    require_text("check description", &check.description)?;
    if check.key.contains(char::is_whitespace) || check.key.contains(':') {
        return Err(AlderError::validation(format!(
            "check key `{}` contains an invalid character",
            check.key
        )));
    }
    Ok(())
}

pub fn validate_handle(handle: &str) -> Result<(&str, &str)> {
    let (kind, value) = handle.split_once(':').ok_or_else(|| {
        AlderError::validation(format!(
            "handle `{handle}` must have the form <kind>:<value>"
        ))
    })?;
    if !valid_name(kind) || value.is_empty() {
        return Err(AlderError::validation(format!(
            "handle `{handle}` must have a valid kind and non-empty value"
        )));
    }
    Ok((kind, value))
}

pub fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
        && value
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        && value
            .chars()
            .last()
            .is_some_and(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;

    use super::*;
    use crate::domain::{
        AttemptDefinition, CheckDefinition, EventPayload, WorkDefinition, WorkOperation,
    };

    fn event(seq: u64, payload: EventPayload) -> Event {
        Event {
            id: format!("event-{seq}"),
            seq,
            at: Utc::now(),
            actor: "test".to_owned(),
            payload,
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

    #[test]
    fn readiness_and_attempt_checks_are_folded() {
        let events = vec![
            event(
                1,
                EventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &["tests"]), add("hm-b", &["hm-a"], &[])],
                },
            ),
            event(
                2,
                EventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
                        id: "hm-a-attempt-1".to_owned(),
                        work_id: "hm-a".to_owned(),
                        metadata: BTreeMap::new(),
                    },
                },
            ),
        ];
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
                EventPayload::WorkChanged {
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
                EventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &[]), add("hm-b", &["hm-a"], &[])],
                },
            ),
            event(
                2,
                EventPayload::WorkDropped {
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
}
