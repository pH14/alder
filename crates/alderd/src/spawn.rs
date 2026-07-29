//! Launch one worker for one work item.
//!
//! The whole dispatch is one command: read the item, record an attempt, cut a
//! worktree and a branch, start a tmux session running the engine on the
//! item's goal, and bind the session to the attempt. Before this, dispatch was
//! two commands and a shell script; the ordering rules below are the reason it
//! is worth being one.
//!
//! **The goal is argv, never keystrokes.** The item's title, spec, acceptance
//! checks and gates are composed into one string and handed to the engine as
//! its final argument. Nothing is typed into the pane, so nothing waits for
//! the engine to boot, nothing can be read as a key name, and a goal
//! containing quotes or semicolons is just a string. There is no sleep
//! anywhere on this path.
//!
//! **The pane outlives the engine.** The command ends `; exec bash`, so a
//! one-shot engine that finishes its turn leaves a live session behind. The
//! handle stays observable, `reconcile` stays truthful, and a ruling can be
//! relayed into the pane afterwards.
//!
//! **An attempt is never left phantom.** Everything that can fail before the
//! attempt exists — an unknown item, a session or worktree already there —
//! fails first, with nothing created. After the attempt exists, any failure
//! ends it with the error as its reason and undoes what this run made.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{
    error::{DriverError, Result},
    limits::Limits,
    tier::{Tier, tier},
};

/// The gates every worker is held to. Keep in step with WORKER.md, which
/// states the same list for a worker that needs it after its launch turn.
pub const GATES: &str = "cargo fmt --check, cargo clippy --workspace --all-targets with zero warnings, cargo test --workspace green";

/// Replaces the whole engine invocation, so a test can spawn a stub instead of
/// a model. The goal is still appended as the final argument.
pub const WORKER_CMD_ENV: &str = "ALDER_WORKER_CMD";

/// Everything the spawn does to the world.
///
/// It is a trait for the same reason the driver's [`crate::effects::Effects`]
/// is one: the ordering rules above are the interesting part, and they are
/// worth testing without a tmux server, a git checkout, or a model.
pub trait SpawnHost {
    fn now(&self) -> DateTime<Utc>;
    /// The project the leader dispatches from.
    fn root(&self) -> &Path;
    /// The `alder` binary, as a path that can be copied into a worktree.
    fn alder_binary(&self) -> PathBuf;
    /// Run `alder <args> --json` and return its one JSON document.
    fn alder(&self, args: &[&str]) -> Result<Value>;
    /// Run `git <args>` in the project root. An error means git could not be
    /// run at all; a git command that ran and failed comes back as `Run`.
    fn git(&self, args: &[&str]) -> Result<Run>;
    fn tmux_session_exists(&self, session: &str) -> Result<bool>;
    fn tmux_new_session(&self, session: &str, cwd: &Path, command: &str) -> Result<()>;
    fn tmux_set_environment(&self, session: &str, name: &str, value: &str) -> Result<()>;
    fn tmux_kill_session(&self, session: &str) -> Result<()>;
    fn path_exists(&self, path: &Path) -> bool;
    fn create_dir_all(&self, path: &Path) -> Result<()>;
    fn copy_file(&self, from: &Path, to: &Path) -> Result<()>;
    fn log(&self, message: &str);
}

