use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use svit::{Builtins, Mount, OpenAI, Reasoner, Svit};

mod tui;

const DEFAULT_MODEL: &str = "gpt-5.6-terra";
const USAGE: &str =
    "usage: lampa [--model <model>] [--mount <name>=<path>] [--mount-rw <name>=<path>]";

/// Host-selected mounts for one console session.
struct Options {
    model: String,
    mounts: Vec<(String, PathBuf, bool)>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lampa: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let options = parse_options(env::args().skip(1))?;
    let mut builder = Svit::builder("svit://local/lampa/process")
        .map_err(|error| error.to_string())?
        .reasoner(Reasoner::new(
            &options.model,
            OpenAI::from_env().map_err(|error| error.to_string())?,
        ))
        .builtins(Builtins::standard());

    // The working directory is the console's default subject. It is mounted
    // read-only; writable roots are an explicit per-run decision.
    let cwd = env::current_dir().map_err(|error| error.to_string())?;
    builder = builder.mount(
        "cwd",
        Mount::folder(&cwd).map_err(|error| error.to_string())?,
    );
    for (name, path, writable) in options.mounts {
        let mount = if writable {
            Mount::writable_folder(&path)
        } else {
            Mount::folder(&path)
        }
        .map_err(|error| format!("{name}: {error}"))?;
        builder = builder.mount(name, mount);
    }

    let svit = builder.build().await.map_err(|error| error.to_string())?;
    tui::run(svit, options.model).await
}

fn parse_options(arguments: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut arguments = arguments.peekable();
    let mut model = None;
    let mut mounts = Vec::new();
    while let Some(flag) = arguments.next() {
        let value = || arguments_value(&flag);
        match flag.as_str() {
            "--model" => {
                model = Some(arguments.next().ok_or_else(value)?);
            }
            "--mount" | "--mount-rw" => {
                let specification = arguments.next().ok_or_else(value)?;
                let (name, path) = specification
                    .split_once('=')
                    .ok_or_else(|| format!("{flag} expects <name>=<path>"))?;
                if name.is_empty() || path.is_empty() {
                    return Err(format!("{flag} expects <name>=<path>"));
                }
                mounts.push((name.to_owned(), PathBuf::from(path), flag == "--mount-rw"));
            }
            _ => return Err(USAGE.into()),
        }
    }
    Ok(Options {
        model: model
            .or_else(|| env::var("SVIT_MODEL").ok())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        mounts,
    })
}

fn arguments_value(flag: &str) -> String {
    format!("{flag} requires a value")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<Options, String> {
        parse_options(arguments.iter().map(|argument| (*argument).to_string()))
    }

    #[test]
    fn named_mounts_record_their_write_grant() {
        let options = parse(&[
            "--mount",
            "docs=/tmp/docs",
            "--mount-rw",
            "notes=/tmp/notes",
        ])
        .expect("valid arguments parse");

        assert_eq!(
            options.mounts,
            vec![
                ("docs".to_owned(), PathBuf::from("/tmp/docs"), false),
                ("notes".to_owned(), PathBuf::from("/tmp/notes"), true),
            ]
        );
    }

    #[test]
    fn malformed_mount_specifications_are_rejected() {
        assert!(parse(&["--mount", "docs"]).is_err());
        assert!(parse(&["--mount", "=/tmp/docs"]).is_err());
        assert!(parse(&["--mount"]).is_err());
        assert!(parse(&["--unknown"]).is_err());
    }

    #[test]
    fn the_model_flag_overrides_the_default() {
        assert_eq!(
            parse(&["--model", "test-model"]).unwrap().model,
            "test-model"
        );
    }
}
