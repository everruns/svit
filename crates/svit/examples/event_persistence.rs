use svit::{EventQuery, Process, Script, TursoProcessStore, value};

const COUNTER: &str = r#"
    (define (main input)
      (let ((count (+ (read "/memory/count") (value-get input "/by"))))
        (do (write "/memory/count" count) count)))
"#;

#[tokio::main]
async fn main() -> svit::Result<()> {
    // Use `TursoProcessStore::open("svit.db")` for a database-backed file.
    let store = TursoProcessStore::memory().await?;
    let process = Process::builder("svit://local/examples/event-persistence")?
        .memory("count", value!(0))
        .library("counter", Script::new(COUNTER))
        .build()?;
    let mut durable = store.create(process).await?;
    durable.exec("/lib/counter", value!({"by": 3})).await?;

    drop(durable);
    let resumed = store
        .resume("svit://local/examples/event-persistence")
        .await?;
    let events = resumed
        .query(EventQuery::new().path_prefix("/memory"))
        .await?;

    assert_eq!(resumed.read("/memory/count")?, Some(&value!(3)));
    assert_eq!(resumed.version(), 1);
    assert_eq!(events.len(), 1);
    println!("event_persistence count=3 version=1 events=1");
    Ok(())
}
