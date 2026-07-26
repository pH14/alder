use serde_json::{Value, json};
use thiserror::Error;

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

    pub fn json(&self) -> Value {
        json!({
            "schema": "alder.error.v0",
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

impl From<rusqlite::Error> for AlderError {
    fn from(value: rusqlite::Error) -> Self {
        Self::new("database_error", value.to_string())
    }
}
