use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Connection, Database, Row, Value as SqlValue, params};

use crate::persistence::{
    DurableProcessHandle, EventQuery, Mutation, PersistedEventRecord, PersistenceSnapshotRecord,
    ProcessStore, touched_paths,
};
use crate::{Activation, Error, Mount, Process, ProcessId, Result, Value};

const SCHEMA_VERSION: &str = "1";
const BASE_FORMAT: &str = "svit-base@1";
const EVENT_FORMAT: &str = "svit-transaction@1";
const SNAPSHOT_FORMAT: &str = "svit-store-snapshot@1";
const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_QUERY_EVENTS: u32 = 4096;
const MAX_QUERY_TEXT_BYTES: usize = 4096;
const MAX_REPLAY_EVENTS: usize = 100_000;
const MAX_FORK_DEPTH: usize = 64;

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

/// One validated retained transaction event returned by a store query.
#[derive(Clone, Debug, PartialEq)]
pub struct PersistedEvent(StoredEvent);

impl PersistedEvent {
    /// Returns the stable address-local event position.
    pub fn position(&self) -> u64 {
        self.0.position
    }

    /// Returns the process version produced by this event.
    pub fn process_version(&self) -> u64 {
        self.0.process_version_after
    }

    /// Returns the canonical paths derived from this event's mutations.
    pub fn touched_paths(&self) -> &[String] {
        &self.0.touched_paths
    }

    /// Returns descriptive, non-authoritative source metadata.
    pub fn source(&self) -> &str {
        &self.0.source
    }

    /// Returns the stored ordered mutations.
    pub fn mutations(&self) -> &[Mutation] {
        &self.0.mutations
    }

    /// Returns the event integrity hash.
    pub fn event_hash(&self) -> &str {
        &self.0.event_hash
    }
}

impl PersistedEventRecord for PersistedEvent {
    fn position(&self) -> u64 {
        PersistedEvent::position(self)
    }

    fn process_version(&self) -> u64 {
        PersistedEvent::process_version(self)
    }

    fn touched_paths(&self) -> &[String] {
        PersistedEvent::touched_paths(self)
    }

    fn source(&self) -> &str {
        PersistedEvent::source(self)
    }

    fn mutations(&self) -> &[Mutation] {
        PersistedEvent::mutations(self)
    }

    fn event_hash(&self) -> &str {
        PersistedEvent::event_hash(self)
    }
}

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Head {
    position: Option<u64>,
    hash: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredEvent {
    event_format: String,
    address: ProcessId,
    position: u64,
    previous_hash: String,
    process_version_before: u64,
    process_version_after: u64,
    mutations: Vec<Mutation>,
    touched_paths: Vec<String>,
    source: String,
    resulting_root_hash: String,
    event_hash: String,
}

#[derive(Serialize)]
struct EventHashMaterial<'a> {
    event_format: &'a str,
    address: &'a ProcessId,
    position: u64,
    previous_hash: &'a str,
    process_version_before: u64,
    process_version_after: u64,
    mutations: &'a [Mutation],
    touched_paths: &'a [String],
    source: &'a str,
    resulting_root_hash: &'a str,
}

impl StoredEvent {
    fn new(
        address: ProcessId,
        head: &Head,
        process_version_before: u64,
        process_version_after: u64,
        mutations: Vec<Mutation>,
        source: impl Into<String>,
        resulting_root_hash: String,
    ) -> Result<Self> {
        let position = match head.position {
            Some(position) => position
                .checked_add(1)
                .ok_or(Error::ResourceLimitExceeded("persistence event position"))?,
            None => 0,
        };
        let touched_paths = touched_paths(&mutations)?;
        let mut event = Self {
            event_format: EVENT_FORMAT.into(),
            address,
            position,
            previous_hash: head.hash.clone(),
            process_version_before,
            process_version_after,
            mutations,
            touched_paths,
            source: source.into(),
            resulting_root_hash,
            event_hash: String::new(),
        };
        event.event_hash = event.compute_hash()?;
        event.validate()?;
        Ok(event)
    }

    fn compute_hash(&self) -> Result<String> {
        canonical_hash(&EventHashMaterial {
            event_format: &self.event_format,
            address: &self.address,
            position: self.position,
            previous_hash: &self.previous_hash,
            process_version_before: self.process_version_before,
            process_version_after: self.process_version_after,
            mutations: &self.mutations,
            touched_paths: &self.touched_paths,
            source: &self.source,
            resulting_root_hash: &self.resulting_root_hash,
        })
    }

