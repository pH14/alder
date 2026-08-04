use std::{env, path::PathBuf, process::ExitCode};

use alderd::{config::Config, driver::Driver, effects::Host};

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
            other => {
                return Err(format!("unknown argument `{other}`\n\n{USAGE}"));
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

    let config =
        Config::load(&root.join(".alder/driver.json")).map_err(|error| error.to_string())?;
    Driver::new(Host::new(root, &config), config).run();
}

const USAGE: &str = "\
alderd — watch the Alder log, and run the configured command when it moves

usage: alderd [--root <project>]

The loop reads .alder/driver.json for its command and its timings, and
reaches the Alder log only by running the `alder` CLI. When a trigger fires —
the head moved past this daemon's local note, a review deadline arrived, a
nudge was requested, or the max-interval ceiling elapsed — it runs `command`
with the trigger names in ALDERD_TRIGGERS. What that command does — sessions,
engines, prompts — is its own business; the daemon appends nothing and knows
nothing about work.";
