use std::process::ExitCode;

use alder::{app::App, cli::Cli, error::AlderError};
use clap::Parser;
use serde_json::{Value, json};

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
        for line in context_lines(&error.context) {
            eprintln!("  {line}");
        }
    }
}

/// Render error context as indented fields rather than as a JSON document.
///
/// A failure printed as a pretty JSON object has the shape of a result, and a
/// caller skimming a terminal reads it as one: that is how a `pass end` that
/// lost its compare-and-append once passed for a receipt, leaving the pass
/// open and its report unwritten. Indented `key: value` lines under an
/// `error [...]` line cannot be mistaken for the one JSON document a
/// successful command prints, and they lose none of the context.
fn context_lines(context: &Value) -> Vec<String> {
    match context {
        Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| format!("{key}: {}", field(value)))
            .collect(),
        Value::Null => Vec::new(),
        other => vec![field(other)],
    }
}

/// One context value on one line. Bare text stays bare; anything that would
/// wrap onto a second line — a nested object, an array, text with a newline in
/// it — is escaped to its compact JSON form, so one field is always one line.
fn field(value: &Value) -> String {
    match value {
        Value::String(text) if !text.contains('\n') => text.clone(),
        other => serde_json::to_string(other).expect("error context serialization"),
    }
}
