use std::{fs, path::Path, time::Duration};

use serde::Deserialize;

use crate::error::{DriverError, Result};

/// The driver's local configuration, read from `.alder/driver.json`.
///
/// `.alder/` is gitignored, so this is deliberately machine-local: what
/// command runs the executor and how aggressively to poll are properties of
/// the box the daemon runs on, not durable project facts.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    /// The shell command a wake runs. It receives the trigger names in
    /// `ALDERD_TRIGGERS` and is the whole of what a wake does; sessions,
    /// engines, and prompts are its business, never the daemon's.
    pub command: String,
    #[serde(default = "default_poll")]
    pub poll_seconds: u64,
    /// How often to stat the local append marker between full polls.
    #[serde(default = "default_hint_poll")]
    pub hint_poll_seconds: u64,
    #[serde(default = "default_debounce")]
    pub debounce_seconds: u64,
    #[serde(default = "default_max_interval")]
    pub max_interval_seconds: u64,
    /// Optional shell command invoked with one message argument.
    #[serde(default)]
    pub notify: Option<String>,
    /// Path to the `alder` binary. The driver reaches the log only through it.
    #[serde(default = "default_alder")]
    pub alder: String,
}

fn default_poll() -> u64 {
    60
}

fn default_hint_poll() -> u64 {
    1
}

fn default_debounce() -> u64 {
    20
}

fn default_max_interval() -> u64 {
    1800
}

fn default_alder() -> String {
    "alder".to_owned()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).map_err(|error| {
            DriverError::new(format!(
                "cannot read driver config `{}`: {error}",
                path.display()
            ))
        })?;
        let config: Self = serde_json::from_slice(&bytes).map_err(|error| {
            DriverError::new(format!(
                "invalid driver config `{}`: {error}",
                path.display()
            ))
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.command.trim().is_empty() {
            return Err(DriverError::new("command cannot be empty"));
        }
        if self.poll_seconds == 0 {
            return Err(DriverError::new("pollSeconds must be positive"));
        }
        if self.hint_poll_seconds == 0 {
            return Err(DriverError::new("hintPollSeconds must be positive"));
        }
        Ok(())
    }

    pub fn poll(&self) -> Duration {
        Duration::from_secs(self.poll_seconds)
    }

    pub fn hint_poll(&self) -> Duration {
        Duration::from_secs(self.hint_poll_seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("driver.json");
        fs::write(&path, body).unwrap();
        (directory, path)
    }

    #[test]
    fn defaults_fill_in_every_optional_field() {
        let (_directory, path) = write(r#"{"command": "alder-pass"}"#);
        let config = Config::load(&path).unwrap();
        assert_eq!(config.command, "alder-pass");
        assert_eq!(config.poll_seconds, 60);
        assert_eq!(config.hint_poll_seconds, 1);
        assert_eq!(config.debounce_seconds, 20);
        assert_eq!(config.max_interval_seconds, 1800);
        assert_eq!(config.alder, "alder");
        assert!(config.notify.is_none());
        assert_eq!(config.poll(), Duration::from_secs(60));
        assert_eq!(config.hint_poll(), Duration::from_secs(1));
    }

    #[test]
    fn every_invalid_field_is_rejected_by_name() {
        for body in [
            r#"{}"#,
            r#"{"command": " "}"#,
            r#"{"command": "c", "pollSeconds": 0}"#,
            r#"{"command": "c", "hintPollSeconds": 0}"#,
            // The fields that left with the execution extraction are unknown
            // now: a stale config fails loudly instead of half-working.
            r#"{"command": "c", "engines": {"claude": {"cmd": "claude"}}}"#,
            r#"{"command": "c", "passDoc": "p"}"#,
            r#"{"command": "c", "tmuxSession": "s"}"#,
            r#"{"command": "c", "maxSessionAgeSeconds": 1}"#,
            r#"not json"#,
        ] {
            let (_directory, path) = write(body);
            assert!(Config::load(&path).is_err(), "{body}");
        }
        assert!(Config::load(Path::new("/nonexistent/driver.json")).is_err());
    }
}
