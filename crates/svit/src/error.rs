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

    /// A host-provided snapshot mount could not be imported or validated.
    #[error("invalid snapshot mount: {0}")]
    InvalidMount(String),

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

    /// Lisp compilation or execution failed. Diagnostics are capped and do not
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
