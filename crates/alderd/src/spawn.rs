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
//! **Repair adopts its own residue.** Each effect says enough about its
//! identity for the next run to converge: a worktree is accepted only on the
//! expected branch, a session is stamped with its attempt as it is created,
//! and an open unbound attempt is reused rather than doubled.

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

pub(crate) const ATTEMPT_ENV: &str = "ALDER_ATTEMPT";
pub(crate) const ENGINE_ENV: &str = "ALDER_ENGINE";
pub(crate) const ENGINE_RUNNING: &str = "running";
pub(crate) const ENGINE_EXITED: &str = "exited";

/// The identity and engine state observable on an existing tmux session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedSession {
    pub attempt_id: Option<String>,
    pub engine_live: bool,
}

/// Everything the spawn does to the world.
///
/// It is a trait for the same reason the driver's [`crate::effects::Effects`]
/// is one: the ordering rules above are the interesting part, and they are
/// worth testing without a tmux server, a git checkout, or a model.
pub trait SpawnHost {
    /// The project the leader dispatches from.
    fn root(&self) -> &Path;
    /// The `alder` binary, as a path that can be copied into a worktree.
    fn alder_binary(&self) -> PathBuf;
    /// Run `alder <args> --json` and return its one JSON document.
    fn alder(&self, args: &[&str]) -> Result<Value>;
    /// Run `git <args>` in the project root. An error means git could not be
    /// run at all; a git command that ran and failed comes back as `Run`.
    fn git(&self, args: &[&str]) -> Result<Run>;
    fn tmux_session(&self, session: &str) -> Result<Option<ObservedSession>>;
    fn tmux_new_session(
        &self,
        session: &str,
        cwd: &Path,
        command: &str,
        attempt_id: &str,
    ) -> Result<()>;
    fn tmux_kill_session(&self, session: &str) -> Result<()>;
    fn path_exists(&self, path: &Path) -> bool;
    fn create_dir_all(&self, path: &Path) -> Result<()>;
    fn copy_file(&self, from: &Path, to: &Path) -> Result<()>;
    fn write_executable(&self, path: &Path, body: &str) -> Result<()>;
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
pub fn pane_command(engine: &[String], goal: &str, session: &str) -> String {
    let mut words = vec!["caffeinate".to_owned(), "-i".to_owned()];
    words.extend(engine.iter().cloned());
    words.push(goal.to_owned());
    let quoted: Vec<_> = words
        .iter()
        .map(|word| crate::effects::quote(word))
        .collect();
    let target = crate::effects::quote(&format!("={session}"));
    format!(
        "{}; tmux set-environment -t {target} {ENGINE_ENV} {ENGINE_EXITED}; exec bash",
        quoted.join(" ")
    )
}

/// The engine invocation, before the goal is appended: the tier's own command,
/// or whatever [`WORKER_CMD_ENV`] replaced it with.
pub fn engine_command(
    tier: &'static Tier,
    git_common_dir: Option<&str>,
    override_command: Option<&str>,
) -> Vec<String> {
    match override_command {
        Some(command) => command.split_whitespace().map(str::to_owned).collect(),
        None => {
            let mut words = tier.command("", git_common_dir);
            words.pop();
            words
        }
    }
}

/// The dispatching project's `.git`, which a worker's linked worktree keeps
/// its index, objects and branch ref inside. Absolute, because it is handed to
/// a sandbox that has no idea what the worker's working directory is.
///
/// Falling back to `<root>/.git` if git cannot answer is deliberate: a worker
/// that cannot commit is useless, and the fallback is right for every ordinary
/// checkout.
fn git_common_dir(host: &impl SpawnHost) -> String {
    let answer = host.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    match answer {
        Ok(run) if run.ok && !run.stdout.trim().is_empty() => run.stdout.trim().to_owned(),
        _ => {
            let fallback = host.root().join(".git");
            host.log(&format!(
                "git could not name its common directory; assuming {}",
                fallback.display()
            ));
            fallback.display().to_string()
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

    let brief = Brief::from_show(&host.alder(&["show", work_id])?)?;
    let worktree_present = verify_worktree(host, &worktree, &branch)?;
    let observed = host.tmux_session(&session)?;
    let open = current_attempt(host, work_id)?;

    if let Some(open) = open {
        if let Some(handle) = &open.handle {
            let expected_handle = format!("tmux:{session}");
            if handle != &expected_handle {
                return Err(DriverError::new(format!(
                    "`{work_id}` already has attempt {} bound to `{handle}`",
                    open.id
                )));
            }
            let observed = observed.ok_or_else(|| {
                DriverError::new(format!(
                    "`{work_id}` has attempt {} bound to `{handle}`, but that session is gone",
                    open.id
                ))
            })?;
            verify_session_identity(&session, &observed, &open.id)?;
            if observed.engine_live {
                return Err(DriverError::new(format!(
                    "session `{session}` is already running attempt {}",
                    open.id
                )));
            }
            if !worktree_present {
                return Err(DriverError::new(format!(
                    "session `{session}` holds attempt {}, but worktree `{}` is gone",
                    open.id,
                    worktree.display()
                )));
            }
            host.log(&format!(
                "adopting the exited pane {session} already bound to {}",
                open.id
            ));
            return Ok(spawned(
                work_id, open.id, tier, session, branch, worktree, true,
            ));
        }

        host.log(&format!(
            "adopting the open unbound attempt {} on {work_id}",
            open.id
        ));
        if let Some(observed) = observed {
            if observed.attempt_id.as_deref() == Some(open.id.as_str()) {
                if !worktree_present {
                    return Err(DriverError::new(format!(
                        "session `{session}` holds attempt {}, but worktree `{}` is gone",
                        open.id,
                        worktree.display()
                    )));
                }
                host.log(&format!(
                    "adopting the session {session} left before attempt {} was bound",
                    open.id
                ));
                bind_attempt(host, &open.id, &session, tier)?;
                return Ok(spawned(
                    work_id, open.id, tier, session, branch, worktree, true,
                ));
            }
            if observed.engine_live {
                return Err(running_session_error(&session, &observed));
            }
            host.log(&format!(
                "replacing the exited pane {session}, which is not attempt {}",
                open.id
            ));
            host.tmux_kill_session(&session)?;
        }
        return launch_attempt(
            host,
            &brief,
            open.id,
            true,
            worktree_present,
            tier,
            override_command,
            session,
            branch,
            worktree,
        );
    }

    if let Some(observed) = observed {
        if observed.engine_live {
            return Err(running_session_error(&session, &observed));
        }
        host.log(&format!(
            "replacing the exited pane {session}, which has no open attempt"
        ));
        host.tmux_kill_session(&session)?;
    }

    let started = host.alder(&["work", "start", work_id])?;
    let attempt_id = started
        .get("attempt_id")
        .and_then(Value::as_str)
        .ok_or_else(|| DriverError::new("`alder work start` reported no attempt ID"))?
        .to_owned();
    launch_attempt(
        host,
        &brief,
        attempt_id,
        false,
        worktree_present,
        tier,
        override_command,
        session,
        branch,
        worktree,
    )
}

#[allow(clippy::too_many_arguments)]
fn launch_attempt(
    host: &impl SpawnHost,
    brief: &Brief,
    attempt_id: String,
    adopted: bool,
    worktree_present: bool,
    tier: &'static Tier,
    override_command: Option<&str>,
    session: String,
    branch: String,
    worktree: PathBuf,
) -> Result<Spawned> {
    let mut made = Made::default();
    let launched = launch(
        host,
        &Launch {
            brief,
            attempt_id: &attempt_id,
            tier,
            override_command,
            session: &session,
            branch: &branch,
            worktree: &worktree,
        },
        worktree_present,
        &mut made,
    );
    match launched {
        Ok(()) => Ok(spawned(
            &brief.id, attempt_id, tier, session, branch, worktree, adopted,
        )),
        Err(error) => {
            undo(host, &made, &session, &worktree);
            end_attempt(host, &attempt_id, &error);
            Err(error)
        }
    }
}

fn spawned(
    work_id: &str,
    attempt_id: String,
    tier: &'static Tier,
    session: String,
    branch: String,
    worktree: PathBuf,
    adopted: bool,
) -> Spawned {
    Spawned {
        work_id: work_id.to_owned(),
        attempt_id,
        tier: tier.name,
        model: tier.model,
        effort: tier.effort,
        session,
        branch,
        worktree,
        adopted,
    }
}

fn current_attempt(host: &impl SpawnHost, work_id: &str) -> Result<Option<OpenAttempt>> {
    let in_flight = host.alder(&["status", "--section", "in_flight"])?;
    Ok(open_attempt(&in_flight, work_id))
}

#[derive(Debug)]
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

fn verify_worktree(host: &impl SpawnHost, worktree: &Path, branch: &str) -> Result<bool> {
    if !host.path_exists(worktree) {
        return Ok(false);
    }
    let path = worktree.display().to_string();
    let run = host.git(&["-C", &path, "symbolic-ref", "--quiet", "--short", "HEAD"])?;
    if !run.ok {
        return Err(DriverError::new(format!(
            "cannot adopt worktree `{path}`: {}",
            first_line(&run.stderr)
        )));
    }
    let actual = run.stdout.trim();
    if actual != branch {
        return Err(DriverError::new(format!(
            "cannot adopt worktree `{path}`: it is on branch `{actual}`, expected `{branch}`"
        )));
    }
    host.log(&format!(
        "adopting the existing worktree {path} on {branch}"
    ));
    Ok(true)
}

fn verify_session_identity(
    session: &str,
    observed: &ObservedSession,
    attempt_id: &str,
) -> Result<()> {
    if let Some(actual) = observed.attempt_id.as_deref()
        && actual != attempt_id
    {
        return Err(DriverError::new(format!(
            "session `{session}` belongs to attempt `{actual}`, not `{attempt_id}`"
        )));
    }
    Ok(())
}

fn running_session_error(session: &str, observed: &ObservedSession) -> DriverError {
    match observed.attempt_id.as_deref() {
        Some(attempt_id) => DriverError::new(format!(
            "session `{session}` is already running attempt {attempt_id}"
        )),
        None => DriverError::new(format!(
            "session `{session}` already has a live engine with no attempt identity"
        )),
    }
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

fn launch(
    host: &impl SpawnHost,
    launch: &Launch<'_>,
    worktree_present: bool,
    made: &mut Made,
) -> Result<()> {
    let worktree = launch.worktree;
    if !worktree_present {
        let branch_exists = host
            .git(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", launch.branch),
            ])?
            .ok;
        // A respawn keeps the branch it already has; a first launch cuts one
        // from main.
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
    }

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
    let git_common_dir = git_common_dir(host);
    let engine = engine_command(launch.tier, Some(&git_common_dir), launch.override_command);
    // How a ruling gets back into a worker that ran one shot and exited. It
    // lives in the worktree because that is where the pane's shell is sitting,
    // and it is written by the table that built the launch so the two cannot
    // drift apart.
    if let Some(script) = launch.tier.resume_script(Some(&git_common_dir)) {
        host.write_executable(&worktree.join(".alder/resume"), &script)?;
    }
    host.tmux_new_session(
        launch.session,
        worktree,
        &pane_command(&engine, &goal, launch.session),
        launch.attempt_id,
    )?;
    made.session = true;

    // Bound last, and only once there is something to bind to: the handle is a
    // claim that a live session exists.
    bind_attempt(host, launch.attempt_id, launch.session, launch.tier)?;
    Ok(())
}

fn bind_attempt(
    host: &impl SpawnHost,
    attempt_id: &str,
    session: &str,
    tier: &'static Tier,
) -> Result<()> {
    let handle = format!("tmux:{session}");
    host.alder(&[
        "attempt",
        "edit",
        attempt_id,
        "--handle",
        &handle,
        "--meta",
        &format!("engine={}", tier.model),
        "--meta",
        &format!("effort={}", tier.effort),
        "--meta",
        &format!("tier={}", tier.name),
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
        worktrees: RefCell<BTreeMap<PathBuf, String>>,
        sessions: RefCell<BTreeMap<String, ObservedSession>>,
        crash_after: RefCell<Option<&'static str>>,
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

        fn crash_if(&self, effect: &'static str) {
            if self.crash_after.borrow().as_ref() == Some(&effect) {
                self.crash_after.borrow_mut().take();
                panic!("simulated process crash after {effect}");
            }
        }
    }

    impl SpawnHost for Fake {
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
            let answer = {
                let alder = self.alder.borrow();
                alder
                    .get(&two)
                    .or_else(|| alder.get(&one))
                    .cloned()
                    .ok_or_else(|| {
                        DriverError::new(format!("no scripted answer for `alder {two}`"))
                    })?
            };
            if two == "work start" {
                let work_id = args.get(2).copied().unwrap_or("al-1");
                let attempt_id = answer
                    .get("attempt_id")
                    .and_then(Value::as_str)
                    .unwrap_or("al-1-attempt-1");
                self.alder.borrow_mut().insert(
                    "status".to_owned(),
                    json!({"in_flight": [{
                        "id": attempt_id,
                        "work_id": work_id,
                        "handle": null,
                    }]}),
                );
                self.crash_if("work start");
            } else if two == "attempt edit" && args.contains(&"--handle") {
                let attempt_id = args.get(2).copied().unwrap_or("al-1-attempt-1");
                let handle = args
                    .iter()
                    .position(|arg| *arg == "--handle")
                    .and_then(|index| args.get(index + 1))
                    .copied()
                    .unwrap_or("tmux:alder-work-al-1");
                self.alder.borrow_mut().insert(
                    "status".to_owned(),
                    json!({"in_flight": [{
                        "id": attempt_id,
                        "work_id": "al-1",
                        "handle": handle,
                    }]}),
                );
                self.crash_if("attempt edit");
            } else if two == "attempt end" {
                self.alder
                    .borrow_mut()
                    .insert("status".to_owned(), json!({"in_flight": []}));
            }
            Ok(answer)
        }

        fn git(&self, args: &[&str]) -> Result<Run> {
            self.calls
                .borrow_mut()
                .push(format!("git {}", args.join(" ")));
            if args.contains(&"--git-common-dir") {
                return Ok(Run {
                    ok: true,
                    stdout: "/projects/alder/.git\n".to_owned(),
                    stderr: String::new(),
                });
            }
            if args.first() == Some(&"-C") {
                let path = PathBuf::from(args.get(1).copied().unwrap_or_default());
                return Ok(match self.worktrees.borrow().get(&path) {
                    Some(branch) => Run {
                        ok: true,
                        stdout: format!("{branch}\n"),
                        stderr: String::new(),
                    },
                    None => Run {
                        ok: false,
                        stdout: String::new(),
                        stderr: "fatal: not a worktree".to_owned(),
                    },
                });
            }
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
            if subcommand == "worktree add" && !self.fail_git.borrow().contains(&subcommand) {
                let path = PathBuf::from(args.get(2).copied().unwrap_or_default());
                let branch = if args.get(3) == Some(&"-b") {
                    args.get(4).copied().unwrap_or_default()
                } else {
                    args.get(3).copied().unwrap_or_default()
                };
                self.worktrees.borrow_mut().insert(path, branch.to_owned());
                self.crash_if("worktree add");
            } else if subcommand == "worktree remove" {
                let path = args.last().copied().unwrap_or_default();
                self.worktrees.borrow_mut().remove(Path::new(path));
            }
            Ok(Run {
                ok: !self.fail_git.borrow().contains(&subcommand),
                stdout: String::new(),
                stderr: "fatal: it did not work".to_owned(),
            })
        }

        fn tmux_session(&self, session: &str) -> Result<Option<ObservedSession>> {
            Ok(self.sessions.borrow().get(session).cloned())
        }

        fn tmux_new_session(
            &self,
            session: &str,
            cwd: &Path,
            command: &str,
            attempt_id: &str,
        ) -> Result<()> {
            self.calls.borrow_mut().push(format!(
                "tmux new-session {session} -c {} -e {ATTEMPT_ENV}={attempt_id} \
                 -e {ENGINE_ENV}={ENGINE_RUNNING} {command}",
                cwd.display(),
            ));
            if self.fail_tmux {
                return Err(DriverError::new("tmux new-session failed: nope"));
            }
            self.sessions.borrow_mut().insert(
                session.to_owned(),
                ObservedSession {
                    attempt_id: Some(attempt_id.to_owned()),
                    engine_live: true,
                },
            );
            self.crash_if("tmux new-session");
            Ok(())
        }

        fn tmux_kill_session(&self, session: &str) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("tmux kill-session {session}"));
            self.sessions.borrow_mut().remove(session);
            Ok(())
        }

        fn path_exists(&self, path: &Path) -> bool {
            self.worktrees.borrow().contains_key(path)
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

        fn write_executable(&self, path: &Path, body: &str) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("write {} {body}", path.display()));
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
        assert!(ordinal("tmux new-session") < ordinal("alder attempt edit"));
        // Identity is part of new-session itself. There is no crash window
        // where the pane exists but its attempt cannot be observed.
        assert!(host.called(
            "tmux new-session alder-work-al-1 -c /projects/alder-work-al-1 \
             -e ALDER_ATTEMPT=al-1-attempt-1 -e ALDER_ENGINE=running"
        ));

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
    fn a_codex_worker_is_given_the_git_common_dir_it_must_commit_through() {
        let host = Fake::new();
        spawn(&host, "al-1", tier("luna").unwrap(), None).unwrap();
        let pane = host
            .calls()
            .into_iter()
            .find(|call| call.starts_with("tmux new-session"))
            .expect("a session is created");
        // Without this the worker's first commit dies on index.lock: its
        // worktree keeps index, objects and branch ref in the project's .git.
        assert!(
            pane.contains(r#"'sandbox_workspace_write.writable_roots=["/projects/alder/.git"]'"#),
            "{pane}"
        );

        // The relay back into a one-shot worker is written where its shell
        // will be sitting, carrying the same rung.
        assert!(host.called("write /projects/alder-work-al-1/.alder/resume"));
        assert!(host.called("codex exec resume"));

        // A claude worker is not sandboxed this way and is given no such root,
        // and nothing to resume: it sits at a prompt and is typed at.
        let host = Fake::new();
        spawn(&host, "al-1", tier("opus").unwrap(), None).unwrap();
        assert!(!host.called("writable_roots"));
        assert!(!host.called(".alder/resume"));
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
    fn a_live_unattributed_session_is_refused_before_an_attempt_is_created() {
        let host = Fake::new();
        host.sessions.borrow_mut().insert(
            "alder-work-al-1".to_owned(),
            ObservedSession {
                attempt_id: None,
                engine_live: true,
            },
        );
        let error = spawn(&host, "al-1", tier("terra").unwrap(), None).unwrap_err();
        assert!(error.message.contains("live engine"), "{error}");
        assert!(!host.called("work start"));
    }

    #[test]
    fn an_existing_worktree_is_adopted_only_on_the_expected_branch() {
        let host = Fake::new();
        host.worktrees.borrow_mut().insert(
            PathBuf::from("/projects/alder-work-al-1"),
            "work/al-other".to_owned(),
        );
        let error = spawn(&host, "al-1", tier("terra").unwrap(), None).unwrap_err();
        assert!(error.message.contains("work/al-other"), "{error}");
        assert!(error.message.contains("expected `work/al-1`"), "{error}");
        assert!(!host.called("work start"));
        assert!(!host.called("attempt end"));

        let host = Fake::new();
        host.worktrees.borrow_mut().insert(
            PathBuf::from("/projects/alder-work-al-1"),
            "work/al-1".to_owned(),
        );
        spawn(&host, "al-1", tier("terra").unwrap(), None).unwrap();
        assert!(!host.called("git worktree add"));
        assert!(host.called("adopting the existing worktree"));
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
        host.sessions.borrow_mut().insert(
            "alder-work-al-1".to_owned(),
            ObservedSession {
                attempt_id: Some("al-1-attempt-1".to_owned()),
                engine_live: true,
            },
        );
        let error = spawn(&host, "al-1", tier("terra").unwrap(), None).unwrap_err();
        assert!(error.message.contains("already running"), "{error}");
        assert!(!host.called("work start"));
        assert!(!host.called("git worktree"));
        assert!(!host.called("attempt end"));
    }

    #[test]
    fn a_process_crash_after_each_effect_converges_on_exactly_one_attempt() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        for boundary in [
            "work start",
            "worktree add",
            "tmux new-session",
            "attempt edit",
        ] {
            let host = Fake::new();
            host.crash_after.borrow_mut().replace(boundary);
            let crashed = catch_unwind(AssertUnwindSafe(|| {
                let _ = spawn(&host, "al-1", tier("terra").unwrap(), None);
            }));
            assert!(crashed.is_err(), "{boundary} did not crash");

            let repaired = spawn(&host, "al-1", tier("terra").unwrap(), None);
            if boundary == "attempt edit" {
                let error = repaired.expect_err("a bound live engine is already converged");
                assert!(error.message.contains("already running"), "{error}");
            } else {
                let spawned = repaired
                    .unwrap_or_else(|error| panic!("repair after {boundary} failed: {error}"));
                assert_eq!(spawned.attempt_id, "al-1-attempt-1", "{boundary}");
                assert!(spawned.adopted, "{boundary}");
            }
            assert_eq!(
                host.calls()
                    .iter()
                    .filter(|call| call.starts_with("alder work start"))
                    .count(),
                1,
                "{boundary}: {:#?}",
                host.calls()
            );
            assert_eq!(
                host.calls()
                    .iter()
                    .filter(|call| call.starts_with("git worktree add"))
                    .count(),
                1,
                "{boundary}: {:#?}",
                host.calls()
            );
            assert_eq!(
                host.calls()
                    .iter()
                    .filter(|call| call.starts_with("tmux new-session"))
                    .count(),
                1,
                "{boundary}: {:#?}",
                host.calls()
            );
            assert_eq!(
                host.calls()
                    .iter()
                    .filter(|call| call.starts_with("alder attempt edit"))
                    .count(),
                1,
                "{boundary}: {:#?}",
                host.calls()
            );
        }
    }

    #[test]
    fn an_exited_unattributed_pane_is_replaced() {
        let host = Fake::new();
        host.sessions.borrow_mut().insert(
            "alder-work-al-1".to_owned(),
            ObservedSession {
                attempt_id: None,
                engine_live: false,
            },
        );
        let spawned = spawn(&host, "al-1", tier("terra").unwrap(), None).unwrap();
        assert_eq!(spawned.attempt_id, "al-1-attempt-1");
        let calls = host.calls();
        let killed = calls
            .iter()
            .position(|call| call == "tmux kill-session alder-work-al-1")
            .unwrap();
        let started = calls
            .iter()
            .position(|call| call.contains("alder work start al-1"))
            .unwrap();
        let launched = calls
            .iter()
            .position(|call| call.starts_with("tmux new-session"))
            .unwrap();
        assert!(killed < started && started < launched, "{calls:#?}");
    }

    #[test]
    fn an_exited_bound_pane_is_adopted_without_relaunching() {
        let host = Fake::new();
        host.alder.borrow_mut().insert(
            "status".to_owned(),
            json!({"in_flight": [{
                "id": "al-1-attempt-1",
                "work_id": "al-1",
                "handle": "tmux:alder-work-al-1",
            }]}),
        );
        host.worktrees.borrow_mut().insert(
            PathBuf::from("/projects/alder-work-al-1"),
            "work/al-1".to_owned(),
        );
        host.sessions.borrow_mut().insert(
            "alder-work-al-1".to_owned(),
            ObservedSession {
                attempt_id: Some("al-1-attempt-1".to_owned()),
                engine_live: false,
            },
        );

        let spawned = spawn(&host, "al-1", tier("terra").unwrap(), None).unwrap();
        assert!(spawned.adopted);
        assert!(!host.called("work start"));
        assert!(!host.called("worktree add"));
        assert!(!host.called("tmux new-session"));
        assert!(!host.called("attempt edit"));
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
