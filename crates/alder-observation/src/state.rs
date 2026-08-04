use std::collections::BTreeMap;

use serde_json::json;

use alder_log::alder_error::{AlderError, Result};

use super::{Observation, ObservationDefinition, ObservationEventPayload, ObservationKey};

/// The serialized shape of the folded observation picture: an ordered list,
/// never a synthetic string key.
pub mod observation_map {
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

/// Fold one observation event, checking its legality against the picture.
pub fn apply(
    observations: &mut BTreeMap<ObservationKey, Observation>,
    payload: &ObservationEventPayload,
    seq: u64,
) -> Result<()> {
    match payload {
        ObservationEventPayload::ObservationReported { observation } => {
            report(observations, observation, seq)
        }
        ObservationEventPayload::ObservationRetired { key } => retire(observations, key),
    }
}

/// The newness check: the observation event one report or retirement implies
/// against the current picture, or `None` when the picture would not change.
///
/// Observers are deliberately dumb and may report the same level forever.
/// The append layer asks this question after every reread, so two identical
/// reports settle as one event instead of making scripts remember prior
/// output, and retiring an already absent key is quiet for the same reason.
pub fn newness_check(
    current: Option<&Observation>,
    key: &ObservationKey,
    level: Option<&str>,
) -> Option<ObservationEventPayload> {
    match (current, level) {
        (Some(observation), Some(level)) if observation.level == level => None,
        (None, None) => None,
        (_, Some(level)) => Some(ObservationEventPayload::ObservationReported {
            observation: ObservationDefinition {
                key: key.clone(),
                level: level.to_owned(),
            },
        }),
        (Some(_), None) => Some(ObservationEventPayload::ObservationRetired { key: key.clone() }),
    }
}

/// Fold one reported level into the current picture.
pub fn report(
    observations: &mut BTreeMap<ObservationKey, Observation>,
    observation: &ObservationDefinition,
    seq: u64,
) -> Result<()> {
    validate_observation_key(&observation.key)?;
    require_text("observation level", &observation.level)?;
    observations.insert(
        observation.key.clone(),
        Observation {
            key: observation.key.clone(),
            level: observation.level.clone(),
            reported_seq: seq,
        },
    );
    Ok(())
}

/// Fold one retirement: the key must be current.
pub fn retire(
    observations: &mut BTreeMap<ObservationKey, Observation>,
    key: &ObservationKey,
) -> Result<()> {
    validate_observation_key(key)?;
    if observations.remove(key).is_none() {
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
    Ok(())
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
