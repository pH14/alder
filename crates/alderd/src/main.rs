use std::{env, path::PathBuf, process::ExitCode};

use alderd::{config::Config, driver::Driver, effects::Host};

fn main() -> ExitCode {
    match start() {
        Ok(never) => never,
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
            other => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
        }
    }
    let root = match root {
        Some(root) => root,
        None => env::current_dir().map_err(|error| error.to_string())?,
    };
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot inspect `{}`: {error}", root.display()))?;

    let config =
        Config::load(&root.join(".alder/driver.json")).map_err(|error| error.to_string())?;
    if !root.join(".alder/config.json").is_file() {
        return Err(format!(
            "`{}` is not an initialized Alder project",
            root.display()
        ));
    }
    Driver::new(Host::new(root, &config), config).run();
}

const USAGE: &str = "\
alderd — decide when to wake the Alder leader

usage: alderd [--root <project>]

Reads .alder/driver.json for engines, the pass document, and its timings.
Reaches the Alder log only by running the `alder` CLI.";
