//! Launch one execution: a prompt, a model at some effort, a branch to leave
//! the result on, and a handle to reach it by.
//!
//! The whole start is one command: cut or adopt a worktree on the given
//! branch, start a tmux session running the tier's engine on the prompt, and
//! print the handle. The runner knows nothing about what the prompt says or
//! what the result means — the branch is where the result lives, and that is
//! the whole contract.
//!
//! **The prompt is argv, never keystrokes.** The prompt file's contents are
//! handed to the engine as its final argument. Nothing is typed into the
//! pane, so nothing waits for the engine to boot, nothing can be read as a
//! key name, and a prompt containing quotes or semicolons is just a string.
//! There is no sleep anywhere on this path. Three tests below hold that, and
//! all three are titled for it: one counts the questions a start puts to the
//! world, and two read the source for a duration or a clock it could wait on
//! — this module's own half, and the two halves of [`crate::host::Host`] the
//! start runs in.
//!
//! **The pane outlives the engine.** The command ends `; exec bash`, so a
//! one-shot engine that finishes its turn leaves a live session behind. The
//! handle stays observable (`status` says `done` rather than `dead`), and a
//! later message can still be delivered with `send`.
//!
//! **Repair adopts its own residue.** A worktree is accepted only on the
//! expected branch, an unregistered directory left by a torn `git worktree
//! add` is swept before a retry, and a session is stamped with its identity
//! as it is created. A live engine under the same handle is refused; an
//! exited pane is replaced, because `start` means "run this prompt" and an
//! exited pane already ran its own.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::{
    error::{Result, RunnerError},
    host::{EngineMarker, RunnerHost},
    limits::Limits,
    tier::Tier,
};

/// Replaces the whole engine invocation, so a test can start a stub instead
/// of a model. The prompt is still appended as the final argument.
pub const RUNNER_CMD_ENV: &str = "ALDER_EXT_RUNNER_CMD";

/// The directory inside a worktree where the runner keeps its own files: the
/// codex resume script, the codex session marker, and the stamp sidecar's
/// log. Nothing else writes here and nothing here is the execution's output.
pub const RUNNER_DIR: &str = ".alder-ext-runner";

/// The session environment the runner stamps at creation, and reads back for
/// adoption, `status`, and `send`. These are the runner's own names: the
/// runner stamps nothing of anyone else's into a session.
pub(crate) const HANDLE_ENV: &str = "ALDER_EXT_RUNNER_HANDLE";
pub(crate) const ENGINE_ENV: &str = "ALDER_EXT_RUNNER_ENGINE";
pub(crate) const TIER_ENV: &str = "ALDER_EXT_RUNNER_TIER";
pub(crate) const WORKTREE_ENV: &str = "ALDER_EXT_RUNNER_WORKTREE";
/// Stamped by `send` when a delivery tore between paste and Enter, so the
/// pane refuses further sends until a human (or `--force`) resolves it.
pub(crate) const TORN_ENV: &str = "ALDER_EXT_RUNNER_TORN";
pub(crate) const ENGINE_RUNNING: &str = "running";
pub(crate) const ENGINE_EXITED: &str = "exited";

/// One started execution.
#[derive(Debug, Clone)]
pub struct Started {
    /// The opaque handle `status`, `send`, and `kill` take. It is also the
    /// tmux session name, but callers never need to know that.
    pub handle: String,
    pub tier: &'static str,
    pub model: &'static str,
    pub effort: &'static str,
    pub branch: String,
    pub worktree: PathBuf,
    /// Whether an existing worktree on the branch was adopted rather than cut.
    pub adopted_worktree: bool,
}

impl Started {
    pub fn summary(&self) -> String {
        let how = if self.adopted_worktree {
            "adopting its worktree"
        } else {
            "cutting a worktree"
        };
        format!(
            "started {} on {} at {} (tier {}, model {}, effort {}, {how})",
            self.handle,
            self.branch,
            self.worktree.display(),
            self.tier,
            self.model,
            self.effort,
        )
    }
}

/// The handle for one branch: deterministic, so a crashed start re-run against
/// the same branch converges on the same session instead of doubling it.
///
/// Known, accepted risk: the slug is lossy (`work/al-1` and `work_al_1` both
/// become `alder-ext-work-al-1`), so two colliding branches share a handle,
/// and the "kill it before starting" refusal can then attribute the running
/// session to the wrong branch — an operator following that message could
/// kill the other branch's execution. A collision refusal was considered and
/// rejected; this comment is the documentation of the risk.
pub fn handle_for_branch(branch: &str) -> String {
    let slug: String = branch
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("alder-ext-{slug}")
}

/// Pick the rung a start actually runs on.
///
/// A rung whose provider is rate-limited right now is served by the rung of
/// equal standing on the other ladder. If both providers are limited there is
/// nothing better to do than the thing that was asked for, so the requested
/// rung stands and the caller is told why.
pub fn dispatch_tier<'table>(
    table: &'table [Tier],
    requested: &'table Tier,
    limits: &Limits,
    now: DateTime<Utc>,
) -> (&'table Tier, Option<String>) {
    let Some(limit) = limits.limited(requested.provider, now) else {
        return (requested, None);
    };
    let counterpart = requested.counterpart(table);
    if limits.limited(counterpart.provider, now).is_some() {
        return (
            requested,
            Some(format!(
                "both providers are rate-limited; starting {} anyway",
                requested.name
            )),
        );
    }
    (
        counterpart,
        Some(format!(
            "{} is rate-limited until {}; starting {} as {} instead",
            requested.provider.as_str(),
            limit.until.to_rfc3339(),
            requested.name,
            counterpart.name
        )),
    )
}

/// The command the pane runs: the engine, on the prompt, ending in a shell.
///
/// Every word is quoted, so the prompt reaches the engine as one argument
/// however it is spelled, and `exec bash` replaces the engine when it exits so
/// the session — and therefore the handle — survives a one-shot run. A Codex
/// launch starts its session-ID watcher before the engine; that watcher is
/// independent of the model's first tool call and returns immediately rather
/// than waiting for Codex to boot.
///
/// Nothing wraps the engine to keep the host awake: keeping a machine awake
/// is the host's business — a launchd or systemd unit, or the machine's own
/// power settings — not something to hard-code into every pane on every
/// platform.
pub fn pane_command(
    engine: &[String],
    prompt: &str,
    session: &str,
    stamp_codex_session: bool,
) -> String {
    let mut words = engine.to_vec();
    words.push(prompt.to_owned());
    let quoted: Vec<_> = words.iter().map(|word| crate::host::quote(word)).collect();
    let target = crate::host::quote(&format!("={session}"));
    let stamp = if stamp_codex_session {
        ".alder-ext-runner/stamp-codex-session; "
    } else {
        ""
    };
    format!(
        "{stamp}{}; tmux set-environment -t {target} {ENGINE_ENV} {ENGINE_EXITED}; exec bash",
        quoted.join(" ")
    )
}

