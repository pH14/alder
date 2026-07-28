//! Everything the driver does to the world, behind one thin trait.
//!
//! The daemon holds no Alder code, no Git library, and no Git shell-out. It
//! reads the log by running `alder … --json` and it drives the leader by
//! running `tmux`. That coupling is deliberately loose: Alder's stable agent
//! surface is its CLI.

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{
    config::{Config, Engine},
    error::{DriverError, Result},
};

pub trait Effects {
    fn now(&self) -> DateTime<Utc>;
    /// Run `alder <args> --json` and return its one JSON document.
    fn alder(&self, args: &[&str]) -> Result<Value>;
    fn tmux_session_exists(&self, session: &str) -> Result<bool>;
    fn tmux_new_session(&self, session: &str, engine: &Engine) -> Result<()>;
    fn tmux_kill_session(&self, session: &str) -> Result<()>;
    fn tmux_send_keys(&self, session: &str, text: &str) -> Result<()>;
    fn tmux_has_clients(&self, session: &str) -> Result<bool>;
    fn read_file(&self, path: &Path) -> Result<Vec<u8>>;
    fn notify(&self, message: &str);
    fn sleep(&self, duration: Duration);
    fn log(&self, message: &str);
}

/// The real world: a project directory, the `alder` binary, and tmux.
///
/// There is deliberately no Git here. The driver learns whether the log moved
/// by comparing the head `alder status` reports with the head the last pass
/// ended at, so it needs no second view of the store.
pub struct Host {
    root: PathBuf,
    alder: String,
    notify: Option<String>,
}

impl Host {
    pub fn new(root: PathBuf, config: &Config) -> Self {
        Self {
            root,
            alder: config.alder.clone(),
            notify: config.notify.clone(),
        }
    }

    fn run(&self, program: &str, args: &[&str]) -> Result<std::process::Output> {
        Command::new(program)
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| DriverError::new(format!("cannot run `{program}`: {error}")))
    }
}

impl Effects for Host {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn alder(&self, args: &[&str]) -> Result<Value> {
        let mut full: Vec<&str> = args.to_vec();
        full.push("--json");
        let output = self.run(&self.alder, &full)?;
        let document: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            DriverError::new(format!(
                "`alder {}` did not print one JSON document: {error}",
                args.join(" ")
            ))
        })?;
        if output.status.success() {
            return Ok(document);
        }
        let code = document
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let message = document
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the command failed");
        Err(DriverError::coded(code, message))
    }

    fn tmux_session_exists(&self, session: &str) -> Result<bool> {
        Ok(self
            .run("tmux", &["has-session", "-t", session])?
            .status
            .success())
    }

    fn tmux_new_session(&self, session: &str, engine: &Engine) -> Result<()> {
        // `caffeinate -i` keeps the Mac from idle-sleeping mid-pass.
        let mut command = vec!["caffeinate".to_owned(), "-i".to_owned(), engine.cmd.clone()];
        command.extend(engine.args.iter().cloned());
        let command = command
            .iter()
            .map(|part| quote(part))
            .collect::<Vec<_>>()
            .join(" ");
        let output = self.run("tmux", &["new-session", "-d", "-s", session, &command])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(DriverError::new(format!(
                "tmux new-session failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    fn tmux_kill_session(&self, session: &str) -> Result<()> {
        self.run("tmux", &["kill-session", "-t", session])?;
        Ok(())
    }

    fn tmux_send_keys(&self, session: &str, text: &str) -> Result<()> {
        // The literal text and the Enter are separate sends so that no part of
        // the message can be interpreted as a key name.
        let typed = self.run("tmux", &["send-keys", "-t", session, "-l", "--", text])?;
        if !typed.status.success() {
            return Err(DriverError::new(format!(
                "tmux send-keys failed: {}",
                String::from_utf8_lossy(&typed.stderr).trim()
            )));
        }
        self.run("tmux", &["send-keys", "-t", session, "Enter"])?;
        Ok(())
    }

    fn tmux_has_clients(&self, session: &str) -> Result<bool> {
        let output = self.run("tmux", &["list-clients", "-t", session])?;
        Ok(output.status.success() && !output.stdout.is_empty())
    }

    fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        std::fs::read(&path)
            .map_err(|error| DriverError::new(format!("cannot read `{}`: {error}", path.display())))
    }

    fn notify(&self, message: &str) {
        self.log(message);
        if let Some(command) = self.notify.as_deref() {
            let _ = Command::new("/bin/sh")
                .args(["-c", command, "alderd", message])
                .current_dir(&self.root)
                .stdin(Stdio::null())
                .output();
        }
    }

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }

    fn log(&self, message: &str) {
        let mut stderr = std::io::stderr();
        let _ = writeln!(stderr, "{} alderd: {message}", Utc::now().to_rfc3339());
    }
}

/// Single-quote one shell word for the command tmux will run.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_words_survive_quotes_intact() {
        assert_eq!(quote("claude"), "'claude'");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
