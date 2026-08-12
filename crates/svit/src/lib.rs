//! Svit is a research runtime for isolated, serializable processes.
//!
//! The current vertical slice runs named, untrusted Svit Lisp scripts against a
//! transactional memory tree. Successful activations commit memory, staged
//! scripts, and message intents together. Failures commit nothing.
//! Multi-client control implements Versioned Atomic State Transitions (VAST):
//! one matching request may commit the next process version, while stale or
//! rejected requests leave committed state unchanged.
//!
//! ```
//! use svit::{Process, Script, value};
//!
//! let mut process = Process::builder("svit://local/example/counter")?
//!     .memory("count", value!(0))
//!     .library("counter", Script::new(r#"
//!         (define (main input)
//!           (let ((count (+ (read "/memory/count") (value-get input "/by"))))
//!             (do (write "/memory/count" count) count)))
//!     "#))
//!     .build()?;
//!
//! let activation = process.exec("/lib/counter", value!({"by": 2}))?;
//! assert_eq!(activation.output, value!(2));
//! # Ok::<(), svit::Error>(())
//! ```

mod builtins;
pub mod control;
mod error;
pub mod hooks;
mod limits;
pub mod mounts;
mod persistence;
mod process;
mod reasoner;
mod reasoning;
mod tools;
#[cfg(feature = "persistence-turso")]
mod turso_persistence;
pub mod value;

pub use async_trait::async_trait;
pub use builtins::{
    Builtin, BuiltinContext, BuiltinExtension, BuiltinManual, BuiltinResult, Builtins,
    HttpAllowlist, HttpRequest, HttpResponse, HttpTransport, HttpTransportError,
    ReqwestHttpTransport,
};
pub use control::{
    ControlClientId, ControlCommand, ControlFailure, ControlOutcome, ControlProtocol,
    ControlRequest, ControlRequestId, ControlResponse, ProcessController, ProcessObservation,
};
pub use error::{Error, Result};
pub use everruns::core::Message;
pub use everruns::{ContentPart, MessageRole, OpenAI};
pub use everruns_test_support::{
    LLMSIM_MODEL_ID, LlmSimConfig, SimToolCall, SimTurn, llm_sim_provider,
};
pub use hooks::{ActivationEvent, ActivationHook, ActivationRequest, ActivationStatus, HookAction};
pub use limits::Limits;
pub use mounts::{
    Locality, Mount, MountAccess, MountDescriptor, MountNode, MountNodeKind, MountPath,
    MountProvider,
};
pub use persistence::{
    DurableProcessHandle, EventQuery, Mutation, PersistedEventRecord, PersistenceSnapshotRecord,
    ProcessStore,
};
pub use process::{Activation, LogRecord, MessageIntent, Process, ProcessBuilder, ProcessId};
pub use reasoner::Reasoner;
pub use reasoning::{
    Events, Inbox, ObserveError, Outbox, Svit, SvitBuilder, SvitError, SvitEvent, SvitResult,
    SvitResumeBuilder,
};
#[cfg(feature = "persistence-turso")]
pub use turso_persistence::{
    DurableProcess, PersistedEvent, PersistenceSnapshot, TursoProcessStore,
};
/// Enabled-by-default local persistence adapter.
#[cfg(feature = "persistence-turso")]
pub type DefaultProcessStore = TursoProcessStore;
pub use value::{Script, Value};
