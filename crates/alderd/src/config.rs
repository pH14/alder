use std::{collections::BTreeMap, fs, path::Path, time::Duration};

use serde::Deserialize;

use crate::error::{DriverError, Result};

/// The driver's local configuration, read from `.alder/driver.json`.
///
/// `.alder/` is gitignored, so this is deliberately machine-local: which
/// engines exist and how aggressively to poll are properties of the box the
/// daemon runs on, not durable project facts.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    /// Engine name to the interactive CLI that provides it.
    pub engines: BTreeMap<String, Engine>,
    /// The pass prompt document a bootstrap injection points at.
    pub pass_doc: String,
    #[serde(default = "default_session")]
    pub tmux_session: String,
    #[serde(default = "default_poll")]
    pub poll_seconds: u64,
    #[serde(default = "default_debounce")]
    pub debounce_seconds: u64,
    #[serde(default = "default_max_interval")]
    pub max_interval_seconds: u64,
    #[serde(default = "default_pass_timeout")]
    pub pass_timeout_seconds: u64,
    #[serde(default = "default_max_passes")]
    pub max_passes_per_session: u32,
    /// Optional shell command invoked with one message argument.
    #[serde(default)]
    pub notify: Option<String>,
    /// Path to the `alder` binary. The driver reaches the log only through it.
    #[serde(default = "default_alder")]
    pub alder: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Engine {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
}

fn default_session() -> String {
    "alder-leader".to_owned()
}

fn default_poll() -> u64 {
    60
}

fn default_debounce() -> u64 {
    20
}

fn default_max_interval() -> u64 {
    1800
}

fn default_pass_timeout() -> u64 {
    3600
}

fn default_max_passes() -> u32 {
    25
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
        if self.engines.is_empty() {
            return Err(DriverError::new("at least one engine must be configured"));
        }
        for (name, engine) in &self.engines {
            if engine.cmd.trim().is_empty() {
                return Err(DriverError::new(format!("engine `{name}` has no command")));
            }
        }
        if self.pass_doc.trim().is_empty() {
            return Err(DriverError::new("passDoc cannot be empty"));
        }
        if self.tmux_session.trim().is_empty() {
            return Err(DriverError::new("tmuxSession cannot be empty"));
        }
        if self.poll_seconds == 0 {
            return Err(DriverError::new("pollSeconds must be positive"));
        }
        if self.pass_timeout_seconds == 0 {
            return Err(DriverError::new("passTimeoutSeconds must be positive"));
        }
        if self.max_passes_per_session == 0 {
            return Err(DriverError::new("maxPassesPerSession must be positive"));
        }
        Ok(())
    }

    pub fn poll(&self) -> Duration {
        Duration::from_secs(self.poll_seconds)
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
        let (_directory, path) =
            write(r#"{"engines": {"claude": {"cmd": "claude"}}, "passDoc": ".alder/PASS.md"}"#);
        let config = Config::load(&path).unwrap();
        assert_eq!(config.tmux_session, "alder-leader");
        assert_eq!(config.poll_seconds, 60);
        assert_eq!(config.debounce_seconds, 20);
        assert_eq!(config.max_interval_seconds, 1800);
        assert_eq!(config.pass_timeout_seconds, 3600);
        assert_eq!(config.max_passes_per_session, 25);
        assert_eq!(config.alder, "alder");
        assert!(config.notify.is_none());
        assert!(config.engines["claude"].args.is_empty());
        assert_eq!(config.poll(), Duration::from_secs(60));
    }

    #[test]
    fn every_invalid_field_is_rejected_by_name() {
        for body in [
            r#"{"engines": {}, "passDoc": "p"}"#,
            r#"{"engines": {"c": {"cmd": " "}}, "passDoc": "p"}"#,
            r#"{"engines": {"c": {"cmd": "c"}}, "passDoc": " "}"#,
            r#"{"engines": {"c": {"cmd": "c"}}, "passDoc": "p", "tmuxSession": " "}"#,
            r#"{"engines": {"c": {"cmd": "c"}}, "passDoc": "p", "pollSeconds": 0}"#,
            r#"{"engines": {"c": {"cmd": "c"}}, "passDoc": "p", "passTimeoutSeconds": 0}"#,
            r#"{"engines": {"c": {"cmd": "c"}}, "passDoc": "p", "maxPassesPerSession": 0}"#,
            r#"{"engines": {"c": {"cmd": "c"}}, "passDoc": "p", "unknown": 1}"#,
            r#"not json"#,
        ] {
            let (_directory, path) = write(body);
            assert!(Config::load(&path).is_err(), "{body}");
        }
        assert!(Config::load(Path::new("/nonexistent/driver.json")).is_err());
    }
}
