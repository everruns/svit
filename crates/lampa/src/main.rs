use std::env;
use std::process::ExitCode;

use svit::{Builtins, HttpAllowlist, OpenAI, Reasoner, Svit};

mod tui;

const DEFAULT_MODEL: &str = "gpt-5.6-terra";

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
    let mut arguments = env::args().skip(1);
    let model = parse_model(&mut arguments)?;
    let svit = Svit::builder("svit://local/lampa/process")
        .map_err(|error| error.to_string())?
        .reasoner(Reasoner::new(
            &model,
            OpenAI::from_env().map_err(|error| error.to_string())?,
        ))
        .builtins(Builtins::standard().with_http_allowlist(lampa_http_allowlist()))
        .build()
        .await
        .map_err(|error| error.to_string())?;
    tui::run(svit, model).await
}

fn parse_model(arguments: &mut impl Iterator<Item = String>) -> Result<String, String> {
    match arguments.next() {
        None => Ok(env::var("SVIT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string())),
        Some(flag) if flag == "--model" => {
            let model = arguments
                .next()
                .ok_or_else(|| "--model requires a value".to_string())?;
            if arguments.next().is_some() {
                return Err("usage: lampa [--model <model>]".into());
            }
            Ok(model)
        }
        Some(_) => Err("usage: lampa [--model <model>]".into()),
    }
}

fn lampa_http_allowlist() -> HttpAllowlist {
    env::var("LAMPA_HTTP_ALLOW")
        .ok()
        .map(|roots| {
            roots
                .split(',')
                .map(str::trim)
                .filter(|root| !root.is_empty())
                .fold(HttpAllowlist::new(), HttpAllowlist::allow)
        })
        .unwrap_or_default()
}