/// What a shell-out did, once it ran at all.
#[derive(Debug, Clone)]
pub struct Run {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// One dispatched worker.
#[derive(Debug, Clone)]
pub struct Spawned {
    pub work_id: String,
    pub attempt_id: String,
    pub tier: &'static str,
    pub model: &'static str,
    pub effort: &'static str,
    pub session: String,
    pub branch: String,
    pub worktree: PathBuf,
    /// Whether the attempt was already open — a `work start` from a phone, or
    /// a crash between `work start` and the launch.
    pub adopted: bool,
}

impl Spawned {
    pub fn summary(&self) -> String {
        let how = if self.adopted { "adopted" } else { "started" };
        format!(
            "spawned {} on {} at {} (tier {}, model {}, effort {}, attempt {} {how})",
            self.session,
            self.branch,
            self.worktree.display(),
            self.tier,
            self.model,
            self.effort,
            self.attempt_id,
        )
    }
}

/// Pick the rung a dispatch actually runs on.
///
/// A rung whose provider is rate-limited right now is served by the rung of
/// equal standing on the other ladder. If both providers are limited there is
/// nothing better to do than the thing that was asked for, so the requested
/// rung stands and the caller is told why.
pub fn dispatch_tier(
    requested: &'static Tier,
    limits: &Limits,
    now: DateTime<Utc>,
) -> (&'static Tier, Option<String>) {
    let Some(limit) = limits.limited(requested.provider, now) else {
        return (requested, None);
    };
    let counterpart = requested.counterpart();
    if limits.limited(counterpart.provider, now).is_some() {
        return (
            requested,
            Some(format!(
                "both providers are rate-limited; dispatching {} anyway",
                requested.name
            )),
        );
    }
    (
        counterpart,
        Some(format!(
            "{} is rate-limited until {}; dispatching {} as {} instead",
            requested.provider.as_str(),
            limit.until.to_rfc3339(),
            requested.name,
            counterpart.name
        )),
    )
}

/// What a worker is told, composed from the item rather than pointed at it.
///
/// A worker that knows what "done" means from its first token spends no turns
/// discovering it, so the goal carries the title, the spec, every acceptance
/// check, and the gates.
#[derive(Debug, Clone)]
pub struct Brief {
    pub id: String,
    pub title: String,
    pub spec: Option<String>,
    pub checks: Vec<(String, String)>,
}

impl Brief {
    /// Read one `alder show <work> --json` document.
    pub fn from_show(document: &Value) -> Result<Self> {
        let current = document
            .get("current")
            .ok_or_else(|| DriverError::new("`alder show` printed no current state"))?;
        let id = text(current, "id")
            .ok_or_else(|| DriverError::new("`alder show` printed no work ID"))?;
        let title = text(current, "title")
            .ok_or_else(|| DriverError::new(format!("`{id}` has no title to work from")))?;
        let spec = text(current, "spec")
            .map(|spec| spec.trim().to_owned())
            .filter(|spec| !spec.is_empty());
        let checks = current
            .get("checks")
            .and_then(Value::as_array)
            .map(|checks| {
                checks
                    .iter()
                    .filter_map(|check| Some((text(check, "key")?, text(check, "description")?)))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            id,
            title,
            spec,
            checks,
        })
    }

