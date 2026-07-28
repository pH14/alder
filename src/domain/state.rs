use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{AlderError, Result};

use super::{
    Attempt, AttemptCheck, AttemptOutcome, AttemptState, CheckStatus, Event, EventPayload, Handoff,
    HandoffState, LoopControl, Pass, PassState, Question, QuestionAnswer, Work, WorkOperation,
    WorkState, WorkStateChange,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectState {
    pub handoffs: BTreeMap<String, Handoff>,
    pub work: BTreeMap<String, Work>,
    pub attempts: BTreeMap<String, Attempt>,
    pub questions: BTreeMap<String, Question>,
    pub passes: BTreeMap<String, Pass>,
    pub loop_control: LoopControl,
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
                if self.attempts.values().any(|attempt| {
                    attempt.state != AttemptState::Ended
                        && attempt.handle.as_deref() == Some(handle)
                }) {
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
                if !state.is_terminal() {
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
                if work.state.is_terminal() {
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
            EventPayload::PassStarted { pass } => {
                if self.passes.contains_key(&pass.id) {
                    return Err(AlderError::validation(format!(
                        "pass `{}` already exists",
                        pass.id
                    )));
                }
                if let Some(open) = self.open_pass() {
                    return Err(AlderError::with_context(
                        "pass_open",
                        format!("pass `{}` is still open", open.id),
                        json!({"pass_id": open.id, "engine": open.engine}),
                    ));
                }
                require_text("pass engine", &pass.engine)?;
                validate_handle(&pass.handle)?;
                let mut triggers = pass.triggers.clone();
                triggers.sort();
                triggers.dedup();
                self.passes.insert(
                    pass.id.clone(),
                    Pass {
                        id: pass.id.clone(),
                        engine: pass.engine.clone(),
                        handle: pass.handle.clone(),
                        triggers,
                        state: PassState::Open,
                        outcome: None,
                        report: None,
                        wake_at: None,
                        rotate: false,
                        why: None,
                        at_head: pass.at_head,
                        started_at: event.at,
                        started_seq: seq,
                        ended_at: None,
                        ended_seq: None,
                    },
                );
                // A wake consumes any pending rotation simply by being later in
                // the log than its request.
                self.loop_control.last_wake_seq = Some(seq);
            }
            EventPayload::PassEnded {
                pass_id,
                outcome,
                report,
                wake_at,
                rotate,
                why,
            } => {
                let pass = self
                    .passes
                    .get_mut(pass_id)
                    .ok_or_else(|| AlderError::not_found("pass", pass_id))?;
                if pass.state == PassState::Ended {
                    return Err(AlderError::with_context(
                        "pass_ended",
                        format!("pass `{pass_id}` has already ended"),
                        json!({"pass_id": pass_id, "outcome": pass.outcome}),
                    ));
                }
                pass.state = PassState::Ended;
                pass.outcome = Some(*outcome);
                pass.report = report.clone().filter(|report| !report.trim().is_empty());
                pass.wake_at = *wake_at;
                pass.rotate = *rotate;
                pass.why = why.clone().filter(|why| !why.trim().is_empty());
                pass.ended_at = Some(event.at);
                pass.ended_seq = Some(seq);
                if *rotate {
                    self.loop_control.rotate_requested_seq = Some(seq);
                }
            }
            EventPayload::LoopPaused { why } => {
                self.loop_control.paused = true;
                self.loop_control.pause_reason = why.clone().filter(|why| !why.trim().is_empty());
            }
            EventPayload::LoopResumed {} => {
                self.loop_control.paused = false;
                self.loop_control.pause_reason = None;
            }
            EventPayload::LoopEngineSelected { engine } => {
                require_text("engine", engine)?;
                self.loop_control.engine = Some(engine.clone());
            }
            EventPayload::LoopRotationRequested { .. } => {
                self.loop_control.rotate_requested_seq = Some(seq);
            }
            EventPayload::LoopNudgeRequested { .. } => {
                self.loop_control.nudge_requested_seq = Some(seq);
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

    /// The one pass that has not ended, if any. Passes are serialized, so this
    /// is the loop's equivalent of one active attempt per work item.
    pub fn open_pass(&self) -> Option<&Pass> {
        self.passes
            .values()
            .find(|pass| pass.state == PassState::Open)
    }

    /// The most recently ended pass in log order.
    pub fn last_ended_pass(&self) -> Option<&Pass> {
        self.passes
            .values()
            .filter(|pass| pass.state == PassState::Ended)
            .max_by_key(|pass| pass.ended_seq)
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

    /// The terminal work state that has stranded this question, if any.
    ///
    /// A question is actionable only while its work is live. Once the work is
    /// done or dropped there is no requirement left to decide about, so the
    /// question stops asking anyone for anything. Nothing is stored: the
    /// derivation reverses itself when the work is reopened, and answering a
    /// stranded question remains legal because a late ruling is harmless.
    pub fn stranded(&self, question: &Question) -> Option<WorkState> {
        self.work
            .get(&question.work_id)
            .map(|work| work.state)
            .filter(|state| state.is_terminal())
    }

    /// Unanswered questions on one work item, in the order they were asked.
    /// A transition to `done` or `dropped` strands exactly these.
    pub fn unanswered_questions(&self, work_id: &str) -> Vec<String> {
        let mut questions: Vec<_> = self
            .questions
            .values()
            .filter(|question| question.work_id == work_id && question.answer.is_none())
            .collect();
        questions.sort_by_key(|question| question.asked_seq);
        questions
            .into_iter()
            .map(|question| question.id.clone())
            .collect()
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
        let pass_prefix = format!("{prefix}-pass-");
        if let Some(id) = self.passes.keys().find(|id| !id.starts_with(&pass_prefix)) {
            return Err(AlderError::with_context(
                "config_conflict",
                format!("configured prefix `{prefix}` does not match pass `{id}`"),
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
        AttemptDefinition, CheckDefinition, CheckUpdate, EventPayload, HandoffDefinition,
        QuestionDefinition, WorkDefinition, WorkOperation,
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

    #[test]
    fn fold_rejects_malformed_envelopes_before_changing_state() {
        let payload = EventPayload::WorkChanged {
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
    fn handoffs_are_integrated_exactly_once() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                EventPayload::HandoffSubmitted {
                    handoff: HandoffDefinition {
                        id: "hm-handoff-one".to_owned(),
                        title: "handoff".to_owned(),
                        artifact_ref: "branch".to_owned(),
                        note: None,
                    },
                },
            ))
            .unwrap();
        let integration = EventPayload::HandoffIntegrated {
            handoff_id: "hm-handoff-one".to_owned(),
            work: WorkDefinition {
                id: "hm-work".to_owned(),
                title: "work".to_owned(),
                spec: None,
                priority: 0,
                requires: Vec::new(),
                checks: Vec::new(),
            },
        };
        state.apply(&event(2, integration.clone())).unwrap();
        assert_eq!(
            state.handoffs["hm-handoff-one"].state,
            HandoffState::Integrated
        );
        assert_eq!(
            state.apply(&event(3, integration)).unwrap_err().code,
            "invalid_transition"
        );
    }

    #[test]
    fn active_attempt_freezes_every_dependency_and_check_edit() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                EventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &["old"]), add("hm-b", &[], &[])],
                },
            ))
            .unwrap();
        state
            .apply(&event(
                2,
                EventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
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
                    EventPayload::WorkChanged {
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
                EventPayload::WorkChanged {
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
                EventPayload::WorkChanged {
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
                    EventPayload::WorkChanged {
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
                EventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &[]), add("hm-b", &[], &[])],
                },
            ))
            .unwrap();
        state
            .apply(&event(
                2,
                EventPayload::QuestionAsked {
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
                    EventPayload::WorkChanged {
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
            });
        }
        state
            .apply(&event(
                4,
                EventPayload::WorkChanged {
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
                EventPayload::WorkChanged {
                    why: Some("resume".to_owned()),
                    operations: vec![unblock_b],
                },
            ))
            .unwrap();
        assert_eq!(state.work["hm-b"].state, WorkState::Open);

        state
            .apply(&event(
                6,
                EventPayload::QuestionAnswered {
                    question_id: "hm-a-question-1".to_owned(),
                    answer: "A".to_owned(),
                },
            ))
            .unwrap();
        state
            .apply(&event(
                7,
                EventPayload::WorkChanged {
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
                EventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &["test"])],
                },
            ))
            .unwrap();
        state
            .apply(&event(
                2,
                EventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
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
                EventPayload::AttemptUpdated {
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
                    EventPayload::WorkChanged {
                        why: None,
                        operations: vec![add("hm-a", &[], &["test"])],
                    },
                ))
                .unwrap();
            state
                .apply(&event(
                    2,
                    EventPayload::AttemptStarted {
                        attempt: AttemptDefinition {
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
                EventPayload::AttemptUpdated {
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
                EventPayload::AttemptUpdated {
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
                EventPayload::AttemptUpdated {
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
                    EventPayload::AttemptUpdated {
                        attempt_id: "hm-a-attempt-1".to_owned(),
                        metadata: BTreeMap::new(),
                        note: None,
                        checks: vec![],
                    },
                ))
                .unwrap_err()
                .message,
            "an attempt update must change metadata, a note, or a check"
        );
    }

    #[test]
    fn a_handle_held_only_by_ended_attempts_may_be_rebound() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                EventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &[]), add("hm-b", &[], &[])],
                },
            ))
            .unwrap();
        state
            .apply(&event(
                2,
                EventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
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
                EventPayload::AttemptBound {
                    attempt_id: "hm-a-attempt-1".to_owned(),
                    handle: "tmux:worker".to_owned(),
                    metadata: BTreeMap::new(),
                },
            ))
            .unwrap();
        state
            .apply(&event(
                4,
                EventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
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
                EventPayload::AttemptBound {
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
                EventPayload::AttemptEnded {
                    attempt_id: "hm-a-attempt-1".to_owned(),
                    outcome: AttemptOutcome::Failed,
                    why: "worker crashed".to_owned(),
                },
            ))
            .unwrap();
        state
            .apply(&event(
                6,
                EventPayload::AttemptBound {
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
                EventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &["test"])],
                },
            ))
            .unwrap();
        state
            .apply(&event(
                2,
                EventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
                        id: "hm-a-attempt-1".to_owned(),
                        work_id: "hm-a".to_owned(),
                        metadata: BTreeMap::new(),
                    },
                },
            ))
            .unwrap();
        let finish = EventPayload::WorkFinished {
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
                EventPayload::AttemptUpdated {
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
                EventPayload::WorkChanged {
                    why: None,
                    operations: vec![add("hm-a", &[], &[]), add("hm-b", &[], &[])],
                },
            ))
            .unwrap();
        assert!(
            state
                .apply(&event(
                    2,
                    EventPayload::WorkFinished {
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
                EventPayload::AttemptStarted {
                    attempt: AttemptDefinition {
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
                    EventPayload::WorkFinished {
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
                    EventPayload::WorkChanged {
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
                EventPayload::WorkChanged {
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
        state.handoffs.insert(
            "wrong-handoff-one".to_owned(),
            Handoff {
                id: "wrong-handoff-one".to_owned(),
                title: "handoff".to_owned(),
                artifact_ref: "ref".to_owned(),
                note: None,
                state: HandoffState::Submitted,
                submitted_seq: 3,
                work_id: None,
                integrated_seq: None,
            },
        );
        assert!(state.validate_prefix("hm").is_err());

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

        assert_eq!(validate_handle("tmux:worker").unwrap(), ("tmux", "worker"));
        for invalid in ["tmux", "Bad:worker", "tmux:"] {
            assert!(validate_handle(invalid).is_err(), "{invalid}");
        }
    }

    fn wake(id: &str) -> EventPayload {
        EventPayload::PassStarted {
            pass: crate::domain::PassDefinition {
                id: id.to_owned(),
                engine: "claude".to_owned(),
                handle: "tmux:alder-leader".to_owned(),
                triggers: vec![
                    crate::domain::PassTrigger::Log,
                    crate::domain::PassTrigger::Log,
                ],
                at_head: 0,
            },
        }
    }

    fn end(id: &str, rotate: bool) -> EventPayload {
        EventPayload::PassEnded {
            pass_id: id.to_owned(),
            outcome: crate::domain::PassOutcome::Ok,
            report: Some("did the work\nsecond line".to_owned()),
            wake_at: None,
            rotate,
            why: None,
        }
    }

    #[test]
    fn only_one_pass_may_be_open_at_a_time() {
        let mut state = ProjectState::default();
        state.apply(&event(1, wake("hm-pass-1"))).unwrap();
        assert_eq!(state.open_pass().unwrap().id, "hm-pass-1");
        // Triggers are deduplicated so the record reads cleanly.
        assert_eq!(state.passes["hm-pass-1"].triggers.len(), 1);

        assert_eq!(
            state.apply(&event(2, wake("hm-pass-2"))).unwrap_err().code,
            "pass_open"
        );
        assert_eq!(
            state.apply(&event(2, wake("hm-pass-1"))).unwrap_err().code,
            "validation_failed"
        );

        state.apply(&event(2, end("hm-pass-1", false))).unwrap();
        assert!(state.open_pass().is_none());
        assert_eq!(state.last_ended_pass().unwrap().id, "hm-pass-1");
        assert_eq!(
            state.passes["hm-pass-1"].report_line(),
            Some("did the work")
        );
        assert_eq!(
            state
                .apply(&event(3, end("hm-pass-1", false)))
                .unwrap_err()
                .code,
            "pass_ended"
        );
        assert_eq!(
            state
                .apply(&event(3, end("hm-pass-9", false)))
                .unwrap_err()
                .code,
            "not_found"
        );
        state.apply(&event(3, wake("hm-pass-2"))).unwrap();
        assert_eq!(state.open_pass().unwrap().id, "hm-pass-2");
    }

    #[test]
    fn a_pass_keeps_the_reason_it_was_ended_for_and_drops_a_blank_one() {
        let ended_with = |why: Option<&str>| {
            let mut state = ProjectState::default();
            state.apply(&event(1, wake("hm-pass-1"))).unwrap();
            state
                .apply(&event(
                    2,
                    EventPayload::PassEnded {
                        pass_id: "hm-pass-1".to_owned(),
                        outcome: crate::domain::PassOutcome::Timeout,
                        report: None,
                        wake_at: None,
                        rotate: false,
                        why: why.map(ToOwned::to_owned),
                    },
                ))
                .unwrap();
            state.passes["hm-pass-1"].why.clone()
        };

        // A pass the driver had to close itself leaves no report, so the
        // reason it records is the only account of what happened.
        assert_eq!(
            ended_with(Some("the pass exceeded its time budget")),
            Some("the pass exceeded its time budget".to_owned())
        );
        assert_eq!(ended_with(Some("   ")), None);
        assert_eq!(ended_with(None), None);
    }

    #[test]
    fn a_wake_consumes_the_rotation_that_precedes_it() {
        let mut state = ProjectState::default();
        assert!(!state.loop_control.rotate_pending());

        state
            .apply(&event(1, EventPayload::LoopRotationRequested { why: None }))
            .unwrap();
        assert!(state.loop_control.rotate_pending());

        state.apply(&event(2, wake("hm-pass-1"))).unwrap();
        assert!(!state.loop_control.rotate_pending());

        // `pass end --rotate` requests the next rotation the same way.
        state.apply(&event(3, end("hm-pass-1", true))).unwrap();
        assert!(state.loop_control.rotate_pending());
        assert!(state.passes["hm-pass-1"].rotate);

        state.apply(&event(4, wake("hm-pass-2"))).unwrap();
        assert!(!state.loop_control.rotate_pending());
    }

    #[test]
    fn a_wake_consumes_the_nudge_that_precedes_it() {
        let mut state = ProjectState::default();
        assert!(!state.loop_control.nudge_pending());

        state
            .apply(&event(1, EventPayload::LoopNudgeRequested { why: None }))
            .unwrap();
        assert!(state.loop_control.nudge_pending());

        state.apply(&event(2, wake("hm-pass-1"))).unwrap();
        assert!(!state.loop_control.nudge_pending());

        // A nudge after the wake waits for the next one.
        state
            .apply(&event(3, EventPayload::LoopNudgeRequested { why: None }))
            .unwrap();
        assert!(state.loop_control.nudge_pending());
        // A nudge is not a rotation: each is consumed independently.
        assert!(!state.loop_control.rotate_pending());

        // Ending the pass is not a wake, so the nudge stays pending.
        state.apply(&event(4, end("hm-pass-1", false))).unwrap();
        assert!(state.loop_control.nudge_pending());

        state.apply(&event(5, wake("hm-pass-2"))).unwrap();
        assert!(!state.loop_control.nudge_pending());
    }

    #[test]
    fn pause_resume_and_engine_selection_are_last_writer_wins() {
        let mut state = ProjectState::default();
        assert!(!state.loop_control.paused);
        assert_eq!(state.loop_control.engine, None);

        state
            .apply(&event(
                1,
                EventPayload::LoopPaused {
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
                EventPayload::LoopPaused {
                    why: Some(" ".to_owned()),
                },
            ))
            .unwrap();
        assert!(state.loop_control.paused);
        assert_eq!(state.loop_control.pause_reason, None);

        state
            .apply(&event(3, EventPayload::LoopResumed {}))
            .unwrap();
        assert!(!state.loop_control.paused);
        assert_eq!(state.loop_control.pause_reason, None);

        for engine in ["claude", "codex"] {
            state
                .apply(&event(
                    4,
                    EventPayload::LoopEngineSelected {
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
                    EventPayload::LoopEngineSelected {
                        engine: " ".to_owned(),
                    },
                ))
                .unwrap_err()
                .code,
            "validation_failed"
        );
    }

    #[test]
    fn passes_must_carry_a_valid_engine_handle_and_prefix() {
        let mut state = ProjectState::default();
        let invalid_engine = EventPayload::PassStarted {
            pass: crate::domain::PassDefinition {
                id: "hm-pass-1".to_owned(),
                engine: " ".to_owned(),
                handle: "tmux:alder-leader".to_owned(),
                triggers: vec![],
                at_head: 0,
            },
        };
        assert_eq!(
            state
                .clone()
                .apply(&event(1, invalid_engine))
                .unwrap_err()
                .code,
            "validation_failed"
        );
        let invalid_handle = EventPayload::PassStarted {
            pass: crate::domain::PassDefinition {
                id: "hm-pass-1".to_owned(),
                engine: "claude".to_owned(),
                handle: "no-kind".to_owned(),
                triggers: vec![],
                at_head: 0,
            },
        };
        assert_eq!(
            state
                .clone()
                .apply(&event(1, invalid_handle))
                .unwrap_err()
                .code,
            "validation_failed"
        );

        state.apply(&event(1, wake("other-pass-1"))).unwrap();
        assert_eq!(
            state.validate_prefix("hm").unwrap_err().code,
            "config_conflict"
        );
    }

    #[test]
    fn cycle_errors_report_the_actual_cycle() {
        let mut state = ProjectState::default();
        let error = state
            .apply(&event(
                1,
                EventPayload::WorkChanged {
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
}