    fn validate(&self) -> Result<()> {
        if self.event_format != EVENT_FORMAT {
            return invalid("unsupported event format");
        }
        if self.source.is_empty() || self.source.len() > MAX_QUERY_TEXT_BYTES {
            return invalid("event source is invalid");
        }
        if self.process_version_before.checked_add(1) != Some(self.process_version_after) {
            return invalid("event version transition is invalid");
        }
        if self.touched_paths != touched_paths(&self.mutations)? {
            return invalid("event touched paths do not match mutations");
        }
        if self.event_hash != self.compute_hash()? {
            return invalid("event hash mismatch");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case", deny_unknown_fields)]
enum BaseOrigin {
    Created { process_snapshot: Vec<u8> },
    Fork { parent: ForkBoundary },
    Snapshot { snapshot_hash: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForkBoundary {
    parent_address: ProcessId,
    parent_head: Head,
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
    process_snapshot: Vec<u8>,
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
    process_snapshot: &'a [u8],
    root_hash: &'a str,
}

impl StoredSnapshot {
    fn new(process: &Process, head: &Head) -> Result<Self> {
        let mut snapshot = Self {
            snapshot_format: SNAPSHOT_FORMAT.into(),
            address: process.id().clone(),
            covered_position: head.position,
            head_hash: head.hash.clone(),
            process_version: process.version(),
            process_snapshot: process.snapshot()?,
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

    fn validate(&self) -> Result<()> {
        if self.snapshot_format != SNAPSHOT_FORMAT || self.snapshot_hash != self.compute_hash()? {
            return invalid("store snapshot hash or format is invalid");
        }
        let process = Process::restore(&self.process_snapshot)?;
        if process.id() != &self.address
            || process.version() != self.process_version
            || process.root_hash() != self.root_hash
        {
            return invalid("store snapshot metadata does not match process state");
        }
        Ok(())
    }
}

#[derive(Clone)]
/// Address-partitioned event storage backed by one local Turso database.
pub struct TursoProcessStore {
    database: Database,
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
        let store = Self { database };
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
        let address = process.id().clone();
        let base = StoredBase::new(
            address.clone(),
            None,
            None,
            process.version(),
            process.root_hash(),
            BaseOrigin::Created {
                process_snapshot: process.snapshot()?,
            },
        )?;
        let head = Head {
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

    /// Restores one process by address from its base and retained event tail.
    pub async fn resume(&self, address: impl Into<String>) -> Result<DurableProcess> {
        let address = ProcessId::new(address)?;
        let row = self.load_svit_row(&address).await?;
        let process = self.restore_at(address.clone(), row.target(), 0).await?;
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
        if text(&row, 0)? != SCHEMA_VERSION {
            return invalid("unsupported persistence schema version");
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

    async fn append(&self, expected: &Head, event: &StoredEvent) -> Result<Head> {
        event.validate()?;
        let event_bytes = encode(event)?;
        if event_bytes.len() > MAX_EVENT_BYTES {
            return Err(Error::ResourceLimitExceeded("persistence event bytes"));
        }
        let event_blob_hash = bytes_hash(&event_bytes);
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
                    event.event_hash.as_str(),
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
                    event.event_hash.as_str(),
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
        transaction.commit().await.map_err(store_error)?;
        Ok(Head {
            position: Some(event.position),
            hash: event.event_hash.clone(),
        })
    }

    async fn query(&self, address: &ProcessId, query: EventQuery) -> Result<Vec<PersistedEvent>> {
        // THREAT[TM-DOS-009]: Query text and result counts are bounded before
        // constructing a database result set.
        if query.limit == 0 || query.limit > MAX_QUERY_EVENTS {
            return Err(Error::ResourceLimitExceeded("persistence query events"));
        }
        if let Some(path) = query.path_prefix.as_deref() {
            validate_query_path(path)?;
        }
        for text in [query.source.as_deref(), query.event_hash.as_deref()]
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
        if let Some(hash) = query.event_hash {
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
            let event: StoredEvent = decode_content_addressed(&row, 0, 1, "event")?;
            event.validate()?;
            if event.address != *address
                || event.position != u64_column(&row, 2)?
                || event.event_hash != text(&row, 3)?
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
            events.push(PersistedEvent(event));
        }
        Ok(events)
    }

    async fn persist_snapshot(
        &self,
        process: &Process,
        expected: &Head,
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

    async fn cut(&self, process: &Process, expected: &Head) -> Result<Head> {
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
        transaction.commit().await.map_err(store_error)?;
        Ok(Head {
            position: expected.position,
            hash: base.base_hash,
        })
    }

    async fn create_fork(
        &self,
        parent: &Process,
        parent_head: &Head,
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
        let child_head = Head {
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
                BaseOrigin::Created { process_snapshot } => Process::restore(process_snapshot)?,
                BaseOrigin::Snapshot { snapshot_hash } => {
                    let snapshot = self.load_snapshot(snapshot_hash).await?;
                    snapshot.validate()?;
                    Process::restore(&snapshot.process_snapshot)?
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

            let start = base.covered_position.map_or(0, |position| position + 1);
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
            let mut expected_position = start;
            let mut expected_hash = base.base_hash.clone();
            let mut count = 0usize;
            while let Some(row) = rows.next().await.map_err(store_error)? {
                count += 1;
                if count > MAX_REPLAY_EVENTS {
                    return Err(Error::ResourceLimitExceeded("persistence replay events"));
                }
                let event: StoredEvent = decode_content_addressed(&row, 0, 1, "event")?;
                // THREAT[TM-PERS-001]: Treat every stored event as untrusted;
                // verify its hash chain, typed reducer, and resulting root.
                event.validate()?;
                if event.address != address
                    || event.position != expected_position
                    || event.previous_hash != expected_hash
                    || event.process_version_before != process.version()
                {
                    return invalid("event sequence or predecessor is invalid");
                }
                process.apply_persisted_mutations(
                    event.process_version_before,
                    event.process_version_after,
                    &event.mutations,
                )?;
                if process.root_hash() != event.resulting_root_hash {
                    return invalid("event reducer root hash mismatch");
                }
                expected_position += 1;
                expected_hash = event.event_hash;
            }
            if expected_position != end + 1 || expected_hash != target.hash {
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
            head: Head {
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
}

/// One in-memory process whose commits are durably serialized by Turso.
pub struct DurableProcess {
    store: TursoProcessStore,
    process: Process,
    head: Head,
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

    /// Reattaches one mount provider to a resumed durable process.
    ///
    /// Providers are never persisted, so a resumed process resolves nothing
    /// below its mount descriptors until the host attaches them again. The
    /// mount's identity must match the committed descriptor: changing it is a
    /// state transition, which a durable process only accepts through a
    /// recorded write.
    pub fn attach_mount(&mut self, name: impl Into<String>, mount: Mount) -> Result<()> {
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
        self.process.attach_mount(name, mount).map(|_| ())
    }

    /// Returns the current committed root hash.
    pub fn root_hash(&self) -> String {
        self.process.root_hash()
    }

    /// Durably commits one host write as a uniform transaction event.
    ///
    /// The transition itself reports the mutations it applied, so the stored
    /// event and a live observer always describe the same change.
    pub async fn write(&mut self, path: &str, value: Value) -> Result<()> {
        let mut candidate = self.process.clone();
        let change = candidate.write(path, value)?;
        self.commit_change(candidate, change, "host.write").await
    }

    /// Durably commits one host removal as a uniform transaction event.
    pub async fn remove(&mut self, path: &str) -> Result<()> {
        let mut candidate = self.process.clone();
        let change = candidate.remove(path)?;
        self.commit_change(candidate, change, "host.remove").await
    }

    /// Durably appends one host-supplied value to the process inbox.
    pub async fn enqueue_inbox(&mut self, value: Value) -> Result<()> {
        let mut candidate = self.process.clone();
        let change = candidate.enqueue_inbox(value)?;
        self.commit_change(candidate, change, "inbox.enqueue").await
    }

    /// Durably removes the exact inbox head observed by a completed turn.
    pub async fn acknowledge_inbox(&mut self, expected: &Value) -> Result<()> {
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
    ) -> Result<()> {
        if change.mutations().is_empty() {
            self.process = candidate;
            return Ok(());
        }
        self.commit_candidate(candidate, change.mutations().to_vec(), source)
            .await
    }

    /// Executes guest Lisp and durably publishes its exact committed write set.
    pub async fn exec(&mut self, path: &str, input: Value) -> Result<Activation> {
        let prepared = self.process.prepare_exec(path, input)?;
        let event = StoredEvent::new(
            self.process.id().clone(),
            &self.head,
            self.process.version(),
            prepared.candidate().version(),
            prepared.mutations().to_vec(),
            "activation",
            prepared.candidate().root_hash(),
        )?;
        match self.store.append(&self.head, &event).await {
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
    pub async fn query(&self, query: EventQuery) -> Result<Vec<PersistedEvent>> {
        self.store.query(self.process.id(), query).await
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
        let event = StoredEvent::new(
            self.process.id().clone(),
            &self.head,
            self.process.version(),
            candidate.version(),
            mutations,
            source,
            candidate.root_hash(),
        )?;
        let head = self.store.append(&self.head, &event).await?;
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

    async fn resume(&self, address: &ProcessId) -> Result<Self::Handle> {
        TursoProcessStore::resume(self, address.to_string()).await
    }
}

#[async_trait]
impl DurableProcessHandle for DurableProcess {
    type Event = PersistedEvent;
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

    async fn write(&mut self, path: &str, value: Value) -> Result<()> {
        DurableProcess::write(self, path, value).await
    }

    async fn remove(&mut self, path: &str) -> Result<()> {
        DurableProcess::remove(self, path).await
    }

    async fn exec(&mut self, path: &str, input: Value) -> Result<Activation> {
        DurableProcess::exec(self, path, input).await
    }

    async fn enqueue_inbox(&mut self, value: Value) -> Result<()> {
        DurableProcess::enqueue_inbox(self, value).await
    }

    async fn acknowledge_inbox(&mut self, expected: &Value) -> Result<()> {
        DurableProcess::acknowledge_inbox(self, expected).await
    }

    async fn query(&self, query: EventQuery) -> Result<Vec<Self::Event>> {
        DurableProcess::query(self, query).await
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
    head: Head,
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
    expected: &Head,
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
    expected: &Head,
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
    if bytes.len() > MAX_EVENT_BYTES && kind == "event" {
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
    use crate::value;

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
}
