//! Svit is a research runtime for isolated, serializable agent processes.
//!
//! The current vertical slice runs named, untrusted Svit Lua scripts against a
//! transactional memory tree. Successful activations commit memory, staged
//! scripts, and message intents together. Failures commit nothing.
//!
//! ```
//! use svit::{Process, value};
//!
//! let mut process = Process::builder("svit://local/example/counter")?
//!     .memory(value!({"count": 0}))
//!     .build()?;
//! process.save_script("counter", r#"
//!     function main(input)
//!         memory.count = memory.count + input.by
//!         return memory.count
//!     end
//! "#)?;
//!
//! let activation = process.run("counter", value!({"by": 2}))?;
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
