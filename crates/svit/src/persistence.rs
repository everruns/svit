//! Adapter-neutral contracts for durable process persistence.

use async_trait::async_trait;
use everruns_core::CompactionCheckpointStore;
use everruns_core::events::Event;
use everruns_host::EventLog;
use everruns_provider::typed_id::SessionId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::{Activation, Change, Mount, Ports, Process, ProcessId, Result, Value};

const TRANSACTION_FORMAT: &str = "svit-transaction@1";
const MAX_TRANSACTION_METADATA_BYTES: usize = 4096;
const MAX_TRANSACTION_BYTES: usize = 16 * 1024 * 1024;

/// One ordered process-tree operation stored in a Svit process transaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Mutation {
    /// Creates or replaces one value at an absolute process path.
    Set {
        /// Absolute path to create or replace.
        path: String,
        /// Persistent replacement value.
        value: Value,
    },
    /// Removes one value at an absolute process path.
    Remove {
        /// Absolute path to remove.
        path: String,
    },
    /// Appends values to an array at an absolute process path.
    Append {
        /// Absolute array path to extend.
        path: String,
        /// Persistent values appended in order.
        values: Vec<Value>,
    },
    /// Removes the exact expected first array value.
    RemoveFront {
        /// Absolute array path to update.
        path: String,
        /// Hash that must match the current first value.
        expected_value_hash: String,
    },
}

impl Mutation {
    /// Returns the absolute process path this operation changes.
    pub fn path(&self) -> &str {
        match self {
            Self::Set { path, .. }
            | Self::Remove { path }
            | Self::Append { path, .. }
            | Self::RemoveFront { path, .. } => path,
        }
    }
}

/// Folds ordered mutations into the canonical set of changed paths.
///
/// Live observers and the durable event index derive their paths here, so a
/// subscriber and a stored event always agree on what a transition touched.
pub(crate) fn touched_paths(mutations: &[Mutation]) -> Result<Vec<String>> {
    let mut paths = std::collections::BTreeSet::new();
    for mutation in mutations {
        let path = mutation.path();
        validate_change_path(path)?;
        paths.insert(path.to_owned());
    }
    Ok(paths.into_iter().collect())
}

pub(crate) fn validate_change_path(path: &str) -> Result<()> {
    let Some(remainder) = path.strip_prefix('/') else {
        return Err(crate::Error::InvalidPath(path.into()));
    };
    if remainder.is_empty()
        || remainder
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(crate::Error::InvalidPath(path.into()));
    }
    Ok(())
}

/// Content-addressed head of one process transaction stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransactionHead {
    pub(crate) position: Option<u64>,
    pub(crate) hash: String,
}

impl TransactionHead {
    /// Creates a validated stream head from adapter-owned metadata.
    pub fn new(position: Option<u64>, hash: impl Into<String>) -> Result<Self> {
        let hash = hash.into();
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(crate::Error::InvalidPersistence(
                "transaction head hash is invalid".into(),
            ));
        }
        Ok(Self { position, hash })
    }

    /// Returns the last transaction position, or `None` at the base boundary.
    pub fn position(&self) -> Option<u64> {
        self.position
    }

    /// Returns the base or last-transaction integrity hash.
    pub fn hash(&self) -> &str {
        &self.hash
    }
}

/// One canonical transition in the sole durable stream for a process.
///
/// Process-state changes occur inside this envelope as typed memory-tree
/// mutations. Canonical reasoning events use the separate paged `EventLog`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessTransaction {
    #[serde(rename = "event_format")]
    pub(crate) transaction_format: String,
    pub(crate) address: ProcessId,
    pub(crate) position: u64,
    pub(crate) previous_hash: String,
    pub(crate) process_version_before: u64,
    pub(crate) process_version_after: u64,
    pub(crate) mutations: Vec<Mutation>,
    pub(crate) touched_paths: Vec<String>,
    pub(crate) source: String,
    pub(crate) resulting_root_hash: String,
    #[serde(rename = "event_hash")]
    pub(crate) transaction_hash: String,
}

#[derive(Serialize)]
struct TransactionHashMaterial<'a> {
    #[serde(rename = "event_format")]
    transaction_format: &'a str,
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

