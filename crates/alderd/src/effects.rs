//! Everything the daemon does to the world, behind one thin trait.
//!
//! The daemon holds no Alder code, no Git library, and — since the execution
//! extraction — no tmux knowledge at all. It reads the log by running
//! `alder … --json`, and when a trigger fires it runs the configured shell
//! command. That coupling is deliberately loose: Alder's stable agent surface
//! is its CLI, the command's contract is one environment variable, and the
//! daemon stays domain-free.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{
    config::Config,
    error::{DriverError, Result},
};

/// The environment variable the configured command receives: the trigger
/// names that caused this run, comma-joined (`log,due`), or `none`.
pub const TRIGGERS_ENV: &str = "ALDERD_TRIGGERS";

/// The bound on the driver's own `alder` read. A CLI that hangs — a wedged
/// remote helper, a dead filesystem — must become an outage the driver can
/// report and retry, not a poll that never returns. Sixty seconds is far
/// beyond any honest `status`, so nothing real is cut off.
const ALDER_TIMEOUT: Duration = Duration::from_secs(60);

/// How often a bounded wait looks at the child between now and its deadline.
const WAIT_SLICE: Duration = Duration::from_millis(50);

pub trait Effects {
    fn now(&self) -> DateTime<Utc>;
    /// Run `alder <args> --json` and return its one JSON document.
    fn alder(&self, args: &[&str]) -> Result<Value>;
    /// Run the configured wake command with [`TRIGGERS_ENV`] set to
    /// `triggers`. Success means the command exited zero.
    fn run_command(&self, command: &str, triggers: &str) -> Result<()>;
    fn read_file(&self, path: &Path) -> Result<Vec<u8>>;
    /// Write a machine-local file, such as the driver's own notes under
    /// `.alder/`. Never a log write: the daemon appends nothing to the log.
    fn write_file(&self, path: &Path, bytes: &[u8]) -> Result<()>;
    /// The modification time of a file, or `None` if it cannot be statted.
    /// The driver uses this on the local append marker; absence is not an
    /// error, it is simply no hint.
    fn file_mtime(&self, path: &Path) -> Option<DateTime<Utc>>;
    fn notify(&self, message: &str);
    fn sleep(&self, duration: Duration);
    fn log(&self, message: &str);
}

/// The real world: a project directory, the `alder` binary, and a shell.
///
/// There is deliberately no Git here. The driver learns whether the log moved
/// by comparing the head `alder status` reports with the head the last pass
/// ended at, so it needs no second view of the store.
pub struct Host {
    root: PathBuf,
    alder: String,
    notify: Option<String>,
    /// The wake command's bound, from `commandTimeoutSeconds`.
    command_timeout: Duration,
}

impl Host {
    pub fn new(root: PathBuf, config: &Config) -> Self {
        Self {
            root,
            alder: config.alder.clone(),
            notify: config.notify.clone(),
            command_timeout: config.command_timeout(),
        }
    }

    /// Run `program` to completion within `timeout`, killing it on the
    /// deadline. Its output is drained on threads, so a chatty child cannot
    /// deadlock against a full pipe while its exit is being polled.
    fn run(&self, program: &str, args: &[&str], timeout: Duration) -> Result<std::process::Output> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| DriverError::new(format!("cannot run `{program}`: {error}")))?;
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let status = wait_bounded(&mut child, timeout)
            .map_err(|error| DriverError::new(format!("cannot wait for `{program}`: {error}")))?;
        let Some(status) = status else {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout.join();
            let _ = stderr.join();
            // The store did not answer within bounds; to the driver that is
            // an outage — reported, counted, retried — never a wedge.
            return Err(DriverError::coded(
                "store_unavailable",
                format!(
                    "`{program}` did not finish within {}s and was killed",
                    timeout.as_secs()
                ),
            ));
        };
        Ok(std::process::Output {
            status,
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
        })
    }

    /// Run `alder <args> --json` and read its one JSON document.
    fn run_alder(&self, args: &[&str]) -> Result<Value> {
        let mut full: Vec<&str> = args.to_vec();
        full.push("--json");
        let output = self.run(&self.alder, &full, ALDER_TIMEOUT)?;
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

    fn say(&self, message: &str) {
        let mut stderr = std::io::stderr();
        let _ = writeln!(stderr, "{} alderd: {message}", Utc::now().to_rfc3339());
    }
}

impl Effects for Host {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn alder(&self, args: &[&str]) -> Result<Value> {
        self.run_alder(args)
    }

    fn run_command(&self, command: &str, triggers: &str) -> Result<()> {
        // `sh -c` in the project root, with stdin closed: a command that asks
        // a question must see EOF rather than block a daemon nobody watches.
        // The command's own stdout and stderr pass straight through to the
        // daemon's, so its noise lands in the same place the daemon's does.
        // The run is bounded by `commandTimeoutSeconds`: a hung command is
        // killed and the wake fails — nothing is noted, so the next poll
        // retries it — instead of wedging the daemon forever.
        let mut child = Command::new("/bin/sh")
            .args(["-c", command])
            .env(TRIGGERS_ENV, triggers)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .spawn()
            .map_err(|error| DriverError::new(format!("cannot run the command: {error}")))?;
        let status = wait_bounded(&mut child, self.command_timeout)
            .map_err(|error| DriverError::new(format!("cannot wait for the command: {error}")))?;
        let Some(status) = status else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(DriverError::new(format!(
                "the command was killed after exceeding commandTimeoutSeconds \
                 ({}s); the wake is not noted and the next poll retries it",
                self.command_timeout.as_secs()
            )));
        };
        if status.success() {
            Ok(())
        } else {
            Err(DriverError::new(format!(
                "the command exited with {status}"
            )))
        }
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

    fn write_file(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        std::fs::write(&path, bytes).map_err(|error| {
            DriverError::new(format!("cannot write `{}`: {error}", path.display()))
        })
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

/// Wait for a child within `timeout`. `Ok(None)` means the deadline passed
/// with the child still running; the caller kills and reaps it. This is a
/// limit on a command that can hang, not a wait for anything to come up.
fn wait_bounded(child: &mut Child, timeout: Duration) -> std::io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(WAIT_SLICE);
    }
}

/// Collect one output stream on its own thread, so reading it can never
/// deadlock against the bounded wait above.
fn drain(stream: Option<impl Read + Send + 'static>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut stream) = stream {
            let _ = stream.read_to_end(&mut bytes);
        }
        bytes
    })
}