    pub fn goal(&self, attempt_id: &str) -> String {
        let mut parts = vec![
            format!("You are the worker for {}, attempt {attempt_id}.", self.id),
            format!("Goal: {}.", self.title),
        ];
        if let Some(spec) = &self.spec {
            parts.push(format!("Spec: {spec}."));
        }
        if self.checks.is_empty() {
            parts.push("No acceptance checks are recorded; the spec is the whole bar.".to_owned());
        } else {
            let checks: Vec<_> = self
                .checks
                .iter()
                .map(|(key, description)| format!("{key} — {description}"))
                .collect();
            parts.push(format!(
                "Done when every check is satisfied: {}.",
                checks.join("; ")
            ));
        }
        parts.push(format!("Gates: {GATES}."));
        parts.push("Read WORKER.md for the protocol, then begin.".to_owned());
        collapse(&parts.join(" "))
    }
}

/// Collapse every run of whitespace to one space.
///
/// The goal is argv, so a newline in it would be harmless — but it is also
/// echoed into logs and pane titles, and a one-line goal reads there.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// The command the pane runs: the engine, on the goal, ending in a shell.
///
/// `caffeinate -i` keeps the Mac from idle-sleeping under a worker. Every word
/// is quoted, so the goal reaches the engine as one argument however it is
/// spelled, and `exec bash` replaces the engine when it exits so the session —
/// and therefore the handle — survives a one-shot run.
pub fn pane_command(engine: &[String], goal: &str) -> String {
    let mut words = vec!["caffeinate".to_owned(), "-i".to_owned()];
    words.extend(engine.iter().cloned());
    words.push(goal.to_owned());
    let quoted: Vec<_> = words
        .iter()
        .map(|word| crate::effects::quote(word))
        .collect();
    format!("{}; exec bash", quoted.join(" "))
}

/// The engine invocation, before the goal is appended: the tier's own command,
/// or whatever [`WORKER_CMD_ENV`] replaced it with.
pub fn engine_command(tier: &'static Tier, override_command: Option<&str>) -> Vec<String> {
    match override_command {
        Some(command) => command.split_whitespace().map(str::to_owned).collect(),
        None => {
            let mut words = tier.command("");
            words.pop();
            words
        }
    }
}

/// One dispatch, start to finish.
pub fn spawn(
    host: &impl SpawnHost,
    work_id: &str,
    tier: &'static Tier,
    override_command: Option<&str>,
) -> Result<Spawned> {
    let session = format!("alder-work-{work_id}");
    let branch = format!("work/{work_id}");
    let worktree = host
        .root()
        .parent()
        .ok_or_else(|| {
            DriverError::new(format!(
                "`{}` has no parent directory to put a worktree beside",
                host.root().display()
            ))
        })?
        .join(format!("alder-work-{work_id}"));

    // Everything that can be known before anything is created, is. An unknown
    // item, a session that is already there, or a worktree left over from a
    // previous run must fail with no attempt recorded and nothing to clean up.
    if host.tmux_session_exists(&session)? {
        return Err(DriverError::new(format!(
            "session `{session}` already exists"
        )));
    }
    if host.path_exists(&worktree) {
        return Err(DriverError::new(format!(
            "worktree `{}` already exists",
            worktree.display()
        )));
    }
    let brief = Brief::from_show(&host.alder(&["show", work_id])?)?;

    let (attempt_id, adopted) = attempt_for(host, work_id)?;

    // From here the attempt exists, so every exit runs through the same
    // undo: kill what was started, remove what was cut, and end the attempt
    // with the error as its reason.
    let mut made = Made::default();
    let launched = launch(
        host,
        &Launch {
            brief: &brief,
            attempt_id: &attempt_id,
            tier,
            override_command,
            session: &session,
            branch: &branch,
            worktree: &worktree,
        },
        &mut made,
    );
    match launched {
        Ok(()) => Ok(Spawned {
            work_id: work_id.to_owned(),
            attempt_id,
            tier: tier.name,
            model: tier.model,
            effort: tier.effort,
            session,
            branch,
            worktree,
            adopted,
        }),
        Err(error) => {
            undo(host, &made, &session, &worktree);
            end_attempt(host, &attempt_id, &error);
            Err(error)
        }
    }
}

/// The attempt this dispatch runs under, and whether it was already there.
///
/// An open attempt with no handle is not a running worker: it is a `work
/// start` from a phone, or a crash between recording the attempt and
/// launching it. Adopting it is what makes `reconcile`'s suggestion to spawn
/// truthful. An open attempt that *is* bound has a session somewhere, and
/// launching a second worker on the same branch is never the repair.
fn attempt_for(host: &impl SpawnHost, work_id: &str) -> Result<(String, bool)> {
    let in_flight = host.alder(&["status", "--section", "in_flight"])?;
    if let Some(open) = open_attempt(&in_flight, work_id) {
        if let Some(handle) = open.handle {
            return Err(DriverError::new(format!(
                "`{work_id}` already has attempt {} bound to `{handle}`",
                open.id
            )));
        }
        host.log(&format!(
            "adopting the open unbound attempt {} on {work_id}",
            open.id
        ));
        return Ok((open.id, true));
    }
    let started = host.alder(&["work", "start", work_id])?;
    let attempt_id = started
        .get("attempt_id")
        .and_then(Value::as_str)
        .ok_or_else(|| DriverError::new("`alder work start` reported no attempt ID"))?;
    Ok((attempt_id.to_owned(), false))
}

struct OpenAttempt {
    id: String,
    handle: Option<String>,
}

/// The in-flight attempt on one item, from a `status --section in_flight`
/// document.
fn open_attempt(document: &Value, work_id: &str) -> Option<OpenAttempt> {
    document
        .get("in_flight")
        .and_then(Value::as_array)?
        .iter()
        .find(|attempt| text(attempt, "work_id").as_deref() == Some(work_id))
        .and_then(|attempt| {
            Some(OpenAttempt {
                id: text(attempt, "id")?,
                handle: text(attempt, "handle"),
            })
        })
}

struct Launch<'a> {
    brief: &'a Brief,
    attempt_id: &'a str,
    tier: &'static Tier,
    override_command: Option<&'a str>,
    session: &'a str,
    branch: &'a str,
    worktree: &'a Path,
}