impl ProcessTransaction {
    /// Builds and validates the next transaction after `head`.
    pub fn new(
        address: ProcessId,
        head: &TransactionHead,
        process_version_before: u64,
        process_version_after: u64,
        mutations: Vec<Mutation>,
        source: impl Into<String>,
        resulting_root_hash: String,
    ) -> Result<Self> {
        let position = match head.position {
            Some(position) => {
                position
                    .checked_add(1)
                    .ok_or(crate::Error::ResourceLimitExceeded(
                        "persistence transaction position",
                    ))?
            }
            None => 0,
        };
        let touched_paths = touched_paths(&mutations)?;
        let mut transaction = Self {
            transaction_format: TRANSACTION_FORMAT.into(),
            address,
            position,
            previous_hash: head.hash.clone(),
            process_version_before,
            process_version_after,
            mutations,
            touched_paths,
            source: source.into(),
            resulting_root_hash,
            transaction_hash: String::new(),
        };
        transaction.transaction_hash = transaction.compute_hash()?;
        transaction.validate()?;
        Ok(transaction)
    }

    /// Decodes and validates a canonical transaction envelope.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_TRANSACTION_BYTES {
            return Err(crate::Error::ResourceLimitExceeded(
                "persistence transaction bytes",
            ));
        }
        let transaction: Self = serde_json::from_slice(bytes).map_err(|_| {
            crate::Error::InvalidPersistence("transaction encoding is invalid".into())
        })?;
        transaction.validate()?;
        Ok(transaction)
    }

    /// Encodes this validated transaction for content-addressed storage.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)
            .map_err(|_| crate::Error::InvalidPersistence("transaction encoding failed".into()))?;
        if bytes.len() > MAX_TRANSACTION_BYTES {
            return Err(crate::Error::ResourceLimitExceeded(
                "persistence transaction bytes",
            ));
        }
        Ok(bytes)
    }

    /// Applies this transaction through Svit's canonical reducer.
    pub fn replay(&self, process: &mut Process) -> Result<()> {
        self.validate()?;
        if process.id() != &self.address {
            return Err(crate::Error::InvalidPersistence(
                "transaction address does not match process".into(),
            ));
        }
        let mut candidate = process.clone();
        candidate.apply_persisted_mutations(
            self.process_version_before,
            self.process_version_after,
            &self.mutations,
        )?;
        if candidate.root_hash() != self.resulting_root_hash {
            return Err(crate::Error::InvalidPersistence(
                "transaction reducer root hash mismatch".into(),
            ));
        }
        *process = candidate;
        Ok(())
    }

    /// Applies one event to a reconstruction that cannot be observed until
    /// the store has validated its requested recovery boundary.
    #[cfg(feature = "persistence-turso")]
    pub(crate) fn replay_unpublished(&self, process: &mut Process) -> Result<()> {
        self.validate()?;
        if process.id() != &self.address {
            return Err(crate::Error::InvalidPersistence(
                "transaction address does not match process".into(),
            ));
        }
        process.apply_persisted_mutations_for_recovery(
            self.process_version_before,
            self.process_version_after,
            &self.mutations,
        )
    }

    /// Verifies that this transaction is the unique next record after `head`.
    pub fn validate_successor(&self, head: &TransactionHead) -> Result<()> {
        self.validate()?;
        let expected_position = match head.position {
            Some(position) => {
                position
                    .checked_add(1)
                    .ok_or(crate::Error::ResourceLimitExceeded(
                        "persistence transaction position",
                    ))?
            }
            None => 0,
        };
        if self.position != expected_position || self.previous_hash != head.hash {
            return Err(crate::Error::InvalidPersistence(
                "transaction sequence or predecessor is invalid".into(),
            ));
        }
        Ok(())
    }

    /// Returns the process address.
    pub fn address(&self) -> &ProcessId {
        &self.address
    }
    /// Returns the stable address-local transaction position.
    pub fn position(&self) -> u64 {
        self.position
    }
    /// Returns the process version produced by this transaction.
    pub fn process_version(&self) -> u64 {
        self.process_version_after
    }
    /// Returns the canonical paths derived from the mutations.
    pub fn touched_paths(&self) -> &[String] {
        &self.touched_paths
    }
    /// Returns descriptive, non-authoritative source metadata.
    pub fn source(&self) -> &str {
        &self.source
    }
    /// Returns the stored ordered mutations.
    pub fn mutations(&self) -> &[Mutation] {
        &self.mutations
    }
    /// Returns the transaction integrity hash.
    pub fn transaction_hash(&self) -> &str {
        &self.transaction_hash
    }
    /// Returns the preceding base or transaction hash.
    pub fn previous_hash(&self) -> &str {
        &self.previous_hash
    }
    /// Returns the process version expected before replay.
    pub fn process_version_before(&self) -> u64 {
        self.process_version_before
    }
    /// Returns the resulting process root hash.
    pub fn resulting_root_hash(&self) -> &str {
        &self.resulting_root_hash
    }

    fn compute_hash(&self) -> Result<String> {
        let bytes = serde_json::to_vec(&TransactionHashMaterial {
            transaction_format: &self.transaction_format,
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
        .map_err(|_| crate::Error::InvalidPersistence("transaction hash encoding failed".into()))?;
        Ok(Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    }

    /// Validates the envelope, mutation projection, version step, and hash.
    pub fn validate(&self) -> Result<()> {
        if self.transaction_format != TRANSACTION_FORMAT {
            return Err(crate::Error::InvalidPersistence(
                "unsupported transaction format".into(),
            ));
        }
        if self.source.is_empty() || self.source.len() > MAX_TRANSACTION_METADATA_BYTES {
            return Err(crate::Error::InvalidPersistence(
                "transaction source is invalid".into(),
            ));
        }
        if self.process_version_before.checked_add(1) != Some(self.process_version_after) {
            return Err(crate::Error::InvalidPersistence(
                "transaction version step is invalid".into(),
            ));
        }
        if self.touched_paths != touched_paths(&self.mutations)? {
            return Err(crate::Error::InvalidPersistence(
                "transaction touched paths do not match mutations".into(),
            ));
        }
        if self.transaction_hash != self.compute_hash()? {
            return Err(crate::Error::InvalidPersistence(
                "transaction hash mismatch".into(),
            ));
        }
        Ok(())
    }
}

/// Bounded filters for retained process transactions.
#[derive(Clone, Debug)]
pub struct TransactionQuery {
    pub(crate) after_position: Option<u64>,
    pub(crate) through_position: Option<u64>,
    pub(crate) path_prefix: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) process_version_from: Option<u64>,
    pub(crate) process_version_through: Option<u64>,
    pub(crate) transaction_hash: Option<String>,
    pub(crate) limit: u32,
}

impl Default for TransactionQuery {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionQuery {
    /// Creates a query over all retained transactions with the default result limit.
    pub fn new() -> Self {
        Self {
            after_position: None,
            through_position: None,
            path_prefix: None,
            source: None,
            process_version_from: None,
            process_version_through: None,
            transaction_hash: None,
            limit: 1024,
        }
    }

    /// Selects transactions strictly after this stable position.
    pub fn after_position(mut self, position: u64) -> Self {
        self.after_position = Some(position);
        self
    }

    /// Selects transactions at or before this stable position.
    pub fn through_position(mut self, position: u64) -> Self {
        self.through_position = Some(position);
        self
    }

    /// Selects transactions touching this absolute path or one of its descendants.
    pub fn path_prefix(mut self, path: impl Into<String>) -> Self {
        self.path_prefix = Some(path.into());
        self
    }

    /// Selects descriptive transaction-source metadata.
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Selects transactions producing this process version or a later one.
    pub fn process_version_from(mut self, version: u64) -> Self {
        self.process_version_from = Some(version);
        self
    }

    /// Selects transactions producing this process version or an earlier one.
    pub fn process_version_through(mut self, version: u64) -> Self {
        self.process_version_through = Some(version);
        self
    }

    /// Selects one process transaction by its integrity hash.
    pub fn transaction_hash(mut self, hash: impl Into<String>) -> Self {
        self.transaction_hash = Some(hash.into());
        self
    }

    /// Replaces the bounded maximum number of returned transactions.
    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    /// Returns the exclusive lower transaction-position bound.
    pub fn after(&self) -> Option<u64> {
        self.after_position
    }

    /// Returns the inclusive upper transaction-position bound.
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

    /// Returns the selected exact transaction hash.
    pub fn hash(&self) -> Option<&str> {
        self.transaction_hash.as_deref()
    }

    /// Returns the requested maximum transaction count.
    pub fn max_transactions(&self) -> u32 {
        self.limit
    }
}

/// Outcome of one canonical event-history retention cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadHistoryCut {
    removed_events: u64,
    retained_from: i32,
}

impl ThreadHistoryCut {
    /// Creates one retention outcome from adapter-owned counters.
    pub fn new(removed_events: u64, retained_from: i32) -> Self {
        Self {
            removed_events,
            retained_from,
        }
    }

    /// Returns how many stored events this cut removed.
    pub fn removed_events(&self) -> u64 {
        self.removed_events
    }

    /// Returns the highest sequence this session no longer retains.
    ///
    /// Reads start after this boundary and appended sequences never reuse it.
    pub fn retained_from(&self) -> i32 {
        self.retained_from
    }
}

/// Reclaims a summarized prefix of one session's canonical event history.
///
/// Canonical events are append-only, so a long-lived Svit grows without bound
/// unless the host reclaims history it no longer needs. A cut is a storage
/// lifecycle operation like [`DurableProcessHandle::cut`]: it changes retention
/// only, never process version, root hash, or event sequence allocation.
#[async_trait]
pub trait ThreadHistoryRetention: Send + Sync {
    /// Returns the highest sequence this session no longer retains.
    ///
    /// Zero means the complete history is retained.
    async fn retained_from(&self, session_id: SessionId) -> Result<i32>;

    /// Returns the highest sequence a compaction checkpoint already replaced.
    ///
    /// Zero means no checkpoint covers this session, so no prefix is safe to
    /// reclaim.
    async fn compacted_through(&self, session_id: SessionId) -> Result<i32>;

    /// Removes every retained event at or below `through_sequence`.
    ///
    /// Implementations must fail closed:
    ///
    /// - [`crate::Error::ThreadHistoryBoundary`] when the boundary is not a
    ///   positive sequence at or below the committed high watermark;
    /// - [`crate::Error::ThreadHistoryUncompacted`] when no compaction
    ///   checkpoint covers the boundary, because the loop would silently lose
    ///   context it can no longer reconstruct;
    /// - [`crate::Error::ThreadHistoryPinned`] when a fork inherits the prefix.
    ///
    /// Cutting a boundary already reclaimed is idempotent and removes nothing.
    async fn cut_thread_events(
        &self,
        session_id: SessionId,
        through_sequence: i32,
    ) -> Result<ThreadHistoryCut>;
}

/// Creates and resumes durable process handles without exposing adapter internals.
#[async_trait]
pub trait ProcessStore: Send + Sync {
    /// Durable process handle returned by this adapter.
    type Handle: DurableProcessHandle;

    /// Creates a new address-bound process and fails if that address exists.
    async fn create(&self, process: Process) -> Result<Self::Handle>;

    /// Imports one current process boundary and starts a new retained-history tail.
    async fn import(&self, process: Process) -> Result<Self::Handle>;

    /// Resumes one existing process by its exact logical address.
    async fn resume(&self, address: &ProcessId) -> Result<Self::Handle>;
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
    /// Returns an owned committed read projection with current runtime mounts.
    fn process_projection(&self) -> Process;
    /// Returns the paged canonical reasoning event log paired with this process.
    ///
    /// The log is durable process infrastructure, not a node in the memory
    /// tree. Implementations must partition it by the same process identity.
    fn event_log(&self) -> Arc<dyn EventLog>;
    /// Returns the durable context checkpoint store paired with this process.
    ///
    /// Checkpoints compact model context only; the canonical event history
    /// remains readable through [`Self::event_log`].
    fn compaction_checkpoint_store(&self) -> Arc<dyn CompactionCheckpointStore>;
    /// Returns the retention control paired with this process event log.
    ///
    /// Retention is host policy: nothing reclaims canonical history on its own.
    fn thread_history_retention(&self) -> Arc<dyn ThreadHistoryRetention>;
    /// Returns whether this process inherited the supplied session's immutable
    /// event-history prefix through a durable fork boundary.
    async fn continues_thread_history(&self, session_id: SessionId) -> Result<bool>;
    /// Idempotently imports canonical events from the former materialized
    /// `/thread/events` representation during bounded-state migration.
    async fn import_thread_events(&self, events: &[Event]) -> Result<()>;
    /// Reads a bounded newest-first window of canonical events for host
    /// presentation without replaying the complete event log.
    async fn recent_thread_events(&self, session_id: SessionId, limit: usize)
    -> Result<Vec<Event>>;
    /// Reads a bounded newest-first window of message-bearing events for host
    /// presentation without replaying the complete event log.
    async fn recent_thread_messages(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<Event>>;
    /// Durably commits one host write.
    async fn write(&mut self, path: &str, value: Value) -> Result<Change>;
    /// Durably commits one host removal.
    async fn remove(&mut self, path: &str) -> Result<Change>;
    /// Executes guest Lisp and durably commits its successful transition.
    async fn exec(&mut self, path: &str, input: Value) -> Result<Activation>;
    /// Executes guest Lisp with this runtime's host-attached ports.
    async fn exec_with_ports(
        &mut self,
        path: &str,
        input: Value,
        ports: &Ports,
    ) -> Result<Activation>;
    /// Durably appends one host-supplied inbox value.
    async fn enqueue_inbox(&mut self, value: Value) -> Result<Change>;
    /// Durably removes the exact expected inbox head.
    async fn acknowledge_inbox(&mut self, expected: &Value) -> Result<Change>;
    /// Attaches one host-selected runtime mount and durably records its descriptor.
    async fn attach_mount(&mut self, name: String, mount: Mount) -> Result<Change>;
    /// Durably initializes Svit-owned conversation and reasoning state.
    async fn initialize_thread_state(&mut self, value: Value) -> Result<Change>;
    /// Replaces legacy materialized thread history with bounded metadata.
    async fn replace_thread_state(&mut self, value: Value) -> Result<Change>;
    /// Durably refreshes the descriptive port catalog from current host grants.
    async fn replace_ports(&mut self, value: Value) -> Result<Change>;
    /// Queries retained process transactions.
    async fn transactions(&self, query: TransactionQuery) -> Result<Vec<ProcessTransaction>>;
    /// Persists an on-demand process snapshot at the current transaction boundary.
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
    fn transaction_query_filters_are_visible_to_adapter_implementations() {
        let query = TransactionQuery::new()
            .after_position(2)
            .through_position(9)
            .path_prefix("/memory")
            .source("activation")
            .process_version_from(3)
            .process_version_through(8)
            .transaction_hash("abc")
            .limit(17);

        assert_eq!(query.after(), Some(2));
        assert_eq!(query.through(), Some(9));
        assert_eq!(query.path(), Some("/memory"));
        assert_eq!(query.source_filter(), Some("activation"));
        assert_eq!(query.version_from(), Some(3));
        assert_eq!(query.version_through(), Some(8));
        assert_eq!(query.hash(), Some("abc"));
        assert_eq!(query.max_transactions(), 17);
    }

    #[test]
    fn canonical_transaction_round_trips_and_replays_without_turso() {
        let process = Process::builder("svit://local/test/portable-transaction")
            .unwrap()
            .memory("status", crate::value!("new"))
            .build()
            .unwrap();
        let mut candidate = process.clone();
        let change = candidate
            .write("/memory/status", crate::value!("ready"))
            .unwrap();
        let head = TransactionHead::new(None, "0".repeat(64)).unwrap();
        let transaction = ProcessTransaction::new(
            process.id().clone(),
            &head,
            process.version(),
            candidate.version(),
            change.mutations().to_vec(),
            "host.write",
            candidate.root_hash(),
        )
        .unwrap();

        let decoded = ProcessTransaction::from_bytes(&transaction.to_bytes().unwrap()).unwrap();
        decoded.validate_successor(&head).unwrap();
        let mut rejected = process.clone();
        let mut replayed = process;
        decoded.replay(&mut replayed).unwrap();

        assert_eq!(decoded, transaction);
        assert_eq!(replayed.version(), candidate.version());
        assert_eq!(replayed.root_hash(), candidate.root_hash());

        let mut wrong_root = transaction;
        wrong_root.resulting_root_hash = "f".repeat(64);
        wrong_root.transaction_hash = wrong_root.compute_hash().unwrap();
        let before_version = rejected.version();
        let before_root_hash = rejected.root_hash();
        assert!(wrong_root.replay(&mut rejected).is_err());
        assert_eq!(rejected.version(), before_version);
        assert_eq!(rejected.root_hash(), before_root_hash);
    }
}
