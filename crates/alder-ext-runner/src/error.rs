use std::fmt;

pub type Result<T> = std::result::Result<T, RunnerError>;

/// The exit codes scripts branch on. The runner still exercises no judgment
/// about what it runs; these classify only *why it refused*, because callers
/// converge on refusals (adopt the live execution, treat a lock loss as
/// already-served, rotate a dead engine) and prose is not a contract.
pub const EXIT_FAILURE: u8 = 1;
/// `start`: the handle is already running a live engine. Stdout carries
/// exactly one machine-readable line: `handle <h>`.
pub const EXIT_ALREADY_RUNNING: u8 = 3;
/// Another operation on this handle holds its lock. For `send`, the caller
/// should treat the delivery as already served by the lock winner.
pub const EXIT_LOCK_HELD: u8 = 4;
/// `start`: a session exists but cannot prove its engine exited. `send`: the
/// execution cannot receive this delivery — the engine exited, nothing
/// answers to the handle, the pane is torn, or a codex session cannot be
/// resumed — and the caller may rotate.
pub const EXIT_UNRECEIVABLE: u8 = 5;

/// One refusal or failure, with the exit code a script branches on and an
/// optional machine-readable stdout line (stderr keeps the prose).
#[derive(Debug, Clone)]
pub struct RunnerError {
    pub message: String,
    /// The process exit code this error maps to; [`EXIT_FAILURE`] unless a
    /// constructor pinned a contract code.
    pub exit: u8,
    /// A machine-readable line printed verbatim on stdout before exiting,
    /// such as `handle <h>` for [`EXIT_ALREADY_RUNNING`].
    pub stdout: Option<String>,
}

impl RunnerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit: EXIT_FAILURE,
            stdout: None,
        }
    }

    /// A refusal with a contract exit code.
    pub fn refusal(exit: u8, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit,
            stdout: None,
        }
    }

    /// Attach the machine-readable stdout line.
    pub fn with_stdout(mut self, line: impl Into<String>) -> Self {
        self.stdout = Some(line.into());
        self
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
        assert_eq!(error.exit, EXIT_FAILURE);
        assert!(error.stdout.is_none());
    }

    #[test]
    fn a_refusal_pins_its_exit_code_and_stdout_line() {
        let refusal = RunnerError::refusal(EXIT_ALREADY_RUNNING, "already running")
            .with_stdout("handle alder-ext-work-x");
        assert_eq!(refusal.exit, EXIT_ALREADY_RUNNING);
        assert_eq!(refusal.stdout.as_deref(), Some("handle alder-ext-work-x"));

        // The contract codes scripts branch on. Changing a value is a
        // breaking change to every caller; this pin makes it a loud one.
        assert_eq!(EXIT_ALREADY_RUNNING, 3);
        assert_eq!(EXIT_LOCK_HELD, 4);
        assert_eq!(EXIT_UNRECEIVABLE, 5);
    }
}
