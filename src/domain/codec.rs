use serde_json::{Map, Value, json};

use alder_log::{EventType, Record, RecordDraft, RecordId, SchemaId};

use crate::error::{AlderError, Result};

use super::{Event, EventDraft, EventPayload};

/// Encode a typed Alder draft as an opaque log record draft.
pub fn encode_draft(draft: &EventDraft) -> Result<RecordDraft> {
    let value = serde_json::to_value(&draft.payload)?;
    let object = value.as_object().ok_or_else(|| {
        AlderError::new(
            "invalid_event",
            "event payload did not serialize to an object",
        )
    })?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| AlderError::new("invalid_event", "event payload has no type"))?;
    let body = object
        .get("body")
        .cloned()
        .ok_or_else(|| AlderError::new("invalid_event", "event payload has no body"))?;
    Ok(RecordDraft::new(
        RecordId::try_from(draft.id.as_str())?,
        draft.at,
        draft.actor.clone(),
        EventType::try_from(kind)?,
        body,
        SchemaId::try_from(draft.schema.as_str())?,
    )?)
}

/// Decode an opaque log record into Alder's typed work event.
pub fn decode_record(record: &Record) -> Result<Event> {
    let mut object = Map::new();
    object.insert("type".to_owned(), json!(record.kind().as_str()));
    object.insert("body".to_owned(), record.body().clone());
    let payload =
        serde_json::from_value::<EventPayload>(Value::Object(object)).map_err(|error| {
            AlderError::with_context(
                "invalid_event",
                format!(
                    "unsupported or invalid Alder event `{}`: {error}",
                    record.kind()
                ),
                json!({"type": record.kind().as_str()}),
            )
        })?;
    Ok(Event {
        id: record.id().as_str().to_owned(),
        seq: record.sequence(),
        at: record.at(),
        actor: record.actor().to_owned(),
        payload,
        schema: record.schema().as_str().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use alder_log::{Head, Log, MemoryLog, Record};
    use serde_json::json;

    use super::*;

    #[test]
    fn current_work_event_wire_format_round_trips_through_the_opaque_codec() {
        let document = json!({
            "id": "event-one",
            "seq": 1,
            "at": "2026-07-27T12:00:00Z",
            "actor": "tester",
            "type": "work.changed",
            "body": {"why": null, "operations": []},
            "schema": "alder.event.v0"
        });
        let record: Record = serde_json::from_value(document.clone()).unwrap();
        let event = decode_record(&record).unwrap();
        let draft = EventDraft {
            id: event.id,
            at: event.at,
            actor: event.actor,
            payload: event.payload,
            schema: event.schema,
        };
        let log = MemoryLog::new();
        let persisted = log
            .append(&Head::empty(), &encode_draft(&draft).unwrap())
            .unwrap();
        assert_eq!(serde_json::to_value(persisted.record).unwrap(), document);
    }

    #[test]
    fn legacy_pass_events_decode_byte_identically_and_fold_inert() {
        let documents = [
            json!({
                "id": "legacy-wake",
                "seq": 1,
                "at": "2026-07-27T12:00:00Z",
                "actor": "alderd",
                "type": "pass.started",
                "body": {"pass": {"id": "hm-pass-1", "engine": "claude",
                          "handle": "tmux:alder-leader", "triggers": ["log", "due"],
                          "at_head": 0}},
                "schema": "alder.event.v0"
            }),
            json!({
                "id": "legacy-end",
                "seq": 2,
                "at": "2026-07-27T12:05:00Z",
                "actor": "leader",
                "type": "pass.ended",
                "body": {"pass_id": "hm-pass-1", "outcome": "ok",
                          "report": "swept the frontier", "wake_at": "2026-07-27T12:25:00Z",
                          "rotate": false, "why": null},
                "schema": "alder.event.v0"
            }),
        ];

        let mut events = Vec::new();
        for document in documents {
            let record: Record = serde_json::from_value(document.clone()).unwrap();
            let event = decode_record(&record).unwrap();
            // The historical body survives whole: re-encoding the decoded
            // event reproduces the record byte for byte.
            let draft = EventDraft {
                id: event.id.clone(),
                at: event.at,
                actor: event.actor.clone(),
                payload: event.payload.clone(),
                schema: event.schema.clone(),
            };
            let encoded = encode_draft(&draft).unwrap();
            assert_eq!(encoded.body(), record.body());
            events.push(event);
        }
        let state = crate::domain::ProjectState::fold(&events).unwrap();
        assert!(state.work.is_empty());
        assert!(state.loop_control.rotate_requested_seq.is_none());
    }

    /// Representative attempt shapes from the real log, written before the
    /// tier field existed: handles like `tmux:alder-work-…`, engine and
    /// session-provenance metadata stamps, and a metadata-only progress
    /// update. All of them decode and fold; the handle survives verbatim as
    /// opaque text, and the absent tier folds as null.
    #[test]
    fn historical_attempt_events_with_engine_and_session_stamps_still_fold() {
        let documents = [
            json!({
                "id": "old-work",
                "seq": 1,
                "at": "2026-07-30T12:00:00Z",
                "actor": "leader",
                "type": "work.changed",
                "body": {"why": null, "operations": [{"op": "add", "work": {"id": "al-old", "title": "old", "spec": null, "priority": 0, "requires": [], "checks": []}}]},
                "schema": "alder.event.v0"
            }),
            json!({
                "id": "old-start",
                "seq": 2,
                "at": "2026-07-30T12:00:01Z",
                "actor": "alderd",
                "type": "attempt.started",
                "body": {"attempt": {"id": "al-old-attempt-1", "work_id": "al-old", "metadata": {}}},
                "schema": "alder.event.v0"
            }),
            json!({
                "id": "old-bind",
                "seq": 3,
                "at": "2026-07-30T12:00:02Z",
                "actor": "alderd",
                "type": "attempt.bound",
                "body": {"attempt_id": "al-old-attempt-1", "handle": "tmux:alder-work-al-old",
                          "metadata": {"engine": "gpt-5.6-terra", "effort": "high", "tier": "terra"}},
                "schema": "alder.event.v0"
            }),
            json!({
                "id": "old-stamp",
                "seq": 4,
                "at": "2026-07-30T12:00:03Z",
                "actor": "alderd",
                "type": "attempt.updated",
                "body": {"attempt_id": "al-old-attempt-1",
                          "metadata": {"codex-session": "019fb2ef-d507-7201-bc36-79d6d5b82336"},
                          "note": null, "checks": []},
                "schema": "alder.event.v0"
            }),
        ];

        let events = documents
            .into_iter()
            .map(|document| {
                let record: Record = serde_json::from_value(document).unwrap();
                decode_record(&record).unwrap()
            })
            .collect::<Vec<_>>();
        let state = crate::domain::ProjectState::fold(&events).unwrap();

        let attempt = &state.attempts["al-old-attempt-1"];
        assert_eq!(attempt.handle.as_deref(), Some("tmux:alder-work-al-old"));
        // The event predates the tier field, so the fold carries none — and
        // the serialized attempt still says so explicitly.
        assert!(attempt.tier.is_none());
        assert!(
            serde_json::to_value(attempt).unwrap()["tier"].is_null(),
            "tier is an explicit null, not an omitted key"
        );
        assert_eq!(attempt.metadata["engine"], "gpt-5.6-terra");
        assert_eq!(
            attempt.metadata["codex-session"],
            "019fb2ef-d507-7201-bc36-79d6d5b82336"
        );
    }

    #[test]
    fn legacy_handoff_integration_decodes_and_replays_created_work() {
        let documents = [
            json!({
                "id": "legacy-integrate",
                "seq": 1,
                "at": "2026-07-27T12:00:00Z",
                "actor": "tester",
                "type": "handoff.integrated",
                "body": {"handoff_id": "hm-handoff-old", "work": {"id": "hm-old", "title": "old", "spec": null, "priority": 0, "requires": [], "checks": []}},
                "schema": "alder.event.v0"
            }),
            json!({
                "id": "dependent-work",
                "seq": 2,
                "at": "2026-07-27T12:00:00Z",
                "actor": "tester",
                "type": "work.changed",
                "body": {"why": null, "operations": [{"op": "add", "work": {"id": "hm-dependent", "title": "dependent", "spec": null, "priority": 0, "requires": ["hm-old"], "checks": []}}]},
                "schema": "alder.event.v0"
            }),
            json!({
                "id": "legacy-attempt-started",
                "seq": 3,
                "at": "2026-07-27T12:00:00Z",
                "actor": "tester",
                "type": "attempt.started",
                "body": {"attempt": {"id": "hm-old-attempt-1", "work_id": "hm-old", "metadata": {}}},
                "schema": "alder.event.v0"
            }),
            json!({
                "id": "legacy-attempt-updated",
                "seq": 4,
                "at": "2026-07-27T12:00:00Z",
                "actor": "tester",
                "type": "attempt.updated",
                "body": {"attempt_id": "hm-old-attempt-1", "metadata": {}, "note": "working", "checks": []},
                "schema": "alder.event.v0"
            }),
            json!({
                "id": "legacy-work-finished",
                "seq": 5,
                "at": "2026-07-27T12:00:00Z",
                "actor": "tester",
                "type": "work.finished",
                "body": {"work_id": "hm-old", "attempt_id": "hm-old-attempt-1", "external": false, "evidence": null},
                "schema": "alder.event.v0"
            }),
        ];

        let events = documents
            .into_iter()
            .map(|document| {
                let record: Record = serde_json::from_value(document).unwrap();
                decode_record(&record).unwrap()
            })
            .collect::<Vec<_>>();
        let state = crate::domain::ProjectState::fold(&events).unwrap();

        assert_eq!(state.work["hm-old"].state, crate::domain::WorkState::Done);
        assert!(state.work.contains_key("hm-dependent"));
        state.validate_graph().unwrap();
    }
}
