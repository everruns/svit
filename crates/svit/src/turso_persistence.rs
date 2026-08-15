use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use everruns::core::events::{
    Event, EventRequest, INPUT_MESSAGE, OUTPUT_MESSAGE_COMPLETED, TOOL_COMPLETED,
};
use everruns::core::typed_id::{EventId, SessionId};
use everruns::core::{
    AgentLoopError, CompactionCheckpoint, CompactionCheckpointPayload, CompactionCheckpointStore,
    ProactiveCompactionAttempt, ProactiveCompactionAttemptTracker,
};
use everruns_host::{
    EventCursor, EventDurability, EventLog, EventLogError, EventPage, EventReadRequest, EventReader,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Connection, Database, Row, Value as SqlValue, params};

use crate::persistence::{
    DurableProcessHandle, Mutation, PersistenceSnapshotRecord, ProcessStore, ProcessTransaction,
    TransactionHead, TransactionQuery,
};
use crate::{Activation, Builtins, Error, Mount, Process, ProcessId, Result, Value};

const SCHEMA_VERSION: &str = "2";
const BASE_FORMAT: &str = "svit-base@1";
const SNAPSHOT_FORMAT: &str = "svit-store-snapshot@2";
const LEGACY_SNAPSHOT_FORMAT: &str = "svit-store-snapshot@1";
const RECOVERY_CHECKPOINT_FORMAT: &str = "svit-recovery-checkpoint@1";
const PROCESS_SNAPSHOT_PREFIX: &[u8] = br#"{"format":"#;
const MAX_TRANSACTION_BYTES: usize = 16 * 1024 * 1024;
const MAX_QUERY_TRANSACTIONS: u32 = 4096;
const MAX_QUERY_TEXT_BYTES: usize = 4096;
const MAX_REPLAY_TRANSACTIONS: usize = 100_000;
const MAX_FORK_DEPTH: usize = 64;
const MAX_COMPACTION_CHECKPOINT_BYTES: usize = 32 * 1024 * 1024;
/// Bounds ordinary resume work without discarding the retained event history.
const RECOVERY_CHECKPOINT_INTERVAL: u64 = 32;

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS svit_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS blobs (
    hash TEXT PRIMARY KEY,
    bytes BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS bases (
    base_hash TEXT PRIMARY KEY,
    address TEXT NOT NULL,
    kind TEXT NOT NULL,
    covered_position INTEGER,
    process_version INTEGER NOT NULL,
    root_hash TEXT NOT NULL,
    base_blob_hash TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS svits (
    address TEXT PRIMARY KEY,
    base_hash TEXT NOT NULL,
    covered_position INTEGER,
    head_position INTEGER,
    head_hash TEXT NOT NULL,
    process_version INTEGER NOT NULL,
    root_hash TEXT NOT NULL,
    FOREIGN KEY(base_hash) REFERENCES bases(base_hash)
);

CREATE TABLE IF NOT EXISTS events (
    address TEXT NOT NULL,
    position INTEGER NOT NULL,
    previous_hash TEXT NOT NULL,
    process_version_before INTEGER NOT NULL,
    process_version_after INTEGER NOT NULL,
    source TEXT NOT NULL,
    resulting_root_hash TEXT NOT NULL,
    event_hash TEXT NOT NULL UNIQUE,
    event_blob_hash TEXT NOT NULL,
    PRIMARY KEY(address, position),
    FOREIGN KEY(address) REFERENCES svits(address)
);

CREATE INDEX IF NOT EXISTS events_by_version
    ON events(address, process_version_after, position);
CREATE INDEX IF NOT EXISTS events_by_source
    ON events(address, source, position);

CREATE TABLE IF NOT EXISTS thread_events (
    address TEXT NOT NULL,
    session_id TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    event_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    event_hash TEXT NOT NULL,
    event_blob_hash TEXT NOT NULL,
    PRIMARY KEY(address, session_id, sequence),
    UNIQUE(address, event_id),
    FOREIGN KEY(address) REFERENCES svits(address) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS thread_events_by_session
    ON thread_events(address, session_id, sequence);
CREATE INDEX IF NOT EXISTS thread_events_by_type
    ON thread_events(address, session_id, event_type, sequence);

CREATE TABLE IF NOT EXISTS thread_event_refs (
    child_address TEXT NOT NULL,
    session_id TEXT NOT NULL,
    parent_address TEXT NOT NULL,
    through_sequence INTEGER NOT NULL,
    PRIMARY KEY(child_address, session_id),
    FOREIGN KEY(child_address) REFERENCES svits(address) ON DELETE CASCADE,
    FOREIGN KEY(parent_address) REFERENCES svits(address)
);
CREATE INDEX IF NOT EXISTS thread_event_refs_by_parent
    ON thread_event_refs(parent_address, session_id, through_sequence);

CREATE TABLE IF NOT EXISTS thread_compaction_checkpoints (
    address TEXT NOT NULL,
    session_id TEXT NOT NULL,
    provider_type TEXT NOT NULL,
    model TEXT NOT NULL,
    format_version INTEGER NOT NULL,
    checkpoint_id TEXT NOT NULL,
    source_sequence INTEGER NOT NULL,
    payload_blob_hash TEXT NOT NULL,
    PRIMARY KEY(address, session_id, provider_type, model, format_version),
    FOREIGN KEY(address) REFERENCES svits(address) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS thread_compaction_checkpoints_by_session
    ON thread_compaction_checkpoints(address, session_id, source_sequence DESC);

CREATE TABLE IF NOT EXISTS event_paths (
    address TEXT NOT NULL,
    position INTEGER NOT NULL,
    path TEXT NOT NULL,
    PRIMARY KEY(address, position, path),
    FOREIGN KEY(address, position) REFERENCES events(address, position) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS event_paths_by_path
    ON event_paths(address, path, position);

CREATE TABLE IF NOT EXISTS snapshots (
    snapshot_hash TEXT PRIMARY KEY,
    address TEXT NOT NULL,
    covered_position INTEGER,
    head_hash TEXT NOT NULL,
    process_version INTEGER NOT NULL,
    root_hash TEXT NOT NULL,
    snapshot_blob_hash TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS recovery_checkpoints (
    address TEXT PRIMARY KEY,
    covered_position INTEGER NOT NULL,
    head_hash TEXT NOT NULL,
    process_version INTEGER NOT NULL,
    root_hash TEXT NOT NULL,
    snapshot_hash TEXT NOT NULL,
    snapshot_blob_hash TEXT NOT NULL,
    FOREIGN KEY(address) REFERENCES svits(address) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS fork_refs (
    child_address TEXT PRIMARY KEY,
    parent_address TEXT NOT NULL,
    parent_position INTEGER,
    parent_head_hash TEXT NOT NULL,
    parent_root_hash TEXT NOT NULL,
    FOREIGN KEY(child_address) REFERENCES svits(address) ON DELETE CASCADE,
    FOREIGN KEY(parent_address) REFERENCES svits(address)
);
CREATE INDEX IF NOT EXISTS fork_refs_by_parent
    ON fork_refs(parent_address, parent_position);

"#;

/// One persisted on-demand process image at an exact event boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct PersistenceSnapshot(StoredSnapshot);

impl PersistenceSnapshot {
    /// Returns the content hash of this store snapshot.
    pub fn snapshot_hash(&self) -> &str {
        &self.0.snapshot_hash
    }

    /// Returns the process version captured by this snapshot.
    pub fn process_version(&self) -> u64 {
        self.0.process_version
    }

    /// Returns the process root hash captured by this snapshot.
    pub fn root_hash(&self) -> &str {
        &self.0.root_hash
    }
}

impl PersistenceSnapshotRecord for PersistenceSnapshot {
    fn snapshot_hash(&self) -> &str {
        PersistenceSnapshot::snapshot_hash(self)
    }

    fn process_version(&self) -> u64 {
        PersistenceSnapshot::process_version(self)
    }

    fn root_hash(&self) -> &str {
        PersistenceSnapshot::root_hash(self)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case", deny_unknown_fields)]
enum BaseOrigin {
    Created { process_snapshot: Vec<u8> },
    Imported { process_snapshot: Vec<u8> },
    Fork { parent: ForkBoundary },
    Snapshot { snapshot_hash: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForkBoundary {
    parent_address: ProcessId,
    parent_head: TransactionHead,
    parent_process_version: u64,
    parent_root_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredBase {
    base_format: String,
    address: ProcessId,
    covered_position: Option<u64>,
    anchor_event_hash: Option<String>,
    process_version: u64,
    root_hash: String,
    origin: BaseOrigin,
    base_hash: String,
}

#[derive(Serialize)]
struct BaseHashMaterial<'a> {
    base_format: &'a str,
    address: &'a ProcessId,
    covered_position: Option<u64>,
    anchor_event_hash: &'a Option<String>,
    process_version: u64,
    root_hash: &'a str,
    origin: &'a BaseOrigin,
}

impl StoredBase {
    fn new(
        address: ProcessId,
        covered_position: Option<u64>,
        anchor_event_hash: Option<String>,
        process_version: u64,
        root_hash: String,
        origin: BaseOrigin,
    ) -> Result<Self> {
        let mut base = Self {
            base_format: BASE_FORMAT.into(),
            address,
            covered_position,
            anchor_event_hash,
            process_version,
            root_hash,
            origin,
            base_hash: String::new(),
        };
        base.base_hash = base.compute_hash()?;
        Ok(base)
    }

    fn compute_hash(&self) -> Result<String> {
        canonical_hash(&BaseHashMaterial {
            base_format: &self.base_format,
            address: &self.address,
            covered_position: self.covered_position,
            anchor_event_hash: &self.anchor_event_hash,
            process_version: self.process_version,
            root_hash: &self.root_hash,
            origin: &self.origin,
        })
    }

    fn validate(&self) -> Result<()> {
        if self.base_format != BASE_FORMAT || self.base_hash != self.compute_hash()? {
            return invalid("base hash or format is invalid");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSnapshot {
    snapshot_format: String,
    address: ProcessId,
    covered_position: Option<u64>,
    head_hash: String,
    process_version: u64,
    /// Structured JSON keeps the canonical process image compact. Legacy
    /// records encoded these bytes as a JSON integer array and remain readable.
    process_snapshot: serde_json::Value,
    root_hash: String,
    snapshot_hash: String,
}

#[derive(Serialize)]
struct SnapshotHashMaterial<'a> {
    snapshot_format: &'a str,
    address: &'a ProcessId,
    covered_position: Option<u64>,
    head_hash: &'a str,
    process_version: u64,
    process_snapshot: &'a serde_json::Value,
    root_hash: &'a str,
}

impl StoredSnapshot {
    fn new(process: &Process, head: &TransactionHead) -> Result<Self> {
        let process_snapshot = serde_json::from_slice(&process.snapshot()?).map_err(|_| {
            Error::InvalidPersistence("process snapshot encoding is invalid".into())
        })?;
        let mut snapshot = Self {
            snapshot_format: SNAPSHOT_FORMAT.into(),
            address: process.id().clone(),
            covered_position: head.position,
            head_hash: head.hash.clone(),
            process_version: process.version(),
            process_snapshot,
            root_hash: process.root_hash(),
            snapshot_hash: String::new(),
        };
        snapshot.snapshot_hash = snapshot.compute_hash()?;
        Ok(snapshot)
    }

    fn compute_hash(&self) -> Result<String> {
        canonical_hash(&SnapshotHashMaterial {
            snapshot_format: &self.snapshot_format,
            address: &self.address,
            covered_position: self.covered_position,
            head_hash: &self.head_hash,
            process_version: self.process_version,
            process_snapshot: &self.process_snapshot,
            root_hash: &self.root_hash,
        })
    }

    fn validate_envelope(&self) -> Result<()> {
        if !matches!(
            self.snapshot_format.as_str(),
            SNAPSHOT_FORMAT | LEGACY_SNAPSHOT_FORMAT
        ) || self.snapshot_hash != self.compute_hash()?
        {
            return invalid("store snapshot hash or format is invalid");
        }
        Ok(())
    }

    fn restore_process(&self) -> Result<Process> {
        let declared_root_hash = self
            .process_snapshot
            .get("root_hash")
            .and_then(serde_json::Value::as_str);
        let bytes = match &self.process_snapshot {
            // `svit-store-snapshot@1` originally serialized `Vec<u8>` as a
            // JSON integer array. Decode it without invalidating old stores.
            serde_json::Value::Array(values)
                if self.snapshot_format == LEGACY_SNAPSHOT_FORMAT
                    && values.iter().all(|value| {
                        value.as_u64().is_some_and(|value| value <= u8::MAX.into())
                    }) =>
            {
                values
                    .iter()
                    .map(|value| value.as_u64().expect("guarded legacy byte") as u8)
                    .collect()
            }
            value => serde_json::to_vec(value).map_err(|_| {
                Error::InvalidPersistence("process snapshot encoding failed".into())
            })?,
        };
        let process = Process::restore(&bytes)?;
        if process.id() != &self.address
            || process.version() != self.process_version
            || declared_root_hash.map_or_else(
                || process.root_hash() != self.root_hash,
                |declared| declared != self.root_hash,
            )
        {
            return invalid("store snapshot metadata does not match process state");
        }
        Ok(process)
    }
}

/// Internal resume cache. Its blob is the process snapshot itself rather than
/// a second JSON envelope containing that snapshot. This keeps startup to one
/// decode and one validation pass over the complete process image.
struct RecoveryCheckpoint {
    address: ProcessId,
    covered_position: u64,
    head_hash: String,
    process_version: u64,
    root_hash: String,
    snapshot_hash: String,
    process_snapshot_hash: String,
    process_snapshot: Vec<u8>,
}

#[derive(Serialize)]
struct RecoveryCheckpointHashMaterial<'a> {
    checkpoint_format: &'a str,
    address: &'a ProcessId,
    covered_position: u64,
    head_hash: &'a str,
    process_version: u64,
    root_hash: &'a str,
    process_snapshot_hash: &'a str,
}

impl RecoveryCheckpoint {
    fn new(process: &Process, head: &TransactionHead) -> Result<Self> {
        let covered_position = head
            .position
            .ok_or_else(|| Error::InvalidPersistence("checkpoint has no event position".into()))?;
        let process_snapshot = process.snapshot()?;
        let process_snapshot_hash = bytes_hash(&process_snapshot);
        let root_hash = process.root_hash();
        let mut checkpoint = Self {
            address: process.id().clone(),
            covered_position,
            head_hash: head.hash.clone(),
            process_version: process.version(),
            root_hash,
            snapshot_hash: String::new(),
            process_snapshot_hash,
            process_snapshot,
        };
        checkpoint.snapshot_hash = checkpoint.compute_hash()?;
        Ok(checkpoint)
    }

    fn compute_hash(&self) -> Result<String> {
        canonical_hash(&RecoveryCheckpointHashMaterial {
            checkpoint_format: RECOVERY_CHECKPOINT_FORMAT,
            address: &self.address,
            covered_position: self.covered_position,
            head_hash: &self.head_hash,
            process_version: self.process_version,
            root_hash: &self.root_hash,
            process_snapshot_hash: &self.process_snapshot_hash,
        })
    }

    fn restore_process(&self) -> Result<Process> {
        let (process, declared_root_hash) =
            Process::restore_with_declared_root_hash(&self.process_snapshot)?;
        if process.id() != &self.address
            || process.version() != self.process_version
            || declared_root_hash != self.root_hash
        {
            return invalid("recovery checkpoint metadata does not match process state");
        }
        Ok(process)
    }
}

struct LoadedRecoveryCheckpoint {
    covered_position: u64,
    head_hash: String,
    root_hash: String,
    process: Process,
}

#[derive(Clone)]
/// Address-partitioned event storage backed by one local Turso database.
pub struct TursoProcessStore {
    database: Database,
    proactive_attempts: Arc<ProactiveCompactionAttemptTracker>,
}

#[derive(Clone)]
struct TursoThreadEventLog {
    store: TursoProcessStore,
    address: ProcessId,
}

#[derive(Clone)]
struct TursoCompactionCheckpointStore {
    store: TursoProcessStore,
    address: ProcessId,
}

#[async_trait]
impl EventReader for TursoThreadEventLog {
    async fn read_page(
        &self,
        request: EventReadRequest,
    ) -> std::result::Result<EventPage, EventLogError> {
        let session_id = request.session_id();
        let connection = self
            .store
            .database
            .connect()
            .map_err(|_| thread_event_backend())?;
        let current_high = thread_history_high(&connection, &self.address, session_id, 0).await?;

        let (after, snapshot) = match request.cursor() {
            None => (0, current_high),
            Some(cursor) => {
                if cursor.session_id() != session_id {
                    return Err(EventLogError::CrossSessionCursor {
                        detail: "cursor belongs to another session".into(),
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
        let mut events = Vec::with_capacity(fetch_limit);
        for sequence in (after + 1)..=snapshot {
            let event = thread_event_at(&connection, &self.address, session_id, sequence, 0)
                .await?
                .ok_or_else(|| thread_event_corruption("thread history has a sequence gap"))?;
            events.push(event);
            if events.len() == fetch_limit {
                break;
            }
        }
        let has_more = events.len() == fetch_limit;
        if has_more {
            events.pop();
        }
        let next_cursor = has_more
            .then(|| {
                let last = events
                    .last()
                    .and_then(|event| event.sequence)
                    .ok_or_else(|| thread_event_corruption("event continuation has no sequence"))?;
                EventCursor::continuation(session_id, last, snapshot)
            })
            .transpose()?;
        EventPage::new(events, next_cursor, snapshot)
    }
}

#[async_trait]
impl EventLog for TursoThreadEventLog {
    async fn append(&self, request: EventRequest) -> std::result::Result<Event, EventLogError> {
        if request.is_ephemeral() {
            return Err(EventLogError::InvalidAppend {
                detail: "ephemeral events are sink-only".into(),
            });
        }
        let session_id = request.session_id;
        let mut connection = self
            .store
            .write_connection()
            .await
            .map_err(|_| thread_event_backend())?;
        let transaction = connection
            .transaction()
            .await
            .map_err(|_| thread_event_backend())?;
        let previous =
            thread_history_high_in_transaction(&transaction, &self.address, session_id).await?;
        let sequence = previous
            .checked_add(1)
            .ok_or_else(|| EventLogError::InvalidAppend {
                detail: "event sequence exhausted".into(),
            })?;
        let event = request.into_event(EventId::new(), sequence);
        let bytes = serde_json::to_vec(&event).map_err(|_| EventLogError::InvalidAppend {
            detail: "event encoding failed".into(),
        })?;
        if bytes.len() > MAX_TRANSACTION_BYTES {
            return Err(EventLogError::InvalidAppend {
                detail: "event byte limit exceeded".into(),
            });
        }
        let hash = bytes_hash(&bytes);
        insert_blob(&transaction, &hash, &bytes)
            .await
            .map_err(thread_event_corruption)?;
        transaction
            .execute(
                "INSERT INTO thread_events \
                 (address, session_id, sequence, event_id, event_type, event_hash, event_blob_hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![
                    self.address.as_str(),
                    session_id.to_string(),
                    i64::from(sequence),
                    event.id.to_string(),
                    event.event_type.as_str(),
                    hash,
                ],
            )
            .await
            .map_err(|error| match error {
                turso::Error::Constraint(_) => EventLogError::InvalidAppend {
                    detail: "event identity or sequence conflicts with an existing event".into(),
                },
                _ => thread_event_backend(),
            })?;
        transaction
            .commit()
            .await
            .map_err(|_| thread_event_backend())?;
        Ok(event)
    }

    fn durability(&self) -> EventDurability {
        EventDurability::CrashDurable
    }
}

async fn thread_history_high(
    connection: &Connection,
    address: &ProcessId,
    session_id: SessionId,
    _depth: usize,
) -> std::result::Result<i32, EventLogError> {
    let mut rows = connection
        .query(
            "SELECT MAX(sequence) FROM thread_events WHERE address = ?1 AND session_id = ?2",
            params![address.as_str(), session_id.to_string()],
        )
        .await
        .map_err(|_| thread_event_backend())?;
    let own_high = rows
        .next()
        .await
        .map_err(|_| thread_event_backend())?
        .map(|row| optional_u64(&row, 0))
        .transpose()
        .map_err(thread_event_corruption)?
        .flatten()
        .map(i32::try_from)
        .transpose()
        .map_err(|_| thread_event_corruption("event sequence exceeds the supported range"))?
        .unwrap_or(0);
    drop(rows);
    let inherited = thread_event_ref(connection, address, session_id).await?;
    Ok(own_high.max(inherited.map(|(_, through)| through).unwrap_or(0)))
}

async fn thread_history_high_in_transaction(
    transaction: &Transaction<'_>,
    address: &ProcessId,
    session_id: SessionId,
) -> std::result::Result<i32, EventLogError> {
    let mut rows = transaction
        .query(
            "SELECT MAX(sequence) FROM thread_events WHERE address = ?1 AND session_id = ?2",
            params![address.as_str(), session_id.to_string()],
        )
        .await
        .map_err(|_| thread_event_backend())?;
    let own_high = rows
        .next()
        .await
        .map_err(|_| thread_event_backend())?
        .map(|row| optional_u64(&row, 0))
        .transpose()
        .map_err(thread_event_corruption)?
        .flatten()
        .map(i32::try_from)
        .transpose()
        .map_err(|_| thread_event_corruption("event sequence exceeds the supported range"))?
        .unwrap_or(0);
    drop(rows);
    let mut refs = transaction
        .query(
            "SELECT parent_address, through_sequence FROM thread_event_refs \
             WHERE child_address = ?1 AND session_id = ?2",
            params![address.as_str(), session_id.to_string()],
        )
        .await
        .map_err(|_| thread_event_backend())?;
    let inherited = refs
        .next()
        .await
        .map_err(|_| thread_event_backend())?
        .map(|row| {
            i32::try_from(u64_column(&row, 1).map_err(thread_event_corruption)?)
                .map_err(|_| thread_event_corruption("event sequence exceeds the supported range"))
        })
        .transpose()?;
    Ok(own_high.max(inherited.unwrap_or(0)))
}

async fn thread_event_ref(
    connection: &Connection,
    address: &ProcessId,
    session_id: SessionId,
) -> std::result::Result<Option<(ProcessId, i32)>, EventLogError> {
    let mut rows = connection
        .query(
            "SELECT parent_address, through_sequence FROM thread_event_refs \
             WHERE child_address = ?1 AND session_id = ?2",
            params![address.as_str(), session_id.to_string()],
        )
        .await
        .map_err(|_| thread_event_backend())?;
    rows.next()
        .await
        .map_err(|_| thread_event_backend())?
        .map(|row| {
            Ok((
                ProcessId::new(text(&row, 0).map_err(thread_event_corruption)?)
                    .map_err(thread_event_corruption)?,
                i32::try_from(u64_column(&row, 1).map_err(thread_event_corruption)?).map_err(
                    |_| thread_event_corruption("event sequence exceeds the supported range"),
                )?,
            ))
        })
        .transpose()
}

async fn thread_event_at(
    connection: &Connection,
    address: &ProcessId,
    session_id: SessionId,
    sequence: i32,
    _depth: usize,
) -> std::result::Result<Option<Event>, EventLogError> {
    let mut current = address.clone();
    for _ in 0..MAX_FORK_DEPTH {
        let mut rows = connection
            .query(
                "SELECT b.hash, b.bytes, e.sequence, e.event_id FROM thread_events e \
                 JOIN blobs b ON b.hash = e.event_blob_hash \
                 WHERE e.address = ?1 AND e.session_id = ?2 AND e.sequence = ?3",
                params![
                    current.as_str(),
                    session_id.to_string(),
                    i64::from(sequence)
                ],
            )
            .await
            .map_err(|_| thread_event_backend())?;
        if let Some(row) = rows.next().await.map_err(|_| thread_event_backend())? {
            let event: Event =
                decode_content_addressed(&row, 0, 1, "event").map_err(thread_event_corruption)?;
            let stored_sequence = i32::try_from(
                u64_column(&row, 2).map_err(thread_event_corruption)?,
            )
            .map_err(|_| thread_event_corruption("event sequence exceeds the supported range"))?;
            let event_id: EventId = text(&row, 3)
                .map_err(thread_event_corruption)?
                .parse()
                .map_err(thread_event_corruption)?;
            if event.session_id != session_id
                || event.sequence != Some(stored_sequence)
                || event.id != event_id
            {
                return Err(thread_event_corruption(
                    "event envelope does not match its storage projection",
                ));
            }
            return Ok(Some(event));
        }
        drop(rows);
        let Some((parent, through)) = thread_event_ref(connection, &current, session_id).await?
        else {
            return Ok(None);
        };
        if sequence > through {
            return Ok(None);
        }
        current = parent;
    }
    Err(thread_event_corruption(
        "thread history fork depth exceeded",
    ))
}

#[async_trait]
impl CompactionCheckpointStore for TursoCompactionCheckpointStore {
    async fn get_latest(
        &self,
        session_id: SessionId,
        provider_type: &str,
        model: &str,
    ) -> everruns::core::error::Result<Option<CompactionCheckpoint>> {
        let connection = self
            .store
            .database
            .connect()
            .map_err(|error| AgentLoopError::store(error.to_string()))?;
        let mut rows = connection
            .query(
                "SELECT checkpoint_id, source_sequence, format_version, b.hash, b.bytes \
                 FROM thread_compaction_checkpoints c \
                 JOIN blobs b ON b.hash = c.payload_blob_hash \
                 WHERE c.address = ?1 AND c.session_id = ?2 AND c.provider_type = ?3 \
                   AND c.model = ?4 ORDER BY c.source_sequence DESC LIMIT 1",
                params![
                    self.address.as_str(),
                    session_id.to_string(),
                    provider_type,
                    model,
                ],
            )
            .await
            .map_err(|error| AgentLoopError::store(error.to_string()))?;
        let Some(row) = rows
            .next()
            .await
            .map_err(|error| AgentLoopError::store(error.to_string()))?
        else {
            return Ok(None);
        };
        let payload: CompactionCheckpointPayload =
            decode_content_addressed(&row, 3, 4, "checkpoint")
                .map_err(|error| AgentLoopError::store(error.to_string()))?;
        let id = text(&row, 0)
            .and_then(|id| {
                id.parse()
                    .map_err(|_| Error::InvalidPersistence("checkpoint ID is invalid".into()))
            })
            .map_err(|error| AgentLoopError::store(error.to_string()))?;
        let source_sequence = i64::try_from(
            u64_column(&row, 1).map_err(|error| AgentLoopError::store(error.to_string()))?,
        )
        .map_err(|_| AgentLoopError::store("checkpoint source sequence exceeds storage range"))?;
        let format_version = u32::try_from(
            u64_column(&row, 2).map_err(|error| AgentLoopError::store(error.to_string()))?,
        )
        .map_err(|_| AgentLoopError::store("checkpoint format version exceeds storage range"))?;
        Ok(Some(CompactionCheckpoint {
            id,
            session_id,
            source_sequence,
            provider_type: provider_type.to_owned(),
            model: model.to_owned(),
            format_version,
            payload,
        }))
    }

    async fn install(
        &self,
        checkpoint: CompactionCheckpoint,
    ) -> everruns::core::error::Result<bool> {
        if checkpoint.source_sequence < 0 {
            return Err(AgentLoopError::store(
                "checkpoint source sequence is invalid",
            ));
        }
        let payload = serde_json::to_vec(&checkpoint.payload)
            .map_err(|error| AgentLoopError::store(error.to_string()))?;
        if payload.len() > MAX_COMPACTION_CHECKPOINT_BYTES {
            return Err(AgentLoopError::store(
                "compaction checkpoint exceeds 32 MiB",
            ));
        }
        let payload_hash = bytes_hash(&payload);
        let mut connection = self
            .store
            .write_connection()
            .await
            .map_err(|error| AgentLoopError::store(error.to_string()))?;
        let transaction = connection
            .transaction()
            .await
            .map_err(|error| AgentLoopError::store(error.to_string()))?;
        insert_blob(&transaction, &payload_hash, &payload)
            .await
            .map_err(|error| AgentLoopError::store(error.to_string()))?;
        let changed = transaction
            .execute(
                "INSERT INTO thread_compaction_checkpoints \
                 (address, session_id, provider_type, model, format_version, checkpoint_id, \
                  source_sequence, payload_blob_hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(address, session_id, provider_type, model, format_version) \
                 DO UPDATE SET checkpoint_id = excluded.checkpoint_id, \
                   source_sequence = excluded.source_sequence, payload_blob_hash = excluded.payload_blob_hash \
                 WHERE thread_compaction_checkpoints.source_sequence < excluded.source_sequence",
                params![
                    self.address.as_str(),
                    checkpoint.session_id.to_string(),
                    checkpoint.provider_type.as_str(),
                    checkpoint.model.as_str(),
                    i64::from(checkpoint.format_version),
                    checkpoint.id.to_string(),
                    checkpoint.source_sequence,
                    payload_hash.as_str(),
                ],
            )
            .await
            .map_err(|error| AgentLoopError::store(error.to_string()))?;
        if changed == 0 {
            delete_unreferenced_blob(&transaction, &payload_hash)
                .await
                .map_err(|error| AgentLoopError::store(error.to_string()))?;
        }
        transaction
            .commit()
            .await
            .map_err(|error| AgentLoopError::store(error.to_string()))?;
        Ok(changed == 1)
    }

    async fn get_proactive_attempt(
        &self,
        session_id: SessionId,
        provider_type: &str,
        model: &str,
    ) -> everruns::core::error::Result<Option<ProactiveCompactionAttempt>> {
        Ok(self
            .store
            .proactive_attempts
            .get(session_id, provider_type, model)
            .await)
    }

    async fn record_proactive_attempt(
        &self,
        session_id: SessionId,
        provider_type: &str,
        model: &str,
        attempt: ProactiveCompactionAttempt,
    ) -> everruns::core::error::Result<()> {
        self.store
            .proactive_attempts
            .record(session_id, provider_type, model, attempt)
            .await;
        Ok(())
    }
}

impl TursoProcessStore {
    /// Opens or creates a local Turso database and validates its schema version.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path
            .as_ref()
            .to_str()
            .ok_or(Error::PersistenceUnavailable)?;
        let database = turso::Builder::new_local(path)
            .build()
            .await
            .map_err(store_error)?;
        let store = Self {
            database,
            proactive_attempts: Arc::new(ProactiveCompactionAttemptTracker::default()),
        };
        store.initialize().await?;
        Ok(store)
    }

    /// Creates an isolated in-memory Turso store for tests or volatile hosts.
    pub async fn memory() -> Result<Self> {
        Self::open(":memory:").await
    }

    /// Creates a new persisted process at its exact address.
    pub async fn create(&self, process: Process) -> Result<DurableProcess> {
        if process.version() != 0 {
            return invalid("created process must begin at version zero");
        }
        let process_snapshot = process.snapshot()?;
        self.insert_process(process, BaseOrigin::Created { process_snapshot })
            .await
    }

    /// Imports a current process boundary without claiming its prior history.
    pub async fn import(&self, process: Process) -> Result<DurableProcess> {
        let process_snapshot = process.snapshot()?;
        self.insert_process(process, BaseOrigin::Imported { process_snapshot })
            .await
    }

    async fn insert_process(&self, process: Process, origin: BaseOrigin) -> Result<DurableProcess> {
        let address = process.id().clone();
        let base = StoredBase::new(
            address.clone(),
            None,
            None,
            process.version(),
            process.root_hash(),
            origin,
        )?;
        let head = TransactionHead {
            position: None,
            hash: base.base_hash.clone(),
        };
        let mut connection = self.write_connection().await?;
        let transaction = connection.transaction().await.map_err(store_error)?;
        insert_base(&transaction, &base).await?;
        let inserted = transaction
            .execute(
                "INSERT INTO svits \
                 (address, base_hash, covered_position, head_position, head_hash, process_version, root_hash) \
                 VALUES (?1, ?2, NULL, NULL, ?3, ?4, ?5)",
                params![
                    address.as_str(),
                    base.base_hash.as_str(),
                    head.hash.as_str(),
                    to_i64(process.version())?,
                    process.root_hash(),
                ],
            )
            .await;
        match inserted {
            Ok(_) => transaction.commit().await.map_err(store_error)?,
            Err(turso::Error::Constraint(_)) => {
                return Err(Error::PersistenceAlreadyExists(address.to_string()));
            }
            Err(error) => return Err(store_error(error)),
        }
        Ok(DurableProcess {
            store: self.clone(),
            process,
            head,
        })
    }

    async fn import_thread_events(&self, address: &ProcessId, events: &[Event]) -> Result<()> {
        let Some(session_id) = events.first().map(|event| event.session_id) else {
            return Ok(());
        };
        let mut event_ids = std::collections::HashSet::with_capacity(events.len());
        for (index, event) in events.iter().enumerate() {
            let expected_sequence = i32::try_from(index + 1)
                .map_err(|_| Error::ResourceLimitExceeded("thread event sequence"))?;
            if event.session_id != session_id
                || event.sequence != Some(expected_sequence)
                || !event_ids.insert(event.id)
                || event.is_ephemeral()
            {
                return invalid("legacy thread event sequence is invalid");
            }
        }

        let mut connection = self.write_connection().await?;
        let transaction = connection.transaction().await.map_err(store_error)?;
        let mut count_rows = transaction
            .query(
                "SELECT COUNT(*) FROM thread_events WHERE address = ?1 AND session_id = ?2",
                params![address.as_str(), session_id.to_string()],
            )
            .await
            .map_err(store_error)?;
        let existing_count = count_rows
            .next()
            .await
            .map_err(store_error)?
            .map(|row| u64_column(&row, 0))
            .transpose()?
            .unwrap_or(0);
        drop(count_rows);
        if existing_count > u64::try_from(events.len()).unwrap_or(u64::MAX) {
            return invalid("thread event log extends beyond legacy history");
        }

        for event in events {
            let sequence = event.sequence.expect("legacy sequence validated above");
            let bytes = serde_json::to_vec(event)
                .map_err(|_| Error::InvalidPersistence("thread event encoding failed".into()))?;
            if bytes.len() > MAX_TRANSACTION_BYTES {
                return Err(Error::ResourceLimitExceeded("thread event bytes"));
            }
            let hash = bytes_hash(&bytes);
            let mut existing = transaction
                .query(
                    "SELECT e.event_id, b.hash, b.bytes FROM thread_events e \
                     JOIN blobs b ON b.hash = e.event_blob_hash \
                     WHERE e.address = ?1 AND e.session_id = ?2 AND e.sequence = ?3",
                    params![
                        address.as_str(),
                        session_id.to_string(),
                        i64::from(sequence)
                    ],
                )
                .await
                .map_err(store_error)?;
            if let Some(row) = existing.next().await.map_err(store_error)? {
                if text(&row, 0)? != event.id.to_string()
                    || text(&row, 1)? != hash
                    || blob(&row, 2)? != bytes
                {
                    return invalid("thread event log conflicts with legacy history");
                }
                continue;
            }
            drop(existing);
            insert_blob(&transaction, &hash, &bytes).await?;
            transaction
                .execute(
                    "INSERT INTO thread_events \
                     (address, session_id, sequence, event_id, event_type, event_hash, event_blob_hash) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![
                        address.as_str(),
                        session_id.to_string(),
                        i64::from(sequence),
                        event.id.to_string(),
                        event.event_type.as_str(),
                        hash,
                    ],
                )
                .await
                .map_err(conflict_or_store)?;
        }
        transaction.commit().await.map_err(store_error)
    }

    async fn recent_thread_events(
        &self,
        address: &ProcessId,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<Event>> {
        if limit == 0 || limit > 4096 {
            return Err(Error::ResourceLimitExceeded("recent thread events"));
        }
        let connection = self.database.connect().map_err(store_error)?;
        let high = thread_history_high(&connection, address, session_id, 0)
            .await
            .map_err(|error| Error::InvalidPersistence(error.to_string()))?;
        let mut events = Vec::with_capacity(limit);
        for sequence in (1..=high).rev() {
            let event = thread_event_at(&connection, address, session_id, sequence, 0)
                .await
                .map_err(|error| Error::InvalidPersistence(error.to_string()))?
                .ok_or_else(|| {
                    Error::InvalidPersistence("thread history has a sequence gap".into())
                })?;
            if matches!(
                event.event_type.as_str(),
                INPUT_MESSAGE | OUTPUT_MESSAGE_COMPLETED | TOOL_COMPLETED
            ) {
                events.push(event);
                if events.len() == limit {
                    break;
                }
            }
        }
        events.reverse();
        Ok(events)
    }

    async fn continues_thread_history(
        &self,
        address: &ProcessId,
        session_id: SessionId,
    ) -> Result<bool> {
        let connection = self.database.connect().map_err(store_error)?;
        let mut rows = connection
            .query(
                "SELECT 1 FROM thread_event_refs WHERE child_address = ?1 AND session_id = ?2",
                params![address.as_str(), session_id.to_string()],
            )
            .await
            .map_err(store_error)?;
        Ok(rows.next().await.map_err(store_error)?.is_some())
    }

    /// Restores one process by address from its base and retained event tail.
    pub async fn resume(&self, address: impl Into<String>) -> Result<DurableProcess> {
        let address = ProcessId::new(address)?;
        let row = self.load_svit_row(&address).await?;
        let process = self.restore_at(address.clone(), row.target(), 0).await?;
        // Existing stores may predate automatic checkpoints. The first full
        // replay establishes a validated recovery boundary for later resumes.
        self.persist_recovery_checkpoint(&process, &row.head)
            .await?;
        Ok(DurableProcess {
            store: self.clone(),
            process,
            head: row.head,
        })
    }

    async fn initialize(&self) -> Result<()> {
        let connection = self.database.connect().map_err(store_error)?;
        connection
            .execute_batch(SCHEMA)
            .await
            .map_err(store_error)?;
        connection
            .execute(
                "INSERT OR IGNORE INTO svit_meta (key, value) VALUES ('schema_version', ?1)",
                [SCHEMA_VERSION],
            )
            .await
            .map_err(store_error)?;
        let mut rows = connection
            .query(
                "SELECT value FROM svit_meta WHERE key = 'schema_version'",
                (),
            )
            .await
            .map_err(store_error)?;
        let row = rows
            .next()
            .await
            .map_err(store_error)?
            .ok_or(Error::PersistenceUnavailable)?;
        match text(&row, 0)?.as_str() {
            SCHEMA_VERSION => {}
            "1" => {
                connection
                    .execute(
                        "UPDATE svit_meta SET value = ?1 WHERE key = 'schema_version'",
                        [SCHEMA_VERSION],
                    )
                    .await
                    .map_err(store_error)?;
            }
            _ => return invalid("unsupported persistence schema version"),
        }
        Ok(())
    }

    async fn write_connection(&self) -> Result<Connection> {
        let mut connection = self.database.connect().map_err(store_error)?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON")
            .await
            .map_err(store_error)?;
        connection.set_transaction_behavior(TransactionBehavior::Immediate);
        Ok(connection)
    }

    async fn append(
        &self,
        expected: &TransactionHead,
        event: &ProcessTransaction,
        candidate: &Process,
    ) -> Result<TransactionHead> {
        event.validate_successor(expected)?;
        if candidate.id() != &event.address || candidate.version() != event.process_version_after {
            return invalid("checkpoint candidate does not match transaction result");
        }
        let event_bytes = event.to_bytes()?;
        let event_blob_hash = bytes_hash(&event_bytes);
        let next_head = TransactionHead {
            position: Some(event.position),
            hash: event.transaction_hash.clone(),
        };
        let checkpoint = (event.position + 1)
            .is_multiple_of(RECOVERY_CHECKPOINT_INTERVAL)
            .then(|| RecoveryCheckpoint::new(candidate, &next_head))
            .transpose()?;
        if checkpoint
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.root_hash != event.resulting_root_hash)
        {
            return invalid("checkpoint root does not match transaction result");
        }
        let mut connection = self.write_connection().await?;
        let transaction = connection.transaction().await.map_err(store_error)?;

        // THREAT[TM-PERS-002]: The event, path projection, and head CAS share
        // one database transaction, so failure cannot publish a partial commit.
        insert_blob(&transaction, &event_blob_hash, &event_bytes).await?;
        transaction
            .execute(
                "INSERT INTO events \
                 (address, position, previous_hash, process_version_before, process_version_after, \
                  source, resulting_root_hash, event_hash, event_blob_hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    event.address.as_str(),
                    to_i64(event.position)?,
                    event.previous_hash.as_str(),
                    to_i64(event.process_version_before)?,
                    to_i64(event.process_version_after)?,
                    event.source.as_str(),
                    event.resulting_root_hash.as_str(),
                    event.transaction_hash.as_str(),
                    event_blob_hash.as_str(),
                ],
            )
            .await
            .map_err(conflict_or_store)?;
        for path in &event.touched_paths {
            transaction
                .execute(
                    "INSERT INTO event_paths (address, position, path) VALUES (?1, ?2, ?3)",
                    params![
                        event.address.as_str(),
                        to_i64(event.position)?,
                        path.as_str()
                    ],
                )
                .await
                .map_err(store_error)?;
        }
        let expected_position = sql_position(expected.position)?;
        let changed = transaction
            .execute(
                "UPDATE svits SET head_position = ?1, head_hash = ?2, process_version = ?3, root_hash = ?4 \
                 WHERE address = ?5 AND head_hash = ?6 \
                   AND ((head_position IS NULL AND ?7 IS NULL) OR head_position = ?8)",
                params![
                    to_i64(event.position)?,
                    event.transaction_hash.as_str(),
                    to_i64(event.process_version_after)?,
                    event.resulting_root_hash.as_str(),
                    event.address.as_str(),
                    expected.hash.as_str(),
                    expected_position.clone(),
                    expected_position,
                ],
            )
            .await
            .map_err(store_error)?;
        if changed != 1 {
            return Err(Error::PersistenceConflict);
        }
        if let Some(checkpoint) = &checkpoint {
            // THREAT[TM-PERS-001]: The checkpoint and event head are published
            // atomically. Resume validates the complete checkpoint before it
            // trusts that boundary and replays the remaining hash-chained tail.
            upsert_recovery_checkpoint(&transaction, checkpoint).await?;
        }
        transaction.commit().await.map_err(store_error)?;
        Ok(next_head)
    }

    async fn query_transactions(
        &self,
        address: &ProcessId,
        query: TransactionQuery,
    ) -> Result<Vec<ProcessTransaction>> {
        // THREAT[TM-DOS-009]: Query text and result counts are bounded before
        // constructing a database result set.
        if query.limit == 0 || query.limit > MAX_QUERY_TRANSACTIONS {
            return Err(Error::ResourceLimitExceeded(
                "persistence query transactions",
            ));
        }
        if let Some(path) = query.path_prefix.as_deref() {
            validate_query_path(path)?;
        }
        for text in [query.source.as_deref(), query.transaction_hash.as_deref()]
            .into_iter()
            .flatten()
        {
            if text.len() > MAX_QUERY_TEXT_BYTES {
                return Err(Error::ResourceLimitExceeded("persistence query text"));
            }
        }

        let query_path = query.path_prefix.clone();
        let mut sql = String::from(
            "SELECT b.hash, b.bytes, e.position, e.event_hash, e.source, e.process_version_after \
             FROM events e JOIN blobs b ON b.hash = e.event_blob_hash \
             WHERE e.address = ?",
        );
        let mut parameters = vec![SqlValue::Text(address.to_string())];
        if let Some(position) = query.after_position {
            sql.push_str(" AND e.position > ?");
            parameters.push(SqlValue::Integer(to_i64(position)?));
        }
        if let Some(position) = query.through_position {
            sql.push_str(" AND e.position <= ?");
            parameters.push(SqlValue::Integer(to_i64(position)?));
        }
        if let Some(source) = query.source {
            sql.push_str(" AND e.source = ?");
            parameters.push(SqlValue::Text(source));
        }
        if let Some(version) = query.process_version_from {
            sql.push_str(" AND e.process_version_after >= ?");
            parameters.push(SqlValue::Integer(to_i64(version)?));
        }
        if let Some(version) = query.process_version_through {
            sql.push_str(" AND e.process_version_after <= ?");
            parameters.push(SqlValue::Integer(to_i64(version)?));
        }
        if let Some(hash) = query.transaction_hash {
            sql.push_str(" AND e.event_hash = ?");
            parameters.push(SqlValue::Text(hash));
        }
        if let Some(path) = query.path_prefix.filter(|path| path != "/") {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM event_paths p \
                 WHERE p.address = e.address AND p.position = e.position \
                   AND (p.path = ? OR substr(p.path, 1, length(?) + 1) = ? || '/'))",
            );
            parameters.push(SqlValue::Text(path.clone()));
            parameters.push(SqlValue::Text(path.clone()));
            parameters.push(SqlValue::Text(path));
        }
        sql.push_str(" ORDER BY e.position LIMIT ?");
        parameters.push(SqlValue::Integer(i64::from(query.limit)));

        let connection = self.database.connect().map_err(store_error)?;
        let mut rows = connection
            .query(sql, parameters)
            .await
            .map_err(store_error)?;
        let mut events = Vec::new();
        while let Some(row) = rows.next().await.map_err(store_error)? {
            let event: ProcessTransaction = decode_content_addressed(&row, 0, 1, "event")?;
            event.validate()?;
            if event.address != *address
                || event.position != u64_column(&row, 2)?
                || event.transaction_hash != text(&row, 3)?
                || event.source != text(&row, 4)?
                || event.process_version_after != u64_column(&row, 5)?
            {
                return invalid("event query projection does not match its envelope");
            }
            if let Some(path) = query_path.as_deref().filter(|path| *path != "/")
                && !event.touched_paths.iter().any(|touched| {
                    touched == path
                        || touched
                            .strip_prefix(path)
                            .is_some_and(|tail| tail.starts_with('/'))
                })
            {
                return invalid("event path projection does not match its envelope");
            }
            events.push(event);
        }
        Ok(events)
    }

    async fn persist_snapshot(
        &self,
        process: &Process,
        expected: &TransactionHead,
    ) -> Result<PersistenceSnapshot> {
        let snapshot = StoredSnapshot::new(process, expected)?;
        let bytes = encode(&snapshot)?;
        let blob_hash = bytes_hash(&bytes);
        let mut connection = self.write_connection().await?;
        let transaction = connection.transaction().await.map_err(store_error)?;
        require_head(&transaction, process.id(), expected).await?;
        insert_blob(&transaction, &blob_hash, &bytes).await?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO snapshots \
                 (snapshot_hash, address, covered_position, head_hash, process_version, root_hash, snapshot_blob_hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    snapshot.snapshot_hash.as_str(),
                    process.id().as_str(),
                    sql_position(snapshot.covered_position)?,
                    snapshot.head_hash.as_str(),
                    to_i64(snapshot.process_version)?,
                    snapshot.root_hash.as_str(),
                    blob_hash.as_str(),
                ],
            )
            .await
            .map_err(store_error)?;
        transaction.commit().await.map_err(store_error)?;
        Ok(PersistenceSnapshot(snapshot))
    }

    async fn persist_recovery_checkpoint(
        &self,
        process: &Process,
        expected: &TransactionHead,
    ) -> Result<()> {
        if expected.position.is_none() {
            return Ok(());
        }
        let connection = self.database.connect().map_err(store_error)?;
        let mut rows = connection
            .query(
                "SELECT c.covered_position, c.head_hash, c.process_version, b.bytes \
                 FROM recovery_checkpoints c JOIN blobs b ON b.hash = c.snapshot_blob_hash \
                 WHERE c.address = ?1",
                [process.id().as_str()],
            )
            .await
            .map_err(store_error)?;
        if let Some(row) = rows.next().await.map_err(store_error)?
            && optional_u64(&row, 0)? == expected.position
            && text(&row, 1)? == expected.hash
            && u64_column(&row, 2)? == process.version()
            && blob(&row, 3)?.starts_with(PROCESS_SNAPSHOT_PREFIX)
        {
            return Ok(());
        }
        drop(rows);
        drop(connection);
        let checkpoint = RecoveryCheckpoint::new(process, expected)?;
        let mut connection = self.write_connection().await?;
        let transaction = connection.transaction().await.map_err(store_error)?;
        require_head(&transaction, process.id(), expected).await?;
        upsert_recovery_checkpoint(&transaction, &checkpoint).await?;
        transaction.commit().await.map_err(store_error)
    }

    async fn cut(&self, process: &Process, expected: &TransactionHead) -> Result<TransactionHead> {
        let snapshot = StoredSnapshot::new(process, expected)?;
        let snapshot_bytes = encode(&snapshot)?;
        let snapshot_blob_hash = bytes_hash(&snapshot_bytes);
        let base = StoredBase::new(
            process.id().clone(),
            expected.position,
            Some(expected.hash.clone()),
            process.version(),
            process.root_hash(),
            BaseOrigin::Snapshot {
                snapshot_hash: snapshot.snapshot_hash.clone(),
            },
        )?;
        let mut connection = self.write_connection().await?;
        let transaction = connection.transaction().await.map_err(store_error)?;
        require_head(&transaction, process.id(), expected).await?;
        let mut references = transaction
            .query(
                "SELECT child_address FROM fork_refs WHERE parent_address = ?1 LIMIT 1",
                [process.id().as_str()],
            )
            .await
            .map_err(store_error)?;
        // THREAT[TM-FORK-003]: A cut cannot remove a parent boundary still
        // referenced by a child. Cutting the child first detaches that edge.
        if references.next().await.map_err(store_error)?.is_some() {
            return Err(Error::PersistenceReferenced);
        }
        drop(references);
        insert_blob(&transaction, &snapshot_blob_hash, &snapshot_bytes).await?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO snapshots \
                 (snapshot_hash, address, covered_position, head_hash, process_version, root_hash, snapshot_blob_hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    snapshot.snapshot_hash.as_str(),
                    process.id().as_str(),
                    sql_position(snapshot.covered_position)?,
                    snapshot.head_hash.as_str(),
                    to_i64(snapshot.process_version)?,
                    snapshot.root_hash.as_str(),
                    snapshot_blob_hash.as_str(),
                ],
            )
            .await
            .map_err(store_error)?;
        insert_base(&transaction, &base).await?;
        let changed = update_base_head(&transaction, process, expected, &base).await?;
        if changed != 1 {
            return Err(Error::PersistenceConflict);
        }
        transaction
            .execute(
                "DELETE FROM event_paths WHERE address = ?1",
                [process.id().as_str()],
            )
            .await
            .map_err(store_error)?;
        transaction
            .execute(
                "DELETE FROM events WHERE address = ?1",
                [process.id().as_str()],
            )
            .await
            .map_err(store_error)?;
        transaction
            .execute(
                "DELETE FROM fork_refs WHERE child_address = ?1",
                [process.id().as_str()],
            )
            .await
            .map_err(store_error)?;
        remove_recovery_checkpoint(&transaction, process.id()).await?;
        transaction.commit().await.map_err(store_error)?;
        Ok(TransactionHead {
            position: expected.position,
            hash: base.base_hash,
        })
    }

    async fn create_fork(
        &self,
        parent: &Process,
        parent_head: &TransactionHead,
        child: Process,
    ) -> Result<DurableProcess> {
        let boundary = ForkBoundary {
            parent_address: parent.id().clone(),
            parent_head: parent_head.clone(),
            parent_process_version: parent.version(),
            parent_root_hash: parent.root_hash(),
        };
        let base = StoredBase::new(
            child.id().clone(),
            None,
            None,
            child.version(),
            child.root_hash(),
            BaseOrigin::Fork {
                parent: boundary.clone(),
            },
        )?;
        let child_head = TransactionHead {
            position: None,
            hash: base.base_hash.clone(),
        };
        let mut connection = self.write_connection().await?;
        let transaction = connection.transaction().await.map_err(store_error)?;
        require_head(&transaction, parent.id(), parent_head).await?;
        insert_base(&transaction, &base).await?;
        let inserted = transaction
            .execute(
                "INSERT INTO svits \
                 (address, base_hash, covered_position, head_position, head_hash, process_version, root_hash) \
                 VALUES (?1, ?2, NULL, NULL, ?3, ?4, ?5)",
                params![
                    child.id().as_str(),
                    base.base_hash.as_str(),
                    child_head.hash.as_str(),
                    to_i64(child.version())?,
                    child.root_hash(),
                ],
            )
            .await;
        match inserted {
            Ok(_) => {}
            Err(turso::Error::Constraint(_)) => {
                return Err(Error::PersistenceAlreadyExists(child.id().to_string()));
            }
            Err(error) => return Err(store_error(error)),
        }
        transaction
            .execute(
                "INSERT INTO fork_refs \
                 (child_address, parent_address, parent_position, parent_head_hash, parent_root_hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    child.id().as_str(),
                    parent.id().as_str(),
                    sql_position(parent_head.position)?,
                    parent_head.hash.as_str(),
                    parent.root_hash(),
                ],
            )
            .await
            .map_err(store_error)?;
        if let Some(session_id) = thread_session_id(parent)? {
            let through_sequence =
                thread_history_high_in_transaction(&transaction, parent.id(), session_id)
                    .await
                    .map_err(|error| Error::InvalidPersistence(error.to_string()))?;
            transaction
                .execute(
                    "INSERT INTO thread_event_refs \
                     (child_address, session_id, parent_address, through_sequence) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        child.id().as_str(),
                        session_id.to_string(),
                        parent.id().as_str(),
                        i64::from(through_sequence),
                    ],
                )
                .await
                .map_err(store_error)?;
            transaction
                .execute(
                    "INSERT INTO thread_compaction_checkpoints \
                     (address, session_id, provider_type, model, format_version, checkpoint_id, \
                      source_sequence, payload_blob_hash) \
                     SELECT ?1, session_id, provider_type, model, format_version, checkpoint_id, \
                            source_sequence, payload_blob_hash \
                     FROM thread_compaction_checkpoints \
                     WHERE address = ?2 AND session_id = ?3 AND source_sequence <= ?4",
                    params![
                        child.id().as_str(),
                        parent.id().as_str(),
                        session_id.to_string(),
                        i64::from(through_sequence),
                    ],
                )
                .await
                .map_err(store_error)?;
        }
        transaction.commit().await.map_err(store_error)?;
        Ok(DurableProcess {
            store: self.clone(),
            process: child,
            head: child_head,
        })
    }

    fn restore_at<'a>(
        &'a self,
        address: ProcessId,
        target: RestoreTarget,
        depth: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Process>> + Send + 'a>> {
        Box::pin(async move {
            if depth >= MAX_FORK_DEPTH {
                return Err(Error::ResourceLimitExceeded("persistence fork depth"));
            }
            let base = self.load_base(&address).await?;
            if target.position.is_some_and(|position| {
                base.covered_position
                    .is_some_and(|covered| position < covered)
            }) {
                return invalid("requested event position was cut");
            }
            let mut process = match &base.origin {
                BaseOrigin::Created { process_snapshot }
                | BaseOrigin::Imported { process_snapshot } => Process::restore(process_snapshot)?,
                BaseOrigin::Snapshot { snapshot_hash } => {
                    let snapshot = self.load_snapshot(snapshot_hash).await?;
                    snapshot.validate_envelope()?;
                    snapshot.restore_process()?
                }
                BaseOrigin::Fork { parent } => {
                    let parent_target = RestoreTarget {
                        position: parent.parent_head.position,
                        hash: parent.parent_head.hash.clone(),
                        process_version: parent.parent_process_version,
                        root_hash: parent.parent_root_hash.clone(),
                    };
                    let parent_process = self
                        .restore_at(parent.parent_address.clone(), parent_target, depth + 1)
                        .await?;
                    parent_process.fork(address.to_string())?
                }
            };
            if process.id() != &address
                || process.version() != base.process_version
                || process.root_hash() != base.root_hash
            {
                return invalid("base metadata does not match reconstructed process");
            }

            let mut start = base.covered_position.map_or(0, |position| position + 1);
            let mut expected_head = TransactionHead {
                position: base.covered_position,
                hash: base.base_hash.clone(),
            };
            let Some(end) = target.position else {
                if target.hash != base.base_hash {
                    return invalid("empty event tail does not match base hash");
                }
                return verify_target(process, &target);
            };
            if end < start {
                if end == base.covered_position.unwrap_or(u64::MAX) && target.hash == base.base_hash
                {
                    return verify_target(process, &target);
                }
                return invalid("event target precedes active base");
            }
            if let Some(checkpoint) = self.load_recovery_checkpoint(&address).await?
                && checkpoint.covered_position >= start
                && checkpoint.covered_position <= end
            {
                // A recovery checkpoint is a cache of an already validated
                // canonical boundary. Retained events at and before it remain
                // queryable, but ordinary resume validates the checkpoint and
                // only reduces the newer hash-chained tail.
                process = checkpoint.process;
                let position = checkpoint.covered_position;
                if position == end {
                    if checkpoint.head_hash != target.hash
                        || checkpoint.root_hash != target.root_hash
                        || process.version() != target.process_version
                    {
                        return invalid("restored process does not match requested boundary");
                    }
                    return Ok(process);
                }
                start = position + 1;
                expected_head = TransactionHead {
                    position: Some(position),
                    hash: checkpoint.head_hash,
                };
            }
            let connection = self.database.connect().map_err(store_error)?;
            let mut rows = connection
                .query(
                    "SELECT b.hash, b.bytes FROM events e JOIN blobs b ON b.hash = e.event_blob_hash \
                     WHERE e.address = ?1 AND e.position >= ?2 AND e.position <= ?3 \
                     ORDER BY e.position",
                    params![address.as_str(), to_i64(start)?, to_i64(end)?],
                )
                .await
                .map_err(store_error)?;
            let mut count = 0usize;
            while let Some(row) = rows.next().await.map_err(store_error)? {
                count += 1;
                if count > MAX_REPLAY_TRANSACTIONS {
                    return Err(Error::ResourceLimitExceeded(
                        "persistence replay transactions",
                    ));
                }
                let event: ProcessTransaction = decode_content_addressed(&row, 0, 1, "event")?;
                // THREAT[TM-PERS-001]: Treat every stored event as untrusted;
                // verify its content hash, hash chain, and typed reducer. This
                // reconstruction is unpublished, so it mutates in place and
                // verifies the complete root at bounded batch boundaries and
                // the requested head instead of cloning and hashing the whole
                // growing process for every record.
                event.validate_successor(&expected_head)?;
                if event.address != address || event.process_version_before != process.version() {
                    return invalid("transaction address or process version is invalid");
                }
                event.replay_unpublished(&mut process)?;
                if count.is_multiple_of(RECOVERY_CHECKPOINT_INTERVAL as usize)
                    || event.position == end
                {
                    process.validate_recovery_boundary()?;
                    if process.root_hash() != event.resulting_root_hash {
                        return invalid("transaction reducer root hash mismatch");
                    }
                }
                expected_head = TransactionHead {
                    position: Some(event.position),
                    hash: event.transaction_hash,
                };
            }
            if expected_head.position != Some(end) || expected_head.hash != target.hash {
                return invalid("event tail is incomplete or has the wrong head");
            }
            verify_target(process, &target)
        })
    }

    async fn load_svit_row(&self, address: &ProcessId) -> Result<SvitRow> {
        let connection = self.database.connect().map_err(store_error)?;
        let mut rows = connection
            .query(
                "SELECT base_hash, covered_position, head_position, head_hash, process_version, root_hash \
                 FROM svits WHERE address = ?1",
                [address.as_str()],
            )
            .await
            .map_err(store_error)?;
        let row = rows
            .next()
            .await
            .map_err(store_error)?
            .ok_or_else(|| Error::PersistenceNotFound(address.to_string()))?;
        Ok(SvitRow {
            base_hash: text(&row, 0)?,
            covered_position: optional_u64(&row, 1)?,
            head: TransactionHead {
                position: optional_u64(&row, 2)?,
                hash: text(&row, 3)?,
            },
            process_version: u64_column(&row, 4)?,
            root_hash: text(&row, 5)?,
        })
    }

    async fn load_base(&self, address: &ProcessId) -> Result<StoredBase> {
        let row = self.load_svit_row(address).await?;
        let connection = self.database.connect().map_err(store_error)?;
        let mut rows = connection
            .query(
                "SELECT b.hash, b.bytes FROM bases x JOIN blobs b ON b.hash = x.base_blob_hash \
                 WHERE x.base_hash = ?1 AND x.address = ?2",
                params![row.base_hash.as_str(), address.as_str()],
            )
            .await
            .map_err(store_error)?;
        let value = rows
            .next()
            .await
            .map_err(store_error)?
            .ok_or_else(|| Error::InvalidPersistence("active base is missing".into()))?;
        let base: StoredBase = decode_content_addressed(&value, 0, 1, "base")?;
        base.validate()?;
        if base.base_hash != row.base_hash || base.covered_position != row.covered_position {
            return invalid("active base metadata is inconsistent");
        }
        Ok(base)
    }

    async fn load_snapshot(&self, snapshot_hash: &str) -> Result<StoredSnapshot> {
        let connection = self.database.connect().map_err(store_error)?;
        let mut rows = connection
            .query(
                "SELECT b.hash, b.bytes FROM snapshots s JOIN blobs b ON b.hash = s.snapshot_blob_hash \
                 WHERE s.snapshot_hash = ?1",
                [snapshot_hash],
            )
            .await
            .map_err(store_error)?;
        let row = rows
            .next()
            .await
            .map_err(store_error)?
            .ok_or_else(|| Error::InvalidPersistence("snapshot is missing".into()))?;
        decode_content_addressed(&row, 0, 1, "snapshot")
    }

    async fn load_recovery_checkpoint(
        &self,
        address: &ProcessId,
    ) -> Result<Option<LoadedRecoveryCheckpoint>> {
        let connection = self.database.connect().map_err(store_error)?;
        let mut rows = connection
            .query(
                "SELECT covered_position, head_hash, process_version, root_hash, snapshot_hash, snapshot_blob_hash \
                 FROM recovery_checkpoints WHERE address = ?1",
                [address.as_str()],
            )
            .await
            .map_err(store_error)?;
        let Some(row) = rows.next().await.map_err(store_error)? else {
            return Ok(None);
        };
        let covered_position = u64_column(&row, 0)?;
        let head_hash = text(&row, 1)?;
        let process_version = u64_column(&row, 2)?;
        let root_hash = text(&row, 3)?;
        let snapshot_hash = text(&row, 4)?;
        let snapshot_blob_hash = text(&row, 5)?;
        drop(rows);

        let mut blobs = connection
            .query(
                "SELECT hash, bytes FROM blobs WHERE hash = ?1",
                [snapshot_blob_hash.as_str()],
            )
            .await
            .map_err(store_error)?;
        let blob_row = blobs.next().await.map_err(store_error)?.ok_or_else(|| {
            Error::InvalidPersistence("recovery checkpoint blob is missing".into())
        })?;
        let stored_blob_hash = text(&blob_row, 0)?;
        let process_snapshot = blob(&blob_row, 1)?;
        if stored_blob_hash != snapshot_blob_hash
            || bytes_hash(&process_snapshot) != snapshot_blob_hash
        {
            return invalid("recovery checkpoint content hash is invalid");
        }
        drop(blobs);

        // Checkpoints written before the direct-image format wrapped the
        // process snapshot in a public StoredSnapshot envelope. Read those
        // once; resume rewrites the current cache in the direct format.
        let process = if process_snapshot.starts_with(PROCESS_SNAPSHOT_PREFIX) {
            let checkpoint = RecoveryCheckpoint {
                address: address.clone(),
                covered_position,
                head_hash: head_hash.clone(),
                process_version,
                root_hash: root_hash.clone(),
                snapshot_hash: snapshot_hash.clone(),
                process_snapshot_hash: snapshot_blob_hash,
                process_snapshot,
            };
            if checkpoint.compute_hash()? != checkpoint.snapshot_hash {
                return invalid("recovery checkpoint metadata hash is invalid");
            }
            checkpoint.restore_process()?
        } else {
            let checkpoint: StoredSnapshot = decode(&process_snapshot, "snapshot")?;
            checkpoint.validate_envelope()?;
            if checkpoint.address != *address
                || checkpoint.covered_position != Some(covered_position)
                || checkpoint.head_hash != head_hash
                || checkpoint.process_version != process_version
                || checkpoint.root_hash != root_hash
                || checkpoint.snapshot_hash != snapshot_hash
            {
                return invalid("recovery checkpoint envelope is inconsistent");
            }
            checkpoint.restore_process()?
        };

        let mut events = connection
            .query(
                "SELECT event_hash, process_version_after, resulting_root_hash FROM events \
                 WHERE address = ?1 AND position = ?2",
                params![address.as_str(), to_i64(covered_position)?],
            )
            .await
            .map_err(store_error)?;
        let event = events.next().await.map_err(store_error)?.ok_or_else(|| {
            Error::InvalidPersistence("recovery checkpoint event is missing".into())
        })?;
        if text(&event, 0)? != head_hash
            || u64_column(&event, 1)? != process_version
            || text(&event, 2)? != root_hash
        {
            return invalid("recovery checkpoint does not match its event boundary");
        }
        Ok(Some(LoadedRecoveryCheckpoint {
            covered_position,
            head_hash,
            root_hash,
            process,
        }))
    }
}

/// One in-memory process whose commits are durably serialized by Turso.
pub struct DurableProcess {
    store: TursoProcessStore,
    process: Process,
    head: TransactionHead,
}

impl DurableProcess {
    /// Returns the process address.
    pub fn id(&self) -> &ProcessId {
        self.process.id()
    }

    /// Returns the committed process version.
    pub fn version(&self) -> u64 {
        self.process.version()
    }

    /// Reads one process value, resolving mount nodes lazily.
    pub fn read(&self, path: &str) -> Result<Option<Value>> {
        self.process.read(path)
    }

    /// Describes one process or mount path.
    pub fn stat(&self, path: &str) -> Result<Option<Value>> {
        self.process.stat(path)
    }

    /// Reattaches one runtime mount whose descriptor already persisted.
    pub fn attach_mount(&mut self, name: impl Into<String>, mount: Mount) -> Result<crate::Change> {
        let name = name.into();
        let descriptor = mount.provider().descriptor().to_value();
        let recorded = match self.process.read("/mounts")? {
            Some(Value::Map(mounts)) => mounts.get(&name).cloned(),
            _ => None,
        };
        if recorded.as_ref() != Some(&descriptor) {
            return Err(Error::InvalidPersistence(format!(
                "mount identity for {name} differs from the persisted descriptor"
            )));
        }
        self.process.attach_mount(name, mount)
    }

    /// Returns the current committed root hash.
    pub fn root_hash(&self) -> String {
        self.process.root_hash()
    }

    /// Serializes the committed process boundary owned by this handle.
    pub fn process_projection(&self) -> Process {
        self.process.clone()
    }

    /// Durably commits one host write as a uniform transaction event.
    ///
    /// The transition itself reports the mutations it applied, so the stored
    /// event and a live observer always describe the same change.
    pub async fn write(&mut self, path: &str, value: Value) -> Result<crate::Change> {
        let mut candidate = self.process.clone();
        let change = candidate.write(path, value)?;
        self.commit_change(candidate, change, "host.write").await
    }

    /// Durably commits one host removal as a uniform transaction event.
    pub async fn remove(&mut self, path: &str) -> Result<crate::Change> {
        let mut candidate = self.process.clone();
        let change = candidate.remove(path)?;
        self.commit_change(candidate, change, "host.remove").await
    }

    /// Durably appends one host-supplied value to the process inbox.
    pub async fn enqueue_inbox(&mut self, value: Value) -> Result<crate::Change> {
        let mut candidate = self.process.clone();
        let change = candidate.enqueue_inbox(value)?;
        self.commit_change(candidate, change, "inbox.enqueue").await
    }

    /// Durably removes the exact inbox head observed by a completed turn.
    pub async fn acknowledge_inbox(&mut self, expected: &Value) -> Result<crate::Change> {
        let mut candidate = self.process.clone();
        let change = candidate.acknowledge_inbox(expected)?;
        self.commit_change(candidate, change, "inbox.acknowledge")
            .await
    }

    /// Publishes a transition that already reported its own mutations.
    ///
    /// A change with no mutations touched no committed state — a granted mount
    /// write is the only such transition — so there is no event to append.
    async fn commit_change(
        &mut self,
        candidate: Process,
        change: crate::Change,
        source: &str,
    ) -> Result<crate::Change> {
        if change.mutations().is_empty() {
            self.process = candidate;
            return Ok(change);
        }
        self.commit_candidate(candidate, change.mutations().to_vec(), source)
            .await?;
        Ok(change)
    }

    /// Durably initializes the host-managed Svit thread projection.
    pub async fn initialize_thread_state(&mut self, value: Value) -> Result<crate::Change> {
        let mut candidate = self.process.clone();
        let change = candidate.initialize_thread_state(value)?;
        self.commit_change(candidate, change, "reasoning.initialize")
            .await
    }

    /// Replaces legacy materialized thread history and immediately publishes a
    /// compact recovery boundary so later resumes never decode that history.
    pub async fn replace_thread_state(&mut self, value: Value) -> Result<crate::Change> {
        let mut candidate = self.process.clone();
        let change = candidate.replace_thread_state(value)?;
        let change = self
            .commit_change(candidate, change, "reasoning.migrate")
            .await?;
        self.store
            .persist_recovery_checkpoint(&self.process, &self.head)
            .await?;
        Ok(change)
    }

    /// Durably refreshes the descriptive built-in catalog.
    pub async fn replace_builtins(&mut self, value: Value) -> Result<crate::Change> {
        let mut candidate = self.process.clone();
        let change = candidate.replace_builtins(value)?;
        self.commit_change(candidate, change, "builtins.refresh")
            .await
    }

    /// Executes guest Lisp and durably publishes its exact committed write set.
    pub async fn exec(&mut self, path: &str, input: Value) -> Result<Activation> {
        let prepared = self.process.prepare_exec(path, input)?;
        self.commit_activation(prepared).await
    }

    /// Executes guest Lisp with host-attached built-ins and durably commits it.
    pub async fn exec_with_builtins(
        &mut self,
        path: &str,
        input: Value,
        builtins: &Builtins,
    ) -> Result<Activation> {
        let context = Arc::new(Mutex::new(self.process.clone()));
        let prepared = self
            .process
            .prepare_exec_with_builtins(path, input, builtins, context)
            .await?;
        self.commit_activation(prepared).await
    }

    async fn commit_activation(
        &mut self,
        prepared: crate::process::PreparedActivation,
    ) -> Result<Activation> {
        let event = ProcessTransaction::new(
            self.process.id().clone(),
            &self.head,
            self.process.version(),
            prepared.candidate().version(),
            prepared.mutations().to_vec(),
            "activation",
            prepared.candidate().root_hash(),
        )?;
        match self
            .store
            .append(&self.head, &event, prepared.candidate())
            .await
        {
            Ok(head) => {
                self.head = head;
                Ok(self.process.commit_prepared(prepared))
            }
            Err(error) => {
                self.process.fail_prepared(&prepared, &error);
                Err(error)
            }
        }
    }

    /// Queries retained transaction envelopes without executing guest code.
    pub async fn transactions(&self, query: TransactionQuery) -> Result<Vec<ProcessTransaction>> {
        self.store
            .query_transactions(self.process.id(), query)
            .await
    }

    /// Persists an on-demand replay or migration snapshot without cutting history.
    pub async fn snapshot(&self) -> Result<PersistenceSnapshot> {
        self.store.persist_snapshot(&self.process, &self.head).await
    }

    /// Replaces retained event history with one exact snapshot base.
    pub async fn cut(&mut self) -> Result<()> {
        self.head = self.store.cut(&self.process, &self.head).await?;
        Ok(())
    }

    /// Creates a child whose immutable base references this exact persisted head.
    pub async fn fork(&self, child: impl Into<String>) -> Result<DurableProcess> {
        let child = self.process.fork(child)?;
        self.store
            .create_fork(&self.process, &self.head, child)
            .await
    }

    async fn commit_candidate(
        &mut self,
        candidate: Process,
        mutations: Vec<Mutation>,
        source: &str,
    ) -> Result<()> {
        let event = ProcessTransaction::new(
            self.process.id().clone(),
            &self.head,
            self.process.version(),
            candidate.version(),
            mutations,
            source,
            candidate.root_hash(),
        )?;
        let head = self.store.append(&self.head, &event, &candidate).await?;
        self.process = candidate;
        self.head = head;
        Ok(())
    }
}

#[async_trait]
impl ProcessStore for TursoProcessStore {
    type Handle = DurableProcess;

    async fn create(&self, process: Process) -> Result<Self::Handle> {
        TursoProcessStore::create(self, process).await
    }

    async fn import(&self, process: Process) -> Result<Self::Handle> {
        TursoProcessStore::import(self, process).await
    }

    async fn resume(&self, address: &ProcessId) -> Result<Self::Handle> {
        TursoProcessStore::resume(self, address.to_string()).await
    }
}

#[async_trait]
impl DurableProcessHandle for DurableProcess {
    type Snapshot = PersistenceSnapshot;

    fn id(&self) -> &ProcessId {
        DurableProcess::id(self)
    }

    fn version(&self) -> u64 {
        DurableProcess::version(self)
    }

    fn read(&self, path: &str) -> Result<Option<Value>> {
        DurableProcess::read(self, path)
    }

    fn root_hash(&self) -> String {
        DurableProcess::root_hash(self)
    }

    fn process_projection(&self) -> Process {
        DurableProcess::process_projection(self)
    }

    fn event_log(&self) -> Arc<dyn EventLog> {
        Arc::new(TursoThreadEventLog {
            store: self.store.clone(),
            address: self.process.id().clone(),
        })
    }

    fn compaction_checkpoint_store(&self) -> Arc<dyn CompactionCheckpointStore> {
        Arc::new(TursoCompactionCheckpointStore {
            store: self.store.clone(),
            address: self.process.id().clone(),
        })
    }

    async fn continues_thread_history(&self, session_id: SessionId) -> Result<bool> {
        self.store
            .continues_thread_history(self.process.id(), session_id)
            .await
    }

    async fn import_thread_events(&self, events: &[Event]) -> Result<()> {
        self.store
            .import_thread_events(self.process.id(), events)
            .await
    }

    async fn recent_thread_events(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<Event>> {
        self.store
            .recent_thread_events(self.process.id(), session_id, limit)
            .await
    }

    async fn write(&mut self, path: &str, value: Value) -> Result<crate::Change> {
        DurableProcess::write(self, path, value).await
    }

    async fn remove(&mut self, path: &str) -> Result<crate::Change> {
        DurableProcess::remove(self, path).await
    }

    async fn exec(&mut self, path: &str, input: Value) -> Result<Activation> {
        DurableProcess::exec(self, path, input).await
    }

    async fn exec_with_builtins(
        &mut self,
        path: &str,
        input: Value,
        builtins: &Builtins,
    ) -> Result<Activation> {
        DurableProcess::exec_with_builtins(self, path, input, builtins).await
    }

    async fn enqueue_inbox(&mut self, value: Value) -> Result<crate::Change> {
        DurableProcess::enqueue_inbox(self, value).await
    }

    async fn acknowledge_inbox(&mut self, expected: &Value) -> Result<crate::Change> {
        DurableProcess::acknowledge_inbox(self, expected).await
    }

    async fn attach_mount(&mut self, name: String, mount: Mount) -> Result<crate::Change> {
        DurableProcess::attach_mount(self, name, mount)
    }

    async fn initialize_thread_state(&mut self, value: Value) -> Result<crate::Change> {
        DurableProcess::initialize_thread_state(self, value).await
    }

    async fn replace_thread_state(&mut self, value: Value) -> Result<crate::Change> {
        DurableProcess::replace_thread_state(self, value).await
    }

    async fn replace_builtins(&mut self, value: Value) -> Result<crate::Change> {
        DurableProcess::replace_builtins(self, value).await
    }

    async fn transactions(&self, query: TransactionQuery) -> Result<Vec<ProcessTransaction>> {
        DurableProcess::transactions(self, query).await
    }

    async fn snapshot(&self) -> Result<Self::Snapshot> {
        DurableProcess::snapshot(self).await
    }

    async fn cut(&mut self) -> Result<()> {
        DurableProcess::cut(self).await
    }

    async fn fork(&self, child: ProcessId) -> Result<Self> {
        DurableProcess::fork(self, child).await
    }
}

struct SvitRow {
    base_hash: String,
    covered_position: Option<u64>,
    head: TransactionHead,
    process_version: u64,
    root_hash: String,
}

impl SvitRow {
    fn target(&self) -> RestoreTarget {
        RestoreTarget {
            position: self.head.position,
            hash: self.head.hash.clone(),
            process_version: self.process_version,
            root_hash: self.root_hash.clone(),
        }
    }
}

struct RestoreTarget {
    position: Option<u64>,
    hash: String,
    process_version: u64,
    root_hash: String,
}

fn verify_target(process: Process, target: &RestoreTarget) -> Result<Process> {
    if process.version() != target.process_version || process.root_hash() != target.root_hash {
        return invalid("restored process does not match requested boundary");
    }
    Ok(process)
}

fn thread_session_id(process: &Process) -> Result<Option<SessionId>> {
    let Some(thread) = process.read("/thread")? else {
        return Ok(None);
    };
    let value = thread.to_json();
    let Some(session_id) = value.get("session_id").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    session_id
        .parse()
        .map(Some)
        .map_err(|_| Error::InvalidPersistence("thread session ID is invalid".into()))
}

async fn insert_blob(transaction: &Transaction<'_>, hash: &str, bytes: &[u8]) -> Result<()> {
    let mut rows = transaction
        .query("SELECT bytes FROM blobs WHERE hash = ?1", [hash])
        .await
        .map_err(store_error)?;
    if let Some(row) = rows.next().await.map_err(store_error)? {
        if blob(&row, 0)? != bytes {
            return invalid("content hash collision");
        }
        return Ok(());
    }
    drop(rows);
    transaction
        .execute(
            "INSERT INTO blobs (hash, bytes) VALUES (?1, ?2)",
            params![hash, bytes],
        )
        .await
        .map_err(store_error)?;
    Ok(())
}

async fn upsert_recovery_checkpoint(
    transaction: &Transaction<'_>,
    checkpoint: &RecoveryCheckpoint,
) -> Result<()> {
    let blob_hash = &checkpoint.process_snapshot_hash;
    let mut rows = transaction
        .query(
            "SELECT snapshot_blob_hash FROM recovery_checkpoints WHERE address = ?1",
            [checkpoint.address.as_str()],
        )
        .await
        .map_err(store_error)?;
    let previous_blob = rows
        .next()
        .await
        .map_err(store_error)?
        .map(|row| text(&row, 0))
        .transpose()?;
    drop(rows);

    insert_blob(transaction, blob_hash, &checkpoint.process_snapshot).await?;
    transaction
        .execute(
            "INSERT INTO recovery_checkpoints \
             (address, covered_position, head_hash, process_version, root_hash, snapshot_hash, snapshot_blob_hash) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(address) DO UPDATE SET \
               covered_position = excluded.covered_position, \
               head_hash = excluded.head_hash, \
               process_version = excluded.process_version, \
               root_hash = excluded.root_hash, \
               snapshot_hash = excluded.snapshot_hash, \
               snapshot_blob_hash = excluded.snapshot_blob_hash",
            params![
                checkpoint.address.as_str(),
                to_i64(checkpoint.covered_position)?,
                checkpoint.head_hash.as_str(),
                to_i64(checkpoint.process_version)?,
                checkpoint.root_hash.as_str(),
                checkpoint.snapshot_hash.as_str(),
                blob_hash.as_str(),
            ],
        )
        .await
        .map_err(store_error)?;
    if let Some(previous_blob) =
        previous_blob.filter(|previous| previous.as_str() != blob_hash.as_str())
    {
        delete_unreferenced_blob(transaction, &previous_blob).await?;
    }
    Ok(())
}

async fn remove_recovery_checkpoint(
    transaction: &Transaction<'_>,
    address: &ProcessId,
) -> Result<()> {
    let mut rows = transaction
        .query(
            "SELECT snapshot_blob_hash FROM recovery_checkpoints WHERE address = ?1",
            [address.as_str()],
        )
        .await
        .map_err(store_error)?;
    let blob_hash = rows
        .next()
        .await
        .map_err(store_error)?
        .map(|row| text(&row, 0))
        .transpose()?;
    drop(rows);
    transaction
        .execute(
            "DELETE FROM recovery_checkpoints WHERE address = ?1",
            [address.as_str()],
        )
        .await
        .map_err(store_error)?;
    if let Some(blob_hash) = blob_hash {
        delete_unreferenced_blob(transaction, &blob_hash).await?;
    }
    Ok(())
}

async fn delete_unreferenced_blob(transaction: &Transaction<'_>, hash: &str) -> Result<()> {
    transaction
        .execute(
            "DELETE FROM blobs WHERE hash = ?1 \
             AND NOT EXISTS (SELECT 1 FROM bases WHERE base_blob_hash = ?1) \
             AND NOT EXISTS (SELECT 1 FROM events WHERE event_blob_hash = ?1) \
             AND NOT EXISTS (SELECT 1 FROM thread_events WHERE event_blob_hash = ?1) \
             AND NOT EXISTS (SELECT 1 FROM thread_compaction_checkpoints WHERE payload_blob_hash = ?1) \
             AND NOT EXISTS (SELECT 1 FROM snapshots WHERE snapshot_blob_hash = ?1) \
             AND NOT EXISTS (SELECT 1 FROM recovery_checkpoints WHERE snapshot_blob_hash = ?1)",
            [hash],
        )
        .await
        .map_err(store_error)?;
    Ok(())
}

async fn insert_base(transaction: &Transaction<'_>, base: &StoredBase) -> Result<()> {
    base.validate()?;
    let bytes = encode(base)?;
    let blob_hash = bytes_hash(&bytes);
    insert_blob(transaction, &blob_hash, &bytes).await?;
    transaction
        .execute(
            "INSERT OR IGNORE INTO bases \
             (base_hash, address, kind, covered_position, process_version, root_hash, base_blob_hash) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                base.base_hash.as_str(),
                base.address.as_str(),
                base_kind(&base.origin),
                sql_position(base.covered_position)?,
                to_i64(base.process_version)?,
                base.root_hash.as_str(),
                blob_hash.as_str(),
            ],
        )
        .await
        .map_err(store_error)?;
    Ok(())
}

async fn require_head(
    transaction: &Transaction<'_>,
    address: &ProcessId,
    expected: &TransactionHead,
) -> Result<()> {
    let mut rows = transaction
        .query(
            "SELECT head_position, head_hash FROM svits WHERE address = ?1",
            [address.as_str()],
        )
        .await
        .map_err(store_error)?;
    let row = rows
        .next()
        .await
        .map_err(store_error)?
        .ok_or_else(|| Error::PersistenceNotFound(address.to_string()))?;
    if optional_u64(&row, 0)? != expected.position || text(&row, 1)? != expected.hash {
        return Err(Error::PersistenceConflict);
    }
    Ok(())
}

async fn update_base_head(
    transaction: &Transaction<'_>,
    process: &Process,
    expected: &TransactionHead,
    base: &StoredBase,
) -> Result<u64> {
    let expected_position = sql_position(expected.position)?;
    transaction
        .execute(
            "UPDATE svits SET base_hash = ?1, covered_position = ?2, head_hash = ?1 \
             WHERE address = ?3 AND head_hash = ?4 \
               AND ((head_position IS NULL AND ?5 IS NULL) OR head_position = ?6)",
            params![
                base.base_hash.as_str(),
                sql_position(base.covered_position)?,
                process.id().as_str(),
                expected.hash.as_str(),
                expected_position.clone(),
                expected_position,
            ],
        )
        .await
        .map_err(store_error)
}

fn validate_query_path(path: &str) -> Result<()> {
    if path == "/" {
        return Ok(());
    }
    let Some(path) = path.strip_prefix('/') else {
        return Err(Error::InvalidPath(path.into()));
    };
    if path.is_empty()
        || path
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(Error::InvalidPath(path.into()));
    }
    Ok(())
}

fn base_kind(origin: &BaseOrigin) -> &'static str {
    match origin {
        BaseOrigin::Created { .. } => "created",
        BaseOrigin::Imported { .. } => "imported",
        BaseOrigin::Fork { .. } => "fork",
        BaseOrigin::Snapshot { .. } => "snapshot",
    }
}

fn canonical_hash(value: &impl Serialize) -> Result<String> {
    Ok(bytes_hash(&encode(value)?))
}

fn bytes_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|_| Error::InvalidPersistence("canonical encoding failed".into()))
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8], kind: &str) -> Result<T> {
    if bytes.len() > MAX_TRANSACTION_BYTES && kind == "event" {
        return Err(Error::ResourceLimitExceeded("persistence event bytes"));
    }
    serde_json::from_slice(bytes)
        .map_err(|_| Error::InvalidPersistence(format!("{kind} decoding failed")))
}

fn decode_content_addressed<T: for<'de> Deserialize<'de>>(
    row: &Row,
    hash_index: usize,
    blob_index: usize,
    kind: &str,
) -> Result<T> {
    let expected_hash = text(row, hash_index)?;
    let bytes = blob(row, blob_index)?;
    if bytes_hash(&bytes) != expected_hash {
        return invalid("content-addressed blob hash mismatch");
    }
    decode(&bytes, kind)
}

fn sql_position(position: Option<u64>) -> Result<SqlValue> {
    position
        .map(to_i64)
        .transpose()
        .map(|position| position.map_or(SqlValue::Null, SqlValue::Integer))
}

fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::ResourceLimitExceeded("persistence integer"))
}