/// The engine invocation, before the prompt is appended: the tier's own
/// command, or whatever [`RUNNER_CMD_ENV`] replaced it with.
pub fn engine_command(
    tier: &Tier,
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

/// The launching repository's `.git`, which an execution's linked worktree
/// keeps its index, objects and branch ref inside. Absolute, because it is
/// handed to a sandbox that has no idea what the execution's working
/// directory is.
///
/// Falling back to `<repo>/.git` if git cannot answer is deliberate: an
/// execution that cannot commit is useless, and the fallback is right for
/// every ordinary checkout.
fn git_common_dir(host: &impl RunnerHost) -> String {
    let answer = host.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    match answer {
        Ok(run) if run.ok && !run.stdout.trim().is_empty() => run.stdout.trim().to_owned(),
        _ => {
            let fallback = host.repo().join(".git");
            host.log(&format!(
                "git could not name its common directory; assuming {}",
                fallback.display()
            ));
            fallback.display().to_string()
        }
    }
}

/// One start, whole.
pub fn start(
    host: &impl RunnerHost,
    branch: &str,
    tier: &'static Tier,
    prompt: &str,
    override_command: Option<&str>,
) -> Result<Started> {
    if branch.trim().is_empty() {
        return Err(RunnerError::new("the branch cannot be empty"));
    }
    if prompt.trim().is_empty() {
        return Err(RunnerError::new("the prompt file is empty; nothing to run"));
    }
    let session = handle_for_branch(branch);
    // Two concurrent starts of one branch serialize on this exclusive
    // per-handle lock, taken before anything is observed or made and held
    // (as a guard) across the whole sequence — session check, worktree cut
    // or adoption, tmux new-session, marker stamping. The loser either
    // refuses immediately on contention, or acquires the lock after the
    // winner finished and then sees the winner's live session below and
    // refuses; either way undo never removes a worktree a winner is using.
    let _lock = host.lock_start(&session)?;
    let worktree_parent = host.repo().parent().ok_or_else(|| {
        RunnerError::new(format!(
            "`{}` has no parent directory to put a worktree beside",
            host.repo().display()
        ))
    })?;
    let worktree = worktree_parent.join(&session);

    sweep_unregistered_worktree(host, worktree_parent, &worktree)?;
    let worktree_present = verify_worktree(host, &worktree, branch)?;

    if let Some(observed) = host.tmux_session(&session)? {
        if observed.engine != EngineMarker::Exited {
            // A proven-running engine is refused; so is a session that
            // carries no marker at all, which proves nothing and therefore
            // must be neither killed nor typed over.
            let why = match observed.engine {
                EngineMarker::Running => "is already running",
                _ => "exists but cannot prove its engine exited",
            };
            return Err(RunnerError::new(format!(
                "handle `{session}` {why}; kill it before starting another \
                 execution on `{branch}`"
            )));
        }
        // `start` means "run this prompt", and an exited pane already ran its
        // own. Its result is safe — it lives on the branch — so the pane is
        // replaced, not adopted.
        host.log(&format!(
            "replacing the exited pane {session}; its result stays on {branch}"
        ));
        host.tmux_kill_session(&session)?;
    }

    let mut made = Made::default();
    let launched = launch(
        host,
        &Launch {
            tier,
            prompt,
            override_command,
            session: &session,
            branch,
            worktree: &worktree,
        },
        worktree_present,
        &mut made,
    );
    match launched {
        Ok(()) => Ok(Started {
            handle: session,
            tier: tier.name,
            model: tier.model,
            effort: tier.effort,
            branch: branch.to_owned(),
            worktree,
            adopted_worktree: worktree_present,
        }),
        Err(error) => {
            undo(host, &made, &session, &worktree, worktree_present);
            Err(error)
        }
    }
}

/// Remove the expected worktree path only when Git has no record of it.
///
/// `git worktree add` creates the directory before its admin entry, so a
/// process killed in that window leaves a path that is neither a worktree nor
/// safe for a later `worktree add`. The inverse can happen if `worktree
/// remove` loses its files after unregistering it. Git's registry is the
/// authority here: a listed worktree is never removed, even if it otherwise
/// looks stale.
fn sweep_unregistered_worktree(
    host: &impl RunnerHost,
    worktree_parent: &Path,
    worktree: &Path,
) -> Result<()> {
    if worktree.parent() != Some(worktree_parent) {
        return Err(RunnerError::new(format!(
            "refusing to sweep `{}` outside worktree parent `{}`",
            worktree.display(),
            worktree_parent.display()
        )));
    }
    if !host.path_exists(worktree) {
        return Ok(());
    }
    let canonical_worktree = host.canonical_path(worktree)?;

    let listed = host.git(&["worktree", "list", "--porcelain", "-z"])?;
    if !listed.ok {
        return Err(RunnerError::new(format!(
            "cannot list registered worktrees before sweeping `{}`: {}",
            worktree.display(),
            first_line(&listed.stderr)
        )));
    }
    let registered = listed
        .stdout
        .split('\0')
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .any(|registered| registered == worktree || registered == canonical_worktree);
    if registered {
        return Ok(());
    }

    host.log(&format!(
        "repair.path-sweep: removing unregistered worktree residue {}",
        worktree.display()
    ));
    host.remove_path(worktree)
}

fn verify_worktree(host: &impl RunnerHost, worktree: &Path, branch: &str) -> Result<bool> {
    if !host.path_exists(worktree) {
        return Ok(false);
    }
    let path = worktree.display().to_string();
    let run = host.git(&["-C", &path, "symbolic-ref", "--quiet", "--short", "HEAD"])?;
    if !run.ok {
        return Err(RunnerError::new(format!(
            "cannot adopt worktree `{path}`: {}",
            first_line(&run.stderr)
        )));
    }
    let actual = run.stdout.trim();
    if actual != branch {
        return Err(RunnerError::new(format!(
            "cannot adopt worktree `{path}`: it is on branch `{actual}`, expected `{branch}`"
        )));
    }
    host.log(&format!(
        "adopting the existing worktree {path} on {branch}"
    ));
    Ok(true)
}

struct Launch<'a> {
    tier: &'static Tier,
    prompt: &'a str,
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
    host: &impl RunnerHost,
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
        // A restart keeps the branch it already has; a first launch cuts one
        // from the repository's current HEAD.
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
            ]
        };
        let add: Vec<&str> = add.iter().map(String::as_str).collect();
        let added = host.git(&add)?;
        if !added.ok {
            return Err(RunnerError::new(format!(
                "git worktree add failed: {}",
                first_line(&added.stderr)
            )));
        }
        made.worktree = true;
    }

    let git_common_dir = git_common_dir(host);
    let engine = engine_command(launch.tier, Some(&git_common_dir), launch.override_command);
    // How a later message gets back into a one-shot execution. It lives in
    // the worktree because that is where the pane's shell is sitting, and it
    // is written by the table that built the launch so the two cannot drift
    // apart.
    if launch.tier.resume_script(None).is_some()
        || launch.tier.codex_session_stamp_script().is_some()
    {
        host.create_dir_all(&worktree.join(RUNNER_DIR))?;
    }
    if let Some(script) = launch.tier.resume_script(Some(&git_common_dir)) {
        host.write_executable(&worktree.join(RUNNER_DIR).join("resume"), &script)?;
    }
    if let Some(script) = launch.tier.codex_session_stamp_script() {
        host.write_executable(
            &worktree.join(RUNNER_DIR).join("stamp-codex-session"),
            script,
        )?;
    }
    host.tmux_new_session(
        launch.session,
        worktree,
        &pane_command(
            &engine,
            launch.prompt,
            launch.session,
            launch.tier.codex_session_stamp_script().is_some(),
        ),
        &[
            (HANDLE_ENV, launch.session.to_owned()),
            (ENGINE_ENV, ENGINE_RUNNING.to_owned()),
            (TIER_ENV, launch.tier.name.to_owned()),
            (WORKTREE_ENV, worktree.display().to_string()),
        ],
    )?;
    made.session = true;
    Ok(())
}

