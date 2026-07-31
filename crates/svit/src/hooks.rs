use std::sync::Arc;

use crate::{ProcessId, Value};

/// An interceptor decision for a typed runtime event.
#[derive(Clone, Debug, PartialEq)]
pub enum HookAction<T> {
    /// Continue with the event, optionally modified by the hook.
    Continue(T),
    /// Deny the operation with a public reason.
    Cancel(String),
}

/// Mutable activation request passed through `before_activation` hooks.
#[derive(Clone, Debug, PartialEq)]
pub struct ActivationRequest {
    /// Named script to invoke.
    pub script: String,
    /// Persistent input value passed to `main`.
    pub input: Value,
}

/// Outcome observed after an activation attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivationStatus {
    /// The activation committed this process version.
    Committed { version: u64 },
    /// The activation failed and committed nothing.
    Failed { error: String },
}

/// Typed event passed to `after_activation` hooks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivationEvent {
    /// Process being activated.
    pub process_id: ProcessId,
    /// Script requested after all before hooks ran.
    pub script: String,
    /// Process version before the attempt.
    pub version_before: u64,
    /// Commit or failure outcome.
    pub status: ActivationStatus,
}

/// Intercepts activation boundaries.
///
/// Hooks are registered before process construction, run in registration
/// order, and are frozen for the process lifetime. They receive owned request
/// data so a policy hook can safely rewrite or deny it.
pub trait ActivationHook: Send + Sync {
    /// Inspects, rewrites, or cancels an activation before guest execution.
    fn before_activation(&self, request: ActivationRequest) -> HookAction<ActivationRequest> {
        HookAction::Continue(request)
    }

    /// Observes the final committed or rolled-back result.
    fn after_activation(&self, _event: &ActivationEvent) {}
}

pub(crate) type SharedHook = Arc<dyn ActivationHook>;
