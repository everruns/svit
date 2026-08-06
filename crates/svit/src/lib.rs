//! Svit is a research runtime for isolated, serializable agent processes.
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
//!     .script("counter", Script::new(r#"
//!         (define (main input)
//!           (let ((count (+ (memory-get "/count") (value-get input "/by"))))
//!             (do (memory-set! "/count" count) count)))
//!     "#))
//!     .build()?;
//!
//! let activation = process.exec("counter", value!({"by": 2}))?;
//! assert_eq!(activation.output, value!(2));
//! # Ok::<(), svit::Error>(())
//! ```

pub mod control;
mod error;
pub mod hooks;
mod limits;
mod process;
pub mod value;

pub use control::{
    ControlClientId, ControlCommand, ControlFailure, ControlOutcome, ControlProtocol,
    ControlRequest, ControlRequestId, ControlResponse, ProcessController, ProcessObservation,
};
pub use error::{Error, Result};
pub use hooks::{ActivationEvent, ActivationHook, ActivationRequest, ActivationStatus, HookAction};
pub use limits::Limits;
pub use process::{Activation, LogRecord, MessageIntent, Process, ProcessBuilder, ProcessId};
pub use value::{Script, Value};
