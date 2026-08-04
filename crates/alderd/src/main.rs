use std::{
    env,
    path::{Path, PathBuf},
    process::ExitCode,
};

use alderd::{
    budget,
    config::Config,
    driver::Driver,
    effects::Host,
    error::DriverError,
    limits::{LIMITS_FILE, Limits},
    spawn::{self, WORKER_CMD_ENV},
    tier::Provider,
};
use chrono::Utc;

fn main() -> ExitCode {
    match start() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("alderd: {message}");
            ExitCode::from(1)
        }
    }
}

fn start() -> Result<ExitCode, String> {
    let mut root: Option<PathBuf> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--root" | "-C" => {
                root = Some(PathBuf::from(
                    arguments.next().ok_or("--root needs a path")?,
                ));
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown argument `{other}`\n\n{USAGE}"));
            }
            other => {
                rest.push(other.to_owned());
                rest.extend(arguments);
                break;
            }
        }
    }
    let root = match root {
        Some(root) => root,
        None => env::current_dir().map_err(|error| error.to_string())?,
    };
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot inspect `{}`: {error}", root.display()))?;
    if !root.join(".alder/config.json").is_file() {
        return Err(format!(
            "`{}` is not an initialized Alder project",
            root.display()
        ));
    }

    let (command, arguments) = match rest.split_first() {
        Some((command, arguments)) => (command.as_str(), arguments),
        // No subcommand is the daemon: the thing alderd has always been.
        None => {
            let config = Config::load(&root.join(".alder/driver.json"))
                .map_err(|error| error.to_string())?;
            Driver::new(Host::new(root, &config), config).run();
        }
    };
    match command {
        "spawn" => spawn_worker(root, arguments),
        "budget" => report_budget(root, arguments),
        "limit" => record_limit(root, arguments),
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
}

/// One dispatch: `alderd spawn <work-id> [tier]`.
fn spawn_worker(root: PathBuf, arguments: &[String]) -> Result<ExitCode, String> {
    let mut positional = arguments
        .iter()
        .filter(|argument| !argument.starts_with('-'));
    let work_id = positional
        .next()
        .ok_or_else(|| format!("spawn needs a work ID\n\n{USAGE}"))?;
    let requested = positional.next().map(String::as_str);
    if let Some(extra) = positional.next() {
        return Err(format!("spawn takes at most two arguments, got `{extra}`"));
    }
    // Resolved before anything is created: an unknown tier must never reach a
    // CLI, where it would launch at whatever that CLI defaults to.
    let requested = spawn::requested_tier(requested).map_err(|error| error.to_string())?;

    let host = Host::for_command(root.clone(), alder_binary(&root));
    let limits = Limits::load(&root.join(LIMITS_FILE)).unwrap_or_else(|error| {
        eprintln!("alderd: ignoring the rate-limit state: {error}");
        Limits::default()
    });
    let (tier, why) = spawn::dispatch_tier(requested, &limits, Utc::now());
    if let Some(why) = why {
        eprintln!("alderd: {why}");
    }
    let override_command = env::var(WORKER_CMD_ENV).ok();
    let spawned = spawn::spawn(&host, work_id, tier, override_command.as_deref())
        .map_err(|error| error.to_string())?;
    println!("{}", spawned.summary());
    Ok(ExitCode::SUCCESS)
}

/// `alderd budget [--hours N] [--json]`.
fn report_budget(root: PathBuf, arguments: &[String]) -> Result<ExitCode, String> {
    let mut hours = 24;
    let mut json = false;
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--hours" => {
                hours = arguments
                    .next()
                    .ok_or("--hours needs a number")?
                    .parse::<i64>()
                    .map_err(|error| format!("--hours: {error}"))?;
                if hours <= 0 {
                    return Err("--hours must be positive".to_owned());
                }
            }
            "--json" => json = true,
            other => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
        }
    }
    let limits = Limits::load(&root.join(LIMITS_FILE)).map_err(|error| error.to_string())?;
    let report = budget::run(Utc::now(), hours, &limits).map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|error| error.to_string())?
        );
    } else {
        for line in report.lines() {
            println!("{line}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `alderd limit <provider> [--minutes N] [--clear] [--why TEXT]`.
fn record_limit(root: PathBuf, arguments: &[String]) -> Result<ExitCode, String> {
    let mut provider: Option<Provider> = None;
    let mut minutes: i64 = 60;
    let mut clear = false;
    let mut why: Option<String> = None;
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--minutes" => {
                minutes = arguments
                    .next()
                    .ok_or("--minutes needs a number")?
                    .parse::<i64>()
                    .map_err(|error| format!("--minutes: {error}"))?;
                if minutes <= 0 {
                    return Err("--minutes must be positive".to_owned());
                }
            }
            "--clear" => clear = true,
            "--why" => why = Some(arguments.next().ok_or("--why needs a reason")?.clone()),
            other if other.starts_with('-') => {
                return Err(format!("unknown argument `{other}`\n\n{USAGE}"));
            }
            other => {
                provider = Some(Provider::parse(other).map_err(|error| error.to_string())?);
            }
        }
    }
    let provider = provider.ok_or_else(|| format!("limit needs a provider\n\n{USAGE}"))?;
    let path = root.join(LIMITS_FILE);
    let mut limits = Limits::load(&path).map_err(|error| error.to_string())?;
    if clear {
        limits.clear(provider);
        limits.save(&path).map_err(|error| error.to_string())?;
        println!("{} is no longer rate-limited", provider.as_str());
        return Ok(ExitCode::SUCCESS);
    }
    let until = Utc::now() + chrono::Duration::minutes(minutes);
    limits.set(provider, until, why);
    limits.save(&path).map_err(|error| error.to_string())?;
    println!(
        "{} is rate-limited until {}",
        provider.as_str(),
        until.to_rfc3339()
    );
    Ok(ExitCode::SUCCESS)
}

/// The `alder` binary this run should use: the environment first, so a test
/// can point at a build; then `.alder/driver.json`; then `PATH`.
fn alder_binary(root: &Path) -> String {
    if let Some(binary) = env::var_os("ALDER_BIN") {
        return binary.to_string_lossy().into_owned();
    }
    Config::load(&root.join(".alder/driver.json"))
        .map(|config| config.alder)
        .unwrap_or_else(|_: DriverError| "alder".to_owned())
}

const USAGE: &str = "\
alderd — wake the Alder executor, and dispatch its workers

usage: alderd [--root <project>]                     run the driving loop
       alderd [--root <project>] spawn <work-id> [tier]
       alderd [--root <project>] budget [--hours <n>] [--json]
       alderd [--root <project>] limit <provider> [--minutes <n>] [--clear] [--why <text>]

The loop reads .alder/driver.json for engines, the pass document, and its
timings, and reaches the Alder log only by running the `alder` CLI.

spawn   Launch one worker for one work item: an attempt, a worktree on
        work/<work-id>, and a tmux session running the engine on the item's
        goal. Tiers are luna, terra, sol (codex) and sonnet, opus, fable
        (claude); each pins a model and an effort. The default is terra. An
        unknown tier is an error, never a CLI default.
budget  Trailing-window token spend per provider, read from local
        transcripts, plus rate-limit state.
limit   Record that a provider is rate-limited, so dispatch serves its rungs
        from the other ladder until then.";
