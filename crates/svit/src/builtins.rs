//! Host-provided `/bin` built-ins for process-owned runtimes.
//!
//! Each standard built-in lives in its own module. Hosts can add individual
//! [`Builtin`] implementations or bundles through [`BuiltinExtension`].
//! Built-ins receive only explicit input and a read-only process context.

mod http;
mod jq;
mod llm;
mod search;
mod spawn;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use everruns::core::ToolExecutionResult;
use serde_json::{Value as JsonValue, json};

use crate::error::sanitize_diagnostic;
use crate::{Process, ProcessId, Reasoner, Value};

pub use http::{
    HttpAllowlist, HttpRequest, HttpResponse, HttpTransport, HttpTransportError,
    ReqwestHttpTransport,
};

use http::{HttpBuiltin, HttpPolicy};
use jq::JqBuiltin;
use llm::LlmBuiltin;
use search::SearchBuiltin;
use spawn::{ChildRegistry, SpawnBuiltin};

pub(super) const MAX_TOOL_INPUT_BYTES: usize = 256 * 1024;
pub(super) const MAX_TOOL_OUTPUT_BYTES: usize = 256 * 1024;

/// Discovery record projected under `/bin/<name>`.
#[derive(Clone, Debug, PartialEq)]
pub struct BuiltinManual {
    /// Concise behavior description.
    pub description: String,
    /// JSON Schema for the built-in input.
    pub input_schema: JsonValue,
    /// Human-readable output contract.
    pub output: String,
    /// Effect class such as `pure`, `read`, or `external`.
    pub effect: String,
    /// Builtin-specific limits exposed to the model.
    pub limits: Vec<String>,
}

impl BuiltinManual {
    /// Creates a host-defined builtin manual.
    pub fn new(description: impl Into<String>, input_schema: JsonValue) -> Self {
        Self {
            description: description.into(),
            input_schema,
            output: "Built-in-defined output.".into(),
            effect: "host-defined".into(),
            limits: Vec::new(),
        }
    }

    /// Sets the output contract.
    pub fn output(mut self, output: impl Into<String>) -> Self {
        self.output = output.into();
        self
    }

    /// Sets the effect class.
    pub fn effect(mut self, effect: impl Into<String>) -> Self {
        self.effect = effect.into();
        self
    }

    /// Replaces the documented builtin limits.
    pub fn limits<I, S>(mut self, limits: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.limits = limits.into_iter().map(Into::into).collect();
        self
    }
}

/// Read-only committed process access supplied to one builtin call.
///
/// The context deliberately exposes no mutation method or ambient host
/// facility. A built-in may still use capabilities explicitly captured by
/// its host-owned implementation.
#[derive(Clone)]
pub struct BuiltinContext {
    process: Arc<Mutex<Process>>,
}

impl BuiltinContext {
    fn new(process: Arc<Mutex<Process>>) -> Self {
        Self { process }
    }

    /// Reads one process value, resolving mount nodes lazily.
    pub fn read(&self, path: &str) -> crate::Result<Option<Value>> {
        self.process
            .lock()
            .map_err(|_| crate::Error::BuiltinContextUnavailable)?
            .read(path)
    }

    /// Describes one process or mount path: kind, access, and locality.
    pub fn stat(&self, path: &str) -> crate::Result<Option<Value>> {
        self.process
            .lock()
            .map_err(|_| crate::Error::BuiltinContextUnavailable)?
            .stat(path)
    }

    /// Discovers direct children of one process or mount directory path.
    pub fn discover(&self, path: &str) -> crate::Result<Vec<String>> {
        self.process
            .lock()
            .map_err(|_| crate::Error::BuiltinContextUnavailable)?
            .discover(path)
    }

    pub(super) fn process(&self) -> Arc<Mutex<Process>> {
        self.process.clone()
    }
}

/// Result returned by a built-in.
#[derive(Clone, Debug, PartialEq)]
pub enum BuiltinResult {
    /// Successful JSON-compatible output.
    Success(JsonValue),
    /// Expected failure safe to expose to the model.
    Error(String),
}

impl BuiltinResult {
    /// Creates a successful typed result.
    pub fn success(value: impl Into<JsonValue>) -> Self {
        Self::Success(value.into())
    }

