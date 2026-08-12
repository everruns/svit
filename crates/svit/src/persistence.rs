//! Adapter-neutral contracts for durable process persistence.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{Activation, Process, ProcessId, Result, Value};

/// One ordered process-tree operation stored in a Svit transaction event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Mutation {
    /// Creates or replaces one value at an absolute process path.
    Set { path: String, value: Value },
    /// Removes one value at an absolute process path.
    Remove { path: String },
    /// Appends values to an array at an absolute process path.
    Append { path: String, values: Vec<Value> },
    /// Removes the exact expected first array value.
    RemoveFront {
        path: String,
        expected_value_hash: String,
    },
}

impl Mutation {
    #[cfg(feature = "persistence-turso")]
    pub(crate) fn path(&self) -> &str {
        match self {
            Self::Set { path, .. }
            | Self::Remove { path }
            | Self::Append { path, .. }
            | Self::RemoveFront { path, .. } => path,
        }
    }
}

/// Bounded filters for retained transaction events.
#[derive(Clone, Debug)]
pub struct EventQuery {
    pub(crate) after_position: Option<u64>,
    pub(crate) through_position: Option<u64>,
    pub(crate) path_prefix: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) process_version_from: Option<u64>,
    pub(crate) process_version_through: Option<u64>,
    pub(crate) event_hash: Option<String>,
    pub(crate) limit: u32,
}

impl Default for EventQuery {
    fn default() -> Self {
        Self::new()
    }
}

impl EventQuery {
    /// Creates a query over all retained events with the default result limit.
    pub fn new() -> Self {
        Self {
            after_position: None,
            through_position: None,
            path_prefix: None,
            source: None,
            process_version_from: None,
            process_version_through: None,
            event_hash: None,
            limit: 1024,
        }
    }

    /// Selects events strictly after this stable position.
    pub fn after_position(mut self, position: u64) -> Self {
        self.after_position = Some(position);
        self
    }

    /// Selects events at or before this stable position.
    pub fn through_position(mut self, position: u64) -> Self {
        self.through_position = Some(position);
        self
    }

    /// Selects events touching this absolute path or one of its descendants.
    pub fn path_prefix(mut self, path: impl Into<String>) -> Self {
        self.path_prefix = Some(path.into());
        self
    }

    /// Selects descriptive event-source metadata.
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Selects events producing this process version or a later one.
    pub fn process_version_from(mut self, version: u64) -> Self {
        self.process_version_from = Some(version);
        self
    }

    /// Selects events producing this process version or an earlier one.
    pub fn process_version_through(mut self, version: u64) -> Self {
        self.process_version_through = Some(version);
        self
    }

    /// Selects one event by its integrity hash.
    pub fn event_hash(mut self, hash: impl Into<String>) -> Self {
        self.event_hash = Some(hash.into());
        self
    }

    /// Replaces the bounded maximum number of returned events.
    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    /// Returns the exclusive lower event-position bound.
    pub fn after(&self) -> Option<u64> {
        self.after_position
    }

    /// Returns the inclusive upper event-position bound.
    pub fn through(&self) -> Option<u64> {
        self.through_position
    }

    /// Returns the selected absolute touched-path prefix.
    pub fn path(&self) -> Option<&str> {
        self.path_prefix.as_deref()
    }

    /// Returns the selected exact source metadata.
    pub fn source_filter(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// Returns the inclusive lower process-version bound.
    pub fn version_from(&self) -> Option<u64> {
        self.process_version_from
    }

    /// Returns the inclusive upper process-version bound.
    pub fn version_through(&self) -> Option<u64> {
        self.process_version_through
    }

    /// Returns the selected exact event hash.
    pub fn hash(&self) -> Option<&str> {
        self.event_hash.as_deref()
    }

    /// Returns the requested maximum event count.
    pub fn max_events(&self) -> u32 {
        self.limit
    }
}

/// Creates and resumes durable process handles without exposing adapter internals.
#[async_trait]
pub trait ProcessStore: Send + Sync {
    /// Durable process handle returned by this adapter.
    type Handle: DurableProcessHandle;