fn optional_u64(row: &Row, index: usize) -> Result<Option<u64>> {
    match row.get_value(index).map_err(store_error)? {
        SqlValue::Null => Ok(None),
        SqlValue::Integer(value) => u64::try_from(value)
            .map(Some)
            .map_err(|_| Error::InvalidPersistence("negative persisted integer".into())),
        _ => invalid("persisted position is not an integer"),
    }
}

fn u64_column(row: &Row, index: usize) -> Result<u64> {
    optional_u64(row, index)?
        .ok_or_else(|| Error::InvalidPersistence("persisted integer is null".into()))
}

fn text(row: &Row, index: usize) -> Result<String> {
    match row.get_value(index).map_err(store_error)? {
        SqlValue::Text(value) => Ok(value),
        _ => invalid("persisted column is not text"),
    }
}

fn blob(row: &Row, index: usize) -> Result<Vec<u8>> {
    match row.get_value(index).map_err(store_error)? {
        SqlValue::Blob(value) => Ok(value),
        _ => invalid("persisted column is not a blob"),
    }
}

fn invalid<T>(message: &str) -> Result<T> {
    Err(Error::InvalidPersistence(message.into()))
}

fn thread_event_backend() -> EventLogError {
    EventLogError::Backend {
        detail: "Svit thread event store is unavailable".into(),
    }
}

