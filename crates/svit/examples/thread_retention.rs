//! Reclaim a compacted prefix of canonical reasoning history.
//!
//! Canonical events are append-only, so a long-lived Svit stays bounded only
//! when its host reclaims history it no longer needs. A cut is refused until a
//! compaction checkpoint proves the prefix already reached replacement context.

use everruns_core::Message;
use everruns_core::compaction_checkpoint::{CompactionCheckpoint, CompactionCheckpointPayload};
use everruns_core::events::{EventContext, EventRequest, InputMessageData};
use everruns_provider::typed_id::{EventId, SessionId};
use svit::{DurableProcessHandle, Error, Process, TursoProcessStore, Value};

const ADDRESS: &str = "svit://local/examples/thread-retention";

#[tokio::main]
async fn main() -> svit::Result<()> {
    let store = TursoProcessStore::memory().await?;
    let session_id = SessionId::new();

    let mut process = Process::new(ADDRESS)?;
    process.replace_thread_state(Value::from_json(serde_json::json!({
        "format": "svit-thread@8",
        "session_id": session_id.to_string(),
        "process_id": ADDRESS,
        "instructions": null,
        "system_prompt": "system",
    }))?)?;
    let handle = store.import(process).await?;

    let log = handle.event_log();
    for index in 0..5 {
        log.append(EventRequest::new(
            session_id,
            EventContext::empty(),
            InputMessageData::new(Message::user(format!("question {index}"))),
        ))
        .await
        .expect("append canonical event");
    }

    let retention = handle.thread_history_retention();
    // Nothing has been compacted yet, so no prefix can be reclaimed.
    assert_eq!(retention.compacted_through(session_id).await?, 0);
    assert!(matches!(
        retention.cut_thread_events(session_id, 3).await,
        Err(Error::ThreadHistoryUncompacted)
    ));

    handle
        .compaction_checkpoint_store()
        .install(CompactionCheckpoint {
            id: EventId::new().uuid(),
            session_id,
            source_sequence: 3,
            provider_type: "example".into(),
            model: "example-model".into(),
            format_version: 1,
            payload: CompactionCheckpointPayload::Summary {
                text: "questions 0 through 2".into(),
            },
        })
        .await
        .expect("install compaction checkpoint");

    let compacted = retention.compacted_through(session_id).await?;
    let cut = retention.cut_thread_events(session_id, compacted).await?;
    assert_eq!(cut.removed_events(), 3);
    assert_eq!(cut.retained_from(), 3);

    // The retained tail reads without a gap, and the reclaimed sequences are
    // never reused by a later append.
    log.append(EventRequest::new(
        session_id,
        EventContext::empty(),
        InputMessageData::new(Message::user("question 5")),
    ))
    .await
    .expect("append after the cut");
    let retained = handle.recent_thread_events(session_id, 10).await?;
    let sequences = retained
        .iter()
        .filter_map(|event| event.sequence)
        .collect::<Vec<_>>();
    assert_eq!(sequences, vec![4, 5, 6]);

    drop(handle);
    let resumed = store.resume(ADDRESS).await?;
    assert_eq!(
        resumed
            .thread_history_retention()
            .retained_from(session_id)
            .await?,
        3
    );
    assert_eq!(resumed.recent_thread_events(session_id, 10).await?.len(), 3);

    println!("thread_retention removed=3 retained_from=3 remaining=3");
    Ok(())
}
