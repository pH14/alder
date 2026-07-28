use std::fmt;

pub type Result<T> = std::result::Result<T, DriverError>;

/// The driver exercises no judgment, so it needs no error taxonomy: either it
/// could read what it needs or it could not. Alder's own structured codes are
/// carried through in `code` when a shell-out produced one.
#[derive(Debug, Clone)]
pub struct DriverError {
    pub message: String,
    pub code: Option<String>,
}

impl DriverError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
        }
    }

    pub fn coded(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: Some(code.into()),
        }
    }

    pub fn is(&self, code: &str) -> bool {
        self.code.as_deref() == Some(code)
    }
}

impl fmt::Display for DriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.code {
            Some(code) => write!(formatter, "[{code}] {}", self.message),
            None => formatter.write_str(&self.message),
        }
    }
}

impl std::error::Error for DriverError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_carried_through_and_matchable() {
        let plain = DriverError::new("tmux is missing");
        assert_eq!(plain.to_string(), "tmux is missing");
        assert!(!plain.is("pass_open"));

        let coded = DriverError::coded("pass_open", "pass `hm-pass-1` is still open");
        assert_eq!(
            coded.to_string(),
            "[pass_open] pass `hm-pass-1` is still open"
        );
        assert!(coded.is("pass_open"));
        assert!(!coded.is("store_unavailable"));
    }
}
