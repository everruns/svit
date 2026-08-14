use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use svit::{
    Builtins, DurableProcess, Error, Mount, OpenAI, Process, Reasoner, Svit, TursoProcessStore,
};

mod tui;

const DEFAULT_MODEL: &str = "gpt-5.6-terra";
const DEFAULT_INSTANCE_ID: &str = "default";
const USAGE: &str = "usage: lampa [--instance <instance-id>] [--model <model>] [--mount <name>=<path>] [--mount-rw <name>=<path>] [--import-legacy <shared-db>]";

/// Host-selected mounts for one console session.
struct Options {
    instance_id: String,
    model: String,
    mounts: Vec<(String, PathBuf, bool)>,
    legacy_import: Option<PathBuf>,
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
    let data_dir = match env::var_os("LAMPA_DATA_DIR") {
        Some(path) => PathBuf::from(path),
        None => default_data_directory()?,
    };
    let process = open_instance_process(
        &data_dir,
        &options.instance_id,
        &mounts,
        options.legacy_import.as_deref(),
    )
    .await?;
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

async fn open_instance_process(
    data_dir: &Path,
    instance_id: &str,
    mounts: &[(String, Mount)],
    legacy_import: Option<&Path>,
) -> Result<DurableProcess, String> {
    let address = lampa_address(instance_id)?;
    let database = database_path_in(data_dir, instance_id)?;
    let database_exists = database
        .try_exists()
        .map_err(|error| format!("could not inspect {}: {error}", database.display()))?;
    if legacy_import.is_some() && database_exists {
        return Err(format!(
            "cannot import into existing instance database {}",
            database.display()
        ));
    }
    let imported = match legacy_import {
        Some(source) => Some(load_legacy_process(source, &address).await?),
        None => None,
    };
    let parent = database
        .parent()
        .expect("an instance database always has a parent directory");
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "could not create Lampa instance directory {}: {error}",
            parent.display()
        )
    })?;
    let store = TursoProcessStore::open(&database)
        .await
        .map_err(|error| error.to_string())?;
    let result = match imported {
        Some(process) => import_lampa_process(&store, process, mounts).await,
        None => open_lampa_process(&store, &address, mounts, !database_exists).await,
    };
    match result {
        Err(Error::PersistenceNotFound(_)) if database_exists => Err(format!(
            "instance database {} does not contain {address}",
            database.display()
        )),
        result => result.map_err(|error| error.to_string()),
    }
}

async fn load_legacy_process(database: &Path, address: &str) -> Result<Process, String> {
    if !database.is_file() {
        return Err(format!(
            "legacy Lampa database does not exist: {}",
            database.display()
        ));
    }
    let store = TursoProcessStore::open(database)
        .await
        .map_err(|error| error.to_string())?;
    store
        .resume(address)
        .await
        .map(|process| process.process_projection())
        .map_err(|error| format!("could not import {address}: {error}"))
}

async fn import_lampa_process(
    store: &TursoProcessStore,
    process: Process,
    mounts: &[(String, Mount)],
) -> svit::Result<DurableProcess> {
    let mut process = store.import(process).await?;
    attach_mounts(&mut process, mounts)?;
    Ok(process)
}

