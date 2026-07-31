use thiserror::Error;

use crate::{Head, RecordId};

/// Errors emitted by a log implementation.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LogError {
    /// A record ID, actor, type, schema, body, or sequence was invalid.
    #[error("invalid record: {message}")]
    InvalidRecord { message: String },
    /// A head was malformed or does not name the requested stored revision.
    #[error("invalid head: {message}")]
    InvalidHead { message: String },
    /// A read range was outside the supplied head.
    #[error("invalid read range: after {after} exceeds head sequence {through}")]
    InvalidRange { after: u64, through: u64 },
    /// A new ID was absent but the expected head was stale.
    #[error("head conflict")]
    HeadConflict { expected: Head, observed: Head },
    /// An existing record used the requested ID with different content.
    #[error("record ID collision: {id}")]
    RecordIdCollision { id: RecordId },
    /// A push was tried and its final outcome could not be determined.
    #[error("unknown append outcome: {message}")]
    UnknownOutcome { message: String },
    /// The immutable persisted record history is malformed or inconsistent.
    #[error("invalid log: {message}")]
    InvalidLog { message: String },
    /// Git or its configured remote could not be used.
    #[error("log unavailable: {message}")]
    Unavailable { message: String },
    /// A local I/O operation failed before a push was tried.
    #[error("local I/O failure: {message}")]
    Io { message: String },
    /// Serialization or deserialization failed outside a persisted log.
    #[error("serialization failure: {message}")]
    Serialization { message: String },
}

impl LogError {
    /// Stable, machine-readable error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRecord { .. } => "invalid_record",
            Self::InvalidHead { .. } => "invalid_head",
            Self::InvalidRange { .. } => "invalid_range",
            Self::HeadConflict { .. } => "head_conflict",
            Self::RecordIdCollision { .. } => "record_id_collision",
            Self::UnknownOutcome { .. } => "unknown_append_outcome",
            Self::InvalidLog { .. } => "invalid_log",
            Self::Unavailable { .. } => "store_unavailable",
            Self::Io { .. } => "io_error",
            Self::Serialization { .. } => "serialization_error",
        }
    }
}
impl From<std::io::Error> for LogError {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            message: error.to_string(),
        }
    }
}
impl From<serde_json::Error> for LogError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization {
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_its_stable_machine_code() {
        let record_id = RecordId::new("record").unwrap();
        let cases = [
            (
                LogError::InvalidRecord {
                    message: String::new(),
                },
                "invalid_record",
            ),
            (
                LogError::InvalidHead {
                    message: String::new(),
                },
                "invalid_head",
            ),
            (
                LogError::InvalidRange {
                    after: 2,
                    through: 1,
                },
                "invalid_range",
            ),
            (
                LogError::HeadConflict {
                    expected: Head::empty(),
                    observed: Head::empty(),
                },
                "head_conflict",
            ),
            (
                LogError::RecordIdCollision { id: record_id },
                "record_id_collision",
            ),
            (
                LogError::UnknownOutcome {
                    message: String::new(),
                },
                "unknown_append_outcome",
            ),
            (
                LogError::InvalidLog {
                    message: String::new(),
                },
                "invalid_log",
            ),
            (
                LogError::Unavailable {
                    message: String::new(),
                },
                "store_unavailable",
            ),
            (
                LogError::Io {
                    message: String::new(),
                },
                "io_error",
            ),
            (
                LogError::Serialization {
                    message: String::new(),
                },
                "serialization_error",
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code);
        }
    }
}
