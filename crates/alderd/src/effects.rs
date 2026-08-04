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
    spawn::{
        ATTEMPT_ENV, ENGINE_ENV, ENGINE_EXITED, ENGINE_RUNNING, ObservedSession, Run, SpawnHost,
    },
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
        // The pane runs the engine itself. See `spawn::pane_command` for why
        // nothing wraps it to keep the host awake.
        let mut command = vec![engine.cmd.clone()];
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
///
/// This block and the inherent one above are the host code a dispatch reaches,
/// and neither may wait for an engine to come up. `spawn`'s tests read both for
/// the words that can only mean waiting; a bounded timeout on a command that
/// can hang is still allowed, spelled with a `Duration`.
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

    fn tmux_session(&self, session: &str) -> Result<Option<ObservedSession>> {
        if !self.session_exists(session)? {
            return Ok(None);
        }
        let target = format!("={session}");
        let environment = |name: &str| -> Result<Option<String>> {
            let output = self.run("tmux", &["show-environment", "-t", &target, name])?;
            if !output.status.success() {
                return Ok(None);
            }
            let line = String::from_utf8_lossy(&output.stdout);
            Ok(line
                .trim()
                .strip_prefix(&format!("{name}="))
                .map(str::to_owned))
        };
        let attempt_id = environment(ATTEMPT_ENV)?;
        let engine_live = match environment(ENGINE_ENV)?.as_deref() {
            Some(ENGINE_EXITED) => false,
            Some(ENGINE_RUNNING) => true,
            Some(_) => true,
            None => {
                // Sessions from before the explicit engine marker can still
                // be repaired. The pane always ends in `exec bash`, so bash
                // is the observable holding state and everything else is
                // conservatively treated as a live engine.
                let output = self.run(
                    "tmux",
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &target,
                        "#{pane_current_command}",
                    ],
                )?;
                if !output.status.success() {
                    return Err(DriverError::new(format!(
                        "cannot inspect tmux session `{session}`: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )));
                }
                String::from_utf8_lossy(&output.stdout).trim() != "bash"
            }
        };
        Ok(Some(ObservedSession {
            attempt_id,
            engine_live,
        }))
    }

    fn tmux_new_session(
        &self,
        session: &str,
        cwd: &Path,
        command: &str,
        attempt_id: &str,
    ) -> Result<()> {
        let cwd = cwd.display().to_string();
        let attempt = format!("{ATTEMPT_ENV}={attempt_id}");
        let engine = format!("{ENGINE_ENV}={ENGINE_RUNNING}");
        let output = self.run(
            "tmux",
            &[
                "new-session",
                "-d",
                "-s",
                session,
                "-c",
                &cwd,
                "-e",
                &attempt,
                "-e",
                &engine,
                command,
            ],
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

    fn tmux_kill_session(&self, session: &str) -> Result<()> {
        self.kill_session(session)
    }

    fn path_exists(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }

    fn canonical_path(&self, path: &Path) -> Result<PathBuf> {
        std::fs::canonicalize(path).map_err(|error| {
            DriverError::new(format!(
                "cannot resolve `{}` before checking Git's worktree registry: {error}",
                path.display()
            ))
        })
    }

    fn remove_path(&self, path: &Path) -> Result<()> {
        remove_residue(
            path,
            || {
                path.symlink_metadata()
                    .map(|metadata| metadata.file_type().is_dir())
            },
            |is_directory| {
                if is_directory {
                    std::fs::remove_dir_all(path)
                } else {
                    // `symlink_metadata` above deliberately does not follow
                    // a symlink, so this removes the residue link rather than
                    // its target outside the worktree parent.
                    std::fs::remove_file(path)
                }
            },
        )
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

/// Remove a worktree residue after inspecting its file kind.
///
/// The path can disappear in either filesystem call: another repair may win
/// that race, which is already the desired state. Keeping those two operations
/// injectable makes both convergence cases deterministic to test.
fn remove_residue(
    path: &Path,
    inspect: impl FnOnce() -> std::io::Result<bool>,
    remove: impl FnOnce(bool) -> std::io::Result<()>,
) -> Result<()> {
    let is_directory = match inspect() {
        Ok(is_directory) => is_directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(DriverError::new(format!(
                "cannot inspect `{}` before removing it: {error}",
                path.display()
            )));
        }
    };
    match remove(is_directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DriverError::new(format!(
            "cannot remove unregistered worktree residue `{}`: {error}",
            path.display()
        ))),
    }
}

/// Single-quote one shell word for the command tmux will run.
pub(crate) fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::spawn::SpawnHost;

    #[test]
    fn shell_words_survive_quotes_intact() {
        assert_eq!(quote("claude"), "'claude'");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn removing_residue_does_not_follow_a_symlink_outside_its_path() {
        let root = tempfile::TempDir::new().expect("a project root");
        let outside = tempfile::TempDir::new().expect("an outside directory");
        let protected = outside.path().join("protected");
        std::fs::write(&protected, "keep").expect("the outside file is written");
        let residue = root.path().join("alder-work-orphan");
        symlink(outside.path(), &residue).expect("the residue link is created");
        let host = Host::for_command(root.path().to_path_buf(), "alder".to_owned());

        SpawnHost::remove_path(&host, &residue).expect("the residue link is removed");

        assert!(residue.symlink_metadata().is_err());
        assert_eq!(std::fs::read_to_string(protected).unwrap(), "keep");
    }

    #[test]
    fn spawn_host_paths_are_real_and_missing_residue_is_harmless() {
        let root = tempfile::TempDir::new().expect("a project root");
        let nested_binary = root.path().join("bin/alder");
        std::fs::create_dir_all(nested_binary.parent().unwrap()).unwrap();
        std::fs::write(&nested_binary, "#!/bin/sh\n").unwrap();
        let host = Host::for_command(root.path().to_path_buf(), "bin/alder".to_owned());

        assert_eq!(
            SpawnHost::alder_binary(&host),
            nested_binary,
            "a configured relative path is rooted in the project"
        );
        assert_eq!(
            SpawnHost::canonical_path(&host, root.path()).unwrap(),
            std::fs::canonicalize(root.path()).unwrap(),
        );
        SpawnHost::remove_path(&host, &root.path().join("already-gone"))
            .expect("removing absent residue converges");
    }

    #[test]
    fn removing_residue_does_not_hide_non_not_found_errors() {
        let root = tempfile::TempDir::new().expect("a project root");
        let not_a_directory = root.path().join("file");
        std::fs::write(&not_a_directory, "not a directory").unwrap();
        let impossible_child = not_a_directory.join("child");
        let host = Host::for_command(root.path().to_path_buf(), "alder".to_owned());

        let error = SpawnHost::remove_path(&host, &impossible_child)
            .expect_err("only a missing path is safe to ignore");

        assert!(error.message.contains("cannot inspect"), "{error}");
    }

    #[test]
    fn a_residue_that_disappears_during_either_filesystem_step_is_already_repaired() {
        let path = Path::new("/projects/alder-work-al-1");
        remove_residue(
            path,
            || Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            |_| panic!("nothing is removed when inspection finds nothing"),
        )
        .expect("a path absent before inspection is converged");
        remove_residue(
            path,
            || Ok(false),
            |_| Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        )
        .expect("a path removed after inspection is converged too");

        let error = remove_residue(
            path,
            || Ok(false),
            |_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        )
        .expect_err("only NotFound is a completed repair");
        assert!(
            error.message.contains("cannot remove unregistered"),
            "{error}"
        );
    }
}
