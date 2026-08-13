//! Transport-neutral control protocol implementing Versioned Atomic State
//! Transitions (VAST) for one Svit process.
//!
//! VAST is the concurrency and commit model. `svit-control@1` is the concrete
//! wire major. The model requires one valid serialization point and does not
//! imply distributed consensus or merge concurrent activations.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::error::sanitize_diagnostic;
use crate::process::validate_script_name;
use crate::{Activation, Error, Process, ProcessId, Result, Value};

const DEFAULT_RECEIPT_LIMIT: usize = 256;
const MAX_RECEIPT_LIMIT: usize = 4096;
const MAX_CONTROL_ID_BYTES: usize = 128;

/// Version of the transport-neutral control envelope.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlProtocol {
    /// First Svit control protocol.
    #[default]
    #[serde(rename = "svit-control@1")]
    V1,
}

/// Client-chosen namespace for idempotency keys.
///
/// This value must remain stable across a retry. It is not an authenticated
/// principal, a transport connection identifier, or an authorization grant.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ControlClientId(String);

impl ControlClientId {
    /// Validates a client correlation identifier.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_control_id("client_id", &value)?;
        Ok(Self(value))
    }

    /// Returns the identifier text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ControlClientId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<ControlClientId> for String {
    fn from(value: ControlClientId) -> Self {
        value.0
    }
}

/// Idempotency key scoped to one client-chosen namespace.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ControlRequestId(String);

impl ControlRequestId {
    /// Validates a request idempotency key.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_control_id("request_id", &value)?;
        Ok(Self(value))
    }

    /// Returns the identifier text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ControlRequestId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<ControlRequestId> for String {
    fn from(value: ControlRequestId) -> Self {
        value.0
    }
}

/// Mutating operation accepted by Svit Control Protocol 1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
// Within one protocol major, unknown fields on a known operation are ignored
// so additive changes remain compatible. Unknown operation tags still fail.
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum ControlCommand {
    /// Run one named script as the process transaction.
    Activate {
        /// Name in the committed script library.
        script: String,
        /// Immutable activation input.
        input: Value,
    },
}

/// One transport-neutral control request.
///
/// THREAT[TM-DOS-005]: This type represents an already-decoded envelope. A
/// transport adapter must cap bytes before deserialization; command values are
/// validated again before receipt retention or execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlRequest {
    /// Protocol version used to decode the envelope.
    pub protocol: ControlProtocol,
    /// Client-chosen idempotency namespace, not an authenticated identity.
    pub client_id: ControlClientId,
    /// Idempotency key scoped to `client_id`.
    pub request_id: ControlRequestId,
    /// Exact process addressed by the request.
    pub process_id: ProcessId,
    /// Compare-and-swap precondition for the committed process version.
    pub expected_version: u64,
    /// Transaction to attempt.
    pub command: ControlCommand,
}

impl ControlRequest {
    /// Builds an activation request with a mandatory version precondition.
    pub fn activate(
        client_id: impl Into<String>,
        request_id: impl Into<String>,
        process_id: ProcessId,
        expected_version: u64,
        script: impl Into<String>,
        input: Value,
    ) -> Result<Self> {
        Ok(Self {
            protocol: ControlProtocol::V1,
            client_id: ControlClientId::new(client_id)?,
            request_id: ControlRequestId::new(request_id)?,
            process_id,
            expected_version,
            command: ControlCommand::Activate {
                script: script.into(),
                input,
            },
        })
    }
}

/// Stable error payload returned without exposing host internals.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlFailure {
    /// Stable machine-readable error category.
    pub code: String,
    /// Capped guest-safe diagnostic.
    pub message: String,
}

/// Transaction result at the process serialization point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControlOutcome {
    /// The activation committed exactly one new process version.
    Committed {
        /// Version against which the transaction was evaluated.
        previous_version: u64,
        /// Committed activation result.
        activation: Activation,
    },
    /// The process changed before this transaction reached the serialization point.
    Conflict {
        /// Version supplied by the client.
        expected_version: u64,
        /// Current committed process version.
        actual_version: u64,
        /// Integrity hash of the current committed root.
        root_hash: String,
    },
    /// The request was evaluated but could not commit.
    Rejected {
        /// Current committed process version, unchanged by this request.
        version: u64,
        /// Integrity hash of the unchanged committed root.
        root_hash: String,
        /// Stable rejection information.
        failure: ControlFailure,
    },
}

