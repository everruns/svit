//! Virtual mount behavior across the process boundary: lazy resolution,
//! readable facts, granted writes, and authority that does not survive a
//! snapshot.

use std::fs;
use std::path::PathBuf;

use svit::{Error, Limits, Mount, Process, Script, Value, value};

const READ_MOUNT: &str = r#"
    (define (main input)
      (value-map
        "greeting" (read "/mounts/files/greeting.txt")
        "nested" (read "/mounts/files/notes/checklist.md")
        "children" (discover "/mounts/files")
        "locality" (value-get (stat "/mounts/files/greeting.txt") "/locality")
        "kind" (value-get (stat "/mounts/files") "/kind")))
"#;

const WRITE_MOUNT: &str = r#"
    (define (main input)
      (do
        (write "/mounts/workspace/note.txt" (value-get input "/text"))
        (write "/memory/written" true)
        "committed"))
"#;

const WRITE_THEN_FAIL: &str = r#"
    (define (main input)
      (do
        (write "/mounts/workspace/note.txt" "must not reach disk")
        (panic "the activation fails after buffering a mount write")))
"#;

fn fixture_folder() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/mount-data")
}

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("svit-mount-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn read_only_process() -> Process {
    Process::builder("svit://local/test/mounts")
        .unwrap()
        .mount("files", Mount::folder(fixture_folder()).unwrap())
        .library("read_mount", Script::new(READ_MOUNT))
        .build()
        .unwrap()
}

#[test]
fn folder_mounts_resolve_lazily_and_expose_node_facts() {
    let mut process = read_only_process();

    let activation = process.exec("/lib/read_mount", value!({})).unwrap();

    assert_eq!(
        activation.output,
        value!({
            "children": ["greeting.txt", "notes"],
            "greeting": "hello from a real folder\n",
            "kind": "directory",
            "locality": "local",
            "nested": "release checklist\n- run just check\n"
        })
    );
}

#[test]
fn the_committed_root_records_mount_identity_without_mount_data() {
    let process = read_only_process();

    let descriptor = process.read("/mounts/files").unwrap().unwrap();
    let snapshot = String::from_utf8(process.snapshot().unwrap()).unwrap();

    assert_eq!(
        process.read("/mounts").unwrap(),
        Some(value!({
            "files": {
                "access": "read",
                "kind": "folder",
                "locality": "local",
                "source": fixture_folder().canonicalize().unwrap().to_str().unwrap()
            }
        }))
    );
    // Reading the mount root answers "what is mounted here, and what is it now".
    assert_eq!(descriptor.to_json()["kind"], "directory");
    assert_eq!(descriptor.to_json()["attached"], true);
    assert_eq!(descriptor.to_json()["locality"], "local");
    // No file content crossed into the process root.
    assert!(!snapshot.contains("hello from a real folder"));
    assert_eq!(process.version(), 0);
}

#[test]
// THREAT[TM-CAP-001]
fn read_only_mounts_reject_host_and_guest_writes() {
    let mut process = read_only_process();
    let snapshot = process.snapshot().unwrap();

    let host = process.write("/mounts/files/greeting.txt", value!("replacement"));

    assert!(matches!(host, Err(Error::MountDenied(_))));
    assert_eq!(process.version(), 0);
    assert_eq!(process.snapshot().unwrap(), snapshot);
    assert_eq!(
        fs::read_to_string(fixture_folder().join("greeting.txt")).unwrap(),
        "hello from a real folder\n"
    );
}

#[test]
// THREAT[TM-CAP-007]
fn mount_paths_cannot_escape_their_root() {
    let process = read_only_process();

    assert!(matches!(
        process.read("/mounts/files/../../etc"),
        Err(Error::InvalidPath(_))
    ));
    assert!(matches!(
        process.discover("/mounts/files/.."),
        Err(Error::InvalidPath(_))
    ));
    // A path that simply does not exist reads as absent, not as an escape.
    assert_eq!(process.read("/mounts/files/absent.txt").unwrap(), None);
}

#[test]
fn granted_mount_writes_commit_with_the_activation() {
    let root = scratch("granted-write");
    fs::write(root.join("note.txt"), "before").unwrap();
    let mut process = Process::builder("svit://local/test/writable-mount")
        .unwrap()
        .mount("workspace", Mount::writable_folder(&root).unwrap())
        .library("write_mount", Script::new(WRITE_MOUNT))
        .build()
        .unwrap();

    let activation = process
        .exec("/lib/write_mount", value!({"text": "after"}))
        .unwrap();
    let written = fs::read_to_string(root.join("note.txt")).unwrap();

    fs::remove_dir_all(&root).unwrap();
    assert_eq!(activation.output, value!("committed"));
    assert_eq!(written, "after");
    assert_eq!(process.read("/memory/written").unwrap(), Some(value!(true)));
    assert_eq!(process.version(), 1);
}

