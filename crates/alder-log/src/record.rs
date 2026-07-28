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
        || value.contains('/')
        || value.contains('\\')
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