/// Undo what this run made, best effort and loudly. An adopted worktree is
/// never removed: it existed before this start and may hold work. A worktree
/// this run did cut is still only removed if no session answers to the handle
/// at undo time — a session that exists (however it got there) may be using
/// the worktree, and a worktree is never pulled out from under a live pane.
fn undo(
    host: &impl RunnerHost,
    made: &Made,
    session: &str,
    worktree: &Path,
    worktree_adopted: bool,
) {
    if made.session {
        host.log(&format!("removing the session {session} after a failure"));
        if let Err(error) = host.tmux_kill_session(session) {
            host.log(&format!("could not kill {session}: {error}"));
        }
    }
    if made.worktree && !worktree_adopted {
        match host.tmux_session(session) {
            Ok(None) => {}
            Ok(Some(_)) => {
                host.log(&format!(
                    "a session still answers to {session}; leaving the worktree {} in place",
                    worktree.display()
                ));
                return;
            }
            Err(error) => {
                host.log(&format!(
                    "cannot prove no session answers to {session} ({error}); leaving the \
                     worktree {} in place",
                    worktree.display()
                ));
                return;
            }
        }
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

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::{BTreeMap, BTreeSet},
    };

    use chrono::Duration;

    use super::*;
    use crate::tier::{Provider, TIERS, lookup};

    fn tier(name: &str) -> &'static Tier {
        lookup(&TIERS, name).expect("a built-in rung")
    }

    fn now() -> DateTime<Utc> {
        "2026-07-29T00:00:00Z".parse().unwrap()
    }

    /// A host that records every call and answers from a script.
    #[derive(Default)]
    struct Fake {
        repo: PathBuf,
        calls: RefCell<Vec<String>>,
        fail_git: RefCell<BTreeSet<String>>,
        existing_branches: RefCell<BTreeSet<String>>,
        worktrees: RefCell<BTreeMap<PathBuf, String>>,
        strays: RefCell<BTreeSet<PathBuf>>,
        common_dir: RefCell<Option<Run>>,
        canonical_paths: RefCell<BTreeMap<PathBuf, PathBuf>>,
        sessions: RefCell<BTreeMap<String, crate::host::ObservedSession>>,
        crash_after: RefCell<Option<&'static str>>,
        fail_tmux: bool,
        lock_contended: bool,
    }

    use crate::host::{ObservedSession, Run, StartLock};

    impl Fake {
        fn new() -> Self {
            Self {
                repo: PathBuf::from("/projects/alder"),
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

    impl RunnerHost for Fake {
        fn repo(&self) -> &Path {
            &self.repo
        }

        fn lock_start(&self, handle: &str) -> Result<StartLock> {
            self.calls.borrow_mut().push(format!("lock {handle}"));
            if self.lock_contended {
                return Err(RunnerError::new(format!(
                    "another start of `{handle}` holds its lock; refusing to race it"
                )));
            }
            Ok(StartLock::unlocked_for_tests())
        }

        fn git(&self, args: &[&str]) -> Result<Run> {
            self.calls
                .borrow_mut()
                .push(format!("git {}", args.join(" ")));
            if args.contains(&"--git-common-dir") {
                if let Some(run) = self.common_dir.borrow().clone() {
                    return Ok(run);
                }
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
            if args == ["worktree", "list", "--porcelain", "-z"] {
                let ok = !self.fail_git.borrow().contains("worktree list");
                let mut stdout = format!("worktree {}\0bare\0\0", self.repo.display());
                for (path, branch) in self.worktrees.borrow().iter() {
                    stdout.push_str(&format!(
                        "worktree {}\0branch refs/heads/{branch}\0\0",
                        path.display()
                    ));
                }
                return Ok(Run {
                    ok,
                    stdout,
                    stderr: "fatal: it did not work".to_owned(),
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
                self.worktrees
                    .borrow_mut()
                    .insert(path.clone(), branch.to_owned());
                self.strays.borrow_mut().remove(&path);
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
            self.calls
                .borrow_mut()
                .push(format!("tmux observe {session}"));
            Ok(self.sessions.borrow().get(session).cloned())
        }

        fn tmux_new_session(
            &self,
            session: &str,
            cwd: &Path,
            command: &str,
            environment: &[(&str, String)],
        ) -> Result<()> {
            let stamped = environment
                .iter()
                .map(|(name, value)| format!("-e {name}={value}"))
                .collect::<Vec<_>>()
                .join(" ");
            self.calls.borrow_mut().push(format!(
                "tmux new-session {session} -c {} {stamped} {command}",
                cwd.display(),
            ));
            if self.fail_tmux {
                return Err(RunnerError::new("tmux new-session failed: nope"));
            }
            self.sessions.borrow_mut().insert(
                session.to_owned(),
                ObservedSession {
                    handle: Some(session.to_owned()),
                    tier: environment
                        .iter()
                        .find(|(name, _)| *name == TIER_ENV)
                        .map(|(_, value)| value.clone()),
                    worktree: Some(cwd.to_path_buf()),
                    engine: EngineMarker::Running,
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
            self.calls
                .borrow_mut()
                .push(format!("exists {}", path.display()));
            self.worktrees.borrow().contains_key(path) || self.strays.borrow().contains(path)
        }

        fn canonical_path(&self, path: &Path) -> Result<PathBuf> {
            self.calls
                .borrow_mut()
                .push(format!("resolve {}", path.display()));
            Ok(self
                .canonical_paths
                .borrow()
                .get(path)
                .cloned()
                .unwrap_or_else(|| path.to_path_buf()))
        }

        fn remove_path(&self, path: &Path) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("remove {}", path.display()));
            self.worktrees.borrow_mut().remove(path);
            self.strays.borrow_mut().remove(path);
            Ok(())
        }

        fn create_dir_all(&self, path: &Path) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("mkdir {}", path.display()));
            Ok(())
        }

        fn write_executable(&self, path: &Path, body: &str) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("write {}", path.display()));
            let _ = body;
            Ok(())
        }

        fn log(&self, message: &str) {
            self.calls.borrow_mut().push(format!("log {message}"));
        }
    }

    const BRANCH: &str = "work/al-1";
    const SESSION: &str = "alder-ext-work-al-1";
    const WORKTREE: &str = "/projects/alder-ext-work-al-1";

    fn run_start(host: &Fake, tier_name: &str) -> Result<Started> {
        start(host, BRANCH, tier(tier_name), "do the thing", None)
    }

    #[test]
    fn handles_are_deterministic_and_name_the_tool() {
        assert_eq!(handle_for_branch("work/al-1"), "alder-ext-work-al-1");
        assert_eq!(handle_for_branch("Fix_Thing"), "alder-ext-fix-thing");
        // Deterministic per branch is what makes a crashed start converge on
        // the same session instead of doubling it.
        assert_eq!(
            handle_for_branch("work/al-1"),
            handle_for_branch("work/al-1")
        );
    }

    #[test]
    fn a_start_sweeps_verifies_cuts_and_launches_in_that_order() {
        let host = Fake::new();
        let started = run_start(&host, "luna").unwrap();
        assert_eq!(started.handle, SESSION);
        assert_eq!(started.branch, BRANCH);
        assert_eq!(started.worktree, Path::new(WORKTREE));
        assert_eq!(
            (started.tier, started.model, started.effort),
            ("luna", "gpt-5.6-luna", "high")
        );
        assert!(!started.adopted_worktree);

        let calls = host.calls();
        let ordinal = |needle: &str| {
            calls
                .iter()
                .position(|call| call.contains(needle))
                .unwrap_or_else(|| panic!("{needle} never happened in {calls:#?}"))
        };
        assert!(ordinal("exists") < ordinal("git worktree add"));
        assert!(ordinal("tmux observe") < ordinal("git worktree add"));
        assert!(ordinal("git worktree add") < ordinal("tmux new-session"));

        // Identity is part of new-session itself. There is no crash window
        // where the pane exists but its handle cannot be observed.
        assert!(host.called(&format!(
            "tmux new-session {SESSION} -c {WORKTREE} \
             -e ALDER_EXT_RUNNER_HANDLE={SESSION} -e ALDER_EXT_RUNNER_ENGINE=running \
             -e ALDER_EXT_RUNNER_TIER=luna -e ALDER_EXT_RUNNER_WORKTREE={WORKTREE}"
        )));
        // The runner stamps nothing of anyone else's into the worktree: no
        // binaries, no configs, only its own resume machinery.
        assert!(!host.called("copy "));
        assert!(!host.called("ALDER_ATTEMPT"));
    }

    #[test]
    fn started_summary_names_the_identity_and_the_worktree_provenance() {
        let cut = Started {
            handle: SESSION.to_owned(),
            tier: "terra",
            model: "gpt-5.6-terra",
            effort: "xhigh",
            branch: BRANCH.to_owned(),
            worktree: PathBuf::from(WORKTREE),
            adopted_worktree: false,
        };
        let adopted = Started {
            adopted_worktree: true,
            ..cut.clone()
        };
        assert_eq!(
            cut.summary(),
            "started alder-ext-work-al-1 on work/al-1 at /projects/alder-ext-work-al-1 \
             (tier terra, model gpt-5.6-terra, effort xhigh, cutting a worktree)"
        );
        assert!(adopted.summary().ends_with("adopting its worktree)"));
    }

    #[test]
    fn git_common_directory_requires_a_successful_nonempty_answer() {
        let host = Fake::new();
        host.common_dir.borrow_mut().replace(Run {
            ok: true,
            stdout: "/shared/alder.git\n".to_owned(),
            stderr: String::new(),
        });
        assert_eq!(git_common_dir(&host), "/shared/alder.git");

        host.common_dir.borrow_mut().replace(Run {
            ok: true,
            stdout: " \n".to_owned(),
            stderr: String::new(),
        });
        assert_eq!(git_common_dir(&host), "/projects/alder/.git");

        host.common_dir.borrow_mut().replace(Run {
            ok: false,
            stdout: "/misleading/alder.git\n".to_owned(),
            stderr: "fatal: not a repository".to_owned(),
        });
        assert_eq!(git_common_dir(&host), "/projects/alder/.git");
    }

    #[test]
    fn a_registered_canonical_worktree_is_never_swept_as_residue() {
        let host = Fake::new();
        let worktree = PathBuf::from(WORKTREE);
        let canonical = PathBuf::from("/private/projects/alder-ext-work-al-1");
        host.strays.borrow_mut().insert(worktree.clone());
        host.canonical_paths
            .borrow_mut()
            .insert(worktree.clone(), canonical.clone());
        host.worktrees
            .borrow_mut()
            .insert(canonical, BRANCH.to_owned());

        sweep_unregistered_worktree(&host, Path::new("/projects"), &worktree)
            .expect("a registered canonical worktree is retained");

        assert!(!host.called(&format!("remove {WORKTREE}")));
        assert!(host.strays.borrow().contains(&worktree));
    }

    #[test]
    fn undo_reports_a_failed_worktree_removal_and_first_line_is_useful() {
        let host = Fake::new();
        host.fail_git
            .borrow_mut()
            .insert("worktree remove".to_owned());
        undo(
            &host,
            &Made {
                worktree: true,
                session: false,
            },
            SESSION,
            Path::new(WORKTREE),
            false,
        );
        assert!(
            host.called(&format!(
                "could not remove {WORKTREE}: fatal: it did not work"
            )),
            "{:#?}",
            host.calls()
        );

        let host = Fake::new();
        undo(
            &host,
            &Made {
                worktree: true,
                session: false,
            },
            SESSION,
            Path::new(WORKTREE),
            false,
        );
        assert!(
            !host.called(&format!("could not remove {WORKTREE}")),
            "a successful cleanup was reported as failed: {:#?}",
            host.calls()
        );
        assert_eq!(
            first_line("\n  first useful line\nsecond line\n"),
            "first useful line"
        );
        assert_eq!(first_line("\n \t\n"), "no output");
    }

    #[test]
    fn the_prompt_reaches_the_pane_as_argv_and_the_pane_outlives_the_engine() {
        let host = Fake::new();
        start(
            &host,
            BRANCH,
            tier("luna"),
            "Fix the thing.\nSecond line with 'quotes' and; semicolons.",
            None,
        )
        .unwrap();
        let pane = host
            .calls()
            .into_iter()
            .find(|call| call.contains("tmux new-session"))
            .expect("a session is created");

        assert!(pane.contains(&format!("-c {WORKTREE}")), "{pane}");
        assert!(pane.ends_with("; exec bash"), "{pane}");
        // The whole prompt is in the pane's argv, exactly as written.
        assert!(
            pane.contains(
                "Fix the thing.\nSecond line with 'quotes' and; semicolons."
                    .replace('\'', "'\\''")
                    .as_str()
            ),
            "{pane}"
        );
        // Nothing is typed at the session, and nothing waits for it to boot.
        assert!(!host.called("send-keys"));
    }

    /// A start asks the world a fixed set of questions and never asks again.
    ///
    /// A wait is an observation made again in the hope of a different answer,
    /// so the two are the same claim seen from either end. What a caller can
    /// observe about whether a start waited is exactly which questions went
    /// out and how many times, and every question this path puts to the world
    /// goes through the host — so the log below is that observation, not a
    /// note about a double's internals. A loop that waited for the engine to
    /// come up — sleeping between looks or spinning — would appear here as
    /// another observation, and would appear whatever else the machine was
    /// doing.
    ///
    /// This is the start's half. `Host` runs no code here, so what the real
    /// commands do once they leave is checked where they really run, in
    /// `tests/start_host.rs`.
    #[test]
    fn the_start_asks_the_world_a_fixed_set_of_questions_and_never_asks_again() {
        let host = Fake::new();
        run_start(&host, "luna").unwrap();
        let calls = host.calls();

        let asked = |question: &str| {
            calls
                .iter()
                .filter(|call| call.starts_with(question))
                .count()
        };
        for (question, times) in [
            // The session is looked at once, before the decision that uses it.
            // Nothing looks a second time to see whether an engine came up.
            (format!("tmux observe {SESSION}"), 1),
            // Two different questions about one path — is there residue to
            // sweep, and is the worktree already cut — not one asked twice.
            (format!("exists {WORKTREE}"), 2),
            ("git rev-parse --verify".to_owned(), 1),
            ("git rev-parse --path-format".to_owned(), 1),
        ] {
            assert_eq!(
                asked(&question),
                times,
                "`{question}` is no longer asked exactly {times} time(s), and \
                 an observation repeated is a wait: {calls:#?}"
            );
        }

        // The same claim over the whole log, so that a wait built on some
        // other observation is caught too. Effects that change the world are
        // left out deliberately: writing one more file into a worktree is not
        // a wait, and should not have to edit a test about waiting.
        let observations: Vec<&String> = calls
            .iter()
            .filter(|call| {
                OBSERVING_EFFECTS
                    .iter()
                    .any(|prefix| call.starts_with(prefix))
            })
            .collect();
        assert_eq!(
            observations.len(),
            5,
            "the start observes the world a different number of times: {observations:#?}"
        );
    }

    /// This module itself cannot wait in process, whatever it is asked to do.
    ///
    /// The counts above are about the questions that reach the world. A
    /// `thread::sleep` here reaches nothing and so shows in no ledger, and
    /// elapsed time cannot show it either: this path's cost is process
    /// creation — `git`, `tmux` — so on a loaded machine a clock reports the
    /// machine rather than the code. What no load can make untrue is the
    /// source. Waiting in process needs a duration or a clock to wait on, and
    /// this module's start half names neither, nor the thread facilities that
    /// would wait on one.
    ///
    /// Only this module, and deliberately. A bounded timeout on a command
    /// that can hang is a limit rather than a wait; but it is a limit on a
    /// *command*, and nothing here runs one. Every command goes out through
    /// `host::Host`, which is where such a timeout would belong and where
    /// this test would be wrong to forbid it. The host is read by the test
    /// after this one, which allows it exactly that and nothing else; what
    /// the host's commands do once they leave is checked where they really
    /// run, in `tests/start_host.rs`.
    ///
    /// `DateTime` is exempt below by naming its uses: `dispatch_tier` takes
    /// the caller's `now` to compare against a recorded rate-limit deadline,
    /// which reads no clock and waits on nothing.
    #[test]
    fn this_modules_start_half_can_name_no_clock_and_no_sleep() {
        let code = without_comments(before_the_tests(include_str!("start.rs")));
        for waiting in [
            "Duration",
            "Instant",
            "SystemTime",
            "thread",
            "sleep",
            "park",
            "yield_now",
            "spin_loop",
            "recv_timeout",
            "elapsed",
            "Utc::now",
        ] {
            assert!(
                !code.contains(waiting),
                "start's own half of start.rs names `{waiting}`. Nothing here \
                 may wait for an engine, and code that cannot name a duration \
                 or a clock cannot wait on one in process. If this is a bounded \
                 timeout on a command that can hang, it belongs on the command, \
                 in `host::Host`, not here."
            );
        }
    }

    /// The host the start runs in cannot wait for an engine either.
    ///
    /// The test above reads this module; this one reads the other half of the
    /// same path, `host::Host` — its inherent block, where every command is
    /// built and run, and its `RunnerHost` block, the only host code a start
    /// reaches. A readiness sleep put there would reach no tmux command, so
    /// it would show in no ledger and in no call list; the source is again
    /// what no load can make untrue.
    ///
    /// `Duration` is permitted here and banned above, and that difference is
    /// the whole point of scanning the two halves separately. This half runs
    /// commands, so a bounded timeout on one that can hang is a limit, and a
    /// limit needs a duration. What a limit does not need is a way to stand
    /// still, a clock to stand still by, or another thread to stand still
    /// until — so two families are refused. Standing still: `sleep`, `park`,
    /// `yield_now`, `spin_loop`, and the `Instant`/`elapsed` pair a
    /// hand-rolled deadline reads. Standing still until another thread says
    /// when: `recv` in every spelling, `Condvar`, `Barrier`.
    ///
    /// The scan is lexical and block-scoped, which is its limit: it holds for
    /// the code these two blocks contain, not for something they call out to.
    #[test]
    fn the_hosts_start_facing_halves_can_name_no_sleep_and_no_clock() {
        let source = include_str!("host.rs");
        for (half, runs_commands) in [
            ("impl Host {", "fn run("),
            ("impl RunnerHost for Host {", "fn tmux_new_session("),
        ] {
            let code = without_comments(impl_block(source, half));
            assert!(
                code.contains(runs_commands),
                "`{runs_commands}` has left `{half}` in host.rs, so this scan \
                 now covers less of the host than it was written to cover"
            );
            for waiting in [
                // Standing still, and the clock a hand-rolled deadline reads.
                "sleep",
                "park",
                "yield_now",
                "spin_loop",
                "Instant",
                "elapsed",
                // Standing still until another thread says when. `recv` takes
                // `recv_timeout` and `try_recv` with it, which is the point: a
                // bounded receive and a polled one are both waits.
                "recv",
                "Condvar",
                "Barrier",
            ] {
                assert!(
                    !code.contains(waiting),
                    "`{half}` in host.rs names `{waiting}`, and standing still \
                     is the one thing a start may not do: it types nothing at a \
                     session and waits for no engine to boot. A bounded timeout \
                     on a command that can hang is a limit rather than a wait — \
                     spell it with a `Duration`, which this test allows, and \
                     with an API that takes one, not with a word that can only \
                     mean waiting."
                );
            }
        }
    }

    /// The host effects a start only reads the world with. A wait has to
    /// repeat one of these; the ones that change the world it has no use for.
    const OBSERVING_EFFECTS: [&str; 5] = [
        "tmux observe",
        "exists ",
        "resolve ",
        "git rev-parse",
        "git worktree list",
    ];

    /// Everything in a module ahead of its own tests.
    fn before_the_tests(source: &str) -> &str {
        source
            .split_once("\n#[cfg(test)]\n")
            .expect("a module under test keeps its tests at the end")
            .0
    }

    /// The body of one top-level `impl` block, its header line excluded.
    ///
    /// A rustfmt'd block is the text between its header and the first line
    /// that closes at column zero. Both ends are required to be there, so
    /// renaming or reshaping a block fails this loudly rather than quietly
    /// scanning an empty string.
    fn impl_block<'a>(source: &'a str, header: &str) -> &'a str {
        let opened = source
            .split_once(&format!("\n{header}\n"))
            .unwrap_or_else(|| panic!("`{header}` is no longer a block of its own"))
            .1;
        opened
            .split_once("\n}\n")
            .unwrap_or_else(|| panic!("`{header}` no longer closes at column zero"))
            .0
    }

    /// Source with its whole-line comments dropped, so that prose about
    /// waiting is not mistaken for code that waits.
    fn without_comments(source: &str) -> String {
        source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_codex_execution_is_given_the_git_common_dir_it_must_commit_through() {
        let host = Fake::new();
        run_start(&host, "luna").unwrap();
        let pane = host
            .calls()
            .into_iter()
            .find(|call| call.contains("tmux new-session"))
            .expect("a session is created");
        // Without this the execution's first commit dies on index.lock: its
        // worktree keeps index, objects and branch ref in the repo's .git.
        assert!(
            pane.contains(r#"'sandbox_workspace_write.writable_roots=["/projects/alder/.git"]'"#),
            "{pane}"
        );

        // The resume machinery is written where its shell will be sitting,
        // carrying the same rung, and the watcher starts before the model.
        assert!(host.called(&format!("write {WORKTREE}/{RUNNER_DIR}/resume")));
        assert!(host.called(&format!(
            "write {WORKTREE}/{RUNNER_DIR}/stamp-codex-session"
        )));
        assert!(
            pane.contains(".alder-ext-runner/stamp-codex-session;"),
            "the Codex watcher must start before the model: {pane}"
        );

        // A claude execution is not sandboxed this way and is given no such
        // root or resume script: it sits at a prompt and is typed at through
        // `send`.
        let host = Fake::new();
        run_start(&host, "opus").unwrap();
        assert!(!host.called("writable_roots"));
        assert!(!host.called("/resume"));
        assert!(!host.called("stamp-codex-session"));
    }

    #[test]
    fn a_live_session_is_refused_before_anything_is_created() {
        let host = Fake::new();
        host.sessions.borrow_mut().insert(
            SESSION.to_owned(),
            ObservedSession {
                handle: Some(SESSION.to_owned()),
                tier: Some("luna".to_owned()),
                worktree: Some(PathBuf::from(WORKTREE)),
                engine: EngineMarker::Running,
            },
        );
        let error = run_start(&host, "terra").unwrap_err();
        assert!(error.message.contains("already running"), "{error}");
        assert!(!host.called("git worktree add"));
        assert!(!host.called("tmux new-session"));
        assert!(!host.called("tmux kill-session"));
    }

    #[test]
    fn a_session_that_cannot_prove_its_engine_exited_is_refused_not_replaced() {
        let host = Fake::new();
        host.sessions.borrow_mut().insert(
            SESSION.to_owned(),
            ObservedSession {
                handle: Some(SESSION.to_owned()),
                tier: Some("luna".to_owned()),
                worktree: Some(PathBuf::from(WORKTREE)),
                engine: EngineMarker::Unproven,
            },
        );
        let error = run_start(&host, "terra").unwrap_err();
        assert!(
            error.message.contains("cannot prove its engine exited"),
            "{error}"
        );
        assert!(!host.called("tmux kill-session"));
        assert!(!host.called("tmux new-session"));
        assert!(!host.called("git worktree add"));
    }

    #[test]
    fn a_start_that_loses_the_handle_lock_refuses_cleanly_and_removes_nothing() {
        // The loser of a double start: the winner holds the per-handle lock,
        // so this start must refuse before observing or making anything, and
        // it must undo nothing — the winner's worktree and session are not
        // its to touch.
        let host = Fake {
            lock_contended: true,
            ..Fake::new()
        };
        let error = run_start(&host, "terra").unwrap_err();
        assert!(error.message.contains("holds its lock"), "{error}");

        let calls = host.calls();
        assert_eq!(
            calls,
            vec![format!("lock {SESSION}")],
            "a lock loser did more than ask for the lock: {calls:#?}"
        );
        assert!(!host.called("git worktree add"));
        assert!(!host.called("tmux new-session"));
        assert!(!host.called("git worktree remove"));
        assert!(!host.called(&format!("remove {WORKTREE}")));
        assert!(!host.called("tmux kill-session"));
    }

    #[test]
    fn the_lock_is_taken_before_anything_is_observed_and_a_start_takes_it_once() {
        let host = Fake::new();
        run_start(&host, "terra").unwrap();
        let calls = host.calls();
        assert_eq!(
            calls.first().map(String::as_str),
            Some(format!("lock {SESSION}").as_str()),
            "the lock is no longer the first thing a start does: {calls:#?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("lock "))
                .count(),
            1,
            "{calls:#?}"
        );
    }

    #[test]
    fn undo_leaves_a_worktree_alone_while_any_session_answers_to_the_handle() {
        // Even a worktree this run cut is not removed while a session with
        // the handle exists at undo time: whatever created that session may
        // be sitting in the worktree.
        let host = Fake::new();
        host.worktrees
            .borrow_mut()
            .insert(PathBuf::from(WORKTREE), BRANCH.to_owned());
        host.sessions.borrow_mut().insert(
            SESSION.to_owned(),
            ObservedSession {
                handle: Some(SESSION.to_owned()),
                tier: None,
                worktree: None,
                engine: EngineMarker::Running,
            },
        );
        undo(
            &host,
            &Made {
                worktree: true,
                session: false,
            },
            SESSION,
            Path::new(WORKTREE),
            false,
        );
        assert!(!host.called("git worktree remove"), "{:#?}", host.calls());
        assert!(host.worktrees.borrow().contains_key(Path::new(WORKTREE)));
        assert!(host.called("leaving the worktree"), "{:#?}", host.calls());
    }

    #[test]
    fn an_exited_pane_is_replaced_because_start_means_run_this_prompt() {
        let host = Fake::new();
        host.worktrees
            .borrow_mut()
            .insert(PathBuf::from(WORKTREE), BRANCH.to_owned());
        host.sessions.borrow_mut().insert(
            SESSION.to_owned(),
            ObservedSession {
                handle: Some(SESSION.to_owned()),
                tier: Some("luna".to_owned()),
                worktree: Some(PathBuf::from(WORKTREE)),
                engine: EngineMarker::Exited,
            },
        );
        let started = run_start(&host, "terra").unwrap();
        assert!(started.adopted_worktree);

        let calls = host.calls();
        let killed = calls
            .iter()
            .position(|call| call == &format!("tmux kill-session {SESSION}"))
            .expect("the exited pane is replaced");
        let launched = calls
            .iter()
            .position(|call| call.contains("tmux new-session"))
            .expect("a fresh session is created");
        assert!(killed < launched, "{calls:#?}");
        assert!(!host.called("git worktree add"), "{calls:#?}");
    }

    #[test]
    fn an_existing_worktree_is_adopted_only_on_the_expected_branch() {
        let host = Fake::new();
        host.worktrees
            .borrow_mut()
            .insert(PathBuf::from(WORKTREE), "work/al-other".to_owned());
        let error = run_start(&host, "terra").unwrap_err();
        assert!(error.message.contains("work/al-other"), "{error}");
        assert!(error.message.contains("expected `work/al-1`"), "{error}");
        assert!(!host.called("tmux new-session"));

        let host = Fake::new();
        host.worktrees
            .borrow_mut()
            .insert(PathBuf::from(WORKTREE), BRANCH.to_owned());
        let started = run_start(&host, "terra").unwrap();
        assert!(started.adopted_worktree);
        assert!(!host.called("git worktree add"));
        assert!(!host.called(&format!("remove {WORKTREE}")));
        assert!(host.called("adopting the existing worktree"));
    }

    #[test]
    fn an_unregistered_directory_from_a_torn_worktree_add_is_swept_before_restart() {
        let host = Fake::new();
        let worktree = PathBuf::from(WORKTREE);
        // `git worktree add` made the directory, then died before it recorded
        // the worktree in Git's admin area.
        host.strays.borrow_mut().insert(worktree.clone());

        run_start(&host, "terra").unwrap();

        let calls = host.calls();
        let swept = calls
            .iter()
            .position(|call| call == &format!("remove {WORKTREE}"))
            .expect("the unregistered residue is removed");
        let added = calls
            .iter()
            .position(|call| call.starts_with("git worktree add"))
            .expect("a fresh worktree is created");
        assert!(swept < added, "{calls:#?}");
        assert!(host.called("git worktree list --porcelain -z"));
        assert!(!host.strays.borrow().contains(&worktree));
    }

    #[test]
    fn an_unregistered_directory_from_a_torn_worktree_remove_is_swept_before_restart() {
        let host = Fake::new();
        let worktree = PathBuf::from(WORKTREE);
        // A remove first drops Git's admin entry; its branch remains for the
        // restart to reuse while the abandoned files are swept away.
        host.existing_branches
            .borrow_mut()
            .insert(BRANCH.to_owned());
        host.strays.borrow_mut().insert(worktree.clone());

        run_start(&host, "terra").unwrap();

        assert!(host.called(&format!("remove {WORKTREE}")));
        assert!(host.called(&format!("git worktree add {WORKTREE} {BRANCH}")));
        assert!(!host.called(&format!("-b {BRANCH}")));
        assert!(!host.strays.borrow().contains(&worktree));
    }

    #[test]
    fn a_sweep_refuses_to_remove_a_path_outside_the_worktree_parent() {
        let host = Fake::new();
        let outside = PathBuf::from("/elsewhere/alder-ext-work-al-1");
        host.strays.borrow_mut().insert(outside.clone());

        let error = sweep_unregistered_worktree(&host, Path::new("/projects"), &outside)
            .expect_err("the sweep is constrained to the worktree parent");

        assert!(error.message.contains("outside worktree parent"), "{error}");
        assert!(!host.called("remove /elsewhere/alder-ext-work-al-1"));
        assert!(host.strays.borrow().contains(&outside));
    }

    #[test]
    fn a_sweep_keeps_the_residue_when_git_cannot_list_its_registry() {
        let host = Fake::new();
        let worktree = PathBuf::from(WORKTREE);
        host.strays.borrow_mut().insert(worktree.clone());
        host.fail_git
            .borrow_mut()
            .insert("worktree list".to_owned());

        let error = run_start(&host, "terra")
            .expect_err("the sweep must fail closed without Git's registry");

        assert!(
            error.message.contains("cannot list registered worktrees"),
            "{error}"
        );
        assert!(!host.called(&format!("remove {WORKTREE}")));
        assert!(host.strays.borrow().contains(&worktree));
    }

    #[test]
    fn a_failure_after_something_was_made_undoes_exactly_what_this_run_made() {
        // Git fails: nothing was made, nothing is undone.
        let host = Fake::new();
        host.fail_git.borrow_mut().insert("worktree add".to_owned());
        let error = run_start(&host, "terra").unwrap_err();
        assert!(error.message.contains("git worktree add failed"), "{error}");
        assert!(!host.called("tmux kill-session"));
        assert!(!host.called("git worktree remove"));

        // tmux fails after the worktree is cut: the worktree goes too.
        let host = Fake {
            fail_tmux: true,
            ..Fake::new()
        };
        let error = run_start(&host, "terra").unwrap_err();
        assert!(error.message.contains("tmux new-session"), "{error}");
        assert!(host.called(&format!("git worktree remove --force {WORKTREE}")));

        // tmux fails on an adopted worktree: the worktree is not this run's
        // to remove.
        let host = Fake {
            fail_tmux: true,
            ..Fake::new()
        };
        host.worktrees
            .borrow_mut()
            .insert(PathBuf::from(WORKTREE), BRANCH.to_owned());
        let error = run_start(&host, "terra").unwrap_err();
        assert!(error.message.contains("tmux new-session"), "{error}");
        assert!(!host.called("git worktree remove"));
        assert!(host.worktrees.borrow().contains_key(Path::new(WORKTREE)));
    }

    #[test]
    fn a_process_crash_after_each_effect_converges_on_exactly_one_execution() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        for boundary in ["worktree add", "tmux new-session"] {
            let host = Fake::new();
            host.crash_after.borrow_mut().replace(boundary);
            let crashed = catch_unwind(AssertUnwindSafe(|| {
                let _ = run_start(&host, "terra");
            }));
            assert!(crashed.is_err(), "{boundary} did not crash");

            let repaired = run_start(&host, "terra");
            if boundary == "tmux new-session" {
                // The session exists and its engine is live: the prompt is
                // already running, which is the converged state.
                let error = repaired.expect_err("a live engine is already converged");
                assert!(error.message.contains("already running"), "{error}");
            } else {
                let started =
                    repaired.unwrap_or_else(|error| panic!("repair after {boundary}: {error}"));
                assert!(started.adopted_worktree, "{boundary}");
            }
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
                    .filter(|call| call.contains("tmux new-session"))
                    .count(),
                1,
                "{boundary}: {:#?}",
                host.calls()
            );
        }
    }

    #[test]
    fn a_restart_reuses_the_branch_it_already_has() {
        let host = Fake::new();
        host.existing_branches
            .borrow_mut()
            .insert(BRANCH.to_owned());
        run_start(&host, "terra").unwrap();
        assert!(host.called(&format!("git worktree add {WORKTREE} {BRANCH}")));
        assert!(!host.called(&format!("-b {BRANCH}")));
    }

    #[test]
    fn the_stub_override_replaces_the_engine_and_still_gets_the_prompt() {
        let host = Fake::new();
        start(
            &host,
            BRANCH,
            tier("sol"),
            "do the thing",
            Some("/tmp/stub.sh --once"),
        )
        .unwrap();
        let pane = host
            .calls()
            .into_iter()
            .find(|call| call.contains("tmux new-session"))
            .expect("a session is created");
        assert!(pane.contains("'/tmp/stub.sh' '--once'"), "{pane}");
        assert!(
            !pane.contains("'codex' 'exec'"),
            "the tier engine leaked through the stub override: {pane}"
        );
        assert!(pane.contains("'do the thing'"), "{pane}");
        // The tier is still what the session records, stub or no stub.
        assert!(pane.contains("-e ALDER_EXT_RUNNER_TIER=sol"), "{pane}");
    }

    #[test]
    fn an_empty_branch_or_prompt_starts_nothing() {
        let host = Fake::new();
        assert!(
            start(&host, " ", tier("terra"), "prompt", None)
                .unwrap_err()
                .message
                .contains("branch")
        );
        assert!(
            start(&host, BRANCH, tier("terra"), " \n", None)
                .unwrap_err()
                .message
                .contains("prompt file is empty")
        );
        assert!(host.calls().is_empty(), "{:#?}", host.calls());
    }

    #[test]
    fn a_rate_limited_provider_is_served_by_the_other_ladder() {
        let mut limits = Limits::default();
        limits.set(Provider::Codex, now() + Duration::hours(1), None);
        let (rung, why) = dispatch_tier(&TIERS, tier("terra"), &limits, now());
        assert_eq!(rung.name, "opus");
        assert!(why.unwrap().contains("rate-limited"));

        // An expired limit is no limit.
        let (rung, why) = dispatch_tier(&TIERS, tier("terra"), &limits, now() + Duration::hours(2));
        assert_eq!(rung.name, "terra");
        assert!(why.is_none());

        // Nothing limited: what was asked for.
        let (rung, why) = dispatch_tier(&TIERS, tier("luna"), &Limits::default(), now());
        assert_eq!(rung.name, "luna");
        assert!(why.is_none());

        // Both limited: what was asked for, and a reason why it stands.
        limits.set(Provider::Claude, now() + Duration::hours(1), None);
        let (rung, why) = dispatch_tier(&TIERS, tier("sol"), &limits, now());
        assert_eq!(rung.name, "sol");
        assert!(why.unwrap().contains("both providers"));
    }
}
