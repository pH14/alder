//! The observer output contracts: what a `list` or `probe` script must say
//! for its words to become observation statements. Script execution — the
//! process, timeout, and retry machinery — lives with the CLI; only the pure
//! contract lives here.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::json;

use alder_log::alder_error::{AlderError, Result};

use super::ObservationKey;

/// One current level reported by an observer script. For `liveness` rows the
/// subject is the opaque handle exactly as the runner bound it; for every
/// other field the subject is stored verbatim as the observation subject.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedObject {
    pub subject: String,
    pub field: String,
    pub level: String,
}

/// One planned change to the durable observation picture: a level to report,
/// or — when `level` is `None` — a key to retire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationChange {
    pub key: ObservationKey,
    pub level: Option<String>,
}

pub fn validate_output(bytes: &[u8]) -> Result<Vec<NormalizedObject>> {
    let objects: Vec<NormalizedObject> = serde_json::from_slice(bytes).map_err(|error| {
        AlderError::with_context(
            "invalid_observation",
            format!("standard output is not one normalized JSON array: {error}"),
            json!({"line": error.line(), "column": error.column()}),
        )
    })?;
    let mut keys = BTreeSet::new();
    for object in &objects {
        if object.subject.trim().is_empty() {
            return Err(AlderError::new(
                "invalid_observation",
                "an observation subject cannot be empty",
            ));
        }
        if object.field.trim().is_empty() || object.level.trim().is_empty() {
            return Err(AlderError::new(
                "invalid_observation",
                "an observation field and level cannot be empty",
            ));
        }
        if !keys.insert((&object.subject, &object.field)) {
            return Err(AlderError::with_context(
                "invalid_observation",
                format!(
                    "duplicate observation key `{}` / `{}`",
                    object.subject, object.field
                ),
                json!({"subject": object.subject, "field": object.field}),
            ));
        }
    }
    Ok(objects)
}

/// A probe answers with exactly one word — `present`, `done`, `absent`, or
/// `unknown` — surrounded by nothing but whitespace. Anything else is an
/// invalid execution and retries like malformed `list` output. `done` is the
/// probe's way of saying "the execution finished and its result is safe":
/// distinct from `absent`, which means nothing runs under the handle at all
/// and there may be nothing to inspect.
pub fn validate_probe_output(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| AlderError::new("invalid_observation", "a probe answer must be UTF-8 text"))?;
    match text.trim() {
        answer @ ("present" | "done" | "absent" | "unknown") => Ok(answer.to_owned()),
        other => Err(AlderError::with_context(
            "invalid_observation",
            "a probe must answer exactly one of `present`, `done`, `absent`, or `unknown`",
            json!({"answer": bounded(other.as_bytes(), 80)}),
        )),
    }
}

pub fn bounded(bytes: &[u8], limit: usize) -> String {
    let value = String::from_utf8_lossy(bytes);
    let mut output: String = value.chars().take(limit).collect();
    if value.chars().count() > limit {
        output.push('…');
    }
    output
}
