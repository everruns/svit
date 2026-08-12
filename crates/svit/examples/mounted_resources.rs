//! Virtual mounts: a real folder and a Turso query resolved through one
//! process namespace, with facts describing what each read costs.

use std::error::Error;
use std::fs;
use std::path::PathBuf;

use svit::{Limits, Mount, Process, Script, value};

const INSPECT: &str = r#"
    (define (main input)
      (value-map
        "folder_text" (read "/mounts/files/greeting.txt")
        "folder_locality" (value-get (stat "/mounts/files/greeting.txt") "/locality")
        "database_name" (read "/mounts/catalog/0/name")
        "database_locality" (value-get (stat "/mounts/catalog") "/locality")
        "notes" (discover "/mounts/files/notes")))
"#;

const RECORD: &str = r#"
    (define (main input)
      (do
        (write "/mounts/workspace/summary.md" (value-get input "/summary"))
        (write "/memory/recorded" true)
        "recorded"))
"#;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let limits = Limits::default();
    let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/mount-data");
    let workspace = std::env::temp_dir().join(format!("svit-mounted-{}", std::process::id()));
    let _ = fs::remove_dir_all(&workspace);
    fs::create_dir_all(&workspace)?;

    let database = turso::Builder::new_local(":memory:").build().await?;
    let connection = database.connect()?;
    connection
        .execute_batch(
            "CREATE TABLE catalog (id INTEGER, name TEXT);\n\
             INSERT INTO catalog VALUES (1, 'alpha'), (2, 'beta');",
        )
        .await?;
    let catalog = Mount::turso_query(
        &connection,
        "SELECT id, name FROM catalog ORDER BY id",
        (),
        &limits,
    )
    .await?;

    let mut process = Process::builder("svit://local/example/mounted-resources")?
        .limits(limits)
        .mount("files", Mount::folder(&folder)?)
        .mount("catalog", catalog)
        .mount("workspace", Mount::writable_folder(&workspace)?)
        .library("inspect", Script::new(INSPECT))
        .library("record", Script::new(RECORD))
        .build()?;

    let activation = process.exec("/lib/inspect", value!({}))?;
    assert_eq!(
        activation.output,
        value!({
            "database_locality": "cache",
            "database_name": "alpha",
            "folder_locality": "local",
            "folder_text": "hello from a real folder\n",
            "notes": ["checklist.md"]
        })
    );

    // The committed root records mount identity, never mount data.
    assert_eq!(
        process.discover("/mounts")?,
        vec![
            "catalog".to_owned(),
            "files".to_owned(),
            "workspace".to_owned()
        ]
    );
    assert!(!String::from_utf8(process.snapshot()?)?.contains("hello from a real folder"));

    // A granted write reaches the real folder as part of the activation.
    let recorded = process.exec("/lib/record", value!({"summary": "two rows\n"}))?;
    assert_eq!(recorded.output, value!("recorded"));
    assert_eq!(
        fs::read_to_string(workspace.join("summary.md"))?,
        "two rows\n"
    );

    // A read-only mount refuses the same write and commits nothing.
    let version = process.version();
    assert!(
        process
            .write("/mounts/files/greeting.txt", value!("no"))
            .is_err()
    );
    assert_eq!(process.version(), version);

    fs::remove_dir_all(&workspace)?;
    println!("{}", activation.output);
    Ok(())
}