fn thread_event_corruption(_error: impl std::fmt::Display) -> EventLogError {
    EventLogError::Corruption {
        detail: "Svit thread event storage is corrupt".into(),
    }
}

fn conflict_or_store(error: turso::Error) -> Error {
    match error {
        turso::Error::Constraint(_) | turso::Error::Busy(_) | turso::Error::BusySnapshot(_) => {
            Error::PersistenceConflict
        }
        other => store_error(other),
    }
}

fn store_error(_error: turso::Error) -> Error {
    Error::PersistenceUnavailable
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::DurableProcessHandle;
    use crate::value;
    use everruns::core::Message;
    use everruns::core::events::{EventContext, InputMessageData};
    use everruns_host::EventReadLimit;

    #[test]
    fn store_snapshots_use_structured_json_and_restore_legacy_bytes() {
        let process = Process::builder("svit://local/persistence/snapshot-format")
            .unwrap()
            .memory("value", value!(42))
            .build()
            .unwrap();
        let head = TransactionHead::new(None, "0".repeat(64)).unwrap();
        let snapshot = StoredSnapshot::new(&process, &head).unwrap();
        assert_eq!(snapshot.snapshot_format, SNAPSHOT_FORMAT);
        assert!(snapshot.process_snapshot.is_object());
        assert_eq!(
            snapshot.restore_process().unwrap().root_hash(),
            process.root_hash()
        );

        let mut legacy = snapshot;
        let bytes = serde_json::to_vec(&legacy.process_snapshot).unwrap();
        legacy.snapshot_format = LEGACY_SNAPSHOT_FORMAT.into();
        legacy.process_snapshot = serde_json::Value::Array(
            bytes
                .into_iter()
                .map(|byte| serde_json::Value::from(u64::from(byte)))
                .collect(),
        );
        legacy.snapshot_hash = legacy.compute_hash().unwrap();
        legacy.validate_envelope().unwrap();
        assert_eq!(
            legacy.restore_process().unwrap().root_hash(),
            process.root_hash()
        );
    }

    #[tokio::test]
    // THREAT[TM-AUD-001] THREAT[TM-FORK-001]
    async fn durable_fork_keeps_only_the_inherited_history_checkpoint() {
        let store = TursoProcessStore::memory().await.unwrap();
        let session_id = SessionId::new();
        let mut process = Process::new("svit://local/persistence/thread-parent").unwrap();
        process
            .replace_thread_state(
                Value::from_json(serde_json::json!({
                    "format": "svit-thread@8",
                    "session_id": session_id.to_string(),
                    "process_id": "svit://local/persistence/thread-parent",
                    "instructions": null,
                    "system_prompt": "system",
                }))
                .unwrap(),
            )
            .unwrap();
        let parent = store.import(process).await.unwrap();
        let log = parent.event_log();
        log.append(EventRequest::new(
            session_id,
            EventContext::empty(),
            InputMessageData::new(Message::user("parent question")),
        ))
        .await
        .unwrap();
        let checkpoints = parent.compaction_checkpoint_store();
        let checkpoint = CompactionCheckpoint {
            id: EventId::new().uuid(),
            session_id,
            source_sequence: 1,
            provider_type: "test".into(),
            model: "test-model".into(),
            format_version: 1,
            payload: CompactionCheckpointPayload::Summary {
                text: "parent summary".into(),
            },
        };
        assert!(checkpoints.install(checkpoint.clone()).await.unwrap());

        let child = parent
            .fork("svit://local/persistence/thread-child")
            .await
            .unwrap();
        log.append(EventRequest::new(
            session_id,
            EventContext::empty(),
            InputMessageData::new(Message::user("later parent question")),
        ))
        .await
        .unwrap();
        assert!(
            checkpoints
                .install(CompactionCheckpoint {
                    id: EventId::new().uuid(),
                    source_sequence: 2,
                    payload: CompactionCheckpointPayload::Summary {
                        text: "later parent summary".into(),
                    },
                    ..checkpoint.clone()
                })
                .await
                .unwrap()
        );
        let page = child
            .event_log()
            .read_page(EventReadRequest::new(session_id, EventReadLimit::default()))
            .await
            .unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(
            page.events[0].id,
            log.read_page(EventReadRequest::new(session_id, EventReadLimit::default()))
                .await
                .unwrap()
                .events[0]
                .id
        );
        assert_eq!(
            child
                .compaction_checkpoint_store()
                .get_latest(session_id, "test", "test-model")
                .await
                .unwrap(),
            Some(checkpoint)
        );
    }

    #[tokio::test]
    // THREAT[TM-PERS-001]
    async fn corrupted_content_addressed_event_fails_closed_on_resume() {
        let store = TursoProcessStore::memory().await.unwrap();
        let process = Process::builder("svit://local/persistence/corrupt")
            .unwrap()
            .memory("value", value!(0))
            .build()
            .unwrap();
        let mut durable = store.create(process).await.unwrap();
        durable.write("/memory/value", value!(1)).await.unwrap();

        let connection = store.database.connect().unwrap();
        connection
            .execute(
                "UPDATE blobs SET bytes = x'00' WHERE hash = \
                 (SELECT event_blob_hash FROM events WHERE address = ?1 AND position = 0)",
                ["svit://local/persistence/corrupt"],
            )
            .await
            .unwrap();

        assert!(matches!(
            store.resume("svit://local/persistence/corrupt").await,
            Err(Error::InvalidPersistence(_))
        ));
    }

    #[tokio::test]
    async fn recovery_checkpoint_bounds_resume_replay() {
        let address = "svit://local/persistence/checkpoint";
        let store = TursoProcessStore::memory().await.unwrap();
        let process = Process::builder(address)
            .unwrap()
            .memory("value", value!(0))
            .build()
            .unwrap();
        let mut durable = store.create(process).await.unwrap();
        for value in 1..=64 {
            durable.write("/memory/value", value!(value)).await.unwrap();
        }
        durable.write("/memory/value", value!(65)).await.unwrap();

        let connection = store.database.connect().unwrap();
        connection
            .execute(
                "UPDATE blobs SET bytes = x'00' WHERE hash = \
                 (SELECT event_blob_hash FROM events WHERE address = ?1 AND position = 0)",
                [address],
            )
            .await
            .unwrap();

        let resumed = store.resume(address).await.unwrap();
        assert_eq!(resumed.read("/memory/value").unwrap(), Some(value!(65)));
    }

    #[tokio::test]
    async fn corrupted_recovery_checkpoint_fails_closed_on_resume() {
        let address = "svit://local/persistence/corrupt-checkpoint";
        let store = TursoProcessStore::memory().await.unwrap();
        let process = Process::builder(address)
            .unwrap()
            .memory("value", value!(0))
            .build()
            .unwrap();
        let mut durable = store.create(process).await.unwrap();
        for value in 1..=RECOVERY_CHECKPOINT_INTERVAL {
            durable.write("/memory/value", value!(value)).await.unwrap();
        }

        let connection = store.database.connect().unwrap();
        connection
            .execute(
                "UPDATE blobs SET bytes = x'00' WHERE hash = \
                 (SELECT snapshot_blob_hash FROM recovery_checkpoints WHERE address = ?1)",
                [address],
            )
            .await
            .unwrap();

        assert!(matches!(
            store.resume(address).await,
            Err(Error::InvalidPersistence(_))
        ));
    }

    #[tokio::test]
    async fn automatic_recovery_checkpoints_keep_only_the_latest_boundary() {
        let address = "svit://local/persistence/latest-checkpoint";
        let store = TursoProcessStore::memory().await.unwrap();
        let process = Process::builder(address).unwrap().build().unwrap();
        let mut durable = store.create(process).await.unwrap();
        for value in 1..=RECOVERY_CHECKPOINT_INTERVAL * 2 {
            durable.write("/memory/value", value!(value)).await.unwrap();
        }

        let connection = store.database.connect().unwrap();
        let mut rows = connection
            .query(
                "SELECT COUNT(*), c.covered_position, b.bytes \
                 FROM recovery_checkpoints c JOIN blobs b ON b.hash = c.snapshot_blob_hash \
                 WHERE c.address = ?1",
                [address],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(u64_column(&row, 0).unwrap(), 1);
        assert_eq!(u64_column(&row, 1).unwrap(), 63);
        let bytes = blob(&row, 2).unwrap();
        assert!(bytes.starts_with(br#"{"format":"#));
        assert!(!bytes.starts_with(br#"{"snapshot_format":"#));
    }
}