async fn open_lampa_process(
    store: &TursoProcessStore,
    address: &str,
    mounts: &[(String, Mount)],
    create: bool,
) -> svit::Result<DurableProcess> {
    match store.resume(address).await {
        Ok(mut process) => {
            attach_mounts(&mut process, mounts)?;
            Ok(process)
        }
        Err(Error::PersistenceNotFound(_)) if create => {
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

fn attach_mounts(process: &mut DurableProcess, mounts: &[(String, Mount)]) -> svit::Result<()> {
    for (name, mount) in mounts {
        process.attach_mount(name.clone(), mount.clone())?;
    }
    Ok(())
}

fn lampa_address(instance_id: &str) -> Result<String, String> {
    if instance_id.is_empty()
        || instance_id.len() > 64
        || !instance_id
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !instance_id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(
            "instance ID must start with a lowercase ASCII letter or digit and contain only 1-64 lowercase letters, digits, '-', or '_'".into(),
        );
    }
    Ok(format!("svit://local/lampa/{instance_id}"))
}

fn database_path_in(data_dir: &Path, instance_id: &str) -> Result<PathBuf, String> {
    lampa_address(instance_id)?;
    Ok(data_dir.join("instances").join(instance_id).join("svit.db"))
}

fn default_data_directory() -> Result<PathBuf, String> {
    platform_data_directory()
        .map(|path| path.join("lampa"))
        .ok_or_else(|| {
            "could not resolve a platform user data directory; set LAMPA_DATA_DIR explicitly".into()
        })
}

#[cfg(target_os = "macos")]
fn platform_data_directory() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
}

#[cfg(target_os = "windows")]
fn platform_data_directory() -> Option<PathBuf> {
    env::var_os("APPDATA").map(PathBuf::from)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_data_directory() -> Option<PathBuf> {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
}

fn parse_options(arguments: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut arguments = arguments.peekable();
    let mut instance_id = None;
    let mut model = None;
    let mut mounts = Vec::new();
    let mut legacy_import = None;
    while let Some(flag) = arguments.next() {
        let value = || arguments_value(&flag);
        match flag.as_str() {
            "--instance" => {
                instance_id = Some(arguments.next().ok_or_else(value)?);
            }
            "--model" => {
                model = Some(arguments.next().ok_or_else(value)?);
            }
            "--import-legacy" => {
                legacy_import = Some(PathBuf::from(arguments.next().ok_or_else(value)?));
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
        legacy_import,
    })
}

fn arguments_value(flag: &str) -> String {
    format!("{flag} requires a value")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use svit::value;
    use tempfile::tempdir;

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
    async fn lampa_instances_use_separate_databases() {
        let data_dir = tempdir().unwrap();
        let mut first = open_instance_process(data_dir.path(), "research-one", &[], None)
            .await
            .unwrap();
        first
            .write("/memory/restarted", value!(true))
            .await
            .unwrap();
        drop(first);

        let second = open_instance_process(data_dir.path(), "research-two", &[], None)
            .await
            .unwrap();
        assert_ne!(
            database_path_in(data_dir.path(), "research-one").unwrap(),
            database_path_in(data_dir.path(), "research-two").unwrap()
        );
        assert_eq!(second.read("/memory/restarted").unwrap(), None);

        let resumed = open_instance_process(data_dir.path(), "research-one", &[], None)
            .await
            .unwrap();
        assert_eq!(
            resumed.read("/memory/restarted").unwrap(),
            Some(value!(true))
        );
    }

    #[tokio::test]
    async fn existing_instance_database_must_contain_its_root_address() {
        let data_dir = tempdir().unwrap();
        let database = database_path_in(data_dir.path(), "research-one").unwrap();
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        let store = TursoProcessStore::open(&database).await.unwrap();
        store
            .create(Process::new("svit://local/lampa/other").unwrap())
            .await
            .unwrap();

        let error = match open_instance_process(data_dir.path(), "research-one", &[], None).await {
            Ok(_) => panic!("mismatched instance database was accepted"),
            Err(error) => error,
        };

        assert!(error.contains("does not contain svit://local/lampa/research-one"));
    }

    #[tokio::test]
    async fn explicit_legacy_import_preserves_current_process_state() {
        let root = tempdir().unwrap();
        let legacy_database = root.path().join("lampa.db");
        let legacy_store = TursoProcessStore::open(&legacy_database).await.unwrap();
        let address = lampa_address("research-one").unwrap();
        let mut legacy = legacy_store
            .create(Process::new(&address).unwrap())
            .await
            .unwrap();
        legacy
            .write("/memory/imported", value!("yes"))
            .await
            .unwrap();
        let expected_version = legacy.version();
        let expected_root_hash = legacy.root_hash();
        drop(legacy);

        let imported =
            open_instance_process(root.path(), "research-one", &[], Some(&legacy_database))
                .await
                .unwrap();

        assert_eq!(
            imported.read("/memory/imported").unwrap(),
            Some(value!("yes"))
        );
        assert_eq!(imported.version(), expected_version);
        assert_eq!(imported.root_hash(), expected_root_hash);
        assert!(
            database_path_in(root.path(), "research-one")
                .unwrap()
                .is_file()
        );

        let error =
            match open_instance_process(root.path(), "research-one", &[], Some(&legacy_database))
                .await
            {
                Ok(_) => panic!("legacy import overwrote an existing instance"),
                Err(error) => error,
            };
        assert!(error.contains("cannot import into existing instance database"));
    }

    #[test]
    fn instance_id_becomes_one_address_segment() {
        assert_eq!(
            lampa_address("research-one").unwrap(),
            "svit://local/lampa/research-one"
        );
        assert!(lampa_address("Research-One").is_err());
        assert!(lampa_address("research.one").is_err());
        assert!(lampa_address(".").is_err());
        assert!(lampa_address("..").is_err());
        assert!(lampa_address("../other").is_err());
        assert!(lampa_address("").is_err());
        assert!(database_path_in(Path::new("/user-data/lampa"), "../other").is_err());
    }

    #[test]
    fn database_lives_below_the_platform_user_data_directory() {
        assert_eq!(
            database_path_in(Path::new("/user-data/lampa"), "research-one").unwrap(),
            Path::new("/user-data/lampa/instances/research-one/svit.db")
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
        let options = parse(&[
            "--instance",
            "experiment-7",
            "--model",
            "test-model",
            "--import-legacy",
            "/tmp/lampa.db",
        ])
        .unwrap();

        assert_eq!(options.instance_id, "experiment-7");
        assert_eq!(options.model, "test-model");
        assert_eq!(options.legacy_import, Some(PathBuf::from("/tmp/lampa.db")));
    }
}
