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
    DurableProcessHandle, Mutation, PersistenceSnapshotRecord, ProcessStore, ProcessTransaction,
    TransactionHead, TransactionQuery,
};
pub use process::{
    Activation, Change, LogRecord, MessageIntent, Process, ProcessBuilder, ProcessId,
};
pub use reasoner::Reasoner;
pub use reasoning::{
    Events, Inbox, ObserveError, Outbox, Svit, SvitBuilder, SvitError, SvitEvent, SvitResult,
    SvitResumeBuilder,
};
#[doc(hidden)]
pub mod __private {
    pub use svit_macros::{compile_svit_script, validate_svit_script, validate_svit_script_file};
}
#[cfg(feature = "persistence-turso")]
pub use turso_persistence::{DurableProcess, PersistenceSnapshot, TursoProcessStore};
/// Enabled-by-default local persistence adapter.
#[cfg(feature = "persistence-turso")]
pub type DefaultProcessStore = TursoProcessStore;
pub use value::{Script, Value};

/// Compile-checks Svit Lisp source and constructs a [`Script`].
///
/// Lisp can be written directly in the macro invocation and is checked as part
/// of Rust compilation:
///
/// ```
/// let script = svit::svit_script! {
///     (define (main input)
///       (value-map "input" input))
/// };
/// ```
///
/// A string literal is also accepted for generated or escaped source:
///
/// ```
/// let script = svit::svit_script!("(define (main input) input)");
/// ```
///
/// The `file` form resolves from the consuming package's manifest directory
/// and embeds the source with [`include_str!`]:
///
/// ```ignore
/// let script = svit::svit_script!(file "scripts/counter.svit-script");
/// ```
///
/// The macro compiles source without running an activation or ordinary
/// top-level forms. Lisp macros and constants may run during compilation, so
/// the build-time compiler uses null I/O, no modules, and the standard Svit
/// limits. Process construction remains authoritative for configured limits,
/// and an activation checks that the script defines `main(input)` and satisfies
/// runtime rules.
#[macro_export]
macro_rules! svit_script {
    (file $path:literal) => {{
        const _: () = $crate::__private::validate_svit_script_file!($path);
        $crate::Script::new(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/",
            $path
        )))
    }};
    ($source:literal) => {{
        const _: () = $crate::__private::validate_svit_script!($source);
        $crate::Script::new($source)
    }};
    ($($source:tt)+) => {{
        $crate::Script::new($crate::__private::compile_svit_script!($($source)+))
    }};
}

/// Defines a Rust test around one compile-checked Svit Lisp script.
///
/// The generated test creates a fresh process, installs the script at
/// `/lib/subject`, and gives the body both the mutable process and script path.
/// The body should execute activations and assert their outputs and committed
/// state. Returning errors with `?` is supported.
///
/// ```
/// svit::svit_script_test!(identity_script, svit::svit_script! {
///     (define (main input) input)
/// }, |process, script| {
///     let activation = process.exec(script, svit::value!({"ok": true}))?;
///     assert_eq!(activation.output, svit::value!({"ok": true}));
/// });
/// ```
#[macro_export]
macro_rules! svit_script_test {
    ($name:ident, $script:expr, |$process:ident, $script_path:ident| $body:block $(,)?) => {
        #[test]
        fn $name() -> $crate::Result<()> {
            let mut $process = $crate::Process::builder(concat!(
                "svit://local/tests/",
                stringify!($name)
            ))?
            .library("subject", $script)
            .build()?;
            let $script_path = "/lib/subject";
            $body
            Ok(())
        }
    };
}