    /// Creates a successful text result.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Success(JsonValue::String(text.into()))
    }

    /// Creates an expected builtin error.
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }

    fn into_value_result(self) -> Result<JsonValue, String> {
        match self {
            Self::Success(value) => {
                if json_exceeds_limit(&value, MAX_TOOL_OUTPUT_BYTES) {
                    Err("built-in output limit exceeded".into())
                } else {
                    Ok(value)
                }
            }
            Self::Error(message) => Err(sanitize_diagnostic(message)),
        }
    }
}

/// One host-owned `/bin` builtin.
///
/// ```
/// use serde_json::{Value as JsonValue, json};
/// use svit::{
///     Builtin, BuiltinContext, BuiltinManual, BuiltinResult,
///     Builtins,
/// };
///
/// struct Echo;
///
/// #[svit::async_trait]
/// impl Builtin for Echo {
///     fn manual(&self) -> BuiltinManual {
///         BuiltinManual::new(
///             "Echo one value.",
///             json!({"type": "object", "properties": {"value": {}}}),
///         )
///         .effect("pure")
///     }
///
///     async fn execute(
///         &self,
///         _context: BuiltinContext,
///         input: JsonValue,
///     ) -> BuiltinResult {
///         BuiltinResult::success(input["value"].clone())
///     }
/// }
///
/// let builtins = Builtins::new().builtin("echo", Box::new(Echo));
/// # let _ = builtins;
/// ```
#[async_trait]
pub trait Builtin: Send + Sync {
    /// Returns the discovery manual projected under `/bin`.
    fn manual(&self) -> BuiltinManual;

    /// Executes one call against explicit input and read-only process context.
    async fn execute(&self, context: BuiltinContext, input: JsonValue) -> BuiltinResult;
}

/// A related bundle of host-owned builtins registered as one unit.
pub trait BuiltinExtension: Send + Sync {
    /// Returns builtin names and implementations contributed by the bundle.
    ///
    /// Later registrations replace earlier entries with the same name.
    fn builtins(&self) -> Vec<(String, Box<dyn Builtin>)>;
}

/// Frozen host configuration for `/bin` built-ins.
///
/// `search` and `jq` are installed by default. Individual host built-ins and
/// extension bundles follow Bashkit's registration rule: later entries replace
/// earlier entries with the same name.
#[derive(Clone)]
pub struct Builtins {
    entries: BTreeMap<String, Arc<dyn Builtin>>,
    children: ChildRegistry,
    standard_model_builtins: bool,
    http_policy: Option<HttpPolicy>,
}

impl Default for Builtins {
    fn default() -> Self {
        let mut builtins = Self {
            entries: BTreeMap::new(),
            children: ChildRegistry::default(),
            standard_model_builtins: false,
            http_policy: None,
        };
        builtins.register("search", Box::new(SearchBuiltin));
        builtins.register("jq", Box::new(JqBuiltin));
        builtins
    }
}

impl Builtins {
    /// Creates the local `search` and `jq` built-ins.
    pub fn new() -> Self {
        Self::default()
    }

    /// Selects the complete standard built-in set.
    ///
    /// `search` and `jq` are available immediately. During construction, Svit
    /// resolves `http`, `llm`, and `spawn` from the instance's configuration.
    /// Selecting the standard set explicitly grants unrestricted HTTP. Hosts
    /// can attenuate it with [`Self::with_http_allowlist`].
    /// Later registrations replace any standard built-in with the same name.
    pub fn standard() -> Self {
        Self {
            standard_model_builtins: true,
            http_policy: Some(HttpPolicy::Unrestricted),
            ..Self::default()
        }
    }

    /// Adds the standard `http` built-in with the supplied URL grants.
    pub fn with_http_allowlist(mut self, http_allowlist: HttpAllowlist) -> Self {
        self.http_policy = Some(HttpPolicy::Allowlist(http_allowlist));
        self
    }

    /// Registers or replaces one host-owned built-in.
    pub fn builtin(mut self, name: impl Into<String>, builtin: Box<dyn Builtin>) -> Self {
        self.register(name, builtin);
        self
    }

    /// Registers a related built-in bundle.
    pub fn extension<E>(mut self, extension: E) -> Self
    where
        E: BuiltinExtension,
    {
        for (name, builtin) in extension.builtins() {
            self.register(name, builtin);
        }
        self
    }

    /// Adds `http` with a host-selected allowlist and transport.
    pub fn http(self, allowlist: HttpAllowlist, transport: impl HttpTransport + 'static) -> Self {
        self.builtin("http", Box::new(HttpBuiltin::new(allowlist, transport)))
    }

