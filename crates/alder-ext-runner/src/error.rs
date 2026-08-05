use std::fmt;

pub type Result<T> = std::result::Result<T, RunnerError>;

/// The runner exercises no judgment about what it runs, so it needs no error
/// taxonomy: either it could do what it was asked or it could not.
#[derive(Debug, Clone)]
pub struct RunnerError {
    pub message: String,
}

impl RunnerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RunnerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RunnerError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_carry_their_message_through() {
        let error = RunnerError::new("tmux is missing");
        assert_eq!(error.to_string(), "tmux is missing");
    }
}
