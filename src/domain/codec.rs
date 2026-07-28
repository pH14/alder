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
}