#[derive(Default)]
struct Made {
    worktree: bool,
    session: bool,
}

fn launch(host: &impl SpawnHost, launch: &Launch<'_>, made: &mut Made) -> Result<()> {
    let worktree = launch.worktree;
    let branch_exists = host
        .git(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{}", launch.branch),
        ])?
        .ok;
    // A respawn keeps the branch it already has; a first launch cuts one from
    // main. Either way the worktree is new.
    let add: Vec<String> = if branch_exists {
        host.log(&format!("reusing the existing branch {}", launch.branch));
        vec![
            "worktree".to_owned(),
            "add".to_owned(),
            worktree.display().to_string(),
            launch.branch.to_owned(),
        ]
    } else {
        vec![
            "worktree".to_owned(),
            "add".to_owned(),
            worktree.display().to_string(),
            "-b".to_owned(),
            launch.branch.to_owned(),
            "main".to_owned(),
        ]
    };
    let add: Vec<&str> = add.iter().map(String::as_str).collect();
    let added = host.git(&add)?;
    if !added.ok {
        return Err(DriverError::new(format!(
            "git worktree add failed: {}",
            first_line(&added.stderr)
        )));
    }
    made.worktree = true;

    // The machine-local config and binary are gitignored, so they do not
    // travel with the checkout; the worker needs both to reach the log. It
    // gets `alder` and nothing else: dispatch is not a worker's to do.
    host.create_dir_all(&worktree.join(".alder/bin"))?;
    host.copy_file(
        &host.root().join(".alder/config.json"),
        &worktree.join(".alder/config.json"),
    )?;
    host.copy_file(&host.alder_binary(), &worktree.join(".alder/bin/alder"))?;

    let goal = launch.brief.goal(launch.attempt_id);
    let engine = engine_command(launch.tier, launch.override_command);
    host.tmux_new_session(launch.session, worktree, &pane_command(&engine, &goal))?;
    made.session = true;
    // The stamp the tmux observer reads to say which attempt a session is.
    host.tmux_set_environment(launch.session, "ALDER_ATTEMPT", launch.attempt_id)?;

    // Bound last, and only once there is something to bind to: the handle is a
    // claim that a live session exists.
    let handle = format!("tmux:{}", launch.session);
    host.alder(&[
        "attempt",
        "edit",
        launch.attempt_id,
        "--handle",
        &handle,
        "--meta",
        &format!("engine={}", launch.tier.model),
        "--meta",
        &format!("effort={}", launch.tier.effort),
        "--meta",
        &format!("tier={}", launch.tier.name),
    ])?;
    Ok(())
}

/// Undo what this run made, best effort and loudly.
fn undo(host: &impl SpawnHost, made: &Made, session: &str, worktree: &Path) {
    if made.session {
        host.log(&format!("removing the session {session} after a failure"));
        if let Err(error) = host.tmux_kill_session(session) {
            host.log(&format!("could not kill {session}: {error}"));
        }
    }
    if made.worktree {
        host.log(&format!(
            "removing the worktree {} after a failure",
            worktree.display()
        ));
        match host.git(&[
            "worktree",
            "remove",
            "--force",
            &worktree.display().to_string(),
        ]) {
            Ok(run) if !run.ok => host.log(&format!(
                "could not remove {}: {}",
                worktree.display(),
                first_line(&run.stderr)
            )),
            Err(error) => host.log(&format!("could not remove {}: {error}", worktree.display())),
            Ok(_) => {}
        }
    }
}

