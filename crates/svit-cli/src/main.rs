use std::env;
use std::process::ExitCode;

use svit::{Process, Value, value};

fn main() -> ExitCode {
    match exec() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("svit: {error}");
            ExitCode::FAILURE
        }
    }
}

fn exec() -> Result<(), String> {
    let mut arguments = env::args().skip(1);
    let command = arguments.next().ok_or_else(usage)?;
    if command != "exec" {
        return Err(usage());
    }
    let script_path = arguments.next().ok_or_else(usage)?;
    let input = match arguments.next() {
        Some(input) => Value::from_json(
            serde_json::from_str(&input).map_err(|error| format!("invalid input JSON: {error}"))?,
        )
        .map_err(|error| error.to_string())?,
        None => Value::Null,
    };
    if arguments.next().is_some() {
        return Err(usage());
    }

    let source = std::fs::read_to_string(&script_path)
        .map_err(|error| format!("cannot read {script_path}: {error}"))?;
    let mut process =
        Process::new("svit://local/cli/process").map_err(|error| error.to_string())?;
    process
        .write("/lib/main", value!({"source": source}))
        .map_err(|error| error.to_string())?;
    let activation = process
        .exec("/lib/main", input)
        .map_err(|error| error.to_string())?;

    let document = serde_json::json!({
        "memory": process
            .read("/memory")
            .map_err(|error| error.to_string())?
            .expect("process root always has memory")
            .to_json(),
        "output": activation.output.to_json(),
        "version": activation.version,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&document).expect("JSON values are serializable")
    );
    Ok(())
}

fn usage() -> String {
    "usage: svit exec <script.svit-script> [input-json]".into()
}