    /// Creates a new address-bound process and fails if that address exists.
    async fn create(&self, process: Process) -> Result<Self::Handle>;

    /// Resumes one existing process by its exact logical address.
    async fn resume(&self, address: &ProcessId) -> Result<Self::Handle>;
}

/// Adapter-neutral read surface for one retained transaction event.
pub trait PersistedEventRecord: Send + Sync {
    /// Returns the stable address-local event position.
    fn position(&self) -> u64;
    /// Returns the process version produced by this event.
    fn process_version(&self) -> u64;
    /// Returns the canonical paths derived from the event mutations.
    fn touched_paths(&self) -> &[String];
    /// Returns descriptive, non-authoritative source metadata.
    fn source(&self) -> &str;
    /// Returns the stored ordered mutations.
    fn mutations(&self) -> &[Mutation];
    /// Returns the event integrity hash.
    fn event_hash(&self) -> &str;
}

/// Adapter-neutral read surface for one on-demand persistence snapshot.
pub trait PersistenceSnapshotRecord: Send + Sync {
    /// Returns the content hash of this store snapshot.
    fn snapshot_hash(&self) -> &str;
    /// Returns the process version captured by this snapshot.
    fn process_version(&self) -> u64;
    /// Returns the process root hash captured by this snapshot.
    fn root_hash(&self) -> &str;
}

/// Adapter-neutral operations supported by one durable process handle.
#[async_trait]
pub trait DurableProcessHandle: Sized + Send + Sync {
    /// Event value returned by retained-history queries.
    type Event: PersistedEventRecord;
    /// Snapshot descriptor returned by on-demand persistence snapshots.
    type Snapshot: PersistenceSnapshotRecord;

    /// Returns the process address.
    fn id(&self) -> &ProcessId;
    /// Returns the committed process version.
    fn version(&self) -> u64;
    /// Reads one process value, resolving mount nodes lazily.
    fn read(&self, path: &str) -> Result<Option<Value>>;
    /// Returns the current committed root hash.
    fn root_hash(&self) -> String;
    /// Durably commits one host write.
    async fn write(&mut self, path: &str, value: Value) -> Result<()>;
    /// Durably commits one host removal.
    async fn remove(&mut self, path: &str) -> Result<()>;
    /// Executes guest Lisp and durably commits its successful transition.
    async fn exec(&mut self, path: &str, input: Value) -> Result<Activation>;
    /// Durably appends one host-supplied inbox value.
    async fn enqueue_inbox(&mut self, value: Value) -> Result<()>;
    /// Durably removes the exact expected inbox head.
    async fn acknowledge_inbox(&mut self, expected: &Value) -> Result<()>;
    /// Queries retained transaction events.
    async fn query(&self, query: EventQuery) -> Result<Vec<Self::Event>>;
    /// Persists an on-demand process snapshot at the current event boundary.
    async fn snapshot(&self) -> Result<Self::Snapshot>;
    /// Replaces retained history with a snapshot base when references permit it.
    async fn cut(&mut self) -> Result<()>;
    /// Creates a child referencing this exact committed boundary.
    async fn fork(&self, child: ProcessId) -> Result<Self>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_query_filters_are_visible_to_adapter_implementations() {
        let query = EventQuery::new()
            .after_position(2)
            .through_position(9)
            .path_prefix("/memory")
            .source("activation")
            .process_version_from(3)
            .process_version_through(8)
            .event_hash("abc")
            .limit(17);

        assert_eq!(query.after(), Some(2));
        assert_eq!(query.through(), Some(9));
        assert_eq!(query.path(), Some("/memory"));
        assert_eq!(query.source_filter(), Some("activation"));
        assert_eq!(query.version_from(), Some(3));
        assert_eq!(query.version_through(), Some(8));
        assert_eq!(query.hash(), Some("abc"));
        assert_eq!(query.max_events(), 17);
    }
}
