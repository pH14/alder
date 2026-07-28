use std::process::ExitCode;

use alder::{app::App, cli::Cli, error::AlderError};
use clap::Parser;
use serde_json::json;

fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args_os().collect();
    let wants_json = arguments.iter().any(|argument| argument == "--json");
    let cli = match Cli::try_parse_from(arguments) {
        Ok(cli) => cli,
        Err(error) => {
            if wants_json && error.use_stderr() {
                let value = json!({
                    "schema": "alder.error.v0",
                    "code": "invalid_command",
                    "message": error.to_string(),
                    "context": {},
                });
                println!(
                    "{}",
                    serde_json::to_string(&value).expect("JSON serialization")
                );
            } else {
                let _ = error.print();
            }
            return ExitCode::from(error.exit_code() as u8);
        }
    };
    match App::run(&cli.command) {
        Ok(output) => {
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string(&output.json).expect("JSON output serialization")
                );
            } else {
                println!("{}", output.human);
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_error(error, cli.json);
            ExitCode::from(1)
        }
    }
}

fn print_error(error: AlderError, json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&error.json()).expect("JSON error serialization")
        );
    } else {
        eprintln!("error [{}]: {}", error.code, error.message);
        if error.context != json!({}) {
            eprintln!(
                "{}",
                serde_json::to_string_pretty(&error.context).expect("error context serialization")
            );
        }
    }
}
