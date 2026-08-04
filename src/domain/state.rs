use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{AlderError, Result};

use chrono::{DateTime, Utc};

use super::{
    Attempt, AttemptCheck, AttemptOutcome, AttemptState, CheckStatus, Event, EventPayload,
    LoopControl, Observation, ObservationKey, Question, QuestionAnswer, Work, WorkOperation,
    WorkState, WorkStateChange,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectState {
    #[serde(with = "observation_map")]
    pub observations: BTreeMap<ObservationKey, Observation>,
    pub work: BTreeMap<String, Work>,
    pub attempts: BTreeMap<String, Attempt>,
    pub questions: BTreeMap<String, Question>,
    pub loop_control: LoopControl,
}

mod observation_map {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};

    use super::{Observation, ObservationKey};

    pub fn serialize<S>(
        observations: &BTreeMap<ObservationKey, Observation>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        observations
            .values()
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<BTreeMap<ObservationKey, Observation>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let observations = Vec::<Observation>::deserialize(deserializer)?;
        let mut folded = BTreeMap::new();
        for observation in observations {
            if folded
                .insert(observation.key.clone(), observation)
                .is_some()
            {
                return Err(D::Error::custom("duplicate observation key"));
            }
        }
        Ok(folded)
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
            EventPayload::ObservationReported { observation } => {
                validate_observation_key(&observation.key)?;
                require_text("observation level", &observation.level)?;
                self.observations.insert(
                    observation.key.clone(),
                    Observation {
                        key: observation.key.clone(),
                        level: observation.level.clone(),
                        reported_seq: seq,
                    },
                );
            }
            EventPayload::ObservationRetired { key } => {
                validate_observation_key(key)?;
                if self.observations.remove(key).is_none() {
                    return Err(AlderError::with_context(
                        "not_found",
                        "observation key is not current",
                        json!({
                            "observer": key.observer,
                            "subject": key.subject,
                            "field": key.field,
                        }),
                    ));
                }
            }
            EventPayload::LegacyHandoffSubmitted { .. }
            | EventPayload::LegacyHandoffWithdrawn { .. } => {}
            EventPayload::LegacyHandoffIntegrated { work, .. } => {
                self.add_work(work, seq)?;
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
                                    WorkStateChange::Block { reason, until } => {
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
                                        // The latest block's statement wins
                                        // whole: a re-block without a deadline
                                        // clears the previous one.
                                        work.block_until = *until;
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
                                        work.block_until = None;
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
                if let Some(tier) = attempt.tier.as_deref() {
                    require_text("attempt tier", tier)?;
                }
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
                        tier: attempt.tier.clone(),
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
                require_text("attempt handle", handle)?;
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
                tier,
                metadata,
                note,
                checks,
            } => {
                if let Some(tier) = tier.as_deref() {
                    require_text("attempt tier", tier)?;
                }
                let attempt = self.active_attempt_mut(attempt_id)?;
                if tier.is_none() && metadata.is_empty() && note.is_none() && checks.is_empty() {
                    return Err(AlderError::validation(
                        "an attempt update must change a tier, metadata, a note, or a check",
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
                if let Some(tier) = tier {
                    attempt.tier = Some(tier.clone());
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
                work.block_until = None;
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
                work.block_until = None;
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
                let surviving = self.unanswered_questions(work_id).into_iter().next();
                let work = self.work.get_mut(work_id).expect("checked above");
                work.outcome = None;
                work.block_until = None;
                match surviving {
                    Some(question_id) => {
                        work.state = WorkState::Blocked;
                        work.block_reason = Some(format!("question {question_id}"));
                    }
                    None => {
                        work.state = WorkState::Open;
                        work.block_reason = None;
                    }
                }
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
            // Passes were run records of the loop reading its own log. They
            // are inert history now: the fold gives them no state, and no
            // append path can produce a new one.
            EventPayload::LegacyPassStarted(_) => {}
            EventPayload::LegacyPassEnded(body) => {
                // A historical `pass end --rotate` was also a rotation
                // request, and that half of the event was a statement about
                // the loop rather than about the pass, so it still folds.
                if body.get("rotate").and_then(serde_json::Value::as_bool) == Some(true) {
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
                block_until: None,
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

    /// The earliest review deadline any blocked work item carries. A deferral
    /// is a statement on the work item — `work block --until` — and this is
    /// its one derived rendezvous: the driver wakes the leader at this time,
    /// and the leader reviews whatever demanded the deferral.
    pub fn next_review_at(&self) -> Option<DateTime<Utc>> {
        self.work
            .values()
            .filter(|work| work.state == WorkState::Blocked)
            .filter_map(|work| work.block_until)
            .min()
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
        if let Some(id) = self.work.keys().find(|id| !id.starts_with(&work_prefix)) {
            return Err(AlderError::with_context(
                "config_conflict",
                format!("configured prefix `{prefix}` does not match work `{id}`"),
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

pub fn validate_observation_key(key: &ObservationKey) -> Result<()> {
    if !valid_name(&key.observer) {
        return Err(AlderError::validation(format!(
            "observation observer `{}` is not a valid name",
            key.observer
        )));
    }
    if !valid_name(&key.field) {
        return Err(AlderError::validation(format!(
            "observation field `{}` is not a valid name",
            key.field
        )));
    }
    require_text("observation subject", &key.subject)?;
    if key.subject.contains('\0') {
        return Err(AlderError::validation(
            "observation subject cannot contain a NUL character",
        ));
    }
    Ok(())
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
        AttemptDefinition, CheckDefinition, CheckUpdate, EventPayload, LegacyHandoffDefinition,
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
    fn legacy_handoff_submission_and_withdrawal_are_inert_history() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                EventPayload::LegacyHandoffSubmitted {
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
                EventPayload::LegacyHandoffWithdrawn {
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
                until: None,
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
                EventPayload::AttemptUpdated {
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
                EventPayload::AttemptUpdated {
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
                EventPayload::AttemptUpdated {
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
                EventPayload::AttemptUpdated {
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
                    EventPayload::AttemptUpdated {
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
                        tier: None,
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
            .apply(&event(1, EventPayload::LoopRotationRequested { why: None }))
            .unwrap();
        state
            .apply(&event(2, EventPayload::LoopNudgeRequested { why: None }))
            .unwrap();
        assert_eq!(state.loop_control.rotate_requested_seq, Some(1));
        assert_eq!(state.loop_control.nudge_requested_seq, Some(2));

        // A later request replaces the recorded sequence; whether either has
        // been acted on is each driver's machine-local knowledge, not a fold
        // fact.
        state
            .apply(&event(3, EventPayload::LoopRotationRequested { why: None }))
            .unwrap();
        assert_eq!(state.loop_control.rotate_requested_seq, Some(3));
        assert_eq!(state.loop_control.nudge_requested_seq, Some(2));
    }

    fn block_until(id: &str, until: Option<&str>) -> EventPayload {
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
        EventPayload::WorkChanged {
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
                EventPayload::WorkChanged {
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
                EventPayload::WorkChanged {
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

    #[test]
    fn observation_picture_serializes_as_an_ordered_list_not_a_synthetic_key() {
        let mut state = ProjectState::default();
        state
            .apply(&event(
                1,
                EventPayload::ObservationReported {
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
