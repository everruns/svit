use std::error::Error;
use std::path::PathBuf;

use svit::{Limits, Process, Script, SnapshotMount, value};

const INSPECT: &str = r#"
    (define (main input)
      (value-map
        "folder_text" (read "/mounts/files/data/greeting.txt")
        "database_name" (read "/mounts/catalog/data/0/name")))
"#;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let limits = Limits::default();
    let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/mount-data");
    let files = SnapshotMount::folder(folder, &limits)?;

    let database = turso::Builder::new_local(":memory:").build().await?;
    let connection = database.connect()?;
    connection
        .execute_batch(
            "CREATE TABLE catalog (id INTEGER, name TEXT);\n\
             INSERT INTO catalog VALUES (1, 'alpha'), (2, 'beta');",
        )
        .await?;
    let catalog = SnapshotMount::turso_query(
        &connection,
        "SELECT id, name FROM catalog ORDER BY id",
        (),
        &limits,
    )
    .await?;

    let mut process = Process::builder("svit://local/example/mounted-resources")?
        .limits(limits)
        .mount("files", files)
        .mount("catalog", catalog)
        .library("inspect", Script::new(INSPECT))
        .build()?;
    let activation = process.exec("inspect", value!({}))?;

    assert_eq!(
        activation.output,
        value!({
            "database_name": "alpha",
            "folder_text": "hello from a real folder\n"
        })
    );
    assert_eq!(process.discover("/mounts")?, vec!["catalog", "files"]);
    println!("{}", activation.output);
    Ok(())
}
