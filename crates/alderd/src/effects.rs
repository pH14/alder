//! Everything the daemon does to the world, behind two thin traits.
//!
//! The daemon holds no Alder code and no Git library. It reads the log by
//! running `alder … --json`, it drives sessions by running `tmux`, and — only
//! on the dispatch path, where a worker needs a worktree of its own — it runs
//! `git`. That coupling is deliberately loose: Alder's stable agent surface is
//! its CLI, and the daemon stays domain-free.
//!
//! The driving loop itself still runs no Git command: its log trigger is a
//! sequence number read from `alder status`.

use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
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
    spawn::{Run, SpawnHost},
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
    /// The modification time of a file, or `None` if it cannot be statted.
    /// The driver uses this on the local append marker; absence is not an
    /// error, it is simply no hint.
    fn file_mtime(&self, path: &Path) -> Option<DateTime<Utc>>;
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

    /// A host for a one-shot command, which needs the `alder` binary and the
    /// project and nothing else. `.alder/driver.json` describes the driving
    /// loop, so a dispatch must not require one to exist.
    pub fn for_command(root: PathBuf, alder: String) -> Self {
        Self {
            root,
            alder,
            notify: None,
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

    /// Run `alder <args> --json` and read its one JSON document.
    fn run_alder(&self, args: &[&str]) -> Result<Value> {
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

    fn session_exists(&self, session: &str) -> Result<bool> {
        Ok(self
            .run("tmux", &["has-session", "-t", session])?
            .status
            .success())
    }

    fn kill_session(&self, session: &str) -> Result<()> {
        self.run("tmux", &["kill-session", "-t", session])?;
        Ok(())
    }

    fn say(&self, message: &str) {
        let mut stderr = std::io::stderr();
        let _ = writeln!(stderr, "{} alderd: {message}", Utc::now().to_rfc3339());
    }

    /// The `alder` binary as a path something can be copied from. A configured
    /// name with no separator is looked up on `PATH`, the way running it does.
    fn alder_path(&self) -> PathBuf {
        let configured = Path::new(&self.alder);
        if configured.components().count() > 1 {
            return if configured.is_absolute() {
                configured.to_path_buf()
            } else {
                self.root.join(configured)
            };
        }
        std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .map(|directory| directory.join(&self.alder))
                    .find(|candidate| candidate.is_file())
                    .unwrap_or_else(|| configured.to_path_buf())
            })
            .unwrap_or_else(|| configured.to_path_buf())
    }
}

impl Effects for Host {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn alder(&self, args: &[&str]) -> Result<Value> {
        self.run_alder(args)
    }

    fn tmux_session_exists(&self, session: &str) -> Result<bool> {
        self.session_exists(session)
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
        self.kill_session(session)
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

    fn file_mtime(&self, path: &Path) -> Option<DateTime<Utc>> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let modified = std::fs::metadata(path).ok()?.modified().ok()?;
        Some(DateTime::<Utc>::from(modified))
    }

    fn notify(&self, message: &str) {
        self.say(message);
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
        self.say(message);
    }
}

/// The dispatch path. Same host, same shell-outs, plus the two things only a
/// spawn needs: a git worktree and a pane started in it.
impl SpawnHost for Host {
    fn root(&self) -> &Path {
        &self.root
    }

    fn alder_binary(&self) -> PathBuf {
        self.alder_path()
    }

    fn alder(&self, args: &[&str]) -> Result<Value> {
        self.run_alder(args)
    }

    fn git(&self, args: &[&str]) -> Result<Run> {
        let output = self.run("git", args)?;
        Ok(Run {
            ok: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn tmux_session_exists(&self, session: &str) -> Result<bool> {
        self.session_exists(session)
    }

    fn tmux_new_session(&self, session: &str, cwd: &Path, command: &str) -> Result<()> {
        let cwd = cwd.display().to_string();
        let output = self.run(
            "tmux",
            &["new-session", "-d", "-s", session, "-c", &cwd, command],
        )?;
        if output.status.success() {
            Ok(())
        } else {
            Err(DriverError::new(format!(
                "tmux new-session failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    fn tmux_set_environment(&self, session: &str, name: &str, value: &str) -> Result<()> {
        let output = self.run("tmux", &["set-environment", "-t", session, name, value])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(DriverError::new(format!(
                "tmux set-environment failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    fn tmux_kill_session(&self, session: &str) -> Result<()> {
        self.kill_session(session)
    }

    fn path_exists(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path).map_err(|error| {
            DriverError::new(format!("cannot create `{}`: {error}", path.display()))
        })
    }

    fn copy_file(&self, from: &Path, to: &Path) -> Result<()> {
        std::fs::copy(from, to).map(|_| ()).map_err(|error| {
            DriverError::new(format!(
                "cannot copy `{}` to `{}`: {error}",
                from.display(),
                to.display()
            ))
        })
    }

    fn write_executable(&self, path: &Path, body: &str) -> Result<()> {
        std::fs::write(path, body)
            .and_then(|()| std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)))
            .map_err(|error| {
                DriverError::new(format!("cannot write `{}`: {error}", path.display()))
            })
    }

    fn log(&self, message: &str) {
        self.say(message);
    }
}

/// Single-quote one shell word for the command tmux will run.
pub(crate) fn quote(value: &str) -> String {
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
