use std::{env, path::PathBuf, process::ExitCode};

use alder_ext_runner::{
    budget, config,
    error::RunnerError,
    host::Host,
    limits::Limits,
    ops,
    start::{self, RUNNER_CMD_ENV},
    tier::{Provider, Tier},
};
use chrono::Utc;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("alder-ext-runner: {error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<ExitCode, RunnerError> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let (command, rest) = match arguments.split_first() {
        Some((command, rest)) => (command.as_str(), rest),
        None => return Err(usage("a command is required")),
    };
    match command {
        "--help" | "-h" => {
            println!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        "start" => start_execution(rest),
        "status" => status(rest),
        "send" => send(rest),
        "kill" => kill(rest),
        "limit" => record_limit(rest),
        "budget" => report_budget(rest),
        other => Err(usage(&format!("unknown command `{other}`"))),
    }
}

fn usage(complaint: &str) -> RunnerError {
    RunnerError::new(format!("{complaint}\n\n{USAGE}"))
}

/// `start --repo <path> --branch <name> --tier <name> --prompt-file <path>`.
fn start_execution(arguments: &[String]) -> Result<ExitCode, RunnerError> {
    let mut repo: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut tier_name: Option<String> = None;
    let mut prompt_file: Option<PathBuf> = None;
    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        let mut value = |flag: &str| {
            iterator
                .next()
                .cloned()
                .ok_or_else(|| usage(&format!("{flag} needs a value")))
        };
        match argument.as_str() {
            "--repo" => repo = Some(PathBuf::from(value("--repo")?)),
            "--branch" => branch = Some(value("--branch")?),
            "--tier" => tier_name = Some(value("--tier")?),
            "--prompt-file" => prompt_file = Some(PathBuf::from(value("--prompt-file")?)),
            other => return Err(usage(&format!("unknown argument `{other}`"))),
        }
    }
    let repo = repo.ok_or_else(|| usage("start needs --repo"))?;
    let branch = branch.ok_or_else(|| usage("start needs --branch"))?;
    let tier_name = tier_name.ok_or_else(|| usage("start needs --tier"))?;
    let prompt_file = prompt_file.ok_or_else(|| usage("start needs --prompt-file"))?;

    // Resolved before anything is created: an unknown tier must never reach a
    // CLI, where it would launch at whatever that CLI defaults to.
    let table = config::load_tiers()?;
    let requested: &'static Tier = alder_ext_runner::tier::lookup(table, &tier_name)?;

    let repo = repo.canonicalize().map_err(|error| {
        RunnerError::new(format!("cannot inspect `{}`: {error}", repo.display()))
    })?;
    let prompt = std::fs::read_to_string(&prompt_file).map_err(|error| {
        RunnerError::new(format!("cannot read `{}`: {error}", prompt_file.display()))
    })?;

    let limits = Limits::load(&config::limits_path()).unwrap_or_else(|error| {
        eprintln!("alder-ext-runner: ignoring the rate-limit state: {error}");
        Limits::default()
    });
    let (tier, why) = start::dispatch_tier(table, requested, &limits, Utc::now());
    if let Some(why) = why {
        eprintln!("alder-ext-runner: {why}");
    }

    let host = Host::new(repo);
    let override_command = env::var(RUNNER_CMD_ENV).ok();
    let started = start::start(&host, &branch, tier, &prompt, override_command.as_deref())?;
    eprintln!("alder-ext-runner: {}", started.summary());
    // The handle is the whole stdout, so a caller can capture it verbatim.
    println!("{}", started.handle);
    Ok(ExitCode::SUCCESS)
}

fn one_handle(arguments: &[String], verb: &str) -> Result<String, RunnerError> {
    match arguments {
        [handle] if !handle.starts_with('-') => Ok(handle.clone()),
        _ => Err(usage(&format!("{verb} takes exactly one handle"))),
    }
}

/// `status <handle>`: one word — running | done | dead.
fn status(arguments: &[String]) -> Result<ExitCode, RunnerError> {
    let handle = one_handle(arguments, "status")?;
    let host = Host::new(current_dir()?);
    let status = ops::status(&host, &handle)?;
    println!("{}", status.word);
    if let Some(detail) = status.detail {
        println!("{detail}");
    }
    Ok(ExitCode::SUCCESS)
}

/// `send <handle> --file <path> [--force]`.
fn send(arguments: &[String]) -> Result<ExitCode, RunnerError> {
    let mut handle: Option<String> = None;
    let mut file: Option<PathBuf> = None;
    let mut force = false;
    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        match argument.as_str() {
            "--file" => {
                file = Some(PathBuf::from(
                    iterator
                        .next()
                        .ok_or_else(|| usage("--file needs a path"))?,
                ));
            }
            "--force" => force = true,
            other if other.starts_with('-') => {
                return Err(usage(&format!("unknown argument `{other}`")));
            }
            other => {
                if handle.replace(other.to_owned()).is_some() {
                    return Err(usage("send takes exactly one handle"));
                }
            }
        }
    }
    let handle = handle.ok_or_else(|| usage("send needs a handle"))?;
    let file = file.ok_or_else(|| usage("send needs --file"))?;
    // No tier table: the delivery route was stamped into the session at
    // start, and `send` reads the stamp rather than the config.
    let host = Host::new(current_dir()?);
    ops::send(&host, &handle, &file, force)?;
    Ok(ExitCode::SUCCESS)
}

