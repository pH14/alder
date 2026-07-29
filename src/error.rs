use serde_json::{Value, json};
use thiserror::Error;

use alder_log::LogError;

pub type Result<T> = std::result::Result<T, AlderError>;

#[derive(Debug, Error)]
#[error("{message}")]
pub struct AlderError {
    pub code: &'static str,
    pub message: String,
    pub context: Value,
}

impl AlderError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            context: json!({}),
        }
    }

    pub fn with_context(code: &'static str, message: impl Into<String>, context: Value) -> Self {
        Self {
            code,
            message: message.into(),
            context,
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new("validation_failed", message)
    }

    pub fn not_found(kind: &str, id: &str) -> Self {
        Self::with_context(
            "not_found",
            format!("{kind} `{id}` was not found"),
            json!({"kind": kind, "id": id}),
        )
    }

    /// A mutation that lost the compare-and-append.
    ///
    /// The caller's question is never what happened to the log; it is whether
    /// its own command took effect. It did not, so that is what this says
    /// first, and `appended` says it again in a field a machine can test.
    /// `event` names the event that was not written when the caller is close
    /// enough to the mutation to know it.
    pub fn lost_append(event: Option<&str>, expected: u64, observed: u64) -> Self {
        let loser = match event {
            Some(event) => format!("`{event}` lost"),
            None => "this mutation lost".to_owned(),
        };
        let mut context = json!({
            "appended": false,
            "expected_head": expected,
            "current_head": observed,
        });
        if let (Some(event), Value::Object(fields)) = (event, &mut context) {
            fields.insert("event".to_owned(), json!(event));
        }
        Self::with_context(
            "head_conflict",
            format!(
                "nothing was appended: {loser} the compare-and-append to another writer, \
                 which moved the shared log from {expected} to {observed}; \
                 reread and run the command again"
            ),
            context,
        )
    }

    pub fn json(&self) -> Value {
        json!({
            "schema": "alder.error.v0",
            "ok": false,
            "code": self.code,
            "message": self.message,
            "context": self.context,
        })
    }
}

impl From<std::io::Error> for AlderError {
    fn from(value: std::io::Error) -> Self {
        Self::with_context(
            "io_error",
            value.to_string(),
            json!({"kind": format!("{:?}", value.kind())}),
        )
    }
}

impl From<serde_json::Error> for AlderError {
    fn from(value: serde_json::Error) -> Self {
        Self::with_context(
            "invalid_json",
            value.to_string(),
            json!({"line": value.line(), "column": value.column()}),
        )
    }
}

impl From<LogError> for AlderError {
    fn from(value: LogError) -> Self {
        match value {
            LogError::InvalidRecord { message } => Self::new("invalid_record", message),
            LogError::InvalidHead { message } => Self::new("invalid_head", message),
            LogError::InvalidRange { after, through } => Self::with_context(
                "invalid_range",
                format!("read offset {after} exceeds head sequence {through}"),
                json!({"after": after, "through": through}),
            ),
            LogError::HeadConflict { expected, observed } => {
                Self::lost_append(None, expected.sequence(), observed.sequence())
            }
            LogError::RecordIdCollision { id } => Self::with_context(
                "record_id_collision",
                format!("record ID `{id}` already exists with different content"),
                json!({"record_id": id.as_str()}),
            ),
            LogError::UnknownOutcome { message } => Self::new("unknown_append_outcome", message),
            LogError::InvalidLog { message } => Self::new("invalid_log", message),
            LogError::Unavailable { message } => Self::new("store_unavailable", message),
            LogError::Io { message } => Self::new("io_error", message),
            LogError::Serialization { message } => Self::new("serialization_error", message),
        }
    }
}

impl From<rusqlite::Error> for AlderError {
    fn from(value: rusqlite::Error) -> Self {
        Self::new("database_error", value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alder_log::Head;

    #[test]
    fn a_lost_append_leads_with_the_effect_on_the_command() {
        // A caller that reads only the first words has already been told the
        // one thing it has to act on.
        let named = AlderError::lost_append(Some("pass.ended"), 343, 344);
        assert_eq!(named.code, "head_conflict");
        assert!(named.message.starts_with("nothing was appended: "));
        assert!(named.message.contains("`pass.ended`"));
        assert_eq!(named.context["appended"], json!(false));
        assert_eq!(named.context["event"], json!("pass.ended"));
        assert_eq!(named.context["expected_head"], json!(343));
        assert_eq!(named.context["current_head"], json!(344));

        // A conflict raised where the event is not known says the same thing
        // and simply names no event.
        let store: AlderError = LogError::HeadConflict {
            expected: Head::empty(),
            observed: Head::empty(),
        }
        .into();
        assert_eq!(store.code, "head_conflict");
        assert!(store.message.starts_with("nothing was appended: "));
        assert_eq!(store.context["appended"], json!(false));
        assert!(store.context.get("event").is_none());

        // Failure is marked in the envelope itself, not only in its schema.
        assert_eq!(named.json()["ok"], json!(false));
    }
}
