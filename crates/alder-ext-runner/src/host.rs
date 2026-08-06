//! Operating-system operations used by the runner.
//!
//! The runner uses the `git` and `tmux` commands instead of linking Git or
//! tmux libraries. The [`Host`] trait lets [`crate::start`] test the order of
//! those operations without a real repository, tmux server, or model.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::{
    error::{Result, RunnerError},
    start::{
        ENGINE_ENV, ENGINE_EXITED, ENGINE_RUNNING, HANDLE_ENV, PROVIDER_ENV, TIER_ENV, WORKTREE_ENV,
    },
};

/// What the model-state marker in a session says. A missing or unrecognized
/// marker gives the runner no safe answer: it will not paste into the pane
/// because no model is known to be listening, and it will not replace the pane
/// because the model is not known to be finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EngineMarker {
    /// The session records `running`: the model is known to be running.
    Running,
    /// The session records `exited`: the model finished and its holding shell
    /// remains.
    Exited,
    /// No marker, or an unrecognized value. The runner cannot safely decide
    /// whether the model is running.
    #[default]
    Unproven,
}

/// Identity and model state read from the environment recorded in a tmux
/// session when it was created.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObservedSession {
    pub handle: Option<String>,
    pub tier: Option<String>,
    /// The provider selected by `start`. `send` uses this value so a later
    /// tier configuration change cannot change how the session receives input.
    pub provider: Option<String>,
    pub worktree: Option<PathBuf>,
    pub engine: EngineMarker,
}

/// Output from one completed external command.
#[derive(Debug, Clone)]
pub struct Run {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// An exclusive lock for one handle, released when dropped. `start`, `send`,
/// and `kill` all take it so that two commands cannot change the same
/// worktree, session, or pane input at the same time.
#[derive(Debug)]
pub struct StartLock {
    _file: Option<std::fs::File>,
}

impl StartLock {
    /// A lock that guards nothing, for tests that use a fake host.
    #[cfg(test)]
    pub(crate) fn unlocked_for_tests() -> Self {
        Self { _file: None }
    }
}

/// Try to take an exclusive advisory lock on `path` and return immediately if
/// another process already holds it. Waiting would let a second `start` run
/// the same prompt after the first one, or let a second `send` add another
/// paste and Enter to the same pane. The caller can decide whether to retry
/// after the lock holder finishes.
pub(crate) fn acquire_start_lock(path: &Path, handle: &str) -> Result<StartLock> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            RunnerError::new(format!("cannot create `{}`: {error}", parent.display()))
        })?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|error| {
            RunnerError::new(format!("cannot open lock `{}`: {error}", path.display()))
        })?;
    match file.try_lock() {
        Ok(()) => Ok(StartLock { _file: Some(file) }),
        Err(std::fs::TryLockError::WouldBlock) => Err(RunnerError::refusal(
            crate::error::EXIT_LOCK_HELD,
            format!("another operation on `{handle}` holds its lock; refusing to race it"),
        )),
        Err(std::fs::TryLockError::Error(error)) => Err(RunnerError::new(format!(
            "cannot lock `{}` for `{handle}`: {error}",
            path.display()
        ))),
    }
}

/// External operations needed by `start`.
pub trait RunnerHost {
    /// The repository where runs are started.
    fn repo(&self) -> &Path;
    /// Take the exclusive lock for a handle. The caller keeps the returned
    /// guard alive until `start`, `send`, or `kill` finishes.
    fn lock_handle(&self, handle: &str) -> Result<StartLock>;
    /// Return the runner-owned state directory for one handle. It must remain
    /// outside the model-writable worktree because the runner later trusts or
    /// executes files from this directory.
    fn handle_state_dir(&self, handle: &str) -> PathBuf;
    /// Run `git <args>` in the repository. An error means git could not be
    /// run at all; a git command that ran and failed comes back as `Run`.
    fn git(&self, args: &[&str]) -> Result<Run>;
    fn tmux_session(&self, session: &str) -> Result<Option<ObservedSession>>;
    fn tmux_new_session(
        &self,
        session: &str,
        cwd: &Path,
        command: &str,
        environment: &[(&str, String)],
    ) -> Result<()>;
    fn tmux_kill_session(&self, session: &str) -> Result<()>;
    fn path_exists(&self, path: &Path) -> bool;
    /// Resolve a path before comparing it with paths reported by Git.
    fn canonical_path(&self, path: &Path) -> Result<PathBuf>;
    /// Remove one unregistered worktree path. If it is a symlink, remove the
    /// link itself instead of following it.
    fn remove_path(&self, path: &Path) -> Result<()>;
    fn create_dir_all(&self, path: &Path) -> Result<()>;
    fn write_executable(&self, path: &Path, body: &str) -> Result<()>;
    /// Whether `path` is itself a symlink (never following it). Absent paths
    /// answer `false`.
    fn is_symlink(&self, path: &Path) -> bool;
    /// Copy one local file into a worktree before its model starts.
    fn copy_file(&self, source: &Path, destination: &Path) -> Result<()>;
    fn log(&self, message: &str);
}

