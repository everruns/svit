use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use svit::{
    Builtins, DurableProcess, Error, Mount, OpenAI, Process, Reasoner, Svit, TursoProcessStore,
};

mod tui;

const DEFAULT_MODEL: &str = "gpt-5.6-terra";
const DEFAULT_INSTANCE_ID: &str = "default";
const USAGE: &str = "usage: lampa [--instance <instance-id>] [--model <model>] [--mount <name>=<path>] [--mount-rw <name>=<path>]";

/// Host-selected mounts for one console session.
struct Options {
    instance_id: String,
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
    let address = lampa_address(&options.instance_id)?;
    let cwd = env::current_dir().map_err(|error| error.to_string())?;
    let mut mounts = vec![(
        "cwd".to_owned(),
        Mount::folder(&cwd).map_err(|error| error.to_string())?,
    )];
    for (name, path, writable) in &options.mounts {
        let mount = if *writable {
            Mount::writable_folder(path)
        } else {
            Mount::folder(path)
        }
        .map_err(|error| format!("{name}: {error}"))?;
        mounts.push((name.clone(), mount));
    }
    let database = match env::var_os("LAMPA_DB") {
        Some(path) => PathBuf::from(path),
        None => default_database_path()?,
    };
    if let Some(parent) = database
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "could not create Lampa data directory {}: {error}",
                parent.display()
            )
        })?;
    }
    let store = TursoProcessStore::open(&database)
        .await
        .map_err(|error| error.to_string())?;
    let process = open_lampa_process(&store, &address, &mounts)
        .await
        .map_err(|error| error.to_string())?;
    let svit = Svit::persisted(process)
        .map_err(|error| error.to_string())?
        .reasoner(Reasoner::new(
            &options.model,
            OpenAI::from_env().map_err(|error| error.to_string())?,
        ))
        .builtins(Builtins::standard())
        .build()
        .await
        .map_err(|error| error.to_string())?;

    tui::run(svit, options.model).await
}

async fn open_lampa_process(
    store: &TursoProcessStore,
    address: &str,
    mounts: &[(String, Mount)],
) -> svit::Result<DurableProcess> {
    match store.resume(address).await {
        Ok(mut process) => {
            for (name, mount) in mounts {
                process.attach_mount(name.clone(), mount.clone())?;
            }
            Ok(process)
        }
        Err(Error::PersistenceNotFound(_)) => {
            let mut builder = Process::builder(address)?;
            for (name, mount) in mounts {
                builder = builder.mount(name.clone(), mount.clone());
            }
            let process = builder.build()?;
            store.create(process).await
        }
        Err(error) => Err(error),
    }
}

fn lampa_address(instance_id: &str) -> Result<String, String> {
    if instance_id.is_empty()
        || instance_id.len() > 64
        || !instance_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("instance ID must contain 1-64 ASCII letters, digits, '-', '_', or '.'".into());
    }
    Ok(format!("svit://local/lampa/{instance_id}"))
}

fn database_path_in(data_dir: &Path) -> PathBuf {
    data_dir.join("lampa").join("lampa.db")
}

fn default_database_path() -> Result<PathBuf, String> {
    dirs::data_dir()
        .map(|path| database_path_in(&path))
        .ok_or_else(|| {
            "could not resolve a platform user data directory; set LAMPA_DB explicitly".into()
        })
}

fn parse_options(arguments: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut arguments = arguments.peekable();
    let mut instance_id = None;
    let mut model = None;
    let mut mounts = Vec::new();
    while let Some(flag) = arguments.next() {
        let value = || arguments_value(&flag);
        match flag.as_str() {
            "--instance" => {
                instance_id = Some(arguments.next().ok_or_else(value)?);
            }
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
    let instance_id = instance_id
        .or_else(|| env::var("LAMPA_INSTANCE_ID").ok())
        .unwrap_or_else(|| DEFAULT_INSTANCE_ID.to_string());
    lampa_address(&instance_id)?;
    Ok(Options {
        instance_id,
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
    use std::path::Path;

    use svit::value;

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

    #[tokio::test]
    async fn lampa_instances_share_a_store_without_sharing_state() {
        let store = TursoProcessStore::memory().await.unwrap();
        let first_address = lampa_address("research-one").unwrap();
        let second_address = lampa_address("research-two").unwrap();
        let mut first = open_lampa_process(&store, &first_address, &[])
            .await
            .unwrap();
        first
            .write("/memory/restarted", value!(true))
            .await
            .unwrap();
        drop(first);

        let second = open_lampa_process(&store, &second_address, &[])
            .await
            .unwrap();
        assert_eq!(second.read("/memory/restarted").unwrap(), None);

        let resumed = open_lampa_process(&store, &first_address, &[])
            .await
            .unwrap();
        assert_eq!(
            resumed.read("/memory/restarted").unwrap(),
            Some(value!(true))
        );
    }

    #[test]
    fn instance_id_becomes_one_address_segment() {
        assert_eq!(
            lampa_address("research-one").unwrap(),
            "svit://local/lampa/research-one"
        );
        assert!(lampa_address("../other").is_err());
        assert!(lampa_address("").is_err());
    }

    #[test]
    fn database_lives_below_the_platform_user_data_directory() {
        assert_eq!(
            database_path_in(Path::new("/user-data")),
            Path::new("/user-data/lampa/lampa.db")
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

    #[test]
    fn instance_and_model_flags_compose() {
        let options = parse(&["--instance", "experiment-7", "--model", "test-model"]).unwrap();

        assert_eq!(options.instance_id, "experiment-7");
        assert_eq!(options.model, "test-model");
    }
}
