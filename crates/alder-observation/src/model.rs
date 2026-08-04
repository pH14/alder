use serde::{Deserialize, Serialize};

/// The observation application's event schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "body")]
pub enum ObservationEventPayload {
    #[serde(rename = "observation.reported")]
    ObservationReported { observation: ObservationDefinition },
    #[serde(rename = "observation.retired")]
    ObservationRetired { key: ObservationKey },
}

impl ObservationEventPayload {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::ObservationReported { .. } => "observation.reported",
            Self::ObservationRetired { .. } => "observation.retired",
        }
    }
}

/// A key in the observation application. Its three parts are deliberately
/// explicit in every event and snapshot; no caller has to parse a synthetic
/// string to learn who reported what about which subject.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObservationKey {
    pub observer: String,
    pub subject: String,
    pub field: String,
}

/// The level supplied by an observer before the fold assigns its sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationDefinition {
    #[serde(flatten)]
    pub key: ObservationKey,
    pub level: String,
}

/// One current belief in the folded observation picture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    #[serde(flatten)]
    pub key: ObservationKey,
    pub level: String,
    pub reported_seq: u64,
}