/// The implementation that runs Git and tmux from a working directory.
pub struct Host {
    repo: PathBuf,
}

impl Host {
    pub fn new(repo: PathBuf) -> Self {
        Self { repo }
    }

    fn run(&self, program: &str, args: &[&str]) -> Result<std::process::Output> {
        Command::new(program)
            .args(args)
            .current_dir(&self.repo)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| RunnerError::new(format!("cannot run `{program}`: {error}")))
    }

    fn session_exists(&self, session: &str) -> Result<bool> {
        Ok(self
            .run("tmux", &["has-session", "-t", &format!("={session}")])?
            .status
            .success())
    }

    /// One variable of a session's environment, or `None` when the session or
    /// the variable is absent.
    pub fn tmux_environment(&self, session: &str, name: &str) -> Result<Option<String>> {
        let target = format!("={session}");
        let output = self.run("tmux", &["show-environment", "-t", &target, name])?;
        if !output.status.success() {
            return Ok(None);
        }
        let line = String::from_utf8_lossy(&output.stdout);
        Ok(line
            .trim()
            .strip_prefix(&format!("{name}="))
            .map(str::to_owned))
    }

    /// Load a local file into a tmux buffer byte for byte. tmux reads the
    /// file itself, so no command substitution can trim a newline or make its
    /// contents shell syntax.
    pub fn tmux_load_buffer(&self, buffer: &str, file: &Path) -> Result<()> {
        self.tmux_ok(&[
            "load-buffer",
            "-b",
            buffer,
            "--",
            &file.display().to_string(),
        ])
    }

    pub fn tmux_set_buffer(&self, buffer: &str, text: &str) -> Result<()> {
        self.tmux_ok(&["set-buffer", "-b", buffer, "--", text])
    }

    /// Paste one buffer into the session's pane and delete it. `-r` preserves
    /// LF as input bytes: without it tmux changes every line break to CR,
    /// which submits multi-line text as separate prompts.
    pub fn tmux_paste_buffer(&self, buffer: &str, session: &str) -> Result<()> {
        let pane = format!("={session}:");
        self.tmux_ok(&["paste-buffer", "-d", "-r", "-b", buffer, "-t", &pane])
    }

    pub fn tmux_submit(&self, session: &str) -> Result<()> {
        let pane = format!("={session}:");
        self.tmux_ok(&["send-keys", "-t", &pane, "Enter"])
    }

    /// Clear the pane's pending input line (C-u), used when a send must back
    /// out text it already pasted rather than let anything submit it.
    pub fn tmux_discard_input(&self, session: &str) -> Result<()> {
        let pane = format!("={session}:");
        self.tmux_ok(&["send-keys", "-t", &pane, "C-u"])
    }

    pub fn tmux_delete_buffer(&self, buffer: &str) {
        let _ = self.run("tmux", &["delete-buffer", "-b", buffer]);
    }

    /// Record one variable in a session after creation. Only the incomplete
    /// send marker uses this method; identity is recorded by `new-session`.
    pub fn tmux_set_session_environment(
        &self,
        session: &str,
        name: &str,
        value: &str,
    ) -> Result<()> {
        let target = format!("={session}");
        self.tmux_ok(&["set-environment", "-t", &target, name, value])
    }

    /// Remove one variable from a session's environment.
    pub fn tmux_unset_session_environment(&self, session: &str, name: &str) -> Result<()> {
        let target = format!("={session}");
        self.tmux_ok(&["set-environment", "-u", "-t", &target, name])
    }

    fn tmux_ok(&self, args: &[&str]) -> Result<()> {
        let output = self.run("tmux", args)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(RunnerError::new(format!(
                "tmux {} failed: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    fn say(&self, message: &str) {
        eprintln!("alder-ext-runner: {message}");
    }
}

/// Neither this implementation nor the methods above may wait for a model to
/// become ready. `start` tests both blocks for code that could wait. A
/// `Duration` used to limit an external command is still allowed.
impl RunnerHost for Host {
    fn repo(&self) -> &Path {
        &self.repo
    }

    fn lock_handle(&self, handle: &str) -> Result<StartLock> {
        let path = crate::config::state_dir().join(format!("start-{handle}.lock"));
        acquire_start_lock(&path, handle)
    }

    fn handle_state_dir(&self, handle: &str) -> PathBuf {
        crate::config::handle_state_dir(handle)
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
        let environment = |name: &str| self.tmux_environment(session, name);
        Ok(Some(ObservedSession {
            handle: environment(HANDLE_ENV)?,
            tier: environment(TIER_ENV)?,
            provider: environment(PROVIDER_ENV)?,
            worktree: environment(WORKTREE_ENV)?.map(PathBuf::from),
            // Only an explicit marker proves anything. A missing marker is
            // fail-safe in both directions: never pasted at (nothing proves
            // an engine is listening) and never replaced (nothing proves its
            // work is done).
            engine: match environment(ENGINE_ENV)?.as_deref() {
                Some(ENGINE_RUNNING) => EngineMarker::Running,
                Some(ENGINE_EXITED) => EngineMarker::Exited,
                _ => EngineMarker::Unproven,
            },
        }))
    }

    fn tmux_new_session(
        &self,
        session: &str,
        cwd: &Path,
        command: &str,
        environment: &[(&str, String)],
    ) -> Result<()> {
        let cwd = cwd.display().to_string();
        let mut args: Vec<String> = vec![
            "new-session".to_owned(),
            "-d".to_owned(),
            "-s".to_owned(),
            session.to_owned(),
            "-c".to_owned(),
            cwd,
        ];
        for (name, value) in environment {
            args.push("-e".to_owned());
            args.push(format!("{name}={value}"));
        }
        args.push(command.to_owned());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.run("tmux", &args)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(RunnerError::new(format!(
                "tmux new-session failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    fn tmux_kill_session(&self, session: &str) -> Result<()> {
        // The exit status is checked rather than swallowed: a kill that tmux
        // refused must not be reported as an ended execution. The caller in
        // `ops::kill` additionally verifies the session is really gone.
        let output = self.run("tmux", &["kill-session", "-t", &format!("={session}")])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(RunnerError::new(format!(
                "tmux kill-session failed for `{session}`: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    fn path_exists(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }

    fn canonical_path(&self, path: &Path) -> Result<PathBuf> {
        std::fs::canonicalize(path).map_err(|error| {
            RunnerError::new(format!(
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
            RunnerError::new(format!("cannot create `{}`: {error}", path.display()))
        })
    }

    fn write_executable(&self, path: &Path, body: &str) -> Result<()> {
        std::fs::write(path, body)
            .and_then(|()| std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)))
            .map_err(|error| {
                RunnerError::new(format!("cannot write `{}`: {error}", path.display()))
            })
    }

    fn is_symlink(&self, path: &Path) -> bool {
        path.symlink_metadata()
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    }

    fn copy_file(&self, source: &Path, destination: &Path) -> Result<()> {
        std::fs::copy(source, destination)
            .map(|_| ())
            .map_err(|error| {
                RunnerError::new(format!(
                    "cannot seed `{}` from `{}`: {error}",
                    destination.display(),
                    source.display()
                ))
            })
    }

    fn log(&self, message: &str) {
        self.say(message);
    }
}

/// Remove an unregistered worktree path after checking its file type.
///
/// The path can disappear during either filesystem call if another process
/// removes it first. That is already the desired result. Keeping the two
/// operations replaceable in tests makes both cases deterministic.
fn remove_residue(
    path: &Path,
    inspect: impl FnOnce() -> std::io::Result<bool>,
    remove: impl FnOnce(bool) -> std::io::Result<()>,
) -> Result<()> {
    let is_directory = match inspect() {
        Ok(is_directory) => is_directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(RunnerError::new(format!(
                "cannot inspect `{}` before removing it: {error}",
                path.display()
            )));
        }
    };
    match remove(is_directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RunnerError::new(format!(
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

    #[test]
    fn the_start_lock_is_exclusive_per_path_and_released_on_drop() {
        let state = tempfile::TempDir::new().expect("a state directory");
        let path = state.path().join("locks/start-alder-ext-work-x.lock");

        let held = acquire_start_lock(&path, "alder-ext-work-x").expect("the first lock is taken");
        let refused = acquire_start_lock(&path, "alder-ext-work-x")
            .expect_err("a concurrent start is refused, not queued");
        assert!(refused.message.contains("holds its lock"), "{refused}");
        assert_eq!(
            refused.exit,
            crate::error::EXIT_LOCK_HELD,
            "scripts branch on exit 4 for lock contention"
        );

        drop(held);
        acquire_start_lock(&path, "alder-ext-work-x")
            .expect("a released lock is free for the next start");
    }

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
        let residue = root.path().join("alder-ext-orphan");
        symlink(outside.path(), &residue).expect("the residue link is created");
        let host = Host::new(root.path().to_path_buf());

        host.remove_path(&residue)
            .expect("the residue link is removed");

        assert!(residue.symlink_metadata().is_err());
        assert_eq!(std::fs::read_to_string(protected).unwrap(), "keep");
    }

    #[test]
    fn host_paths_are_real_and_missing_residue_is_harmless() {
        let root = tempfile::TempDir::new().expect("a project root");
        let host = Host::new(root.path().to_path_buf());

        assert_eq!(
            host.canonical_path(root.path()).unwrap(),
            std::fs::canonicalize(root.path()).unwrap(),
        );
        host.remove_path(&root.path().join("already-gone"))
            .expect("removing absent residue converges");
    }

    #[test]
    fn removing_residue_does_not_hide_non_not_found_errors() {
        let root = tempfile::TempDir::new().expect("a project root");
        let not_a_directory = root.path().join("file");
        std::fs::write(&not_a_directory, "not a directory").unwrap();
        let impossible_child = not_a_directory.join("child");
        let host = Host::new(root.path().to_path_buf());

        let error = host
            .remove_path(&impossible_child)
            .expect_err("only a missing path is safe to ignore");

        assert!(error.message.contains("cannot inspect"), "{error}");
    }

    #[test]
    fn a_residue_that_disappears_during_either_filesystem_step_is_already_repaired() {
        let path = Path::new("/projects/alder-ext-run");
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