/// `kill <handle>`.
fn kill(arguments: &[String]) -> Result<ExitCode, RunnerError> {
    let handle = one_handle(arguments, "kill")?;
    let host = Host::new(current_dir()?);
    match ops::kill(&host, &handle)? {
        ops::Killed::Killed => println!("killed {handle}"),
        ops::Killed::AlreadyDead => {
            println!("no execution answers to `{handle}`; nothing to kill");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `limit <provider> [--minutes N] [--clear] [--why TEXT]`.
fn record_limit(arguments: &[String]) -> Result<ExitCode, RunnerError> {
    let mut provider: Option<Provider> = None;
    let mut minutes: i64 = 60;
    let mut clear = false;
    let mut why: Option<String> = None;
    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        match argument.as_str() {
            "--minutes" => {
                minutes = iterator
                    .next()
                    .ok_or_else(|| usage("--minutes needs a number"))?
                    .parse::<i64>()
                    .map_err(|error| RunnerError::new(format!("--minutes: {error}")))?;
                if minutes <= 0 {
                    return Err(RunnerError::new("--minutes must be positive"));
                }
            }
            "--clear" => clear = true,
            "--why" => {
                why = Some(
                    iterator
                        .next()
                        .ok_or_else(|| usage("--why needs a reason"))?
                        .clone(),
                );
            }
            other if other.starts_with('-') => {
                return Err(usage(&format!("unknown argument `{other}`")));
            }
            other => provider = Some(Provider::parse(other)?),
        }
    }
    let provider = provider.ok_or_else(|| usage("limit needs a provider"))?;
    let path = config::limits_path();
    // One locked read-modify-write: concurrent `limit` commands serialize
    // instead of dropping each other's entries, and a corrupt file fails
    // open — loudly — rather than blocking the record.
    if clear {
        Limits::update(&path, |limits| limits.clear(provider))?;
        println!("{} is no longer rate-limited", provider.as_str());
        return Ok(ExitCode::SUCCESS);
    }
    let until = Utc::now() + chrono::Duration::minutes(minutes);
    Limits::update(&path, |limits| limits.set(provider, until, why))?;
    println!(
        "{} is rate-limited until {}",
        provider.as_str(),
        until.to_rfc3339()
    );
    Ok(ExitCode::SUCCESS)
}

/// `budget [--hours N] [--json]`.
fn report_budget(arguments: &[String]) -> Result<ExitCode, RunnerError> {
    let mut hours = 24;
    let mut json = false;
    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        match argument.as_str() {
            "--hours" => {
                hours = iterator
                    .next()
                    .ok_or_else(|| usage("--hours needs a number"))?
                    .parse::<i64>()
                    .map_err(|error| RunnerError::new(format!("--hours: {error}")))?;
                if hours <= 0 {
                    return Err(RunnerError::new("--hours must be positive"));
                }
            }
            "--json" => json = true,
            other => return Err(usage(&format!("unknown argument `{other}`"))),
        }
    }
    let limits = Limits::load(&config::limits_path())?;
    let report = budget::run(Utc::now(), hours, &limits)?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|error| RunnerError::new(error.to_string()))?
        );
    } else {
        for line in report.lines() {
            println!("{line}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn current_dir() -> Result<PathBuf, RunnerError> {
    env::current_dir().map_err(|error| RunnerError::new(error.to_string()))
}

const USAGE: &str = "\
alder-ext-runner — give a prompt to a model at some effort; get a handle

usage: alder-ext-runner start --repo <path> --branch <name> --tier <name> --prompt-file <path>
       alder-ext-runner status <handle>
       alder-ext-runner send <handle> --file <path> [--force]
       alder-ext-runner kill <handle>
       alder-ext-runner limit <provider> [--minutes <n>] [--clear] [--why <text>]
       alder-ext-runner budget [--hours <n>] [--json]

start   Launch one execution: a worktree beside the repo on the given branch,
        and a session running the tier's engine with the prompt file's
        contents as its final argument. Prints the handle and exits; the
        result's location is the branch. An unknown tier is an error, never a
        CLI default.
status  One word about a handle: running, done (the engine exited; the branch
        holds whatever it left), or dead (nothing answers to the handle).
send    Deliver a local file's contents as input to the running execution.
        Delivery is at-least-once. A send that tears between paste and Enter
        leaves the pane refusing further sends until a human resolves it or
        --force delivers anyway.
kill    End the execution. The worktree and branch remain.
limit   Record that a provider is rate-limited, so start serves its rungs
        from the other ladder until then.
budget  Trailing-window token spend per provider, read from local
        transcripts, plus rate-limit state.";
