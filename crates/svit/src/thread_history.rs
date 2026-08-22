//! Volatile canonical event history with the same retention contract as the
//! durable adapter.
//!
//! A volatile Svit keeps its canonical events in host memory. Without a
//! retention boundary that history grows for the whole lifetime of the running
//! process, so this log implements [`ThreadHistoryRetention`] exactly as the
//! Turso adapter does: the host reclaims a prefix only once a compaction
//! checkpoint already replaced it in model context.

use async_trait::async_trait;
use everruns_core::compaction_checkpoint::{
    CompactionCheckpoint, CompactionCheckpointStore, ProactiveCompactionAttempt,
};
use everruns_core::events::{Event, EventRequest};
use everruns_host::{
    EventCursor, EventDurability, EventLog, EventLogError, EventPage, EventReadRequest, EventReader,
};
use everruns_provider::AgentLoopError;
use everruns_provider::typed_id::{EventId, SessionId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::persistence::{ThreadHistoryCut, ThreadHistoryRetention};
use crate::{Error, Result};

/// One session's volatile canonical history and compaction checkpoints.
#[derive(Default)]
struct VolatileSession {
    events: Vec<Event>,
    high: i32,
    retained_from: i32,
    checkpoints: HashMap<(String, String, u32), CompactionCheckpoint>,
    attempts: HashMap<(String, String), ProactiveCompactionAttempt>,
}

impl VolatileSession {
    fn compacted_through(&self) -> i32 {
        self.checkpoints
            .values()
            .map(|checkpoint| checkpoint.source_sequence)
            .max()
            .and_then(|sequence| i32::try_from(sequence).ok())
            .unwrap_or(0)
    }
}

#[derive(Default)]
struct VolatileHistory {
    sessions: Mutex<HashMap<SessionId, VolatileSession>>,
}

impl VolatileHistory {
    fn with_session<T>(
        &self,
        session_id: SessionId,
        apply: impl FnOnce(&mut VolatileSession) -> T,
    ) -> std::result::Result<T, ()> {
        let mut sessions = self.sessions.lock().map_err(|_| ())?;
        Ok(apply(sessions.entry(session_id).or_default()))
    }
}

/// Volatile canonical event log paired with one running Svit.
#[derive(Clone, Default)]
pub(crate) struct VolatileThreadEventLog {
    history: Arc<VolatileHistory>,
}

impl VolatileThreadEventLog {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns the checkpoint store sharing this log's session state.
    ///
    /// Retention needs the compaction boundary, so both surfaces read the same
    /// state rather than two independently installed stores.
    pub(crate) fn checkpoint_store(&self) -> VolatileCompactionCheckpointStore {
        VolatileCompactionCheckpointStore {
            history: self.history.clone(),
        }
    }
}

fn unavailable() -> EventLogError {
    EventLogError::Backend {
        detail: "volatile thread history is unavailable".into(),
    }
}

#[async_trait]
impl EventReader for VolatileThreadEventLog {
    async fn read_page(
        &self,
        request: EventReadRequest,
    ) -> std::result::Result<EventPage, EventLogError> {
        let session_id = request.session_id();
        let (events, current_high, floor) = self
            .history
            .with_session(session_id, |session| {
                (session.events.clone(), session.high, session.retained_from)
            })
            .map_err(|()| unavailable())?;

        let (after, snapshot) = match request.cursor() {
            None => (floor, current_high),
            Some(cursor) => {
                if cursor.session_id() != session_id {
                    return Err(EventLogError::CrossSessionCursor {
                        detail: "cursor belongs to another session".into(),
                    });
                }
                // A retention cut removed everything at or below the boundary,
                // so a cursor that predates it can no longer be continued.
                if cursor.after_sequence() < floor {
                    return Err(EventLogError::ExpiredCursor {
                        detail: "cursor precedes the retained history boundary".into(),
                    });
                }
                let snapshot = cursor.snapshot_high_watermark().unwrap_or(current_high);
                if snapshot > current_high {
                    return Err(EventLogError::ExpiredCursor {
                        detail: "cursor snapshot is no longer available".into(),
                    });
                }
                if cursor.after_sequence() > snapshot {
                    return Err(EventLogError::IncompatibleCursor {
                        detail: "cursor position exceeds its snapshot".into(),
                    });
                }
                (cursor.after_sequence(), snapshot)
            }
        };
        if snapshot == 0 {
            return EventPage::new(Vec::new(), None, 0);
        }

        let fetch_limit =
            request
                .limit()
                .get()
                .checked_add(1)
                .ok_or_else(|| EventLogError::InvalidRead {
                    detail: "event page limit overflowed".into(),
                })?;
        let mut selected = events
            .into_iter()
            .filter(|event| {
                event
                    .sequence
                    .is_some_and(|sequence| sequence > after && sequence <= snapshot)
            })
            .take(fetch_limit)
            .collect::<Vec<_>>();
        let has_more = selected.len() == fetch_limit;
        if has_more {
            selected.pop();
        }
        let next_cursor = has_more
            .then(|| {
                let last = selected
                    .last()
                    .and_then(|event| event.sequence)
                    .ok_or_else(|| EventLogError::Corruption {
                        detail: "event continuation has no sequence".into(),
                    })?;
                EventCursor::continuation(session_id, last, snapshot)
            })
            .transpose()?;
        EventPage::new(selected, next_cursor, snapshot)
    }
}

#[async_trait]
impl EventLog for VolatileThreadEventLog {
    async fn append(&self, request: EventRequest) -> std::result::Result<Event, EventLogError> {
        if request.is_ephemeral() {
            return Err(EventLogError::InvalidAppend {
                detail: "ephemeral events are sink-only".into(),
            });
        }
        let session_id = request.session_id;
        let sequence = self
            .history
            .with_session(session_id, |session| session.high)
            .map_err(|()| unavailable())?
            .checked_add(1)
            .ok_or_else(|| EventLogError::InvalidAppend {
                detail: "event sequence exhausted".into(),
            })?;
        let event = request.into_event(EventId::new(), sequence);
        self.history
            .with_session(session_id, |session| {
                // A reclaimed prefix still bounds the session, so `high` is
                // never recomputed from the retained events.
                session.high = sequence;
                session.events.push(event.clone());
            })
            .map_err(|()| unavailable())?;
        Ok(event)
    }

    fn durability(&self) -> EventDurability {
        EventDurability::Volatile
    }
}

#[async_trait]
impl ThreadHistoryRetention for VolatileThreadEventLog {
    async fn retained_from(&self, session_id: SessionId) -> Result<i32> {
        self.history
            .with_session(session_id, |session| session.retained_from)
            .map_err(|()| Error::PersistenceUnavailable)
    }

    async fn compacted_through(&self, session_id: SessionId) -> Result<i32> {
        self.history
            .with_session(session_id, |session| session.compacted_through())
            .map_err(|()| Error::PersistenceUnavailable)
    }

    // THREAT[TM-AUD-002]: Volatile history enforces the same boundary rules as
    // the durable adapter, so a host cannot reclaim context the loop still
    // needs by choosing the volatile mode.
    async fn cut_thread_events(
        &self,
        session_id: SessionId,
        through_sequence: i32,
    ) -> Result<ThreadHistoryCut> {
        if through_sequence <= 0 {
            return Err(Error::ThreadHistoryBoundary);
        }
        self.history
            .with_session(session_id, |session| {
                if through_sequence > session.high {
                    return Err(Error::ThreadHistoryBoundary);
                }
                if through_sequence <= session.retained_from {
                    return Ok(ThreadHistoryCut::new(0, session.retained_from));
                }
                if session.compacted_through() < through_sequence {
                    return Err(Error::ThreadHistoryUncompacted);
                }
                let before = session.events.len();
                session.events.retain(|event| {
                    event
                        .sequence
                        .is_none_or(|sequence| sequence > through_sequence)
                });
                session.retained_from = through_sequence;
                let removed = (before - session.events.len()) as u64;
                Ok(ThreadHistoryCut::new(removed, through_sequence))
            })
            .map_err(|()| Error::PersistenceUnavailable)?
    }
}

/// Volatile compaction checkpoints sharing one running Svit's session state.
#[derive(Clone)]
pub(crate) struct VolatileCompactionCheckpointStore {
    history: Arc<VolatileHistory>,
}

fn checkpoint_unavailable() -> AgentLoopError {
    AgentLoopError::store("volatile compaction checkpoints are unavailable")
}

#[async_trait]
impl CompactionCheckpointStore for VolatileCompactionCheckpointStore {
    async fn get_latest(
        &self,
        session_id: SessionId,
        provider_type: &str,
        model: &str,
    ) -> everruns_provider::error::Result<Option<CompactionCheckpoint>> {
        self.history
            .with_session(session_id, |session| {
                session
                    .checkpoints
                    .values()
                    .filter(|checkpoint| {
                        checkpoint.provider_type == provider_type && checkpoint.model == model
                    })
                    .max_by_key(|checkpoint| checkpoint.source_sequence)
                    .cloned()
            })
            .map_err(|()| checkpoint_unavailable())
    }

    async fn install(
        &self,
        checkpoint: CompactionCheckpoint,
    ) -> everruns_provider::error::Result<bool> {
        if checkpoint.source_sequence < 0 {
            return Err(AgentLoopError::store(
                "checkpoint source sequence is invalid",
            ));
        }
        let session_id = checkpoint.session_id;
        self.history
            .with_session(session_id, |session| {
                let key = (
                    checkpoint.provider_type.clone(),
                    checkpoint.model.clone(),
                    checkpoint.format_version,
                );
                if let Some(existing) = session.checkpoints.get(&key)
                    && existing.source_sequence >= checkpoint.source_sequence
                {
                    return false;
                }
                session.checkpoints.insert(key, checkpoint);
                true
            })
            .map_err(|()| checkpoint_unavailable())
    }

    async fn get_proactive_attempt(
        &self,
        session_id: SessionId,
        provider_type: &str,
        model: &str,
    ) -> everruns_provider::error::Result<Option<ProactiveCompactionAttempt>> {
        self.history
            .with_session(session_id, |session| {
                session
                    .attempts
                    .get(&(provider_type.to_owned(), model.to_owned()))
                    .copied()
            })
            .map_err(|()| checkpoint_unavailable())
    }

    async fn record_proactive_attempt(
        &self,
        session_id: SessionId,
        provider_type: &str,
        model: &str,
        attempt: ProactiveCompactionAttempt,
    ) -> everruns_provider::error::Result<()> {
        self.history
            .with_session(session_id, |session| {
                session
                    .attempts
                    .insert((provider_type.to_owned(), model.to_owned()), attempt);
            })
            .map_err(|()| checkpoint_unavailable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::Message;
    use everruns_core::compaction_checkpoint::CompactionCheckpointPayload;
    use everruns_core::events::{EventContext, InputMessageData};
    use everruns_host::EventReadLimit;

    async fn append(log: &VolatileThreadEventLog, session_id: SessionId, text: &str) -> Event {
        log.append(EventRequest::new(
            session_id,
            EventContext::empty(),
            InputMessageData::new(Message::user(text)),
        ))
        .await
        .unwrap()
    }

    async fn install(log: &VolatileThreadEventLog, session_id: SessionId, source_sequence: i64) {
        log.checkpoint_store()
            .install(CompactionCheckpoint {
                id: EventId::new().uuid(),
                session_id,
                source_sequence,
                provider_type: "test".into(),
                model: "test-model".into(),
                format_version: 1,
                payload: CompactionCheckpointPayload::Summary {
                    text: "summary".into(),
                },
            })
            .await
            .unwrap();
    }

    async fn sequences(log: &VolatileThreadEventLog, session_id: SessionId) -> Vec<i32> {
        log.read_page(EventReadRequest::new(session_id, EventReadLimit::default()))
            .await
            .unwrap()
            .events
            .iter()
            .filter_map(|event| event.sequence)
            .collect()
    }

    #[tokio::test]
    // THREAT[TM-AUD-002]
    async fn a_compacted_volatile_prefix_is_reclaimed_without_reusing_sequences() {
        let log = VolatileThreadEventLog::new();
        let session_id = SessionId::new();
        for index in 0..4 {
            append(&log, session_id, &format!("question {index}")).await;
        }
        install(&log, session_id, 2).await;

        assert_eq!(log.compacted_through(session_id).await.unwrap(), 2);
        let cut = log.cut_thread_events(session_id, 2).await.unwrap();
        assert_eq!(cut.removed_events(), 2);
        assert_eq!(cut.retained_from(), 2);
        assert_eq!(log.retained_from(session_id).await.unwrap(), 2);
        assert_eq!(sequences(&log, session_id).await, vec![3, 4]);

        let appended = append(&log, session_id, "next question").await;
        assert_eq!(appended.sequence, Some(5));
        assert_eq!(sequences(&log, session_id).await, vec![3, 4, 5]);
    }

    #[tokio::test]
    // THREAT[TM-AUD-002]
    async fn an_uncompacted_or_out_of_range_volatile_cut_fails_closed() {
        let log = VolatileThreadEventLog::new();
        let session_id = SessionId::new();
        append(&log, session_id, "only question").await;

        assert!(matches!(
            log.cut_thread_events(session_id, 1).await,
            Err(Error::ThreadHistoryUncompacted)
        ));
        assert!(matches!(
            log.cut_thread_events(session_id, 0).await,
            Err(Error::ThreadHistoryBoundary)
        ));
        assert!(matches!(
            log.cut_thread_events(session_id, 2).await,
            Err(Error::ThreadHistoryBoundary)
        ));
        assert_eq!(log.retained_from(session_id).await.unwrap(), 0);
        assert_eq!(sequences(&log, session_id).await, vec![1]);
    }

    #[tokio::test]
    async fn a_cursor_below_the_retained_boundary_expires() {
        let log = VolatileThreadEventLog::new();
        let session_id = SessionId::new();
        for index in 0..3 {
            append(&log, session_id, &format!("question {index}")).await;
        }
        let stale = EventCursor::continuation(session_id, 1, 3).unwrap();
        install(&log, session_id, 2).await;
        log.cut_thread_events(session_id, 2).await.unwrap();

        let error = log
            .read_page(
                EventReadRequest::new(session_id, EventReadLimit::default()).with_cursor(stale),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, EventLogError::ExpiredCursor { .. }));
    }

    #[tokio::test]
    async fn a_repeated_volatile_cut_removes_nothing() {
        let log = VolatileThreadEventLog::new();
        let session_id = SessionId::new();
        append(&log, session_id, "question").await;
        install(&log, session_id, 1).await;

        assert_eq!(
            log.cut_thread_events(session_id, 1)
                .await
                .unwrap()
                .removed_events(),
            1
        );
        let repeated = log.cut_thread_events(session_id, 1).await.unwrap();
        assert_eq!(repeated.removed_events(), 0);
        assert_eq!(repeated.retained_from(), 1);
    }
}