#[test]
// THREAT[TM-EFF-006]
fn a_failed_activation_leaves_mount_writes_unapplied() {
    let root = scratch("rolled-back-write");
    fs::write(root.join("note.txt"), "before").unwrap();
    let mut process = Process::builder("svit://local/test/rolled-back-mount")
        .unwrap()
        .memory("count", value!(0))
        .mount("workspace", Mount::writable_folder(&root).unwrap())
        .library("write_then_fail", Script::new(WRITE_THEN_FAIL))
        .build()
        .unwrap();
    let snapshot = process.snapshot().unwrap();

    let result = process.exec("/lib/write_then_fail", value!({}));
    let unchanged = fs::read_to_string(root.join("note.txt")).unwrap();

    fs::remove_dir_all(&root).unwrap();
    assert!(result.is_err());
    assert_eq!(unchanged, "before");
    assert_eq!(process.version(), 0);
    assert_eq!(process.snapshot().unwrap(), snapshot);
}

#[test]
// THREAT[TM-CAP-008]
fn restoring_a_snapshot_keeps_mount_identity_but_not_mount_authority() {
    let process = read_only_process();
    let snapshot = process.snapshot().unwrap();

    let mut restored = Process::restore(&snapshot).unwrap();
    let root_record = restored.read("/mounts/files").unwrap().unwrap();
    let denied_read = restored.read("/mounts/files/greeting.txt");
    let denied_listing = restored.discover("/mounts/files");

    assert_eq!(restored.root_hash(), process.root_hash());
    assert_eq!(root_record.to_json()["attached"], false);
    assert_eq!(root_record.to_json()["kind"], "folder");
    assert!(matches!(denied_read, Err(Error::MountUnavailable(_))));
    assert!(matches!(denied_listing, Err(Error::MountUnavailable(_))));

    restored
        .attach_mount("files", Mount::folder(fixture_folder()).unwrap())
        .unwrap();

    assert_eq!(
        restored.read("/mounts/files/greeting.txt").unwrap(),
        Some(value!("hello from a real folder\n"))
    );
    // Reattaching identical identity is not a state transition.
    assert_eq!(restored.version(), process.version());
    assert_eq!(restored.root_hash(), process.root_hash());
}

#[test]
fn stat_describes_committed_nodes_with_the_same_facts_vocabulary() {
    let process = read_only_process();

    let memory = process.stat("/memory").unwrap().unwrap().to_json();
    let script = process.stat("/lib/read_mount").unwrap().unwrap().to_json();

    assert_eq!(memory["kind"], "directory");
    assert_eq!(memory["locality"], "cache");
    assert_eq!(memory["access"], "read-write");
    assert_eq!(script["kind"], "leaf");
    assert_eq!(script["facts"]["content"], "svit-script");
    assert_eq!(process.stat("/memory/absent").unwrap(), None);
}

#[test]
// THREAT[TM-DOS-010]
fn mount_reads_fail_closed_at_the_configured_limits() {
    let root = scratch("limits");
    fs::write(root.join("large.txt"), "x".repeat(4096)).unwrap();
    let limits = Limits {
        max_text_bytes: 2048,
        ..Limits::default()
    };
    let process = Process::builder("svit://local/test/mount-limits")
        .unwrap()
        .limits(limits)
        .mount("files", Mount::folder(&root).unwrap())
        .build()
        .unwrap();

    let result = process.read("/mounts/files/large.txt");

    fs::remove_dir_all(&root).unwrap();
    assert!(matches!(
        result,
        Err(Error::ResourceLimitExceeded("mount leaf bytes"))
    ));
}

#[test]
fn forked_processes_share_the_mounts_the_host_attached() {
    let process = read_only_process();

    let child = process.fork("svit://local/test/mounts-child").unwrap();

    assert_eq!(
        child.read("/mounts/files/greeting.txt").unwrap(),
        Some(value!("hello from a real folder\n"))
    );
}

#[tokio::test]
#[cfg(feature = "turso-mount")]
async fn turso_query_mounts_report_cache_locality() {
    let database = turso::Builder::new_local(":memory:").build().await.unwrap();
    let connection = database.connect().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE items (id INTEGER, name TEXT);\n\
             INSERT INTO items VALUES (1, 'alpha'), (2, 'beta');",
        )
        .await
        .unwrap();
    let limits = Limits::default();
    let catalog = Mount::turso_query(
        &connection,
        "SELECT id, name FROM items ORDER BY id",
        (),
        &limits,
    )
    .await
    .unwrap();

    let process = Process::builder("svit://local/test/turso-mount")
        .unwrap()
        .limits(limits)
        .mount("catalog", catalog)
        .build()
        .unwrap();

    assert_eq!(
        process.read("/mounts/catalog/0/name").unwrap(),
        Some(value!("alpha"))
    );
    assert_eq!(
        process.discover("/mounts/catalog").unwrap(),
        vec!["0".to_owned(), "1".to_owned()]
    );
    // Query rows are already materialized, so the mount says so rather than
    // claiming a live remote view.
    assert_eq!(
        process.stat("/mounts/catalog").unwrap().unwrap().to_json()["locality"],
        "cache"
    );
}

#[test]
fn unknown_mount_names_are_invalid_paths() {
    let process = read_only_process();

    assert!(matches!(
        process.read("/mounts/absent/thing"),
        Err(Error::InvalidPath(_))
    ));
    assert!(matches!(
        process.read("/mounts/not a name/thing"),
        Err(Error::InvalidMount(_))
    ));
    assert!(matches!(
        process.read("/mounts").unwrap(),
        Some(Value::Map(_))
    ));
}
