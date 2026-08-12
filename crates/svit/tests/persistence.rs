#![cfg(feature = "persistence-turso")]

use svit::{
    DurableProcessHandle, Error, EventQuery, Process, ProcessId, ProcessStore, Script,
    TursoProcessStore, Value, value,
};

const COUNTER: &str = r#"
    (define (main input)
      (let ((count (+ (read "/memory/count") (value-get input "/by"))))
        (do
          (write "/memory/count" count)
          (write "/memory/last" (value-get input "/by"))
          count)))
"#;

#[tokio::test]
async fn committed_events_resume_without_reexecuting_guest_code() {
    let store = TursoProcessStore::memory().await.unwrap();
    let process = Process::builder("svit://local/persistence/resume")
        .unwrap()
        .memory("count", value!(0))
        .library("counter", Script::new(COUNTER))
        .build()
        .unwrap();
    let mut durable = store.create(process).await.unwrap();

    let activation = durable
        .exec("/lib/counter", value!({"by": 2}))
        .await
        .unwrap();
    assert_eq!(activation.output, value!(2));
    assert_eq!(durable.version(), 1);

    drop(durable);
    let resumed = store
        .resume("svit://local/persistence/resume")
        .await
        .unwrap();
    assert_eq!(resumed.version(), 1);
    assert_eq!(resumed.read("/memory/count").unwrap(), Some(&value!(2)));
    assert_eq!(resumed.read("/memory/last").unwrap(), Some(&value!(2)));

    let events = resumed
        .query(EventQuery::new().path_prefix("/memory"))
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].touched_paths(),
        &["/memory/count".to_owned(), "/memory/last".to_owned()]
    );
}

#[tokio::test]
async fn local_database_reopens_by_address() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("svit.db");
    let store = TursoProcessStore::open(&path).await.unwrap();
    let process = Process::builder("svit://local/persistence/reopen")
        .unwrap()
        .memory("state", value!("new"))
        .build()
        .unwrap();
    let mut durable = ProcessStore::create(&store, process).await.unwrap();
    DurableProcessHandle::write(&mut durable, "/memory/state", value!("committed"))
        .await
        .unwrap();
    drop(durable);
    drop(store);

    let reopened = TursoProcessStore::open(path).await.unwrap();
    let address = ProcessId::new("svit://local/persistence/reopen").unwrap();
    let resumed = ProcessStore::resume(&reopened, &address).await.unwrap();
    assert_eq!(
        resumed.read("/memory/state").unwrap(),
        Some(&value!("committed"))
    );
}

#[tokio::test]
// THREAT[TM-FORK-003]
async fn fork_references_history_until_the_child_is_cut() {
    let store = TursoProcessStore::memory().await.unwrap();
    let process = Process::builder("svit://local/persistence/parent")
        .unwrap()
        .memory("count", value!(0))
        .build()
        .unwrap();
    let mut parent = store.create(process).await.unwrap();
    parent.write("/memory/count", value!(1)).await.unwrap();

    let mut child = parent.fork("svit://local/persistence/child").await.unwrap();
    child.write("/memory/count", value!(2)).await.unwrap();
    assert_eq!(parent.read("/memory/count").unwrap(), Some(&value!(1)));
    assert_eq!(child.read("/memory/count").unwrap(), Some(&value!(2)));

    assert!(parent.cut().await.is_err());
    let child_snapshot = child.snapshot().await.unwrap();
    assert_eq!(child_snapshot.process_version(), 2);
    child.cut().await.unwrap();
    parent.cut().await.unwrap();

    drop(parent);
    drop(child);
    let resumed_parent = store
        .resume("svit://local/persistence/parent")
        .await
        .unwrap();
    let resumed_child = store
        .resume("svit://local/persistence/child")
        .await
        .unwrap();
    assert_eq!(
        resumed_parent.read("/memory/count").unwrap(),
        Some(&value!(1))
    );
    assert_eq!(
        resumed_child.read("/memory/count").unwrap(),
        Some(&value!(2))
    );
    assert!(
        resumed_parent
            .query(EventQuery::new())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        resumed_child
            .query(EventQuery::new())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
// THREAT[TM-DOS-009]
async fn event_queries_fail_closed_outside_the_result_bound() {
    let store = TursoProcessStore::memory().await.unwrap();
    let process = Process::builder("svit://local/persistence/query-limit")
        .unwrap()
        .build()
        .unwrap();
    let durable = store.create(process).await.unwrap();

    assert!(matches!(
        durable.query(EventQuery::new().limit(4097)).await,
        Err(Error::ResourceLimitExceeded("persistence query events"))
    ));
}

#[tokio::test]
// THREAT[TM-PERS-002]
async fn stale_writer_cannot_publish_or_mutate_its_local_process() {
    let store = TursoProcessStore::memory().await.unwrap();
    let process = Process::builder("svit://local/persistence/cas")
        .unwrap()
        .memory("count", value!(0))
        .build()
        .unwrap();
    store.create(process).await.unwrap();
    let mut first = store.resume("svit://local/persistence/cas").await.unwrap();
    let mut stale = store.resume("svit://local/persistence/cas").await.unwrap();

    first.write("/memory/count", value!(1)).await.unwrap();
    assert!(matches!(
        stale.write("/memory/count", value!(2)).await,
        Err(Error::PersistenceConflict)
    ));
    assert_eq!(stale.version(), 0);
    assert_eq!(stale.read("/memory/count").unwrap(), Some(&value!(0)));

    let resumed = store.resume("svit://local/persistence/cas").await.unwrap();
    assert_eq!(resumed.version(), 1);
    assert_eq!(resumed.read("/memory/count").unwrap(), Some(&value!(1)));
}

#[tokio::test]
async fn every_process_transition_has_a_replayable_uniform_event() {
    let store = TursoProcessStore::memory().await.unwrap();
    let process = Process::builder("svit://local/persistence/mutations")
        .unwrap()
        .library("noop", Script::new("(define (main input) input)"))
        .build()
        .unwrap();
    let mut durable = store.create(process).await.unwrap();

    durable
        .write(
            "/lib/echo",
            value!({"source": "(define (main input) input)"}),
        )
        .await
        .unwrap();
    durable.exec("/lib/noop", Value::Null).await.unwrap();
    durable
        .enqueue_inbox(value!({"message": "one"}))
        .await
        .unwrap();
    durable
        .acknowledge_inbox(&value!({"message": "one"}))
        .await
        .unwrap();
    let expected_hash = durable.root_hash();
    let expected_version = durable.version();

    let all = durable.query(EventQuery::new()).await.unwrap();
    assert_eq!(all.len(), 4);
    assert!(all[1].mutations().is_empty());
    let selected = durable
        .query(
            EventQuery::new()
                .process_version_from(2)
                .process_version_through(3)
                .path_prefix("/inbox"),
        )
        .await
        .unwrap();
    assert_eq!(selected.len(), 1);
    let by_hash = durable
        .query(EventQuery::new().event_hash(all[0].event_hash()))
        .await
        .unwrap();
    assert_eq!(by_hash, vec![all[0].clone()]);

    drop(durable);
    let mut resumed = store
        .resume("svit://local/persistence/mutations")
        .await
        .unwrap();
    assert_eq!(resumed.version(), expected_version);
    assert_eq!(resumed.root_hash(), expected_hash);
    assert!(
        resumed
            .read("/inbox")
            .unwrap()
            .unwrap()
            .to_json()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        resumed
            .exec("/lib/echo", value!("ready"))
            .await
            .unwrap()
            .output,
        value!("ready")
    );
}