/// Control response suitable for encoding over a host-selected transport.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlResponse {
    /// Protocol version used for this response.
    pub protocol: ControlProtocol,
    /// Client-chosen idempotency namespace copied from the request.
    pub client_id: ControlClientId,
    /// Request idempotency key copied from the request.
    pub request_id: ControlRequestId,
    /// Process that evaluated the request.
    pub process_id: ProcessId,
    /// True when this result came from the bounded receipt cache.
    pub replayed: bool,
    /// Transaction result.
    pub outcome: ControlOutcome,
}

/// Current committed identity of a controlled process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessObservation {
    /// Controlled process address.
    pub process_id: ProcessId,
    /// Current committed version.
    pub version: u64,
    /// Integrity hash of the current committed root.
    pub root_hash: String,
}

/// Thread-safe in-memory VAST serialization point for one Svit process.
///
/// The controller is a reference adapter for the protocol. Durable hosts must
/// make the process commit and receipt write atomic in their own storage.
// THREAT[TM-EFF-004]: One controller serializes one in-memory process, but it
// is not a distributed lease. A host must prevent two active controllers for
// the same logical process.
pub struct ProcessController {
    inner: Mutex<ControllerState>,
    receipt_limit: usize,
}

struct ControllerState {
    process: Process,
    receipts: BTreeMap<RequestKey, Receipt>,
    receipt_order: VecDeque<RequestKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RequestKey {
    client_id: ControlClientId,
    request_id: ControlRequestId,
}

struct Receipt {
    request: ControlRequest,
    response: ControlResponse,
}

impl ProcessController {
    /// Controls a process with a bounded default receipt cache.
    pub fn new(process: Process) -> Self {
        Self::with_receipt_limit(process, DEFAULT_RECEIPT_LIMIT)
            .expect("default control receipt limit is valid")
    }

    /// Controls a process with an explicit in-memory receipt count limit.
    pub fn with_receipt_limit(process: Process, receipt_limit: usize) -> Result<Self> {
        // THREAT[TM-DOS-004]: Receipt retention is bounded independently of
        // client-controlled request identifiers.
        if !(1..=MAX_RECEIPT_LIMIT).contains(&receipt_limit) {
            return Err(Error::InvalidControlConfiguration(
                "control receipt limit must be between 1 and 4096",
            ));
        }
        Ok(Self {
            inner: Mutex::new(ControllerState {
                process,
                receipts: BTreeMap::new(),
                receipt_order: VecDeque::new(),
            }),
            receipt_limit,
        })
    }

    /// Returns the current committed version and root hash.
    pub fn observe(&self) -> Result<ProcessObservation> {
        let state = self.lock()?;
        Ok(observe(&state.process))
    }

    /// Serializes the current committed process for inspection or restore.
    pub fn snapshot(&self) -> Result<Vec<u8>> {
        self.lock()?.process.snapshot()
    }

    /// Evaluates one request at the process serialization point.
    pub fn execute(&self, request: ControlRequest) -> Result<ControlResponse> {
        let mut state = self.lock()?;
        // THREAT[TM-AUTH-001]: Client and process ids are correlation and
        // routing data only. This layer never treats them as authentication.
        let key = RequestKey {
            client_id: request.client_id.clone(),
            request_id: request.request_id.clone(),
        };

        if let Some(receipt) = state.receipts.get(&key) {
            if receipt.request == request {
                let mut response = receipt.response.clone();
                response.replayed = true;
                return Ok(response);
            }
            return Ok(rejected(
                &request,
                &state.process,
                "request_id_reused",
                "request id was already used for a different request",
            ));
        }

        if request.process_id != *state.process.id() {
            return Ok(rejected(
                &request,
                &state.process,
                "process_mismatch",
                "request addressed a different process",
            ));
        }
        if let Err(error) = validate_command(&request, &state.process) {
            return Ok(rejected_with_failure(
                &request,
                &state.process,
                failure(error),
            ));
        }

        // VAST boundary: under this lock, a stale request cannot execute; a
        // matching request either commits exactly one next version or leaves
        // the committed process root unchanged.
        // THREAT[TM-EFF-002]: Checking the expected version under the same lock
        // as activation commit makes this the single linearization point for
        // all clients of the process.
        let response = if request.expected_version != state.process.version() {
            conflict(&request, &state.process)
        } else {
            execute_command(&request, &mut state.process)
        };

        // THREAT[TM-EFF-003]: Cache the terminal response before releasing the
        // controller lock. Version CAS still prevents a duplicate commit after
        // bounded receipt eviction.
        state.receipts.insert(
            key.clone(),
            Receipt {
                request,
                response: response.clone(),
            },
        );
        state.receipt_order.push_back(key);
        while state.receipts.len() > self.receipt_limit {
            if let Some(expired) = state.receipt_order.pop_front() {
                state.receipts.remove(&expired);
            }
        }

        Ok(response)
    }