    /// Adds `llm` using one host-selected reasoner.
    pub fn llm(self, reasoner: Reasoner) -> Self {
        self.builtin("llm", Box::new(LlmBuiltin::new(reasoner)))
    }

    /// Adds `spawn`, which forks and runs one child Svit turn.
    pub fn spawn(mut self, reasoner: Reasoner) -> Self {
        let builtin = SpawnBuiltin::new(reasoner, self.children.clone());
        self.register("spawn", Box::new(builtin));
        self
    }

    pub(crate) fn resolve(mut self, reasoner: &Reasoner) -> Result<Self, HttpTransportError> {
        let http_policy = self.http_policy.take();
        if http_policy.is_none() && !self.standard_model_builtins {
            return Ok(self);
        }

        if let Some(http_policy) = http_policy
            && !self.entries.contains_key("http")
        {
            let transport = ReqwestHttpTransport::new()?;
            let builtin = match http_policy {
                HttpPolicy::Unrestricted => HttpBuiltin::unrestricted(transport),
                HttpPolicy::Allowlist(allowlist) => HttpBuiltin::new(allowlist, transport),
            };
            self.register("http", Box::new(builtin));
        }
        if self.standard_model_builtins && !self.entries.contains_key("llm") {
            self.register("llm", Box::new(LlmBuiltin::new(reasoner.clone())));
        }
        if self.standard_model_builtins && !self.entries.contains_key("spawn") {
            let builtin = SpawnBuiltin::new(reasoner.clone(), self.children.clone());
            self.register("spawn", Box::new(builtin));
        }
        Ok(self)
    }

    /// Returns completed local child addresses in deterministic order.
    pub fn child_ids(&self) -> Vec<ProcessId> {
        self.children.ids()
    }

    /// Snapshots one completed local child.
    pub fn child_snapshot(&self, id: &ProcessId) -> crate::Result<Option<Vec<u8>>> {
        self.children.snapshot(id)
    }

    fn register(&mut self, name: impl Into<String>, builtin: Box<dyn Builtin>) {
        self.entries.insert(name.into(), Arc::from(builtin));
    }

    pub(crate) fn catalog(&self) -> JsonValue {
        JsonValue::Object(
            self.entries
                .iter()
                .map(|(name, builtin)| {
                    let manual = builtin.manual();
                    (
                        name.clone(),
                        json!({
                            "name": name,
                            "description": manual.description,
                            "input_schema": manual.input_schema,
                            "output": manual.output,
                            "effect": manual.effect,
                            "limits": manual.limits,
                        }),
                    )
                })
                .collect(),
        )
    }

    pub(crate) async fn execute(
        &self,
        name: &str,
        input: JsonValue,
        process: Arc<Mutex<Process>>,
    ) -> ToolExecutionResult {
        match self.execute_value(name, input, process).await {
            Ok(value) => ToolExecutionResult::success(value),
            Err(message) => ToolExecutionResult::tool_error(message),
        }
    }

    pub(crate) async fn execute_value(
        &self,
        name: &str,
        input: JsonValue,
        process: Arc<Mutex<Process>>,
    ) -> Result<JsonValue, String> {
        let Some(builtin) = self.entries.get(name).cloned() else {
            return Err("built-in not found".into());
        };
        // THREAT[TM-DOS-008]: Every built-in and host extension crosses the
        // same aggregate JSON input/output limits before and after execution.
        if json_exceeds_limit(&input, MAX_TOOL_INPUT_BYTES) {
            return Err("built-in input limit exceeded".into());
        }
        // THREAT[TM-CAP-006]: Registration is host-only, and the public call
        // context grants committed reads without process mutation authority.
        builtin
            .execute(BuiltinContext::new(process), input)
            .await
            .into_value_result()
    }
}

fn json_exceeds_limit(value: &JsonValue, limit: usize) -> bool {
    match serde_json::to_vec(value) {
        Ok(bytes) => bytes.len() > limit,
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_registry_grants_unrestricted_http_tm_cap_004() {
        assert!(matches!(
            Builtins::standard().http_policy,
            Some(HttpPolicy::Unrestricted)
        ));
        assert!(matches!(
            Builtins::standard()
                .with_http_allowlist(HttpAllowlist::new())
                .http_policy,
            Some(HttpPolicy::Allowlist(_))
        ));
    }
}
