use crate::LogError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::{fmt, str::FromStr};

/// Maximum UTF-8 byte length of a record ID, event type, schema, or actor.
pub const MAX_TEXT_BYTES: usize = 256;
/// Maximum serialized UTF-8 byte length of a record body.
pub const MAX_BODY_BYTES: usize = 1_048_576;

/// A validated, Git-path-safe record identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordId(String);
/// A validated opaque event type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventType(String);
/// A validated opaque event schema identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaId(String);

macro_rules! validated_string {
    ($name:ident, $validate:ident) => {
        impl $name {
            /// Construct a validated value.
            pub fn new(value: impl Into<String>) -> Result<Self, LogError> {
                let value = value.into();
                $validate(&value)?;
                Ok(Self(value))
            }
            /// Return the validated text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl TryFrom<String> for $name {
            type Error = LogError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
        impl TryFrom<&str> for $name {
            type Error = LogError;
            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
        impl FromStr for $name {
            type Err = LogError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }
        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}
validated_string!(RecordId, validate_record_id);
validated_string!(EventType, validate_identifier);
validated_string!(SchemaId, validate_identifier);

/// A pinned, immutable position in a log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Head {
    revision: Option<String>,
    #[serde(rename = "seq")]
    sequence: u64,
}
impl Head {
    /// The head of an empty log.
    pub fn empty() -> Self {
        Self {
            revision: None,
            sequence: 0,
        }
    }
    /// Build a valid head from its serialized parts.
    pub fn try_from_parts(sequence: u64, revision: Option<String>) -> Result<Self, LogError> {
        match (sequence, revision) {
            (0, None) => Ok(Self::empty()),
            (0, Some(_)) => Err(invalid_head("an empty head cannot have a revision")),
            (_, None) => Err(invalid_head("a non-empty head requires a revision")),
            (_, Some(revision))
                if revision.is_empty()
                    || revision.len() > MAX_TEXT_BYTES
                    || invalid_control(&revision) =>
            {
                Err(invalid_head("head revision is invalid"))
            }
            (_, Some(revision)) => Ok(Self {
                revision: Some(revision),
                sequence,
            }),
        }
    }
    /// The number of records included in this head.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    /// The revision, if the log is non-empty.
    pub fn revision(&self) -> Option<&str> {
        self.revision.as_deref()
    }
    /// Whether this is the empty head.
    pub fn is_empty(&self) -> bool {
        self.sequence == 0
    }
}
impl<'de> Deserialize<'de> for Head {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            revision: Option<String>,
            #[serde(rename = "seq")]
            sequence: u64,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::try_from_parts(wire.sequence, wire.revision).map_err(serde::de::Error::custom)
    }
}

/// A persisted opaque record envelope.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Record {
    id: RecordId,
    #[serde(rename = "seq")]
    sequence: u64,
    at: DateTime<Utc>,
    actor: String,
    #[serde(rename = "type")]
    kind: EventType,
    body: Value,
    schema: SchemaId,
}
impl Record {
    pub(crate) fn materialize(draft: &RecordDraft, sequence: u64) -> Self {
        Self {
            id: draft.id.clone(),
            sequence,
            at: draft.at,
            actor: draft.actor.clone(),
            kind: draft.kind.clone(),
            body: draft.body.clone(),
            schema: draft.schema.clone(),
        }
    }
    fn try_from_parts(
        id: RecordId,
        sequence: u64,
        at: DateTime<Utc>,
        actor: String,
        kind: EventType,
        body: Value,
        schema: SchemaId,
    ) -> Result<Self, LogError> {
        if sequence == 0 {
            return Err(invalid_record("record sequence must be positive"));
        }
        validate_actor(&actor)?;
        validate_body(&body)?;
        Ok(Self {
            id,
            sequence,
            at,
            actor,
            kind,
            body,
            schema,
        })
    }
    /// The caller-supplied stable record ID.
    pub fn id(&self) -> &RecordId {
        &self.id
    }
    /// The store-assigned contiguous sequence number.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    /// The caller-supplied timestamp.
    pub fn at(&self) -> DateTime<Utc> {
        self.at
    }
    /// The caller-supplied actor.
    pub fn actor(&self) -> &str {
        &self.actor
    }
    /// The opaque event type.
    pub fn kind(&self) -> &EventType {
        &self.kind
    }
    /// The opaque JSON body.
    pub fn body(&self) -> &Value {
        &self.body
    }
    /// The opaque schema ID.
    pub fn schema(&self) -> &SchemaId {
        &self.schema
    }
    pub(crate) fn matches_draft(&self, draft: &RecordDraft) -> bool {
        self.id == draft.id
            && self.at == draft.at
            && self.actor == draft.actor
            && self.kind == draft.kind
            && self.body == draft.body
            && self.schema == draft.schema
    }
}
impl<'de> Deserialize<'de> for Record {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            id: RecordId,
            #[serde(rename = "seq")]
            sequence: u64,
            at: DateTime<Utc>,
            actor: String,
            #[serde(rename = "type")]
            kind: EventType,
            body: Value,
            schema: SchemaId,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::try_from_parts(
            wire.id,
            wire.sequence,
            wire.at,
            wire.actor,
            wire.kind,
            wire.body,
            wire.schema,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// A validated, unsequenced record submitted to a log.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordDraft {
    id: RecordId,
    at: DateTime<Utc>,
    actor: String,
    kind: EventType,
    body: Value,
    schema: SchemaId,
}
impl RecordDraft {
    /// Construct a draft. Callers retain this exact draft for retries.
    pub fn new(
        id: RecordId,
        at: DateTime<Utc>,
        actor: impl Into<String>,
        kind: EventType,
        body: Value,
        schema: SchemaId,
    ) -> Result<Self, LogError> {
        let actor = actor.into();
        validate_actor(&actor)?;
        validate_body(&body)?;
        Ok(Self {
            id,
            at,
            actor,
            kind,
            body,
            schema,
        })
    }
    /// The stable record ID.
    pub fn id(&self) -> &RecordId {
        &self.id
    }
    /// The timestamp.
    pub fn at(&self) -> DateTime<Utc> {
        self.at
    }
    /// The actor.
    pub fn actor(&self) -> &str {
        &self.actor
    }
    /// The opaque event type.
    pub fn kind(&self) -> &EventType {
        &self.kind
    }
    /// The opaque JSON body.
    pub fn body(&self) -> &Value {
        &self.body
    }
    /// The opaque schema ID.
    pub fn schema(&self) -> &SchemaId {
        &self.schema
    }
}
fn invalid_record(message: impl Into<String>) -> LogError {
    LogError::InvalidRecord {
        message: message.into(),
    }
}
fn invalid_head(message: impl Into<String>) -> LogError {
    LogError::InvalidHead {
        message: message.into(),
    }
}
fn validate_record_id(value: &str) -> Result<(), LogError> {
    if value.is_empty()
        || value.len() > MAX_TEXT_BYTES
        || value == "."
        || value == ".."
        || value.contains("..")
        // The alphabet below excludes both path separators, so repeating
        // their checks would add an unobservable branch without more safety.
        || value
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        Err(invalid_record(
            "record ID must be a non-empty Git filename component of at most 256 ASCII characters",
        ))
    } else {
        Ok(())
    }
}
fn validate_identifier(value: &str) -> Result<(), LogError> {
    if value.is_empty()
        || value.len() > MAX_TEXT_BYTES
        || !value.is_ascii()
        || value
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        Err(invalid_record(
            "event types and schema IDs must be non-empty ASCII identifiers of at most 256 characters",
        ))
    } else {
        Ok(())
    }
}
fn validate_actor(value: &str) -> Result<(), LogError> {
    if value.is_empty() || value.len() > MAX_TEXT_BYTES || invalid_control(value) {
        Err(invalid_record(
            "actor must be non-empty, at most 256 bytes, and contain no NUL, carriage return, or newline",
        ))
    } else {
        Ok(())
    }
}
fn validate_body(value: &Value) -> Result<(), LogError> {
    if serde_json::to_vec(value)
        .map_err(|error| invalid_record(format!("body cannot be serialized: {error}")))?
        .len()
        > MAX_BODY_BYTES
    {
        Err(invalid_record(
            "record body exceeds the 1 MiB serialized limit",
        ))
    } else {
        Ok(())
    }
}
fn invalid_control(value: &str) -> bool {
    value.contains('\0') || value.contains('\r') || value.contains('\n')
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;

    fn assert_invalid_record_id(value: String) {
        assert!(matches!(
            validate_record_id(&value),
            Err(LogError::InvalidRecord { .. })
        ));
    }

    fn assert_invalid_identifier(value: String) {
        assert!(matches!(
            validate_identifier(&value),
            Err(LogError::InvalidRecord { .. })
        ));
    }

    #[test]
    fn heads_require_a_consistent_sequence_and_clean_revision() {
        let longest_revision = "a".repeat(MAX_TEXT_BYTES);
        let head = Head::try_from_parts(1, Some(longest_revision.clone())).unwrap();
        assert_eq!(head.revision(), Some(longest_revision.as_str()));
        assert!(!head.is_empty());
        assert!(Head::empty().is_empty());

        assert!(matches!(
            Head::try_from_parts(0, Some("revision".to_owned())),
            Err(LogError::InvalidHead { .. })
        ));
        assert!(matches!(
            Head::try_from_parts(1, None),
            Err(LogError::InvalidHead { .. })
        ));
        for revision in [
            String::new(),
            "a".repeat(MAX_TEXT_BYTES + 1),
            "revision\0nul".to_owned(),
            "revision\rreturn".to_owned(),
            "revision\nnewline".to_owned(),
        ] {
            assert!(matches!(
                Head::try_from_parts(1, Some(revision)),
                Err(LogError::InvalidHead { .. })
            ));
        }
    }

    #[test]
    fn record_ids_reject_each_disallowed_filename_component_reason() {
        validate_record_id("record-_.9").unwrap();
        validate_record_id(&"a".repeat(MAX_TEXT_BYTES)).unwrap();

        for value in [
            String::new(),
            "a".repeat(MAX_TEXT_BYTES + 1),
            ".".to_owned(),
            "..".to_owned(),
            "double..period".to_owned(),
            "slash/name".to_owned(),
            "backslash\\name".to_owned(),
            "has space".to_owned(),
            "non-ascii-é".to_owned(),
        ] {
            assert_invalid_record_id(value);
        }
    }

    #[test]
    fn identifiers_reject_each_disallowed_reason() {
        validate_identifier("event.type_v1-name").unwrap();
        validate_identifier(&"a".repeat(MAX_TEXT_BYTES)).unwrap();

        for value in [
            String::new(),
            "a".repeat(MAX_TEXT_BYTES + 1),
            "non-ascii-é".to_owned(),
            "has space".to_owned(),
        ] {
            assert_invalid_identifier(value);
        }
    }

    #[test]
    fn actors_reject_empty_oversized_and_each_control_character() {
        validate_actor("a person").unwrap();
        validate_actor(&"a".repeat(MAX_TEXT_BYTES)).unwrap();

        for value in [
            String::new(),
            "a".repeat(MAX_TEXT_BYTES + 1),
            "actor\0nul".to_owned(),
            "actor\rreturn".to_owned(),
            "actor\nnewline".to_owned(),
        ] {
            assert!(matches!(
                validate_actor(&value),
                Err(LogError::InvalidRecord { .. })
            ));
        }
    }

    #[test]
    fn bodies_allow_exactly_one_mebibyte_and_reject_one_byte_more() {
        let at_limit = Value::String("x".repeat(MAX_BODY_BYTES - 2));
        let over_limit = Value::String("x".repeat(MAX_BODY_BYTES - 1));
        assert_eq!(serde_json::to_vec(&at_limit).unwrap().len(), MAX_BODY_BYTES);
        assert_eq!(
            serde_json::to_vec(&over_limit).unwrap().len(),
            MAX_BODY_BYTES + 1
        );
        validate_body(&at_limit).unwrap();
        assert!(matches!(
            validate_body(&over_limit),
            Err(LogError::InvalidRecord { .. })
        ));
    }

    #[test]
    fn record_and_draft_accessors_return_their_stored_values() {
        let record: Record = serde_json::from_value(json!({
            "id": "stored-record",
            "seq": 1,
            "at": "2026-07-27T12:00:00Z",
            "actor": "stored actor",
            "type": "example.changed",
            "body": {"state": "stored"},
            "schema": "example.v1"
        }))
        .unwrap();
        assert_eq!(record.actor(), "stored actor");
        assert_eq!(record.body(), &json!({"state": "stored"}));

        let draft = RecordDraft::new(
            RecordId::new("draft-record").unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 27, 12, 0, 0).unwrap(),
            "draft actor",
            EventType::new("example.changed").unwrap(),
            json!({"state": "draft"}),
            SchemaId::new("example.v1").unwrap(),
        )
        .unwrap();
        assert_eq!(draft.actor(), "draft actor");
        assert_eq!(draft.body(), &json!({"state": "draft"}));
    }
}
