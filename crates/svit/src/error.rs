use thiserror::Error;

/// Errors returned by the Svit process API.
#[derive(Debug, Error)]
pub enum Error {
    /// A script name is empty, too long, or contains unsupported characters.
    #[error("invalid script name: {0}")]
    InvalidScriptName(String),

    /// The requested named script does not exist.
    #[error("script not found: {0}")]
    ScriptNotFound(String),

    /// A state path is malformed or addresses a non-container value.
    #[error("invalid state path: {0}")]
    InvalidPath(String),

    /// A guest value cannot be persisted in the canonical state tree.
    #[error("invalid persistent value: {0}")]
    InvalidValue(String),

    /// A mount source could not be resolved or its record failed validation.
    #[error("invalid mount: {0}")]
    InvalidMount(String),

    /// The addressed mount has no attached provider in this runtime.
    ///
    /// Restored snapshots keep mount identity but never mount authority. The
    /// host must reattach a provider before the mount resolves again.
    #[error("mount provider is not attached: {0}")]
    MountUnavailable(String),

    /// The mount exists but the host did not grant the attempted operation.
    #[error("mount access denied: {0}")]
    MountDenied(String),

    /// The committed inbox head changed before the runtime acknowledged it.
    #[error("inbox head changed before acknowledgement")]
    InboxConflict,

    /// A locally retained spawned child cannot be accessed.
    #[error("spawned child process is unavailable")]
    ChildUnavailable,

    /// A built-in cannot access its read-only process context.
    #[error("built-in process context is unavailable")]
    BuiltinContextUnavailable,

    /// A host-supplied resource configuration exceeds the runtime's hard
    /// safety envelope.
    #[error("invalid process limits: {0}")]
    InvalidLimits(&'static str),

    /// The guest exceeded the activation execution budget.
    #[error("activation execution limit exceeded")]
    ExecutionLimitExceeded,

    /// The guest exceeded a configured runtime or output limit.
    #[error("activation resource limit exceeded: {0}")]
    ResourceLimitExceeded(&'static str),

    /// An interceptor hook denied the activation.
    #[error("activation cancelled by hook: {0}")]
    HookCancelled(String),

    /// Guest compilation or execution failed. Diagnostics are capped and do not
    /// include Rust backtraces or host paths.
    #[error("script failed: {0}")]
    Script(String),

    /// A snapshot is malformed or violates process invariants.
    #[error("invalid snapshot: {0}")]
    InvalidSnapshot(String),

    /// A client or request identifier is invalid for the control protocol.
    #[error("invalid control identifier: {0}")]
    InvalidControlId(String),

    /// Host configuration for the control protocol is outside hard bounds.
    #[error("invalid control configuration: {0}")]
    InvalidControlConfiguration(&'static str),

    /// The in-memory process controller cannot access its serialized state.
    #[error("process controller unavailable")]
    ControlUnavailable,

    /// No persisted process exists at the requested logical address.
    #[error("persisted process not found: {0}")]
    PersistenceNotFound(String),

    /// A persisted process already exists at the requested logical address.
    #[error("persisted process already exists: {0}")]
    PersistenceAlreadyExists(String),

    /// The persisted head changed before this transaction could commit.
    #[error("persisted process head conflict")]
    PersistenceConflict,

    /// Retained fork history prevents the requested lifecycle change.
    #[error("persisted process history is referenced by a fork")]
    PersistenceReferenced,

    /// Persisted bytes or metadata violate the event-source invariants.
    #[error("invalid persisted process: {0}")]
    InvalidPersistence(String),

    /// The persistence engine could not complete an operation safely.
    #[error("persistence store unavailable")]
    PersistenceUnavailable,
}

/// Convenient result alias for the Svit API.
pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn sanitize_diagnostic(message: impl std::fmt::Display) -> String {
    // THREAT[TM-INF-001]: Cap diagnostics before they cross the runtime API.
    // Virtual chunk names avoid host paths; callers never receive backtraces.
    const MAX_BYTES: usize = 1024;
    let message = message.to_string();
    if message.len() <= MAX_BYTES {
        return message;
    }

    let mut end = MAX_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &message[..end])
}