/// Never leave a phantom in-flight worker behind.
fn end_attempt(host: &impl SpawnHost, attempt_id: &str, error: &DriverError) {
    let why = format!("spawn failed: {error}");
    if let Err(problem) = host.alder(&[
        "attempt",
        "end",
        attempt_id,
        "--outcome",
        "not-started",
        "--why",
        &why,
    ]) {
        host.log(&format!(
            "could not end {attempt_id} after a failed spawn: {problem}"
        ));
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
        .to_owned()
}

/// Resolve the tier a dispatch names, defaulting when it names none.
pub fn requested_tier(name: Option<&str>) -> Result<&'static Tier> {
    tier(name.unwrap_or(crate::tier::DEFAULT_TIER))
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::{BTreeMap, BTreeSet},
    };

    use chrono::Duration;
    use serde_json::json;

    use super::*;
    use crate::tier::Provider;

    fn now() -> DateTime<Utc> {
        "2026-07-29T00:00:00Z".parse().unwrap()
    }

    /// A host that records every call and answers from a script.
    #[derive(Default)]
    struct Fake {
        root: PathBuf,
        calls: RefCell<Vec<String>>,
        /// `alder <first arg> <second>` prefix to the document it answers.
        alder: RefCell<BTreeMap<String, Value>>,
        fail_alder: RefCell<BTreeSet<String>>,
        fail_git: RefCell<BTreeSet<String>>,
        existing_branches: RefCell<BTreeSet<String>>,
        existing_paths: RefCell<BTreeSet<PathBuf>>,
        existing_sessions: RefCell<BTreeSet<String>>,
        fail_tmux: bool,
    }

    impl Fake {
        fn new() -> Self {
            let mut alder = BTreeMap::new();
            alder.insert(
                "show".to_owned(),
                json!({"current": {
                    "id": "al-1",
                    "title": "Make it work",
                    "spec": "docs/SPEC.md",
                    "checks": [
                        {"key": "one", "description": "the first bar"},
                        {"key": "two", "description": "the second bar"},
                    ],
                }}),
            );
            alder.insert("status".to_owned(), json!({"in_flight": []}));
            alder.insert(
                "work start".to_owned(),
                json!({"attempt_id": "al-1-attempt-1"}),
            );
            alder.insert("attempt edit".to_owned(), json!({"ok": true}));
            alder.insert("attempt end".to_owned(), json!({"ok": true}));
            Self {
                root: PathBuf::from("/projects/alder"),
                alder: RefCell::new(alder),
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn called(&self, needle: &str) -> bool {
            self.calls().iter().any(|call| call.contains(needle))
        }
    }

    impl SpawnHost for Fake {
        fn now(&self) -> DateTime<Utc> {
            now()
        }

        fn root(&self) -> &Path {
            &self.root
        }

        fn alder_binary(&self) -> PathBuf {
            PathBuf::from("/projects/alder/target/debug/alder")
        }

        fn alder(&self, args: &[&str]) -> Result<Value> {
            self.calls
                .borrow_mut()
                .push(format!("alder {}", args.join(" ")));
            let one = args.first().copied().unwrap_or_default().to_owned();
            let two = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
            if self.fail_alder.borrow().contains(&one) || self.fail_alder.borrow().contains(&two) {
                return Err(DriverError::new(format!("`alder {two}` failed")));
            }
            let alder = self.alder.borrow();
            alder
                .get(&two)
                .or_else(|| alder.get(&one))
                .cloned()
                .ok_or_else(|| DriverError::new(format!("no scripted answer for `alder {two}`")))
        }

        fn git(&self, args: &[&str]) -> Result<Run> {
            self.calls
                .borrow_mut()
                .push(format!("git {}", args.join(" ")));
            if args.first() == Some(&"rev-parse") {
                let branch = args.last().copied().unwrap_or_default();
                let ok = self
                    .existing_branches
                    .borrow()
                    .iter()
                    .any(|name| branch.ends_with(name.as_str()));
                return Ok(Run {
                    ok,
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            let subcommand = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
            Ok(Run {
                ok: !self.fail_git.borrow().contains(&subcommand),
                stdout: String::new(),
                stderr: "fatal: it did not work".to_owned(),
            })
        }

        fn tmux_session_exists(&self, session: &str) -> Result<bool> {
            Ok(self.existing_sessions.borrow().contains(session))
        }

        fn tmux_new_session(&self, session: &str, cwd: &Path, command: &str) -> Result<()> {
            self.calls.borrow_mut().push(format!(
                "tmux new-session {session} -c {} {command}",
                cwd.display()
            ));
            if self.fail_tmux {
                return Err(DriverError::new("tmux new-session failed: nope"));
            }
            self.existing_sessions
                .borrow_mut()
                .insert(session.to_owned());
            Ok(())
        }

        fn tmux_set_environment(&self, session: &str, name: &str, value: &str) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("tmux set-environment {session} {name} {value}"));
            Ok(())
        }

        fn tmux_kill_session(&self, session: &str) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("tmux kill-session {session}"));
            self.existing_sessions.borrow_mut().remove(session);
            Ok(())
        }

        fn path_exists(&self, path: &Path) -> bool {
            self.existing_paths.borrow().contains(path)
        }

        fn create_dir_all(&self, path: &Path) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("mkdir {}", path.display()));
            Ok(())
        }

        fn copy_file(&self, from: &Path, to: &Path) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("copy {} {}", from.display(), to.display()));
            Ok(())
        }

        fn log(&self, message: &str) {
            self.calls.borrow_mut().push(format!("log {message}"));
        }
    }

    #[test]
    fn a_dispatch_records_starts_launches_and_binds_in_that_order() {
        let host = Fake::new();
        let spawned = spawn(&host, "al-1", tier("luna").unwrap(), None).unwrap();
        assert_eq!(spawned.attempt_id, "al-1-attempt-1");
        assert_eq!(spawned.session, "alder-work-al-1");
        assert_eq!(spawned.branch, "work/al-1");
        assert_eq!(spawned.worktree, Path::new("/projects/alder-work-al-1"));
        assert_eq!(
            (spawned.tier, spawned.model, spawned.effort),
            ("luna", "gpt-5.6-luna", "high")
        );
        assert!(!spawned.adopted);

        let calls = host.calls();
        let ordinal = |needle: &str| {
            calls
                .iter()
                .position(|call| call.contains(needle))
                .unwrap_or_else(|| panic!("{needle} never happened in {calls:#?}"))
        };
        assert!(ordinal("alder show al-1") < ordinal("alder work start al-1"));
        assert!(ordinal("alder work start al-1") < ordinal("git worktree add"));
        assert!(ordinal("git worktree add") < ordinal("tmux new-session"));
        assert!(ordinal("tmux new-session") < ordinal("tmux set-environment"));
        assert!(ordinal("tmux set-environment") < ordinal("alder attempt edit"));

        // The worker gets alder and its config, and nothing else.
        assert!(host.called(
            "copy /projects/alder/.alder/config.json /projects/alder-work-al-1/.alder/config.json"
        ));
        assert!(host.called(
            "copy /projects/alder/target/debug/alder /projects/alder-work-al-1/.alder/bin/alder"
        ));
        assert!(!host.called("alderd"));

        // The handle and the tier are stamped together.
        assert!(host.called(
            "alder attempt edit al-1-attempt-1 --handle tmux:alder-work-al-1 \
             --meta engine=gpt-5.6-luna --meta effort=high --meta tier=luna"
        ));
    }

    #[test]
    fn the_goal_reaches_the_pane_as_argv_and_the_pane_outlives_the_engine() {
        let host = Fake::new();
        spawn(&host, "al-1", tier("luna").unwrap(), None).unwrap();
        let pane = host
            .calls()
            .into_iter()
            .find(|call| call.starts_with("tmux new-session"))
            .expect("a session is created");

        assert!(pane.contains("-c /projects/alder-work-al-1"), "{pane}");
        assert!(pane.ends_with("; exec bash"), "{pane}");
        // Every part of the brief is in the pane's argv.
        for part in [
            "You are the worker for al-1, attempt al-1-attempt-1.",
            "Goal: Make it work.",
            "Spec: docs/SPEC.md.",
            "one — the first bar; two — the second bar",
            "cargo clippy --workspace --all-targets",
            "Read WORKER.md for the protocol, then begin.",
        ] {
            assert!(pane.contains(part), "pane command omits `{part}`: {pane}");
        }
        // Nothing is typed at the session, and nothing waits for it to boot.
        assert!(!host.called("send-keys"));
    }

    #[test]
    fn an_unknown_item_fails_before_anything_is_created() {
        let host = Fake::new();
        host.fail_alder.borrow_mut().insert("show".to_owned());
        let error = spawn(&host, "al-nope", tier("terra").unwrap(), None).unwrap_err();
        assert!(error.message.contains("show"), "{error}");
        assert!(!host.called("work start"));
        assert!(!host.called("git worktree"));
        assert!(!host.called("tmux new-session"));
        assert!(!host.called("attempt end"));
    }

    #[test]
    fn an_existing_session_or_worktree_fails_before_anything_is_created() {
        let host = Fake::new();
        host.existing_sessions
            .borrow_mut()
            .insert("alder-work-al-1".to_owned());
        let error = spawn(&host, "al-1", tier("terra").unwrap(), None).unwrap_err();
        assert!(error.message.contains("already exists"), "{error}");

        let host = Fake::new();
        host.existing_paths
            .borrow_mut()
            .insert(PathBuf::from("/projects/alder-work-al-1"));
        let error = spawn(&host, "al-1", tier("terra").unwrap(), None).unwrap_err();
        assert!(error.message.contains("already exists"), "{error}");
        assert!(!host.called("work start"));
        assert!(!host.called("attempt end"));
    }

    #[test]
    fn a_failure_after_the_attempt_exists_ends_it_and_undoes_the_launch() {
        // Git fails: the attempt is ended, and nothing is left behind.
        let host = Fake::new();
        host.fail_git.borrow_mut().insert("worktree add".to_owned());
        let error = spawn(&host, "al-1", tier("terra").unwrap(), None).unwrap_err();
        assert!(error.message.contains("git worktree add failed"), "{error}");
        assert!(
            host.called(
                "alder attempt end al-1-attempt-1 --outcome not-started --why spawn failed:"
            )
        );
        assert!(!host.called("tmux kill-session"));
        assert!(!host.called("git worktree remove"));

        // tmux fails after the worktree is cut: the worktree goes too.
        let host = Fake {
            fail_tmux: true,
            ..Fake::new()
        };
        let error = spawn(&host, "al-1", tier("terra").unwrap(), None).unwrap_err();
        assert!(error.message.contains("tmux new-session"), "{error}");
        assert!(host.called("git worktree remove --force /projects/alder-work-al-1"));
        assert!(host.called("alder attempt end al-1-attempt-1 --outcome not-started"));

        // The bind fails: the session it would have bound to is killed.
        let host = Fake::new();
        host.fail_alder
            .borrow_mut()
            .insert("attempt edit".to_owned());
        let error = spawn(&host, "al-1", tier("terra").unwrap(), None).unwrap_err();
        assert!(error.message.contains("attempt edit"), "{error}");
        assert!(host.called("tmux kill-session alder-work-al-1"));
        assert!(host.called("git worktree remove --force /projects/alder-work-al-1"));
        assert!(host.called("alder attempt end al-1-attempt-1 --outcome not-started"));
    }

    #[test]
    fn an_open_unbound_attempt_is_adopted_rather_than_doubled() {
        let host = Fake::new();
        host.alder.borrow_mut().insert(
            "status".to_owned(),
            json!({"in_flight": [
                {"id": "al-9-attempt-1", "work_id": "al-9", "handle": null},
                {"id": "al-1-attempt-4", "work_id": "al-1", "handle": null},
            ]}),
        );
        let spawned = spawn(&host, "al-1", tier("terra").unwrap(), None).unwrap();
        assert_eq!(spawned.attempt_id, "al-1-attempt-4");
        assert!(spawned.adopted);
        assert!(!host.called("work start"));
        assert!(host.called("alder attempt edit al-1-attempt-4 --handle tmux:alder-work-al-1"));
    }

    #[test]
    fn a_bound_attempt_means_a_live_worker_and_is_never_doubled() {
        let host = Fake::new();
        host.alder.borrow_mut().insert(
            "status".to_owned(),
            json!({"in_flight": [
                {"id": "al-1-attempt-1", "work_id": "al-1", "handle": "tmux:alder-work-al-1"},
            ]}),
        );
        let error = spawn(&host, "al-1", tier("terra").unwrap(), None).unwrap_err();
        assert!(error.message.contains("already has attempt"), "{error}");
        assert!(!host.called("git worktree"));
        assert!(!host.called("attempt end"));
    }

    #[test]
    fn a_respawn_reuses_the_branch_it_already_has() {
        let host = Fake::new();
        host.existing_branches
            .borrow_mut()
            .insert("work/al-1".to_owned());
        spawn(&host, "al-1", tier("terra").unwrap(), None).unwrap();
        assert!(host.called("git worktree add /projects/alder-work-al-1 work/al-1"));
        assert!(!host.called("-b work/al-1"));
    }

    #[test]
    fn the_stub_override_replaces_the_engine_and_still_gets_the_goal() {
        let host = Fake::new();
        spawn(
            &host,
            "al-1",
            tier("sol").unwrap(),
            Some("/tmp/stub.sh --once"),
        )
        .unwrap();
        let pane = host
            .calls()
            .into_iter()
            .find(|call| call.starts_with("tmux new-session"))
            .expect("a session is created");
        assert!(pane.contains("'/tmp/stub.sh' '--once'"), "{pane}");
        assert!(!pane.contains("codex"), "{pane}");
        assert!(pane.contains("You are the worker for al-1"), "{pane}");
        // The tier is still what the attempt records, stub or no stub.
        assert!(host.called("--meta engine=gpt-5.6-sol --meta effort=xhigh --meta tier=sol"));
    }

    #[test]
    fn an_item_with_no_checks_says_so_rather_than_leaving_a_gap() {
        let brief = Brief::from_show(&json!({"current": {
            "id": "al-2",
            "title": "Tidy the docs",
            "spec": null,
            "checks": [],
        }}))
        .unwrap();
        let goal = brief.goal("al-2-attempt-1");
        assert!(goal.contains("No acceptance checks are recorded"), "{goal}");
        assert!(!goal.contains("Spec:"), "{goal}");
        assert!(goal.contains(GATES), "{goal}");
    }

    #[test]
    fn a_goal_is_one_line_however_the_item_is_written() {
        let brief = Brief::from_show(&json!({"current": {
            "id": "al-3",
            "title": "Fix   the\nthing",
            "spec": "  docs/A.md  ",
            "checks": [{"key": "k", "description": "line one\nline two"}],
        }}))
        .unwrap();
        let goal = brief.goal("al-3-attempt-1");
        assert!(!goal.contains('\n'), "{goal}");
        assert!(goal.contains("Fix the thing"), "{goal}");
        assert!(goal.contains("Spec: docs/A.md."), "{goal}");
        assert!(goal.contains("line one line two"), "{goal}");
    }

    #[test]
    fn a_show_document_that_is_not_one_is_rejected() {
        assert!(Brief::from_show(&json!({})).is_err());
        assert!(Brief::from_show(&json!({"current": {"title": "no id"}})).is_err());
        assert!(Brief::from_show(&json!({"current": {"id": "al-4"}})).is_err());
    }

    #[test]
    fn a_rate_limited_provider_is_served_by_the_other_ladder() {
        let mut limits = Limits::default();
        limits.set(Provider::Codex, now() + Duration::hours(1), None);
        let (rung, why) = dispatch_tier(tier("terra").unwrap(), &limits, now());
        assert_eq!(rung.name, "opus");
        assert!(why.unwrap().contains("rate-limited"));

        // An expired limit is no limit.
        let (rung, why) =
            dispatch_tier(tier("terra").unwrap(), &limits, now() + Duration::hours(2));
        assert_eq!(rung.name, "terra");
        assert!(why.is_none());

        // Nothing limited: what was asked for.
        let (rung, why) = dispatch_tier(tier("luna").unwrap(), &Limits::default(), now());
        assert_eq!(rung.name, "luna");
        assert!(why.is_none());

        // Both limited: what was asked for, and a reason why it stands.
        limits.set(Provider::Claude, now() + Duration::hours(1), None);
        let (rung, why) = dispatch_tier(tier("sol").unwrap(), &limits, now());
        assert_eq!(rung.name, "sol");
        assert!(why.unwrap().contains("both providers"));
    }

    #[test]
    fn the_default_tier_is_terra_and_an_unknown_one_never_launches() {
        assert_eq!(requested_tier(None).unwrap().name, "terra");
        assert_eq!(requested_tier(Some("fable")).unwrap().name, "fable");
        assert!(requested_tier(Some("gpt-5.6-luna")).is_err());
    }
}
