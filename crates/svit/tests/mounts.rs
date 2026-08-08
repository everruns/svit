use std::path::PathBuf;

use svit::{Error, Limits, Process, Script, SnapshotMount, value};

const READ_MOUNTS: &str = r#"
    (define (main input)
      (value-map
        "greeting" (read "/mounts/files/data/greeting.txt")
        "first_item" (read "/mounts/catalog/data/0/name")))
"#;

fn fixture_folder() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/mount-data")
}

#[tokio::test]
async fn folder_and_turso_mounts_are_guest_readable_and_snapshot_stable() {
    let limits = Limits::default();
    let database = turso::Builder::new_local(":memory:").build().await.unwrap();
    let connection = database.connect().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE items (id INTEGER, name TEXT);\n\
             INSERT INTO items VALUES (1, 'alpha'), (2, 'beta');",
        )
        .await
        .unwrap();

    let files = SnapshotMount::folder(fixture_folder(), &limits).unwrap();
    let catalog = SnapshotMount::turso_query(
        &connection,
        "SELECT id, name FROM items ORDER BY id",
        (),
        &limits,
    )
    .await
    .unwrap();
    let mut process = Process::builder("svit://local/test/mounts")
        .unwrap()
        .limits(limits)
        .mount("files", files)
        .mount("catalog", catalog)
        .library("read_mounts", Script::new(READ_MOUNTS))
        .build()
        .unwrap();

    let activation = process.exec("/lib/read_mounts", value!({})).unwrap();
    assert_eq!(
        activation.output,
        value!({"first_item": "alpha", "greeting": "hello from a real folder\n"})
    );

    let snapshot = process.snapshot().unwrap();
    let restored = Process::restore(&snapshot).unwrap();
    assert_eq!(restored.root_hash(), process.root_hash());
    assert_eq!(
        restored.read("/mounts/catalog/data/1/name").unwrap(),
        Some(&value!("beta"))
    );
}

#[test]
// THREAT[TM-CAP-001]
fn mount_writes_fail_without_mutating_the_process() {
    let limits = Limits::default();
    let files = SnapshotMount::folder(fixture_folder(), &limits).unwrap();
    let mut process = Process::builder("svit://local/test/read-only-mount")
        .unwrap()
        .mount("files", files)
        .build()
        .unwrap();
    let snapshot = process.snapshot().unwrap();

    let result = process.write("/mounts/files/data/greeting.txt", value!("replacement"));

    assert!(matches!(result, Err(Error::InvalidPath(_))));
    assert_eq!(process.version(), 0);
    assert_eq!(process.snapshot().unwrap(), snapshot);
}