    fn lock(&self) -> Result<MutexGuard<'_, ControllerState>> {
        self.inner.lock().map_err(|_| Error::ControlUnavailable)
    }
}

fn execute_command(request: &ControlRequest, process: &mut Process) -> ControlResponse {
    let previous_version = process.version();
    let result = match &request.command {
        ControlCommand::Activate { script, input } => {
            process.exec(&format!("/lib/{script}"), input.clone())
        }
    };
    let outcome = match result {
        Ok(activation) => ControlOutcome::Committed {
            previous_version,
            activation,
        },
        Err(error) => ControlOutcome::Rejected {
            version: process.version(),
            root_hash: process.root_hash(),
            failure: failure(error),
        },
    };
    response(request, process.id().clone(), outcome)
}

fn conflict(request: &ControlRequest, process: &Process) -> ControlResponse {
    response(
        request,
        process.id().clone(),
        ControlOutcome::Conflict {
            expected_version: request.expected_version,
            actual_version: process.version(),
            root_hash: process.root_hash(),
        },
    )
}

fn rejected(
    request: &ControlRequest,
    process: &Process,
    code: &str,
    message: &str,
) -> ControlResponse {
    rejected_with_failure(
        request,
        process,
        ControlFailure {
            code: code.into(),
            message: message.into(),
        },
    )
}

fn rejected_with_failure(
    request: &ControlRequest,
    process: &Process,
    failure: ControlFailure,
) -> ControlResponse {
    response(
        request,
        process.id().clone(),
        ControlOutcome::Rejected {
            version: process.version(),
            root_hash: process.root_hash(),
            failure,
        },
    )
}

fn response(
    request: &ControlRequest,
    process_id: ProcessId,
    outcome: ControlOutcome,
) -> ControlResponse {
    ControlResponse {
        protocol: ControlProtocol::V1,
        client_id: request.client_id.clone(),
        request_id: request.request_id.clone(),
        process_id,
        replayed: false,
        outcome,
    }
}

fn observe(process: &Process) -> ProcessObservation {
    ProcessObservation {
        process_id: process.id().clone(),
        version: process.version(),
        root_hash: process.root_hash(),
    }
}

fn failure(error: Error) -> ControlFailure {
    let code = match &error {
        Error::InvalidScriptName(_) => "invalid_script_name",
        Error::ScriptNotFound(_) => "script_not_found",
        Error::InvalidPath(_) => "invalid_path",
        Error::InvalidValue(_) => "invalid_value",
        Error::InvalidMount(_) => "invalid_mount",
        Error::MountUnavailable(_) => "mount_unavailable",
        Error::MountDenied(_) => "mount_denied",
        Error::InvalidLimits(_) => "invalid_limits",
        Error::ExecutionLimitExceeded => "execution_limit_exceeded",
        Error::ResourceLimitExceeded(_) => "resource_limit_exceeded",
        Error::HookCancelled(_) => "hook_cancelled",
        Error::Script(_) => "script_failed",
        Error::InvalidSnapshot(_) => "invalid_snapshot",
        Error::InvalidControlId(_) => "invalid_control_id",
        Error::InvalidControlConfiguration(_) => "invalid_control_configuration",
        Error::ControlUnavailable => "control_unavailable",
        Error::InboxConflict => "inbox_conflict",
        Error::ChildUnavailable => "child_unavailable",
        Error::BuiltinContextUnavailable => "builtin_context_unavailable",
        Error::PersistenceNotFound(_) => "persistence_not_found",
        Error::PersistenceAlreadyExists(_) => "persistence_already_exists",
        Error::PersistenceConflict => "persistence_conflict",
        Error::PersistenceReferenced => "persistence_referenced",
        Error::InvalidPersistence(_) => "invalid_persistence",
        Error::PersistenceUnavailable => "persistence_unavailable",
    };
    ControlFailure {
        code: code.into(),
        message: sanitize_diagnostic(error),
    }
}

fn validate_command(request: &ControlRequest, process: &Process) -> Result<()> {
    match &request.command {
        ControlCommand::Activate { script, input } => {
            validate_script_name(script)?;
            input.validate(process.limits(), false)
        }
    }
}

fn validate_control_id(field: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_CONTROL_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(Error::InvalidControlId(field.into()));
    }
    Ok(())
}
