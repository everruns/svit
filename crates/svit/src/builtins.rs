//! Host-provided `/ports` ports for process-owned runtimes.
//!
//! Each built-in port lives in its own module. Hosts add individual
//! [`Port`] implementations or bundles through [`PortExtension`].
//! Ports receive only explicit input and a read-only process context.

#[path = "builtins/http.rs"]
mod http;
#[path = "builtins/llm.rs"]
mod llm;
#[path = "builtins/spawn.rs"]
mod spawn;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value as JsonValue, json};

use crate::error::sanitize_diagnostic;
use crate::{Process, ProcessId, Reasoner, Value};

pub use http::{
    HttpAllowlist, HttpRequest, HttpResponse, HttpTransport, HttpTransportError,
    ReqwestHttpTransport,
};

use http::HttpPort;
use llm::LlmPort;
use spawn::{ChildRegistry, SpawnPort};

pub(super) const MAX_PORT_INPUT_BYTES: usize = 256 * 1024;

/// Discovery record projected under `/ports/<name>`.
#[derive(Clone, Debug, PartialEq)]
pub struct PortDescriptor {
    /// Version of this port contract.
    pub version: String,
    /// Concise behavior description.
    pub description: String,
    /// JSON Schema for the port input.
    pub input_schema: JsonValue,
    /// Human-readable output contract.
    pub output: String,
    /// Effect class such as `pure`, `read`, or `external`.
    pub effect: String,
    /// Port-specific limits exposed to the model.
    pub limits: Vec<String>,
}

impl PortDescriptor {
    /// Creates a host-defined port descriptor.
    pub fn new(description: impl Into<String>, input_schema: JsonValue) -> Self {
        Self {
            version: "v1".into(),
            description: description.into(),
            input_schema,
            output: "Port-defined output.".into(),
            effect: "host-defined".into(),
            limits: Vec::new(),
        }
    }

    /// Sets the port contract version.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
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

    /// Replaces the documented port limits.
    pub fn limits<I, S>(mut self, limits: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.limits = limits.into_iter().map(Into::into).collect();
        self
    }
}

/// Read-only committed process access supplied to one port call.
///
/// The context deliberately exposes no mutation method or ambient host
/// facility. A port may still use capabilities explicitly captured by
/// its host-owned implementation.
#[derive(Clone)]
pub struct PortContext {
    process: Arc<Mutex<Process>>,
}

impl PortContext {
    fn new(process: Arc<Mutex<Process>>) -> Self {
        Self { process }
    }

    /// Reads one process value, resolving mount nodes lazily.
    pub fn read(&self, path: &str) -> crate::Result<Option<Value>> {
        self.process
            .lock()
            .map_err(|_| crate::Error::PortContextUnavailable)?
            .read(path)
    }

    /// Describes one process or mount path: kind, access, and locality.
    pub fn stat(&self, path: &str) -> crate::Result<Option<Value>> {
        self.process
            .lock()
            .map_err(|_| crate::Error::PortContextUnavailable)?
            .stat(path)
    }

    /// Discovers direct children of one process or mount directory path.
    pub fn discover(&self, path: &str) -> crate::Result<Vec<String>> {
        self.process
            .lock()
            .map_err(|_| crate::Error::PortContextUnavailable)?
            .discover(path)
    }

    pub(super) fn process(&self) -> Arc<Mutex<Process>> {
        self.process.clone()
    }
}

/// Result returned by a port.
#[derive(Clone, Debug, PartialEq)]
pub enum PortResult {
    /// Successful JSON-compatible output.
    Success(JsonValue),
    /// Expected failure safe to expose to the model.
    Error(String),
}

impl PortResult {
    /// Creates a successful typed result.
    pub fn success(value: impl Into<JsonValue>) -> Self {
        Self::Success(value.into())
    }

    /// Creates a successful text result.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Success(JsonValue::String(text.into()))
    }

    /// Creates an expected port error.
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }

    fn into_value_result(self) -> Result<JsonValue, String> {
        match self {
            Self::Success(value) => Ok(value),
            Self::Error(message) => Err(sanitize_diagnostic(message)),
        }
    }
}

/// One host-owned `/ports` port.
///
/// ```
/// use serde_json::{Value as JsonValue, json};
/// use svit::{
///     Port, PortContext, PortDescriptor, PortResult,
///     Ports,
/// };
///
/// struct Echo;
///
/// #[svit::async_trait]
/// impl Port for Echo {
///     fn descriptor(&self) -> PortDescriptor {
///         PortDescriptor::new(
///             "Echo one value.",
///             json!({"type": "object", "properties": {"value": {}}}),
///         )
///         .effect("pure")
///     }
///
///     async fn execute(
///         &self,
///         _context: PortContext,
///         input: JsonValue,
///     ) -> PortResult {
///         PortResult::success(input["value"].clone())
///     }
/// }
///
/// let ports = Ports::new().port("echo", Box::new(Echo));
/// # let _ = ports;
/// ```
#[async_trait]
pub trait Port: Send + Sync {
    /// Returns the discovery descriptor projected under `/ports`.
    fn descriptor(&self) -> PortDescriptor;

    /// Executes one call against explicit input and read-only process context.
    async fn execute(&self, context: PortContext, input: JsonValue) -> PortResult;
}

/// A related bundle of host-owned ports registered as one unit.
pub trait PortExtension: Send + Sync {
    /// Returns port names and implementations contributed by the bundle.
    ///
    /// Later registrations replace earlier entries with the same name.
    fn ports(&self) -> Vec<(String, Box<dyn Port>)>;
}

/// Frozen host configuration for `/ports` ports.
///
/// Individual host ports and extension bundles follow Bashkit's registration rule: later entries replace
/// earlier entries with the same name.
///
/// Hosts must register every port explicitly; there is no authority-bearing
/// standard bundle:
///
/// ```compile_fail
/// use svit::Ports;
///
/// let _ = Ports::standard();
/// ```
#[derive(Clone, Default)]
pub struct Ports {
    entries: BTreeMap<String, Arc<dyn Port>>,
    children: ChildRegistry,
}

impl Ports {
    /// Creates an empty host port registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers or replaces one host-owned port.
    pub fn port(mut self, name: impl Into<String>, port: Box<dyn Port>) -> Self {
        self.register(name, port);
        self
    }

    /// Registers a related port bundle.
    pub fn extension<E>(mut self, extension: E) -> Self
    where
        E: PortExtension,
    {
        for (name, port) in extension.ports() {
            self.register(name, port);
        }
        self
    }

    /// Adds `http` with a host-selected allowlist and transport.
    pub fn http(self, allowlist: HttpAllowlist, transport: impl HttpTransport + 'static) -> Self {
        self.port("http", Box::new(HttpPort::new(allowlist, transport)))
    }

    /// Adds `http` with unrestricted HTTP(S) destinations.
    ///
    /// This is an explicit research-host grant. Prefer [`Self::http`] with an
    /// allowlist whenever the permitted destinations are known.
    pub fn http_unrestricted(self, transport: impl HttpTransport + 'static) -> Self {
        // THREAT[TM-CAP-004]: Unrestricted network authority exists only
        // behind this explicit host call; no bundle or build step adds it.
        self.port("http", Box::new(HttpPort::unrestricted(transport)))
    }

    /// Adds `llm` using one host-selected reasoner.
    pub fn llm(self, reasoner: Reasoner) -> Self {
        self.port("llm", Box::new(LlmPort::new(reasoner)))
    }

    /// Adds `spawn`, which forks and runs one child Svit turn.
    pub fn spawn(mut self, reasoner: Reasoner) -> Self {
        let port = SpawnPort::new(reasoner, self.children.clone());
        self.register("spawn", Box::new(port));
        self
    }

    /// Returns completed local child addresses in deterministic order.
    pub fn child_ids(&self) -> Vec<ProcessId> {
        self.children.ids()
    }

    /// Snapshots one completed local child.
    pub fn child_snapshot(&self, id: &ProcessId) -> crate::Result<Option<Vec<u8>>> {
        self.children.snapshot(id)
    }

    fn register(&mut self, name: impl Into<String>, port: Box<dyn Port>) {
        self.entries.insert(name.into(), Arc::from(port));
    }

    pub(crate) fn catalog(&self) -> JsonValue {
        JsonValue::Object(
            self.entries
                .iter()
                .map(|(name, port)| {
                    let descriptor = port.descriptor();
                    (
                        name.clone(),
                        json!({
                            "name": name,
                            "version": descriptor.version,
                            "description": descriptor.description,
                            "input_schema": descriptor.input_schema,
                            "output": descriptor.output,
                            "effect": descriptor.effect,
                            "limits": descriptor.limits,
                        }),
                    )
                })
                .collect(),
        )
    }

    pub(crate) async fn execute_value(
        &self,
        name: &str,
        input: JsonValue,
        process: Arc<Mutex<Process>>,
    ) -> Result<JsonValue, String> {
        let Some(port) = self.entries.get(name).cloned() else {
            return Err("port not found".into());
        };
        // THREAT[TM-DOS-008]: Every port input is bounded before execution.
        // A port result is activation-local; the persistent-value boundary
        // still bounds anything a script returns, writes, logs, or sends.
        if json_exceeds_limit(&input, MAX_PORT_INPUT_BYTES) {
            return Err("port input limit exceeded".into());
        }
        // THREAT[TM-CAP-006]: Registration is host-only, and the public call
        // context grants committed reads without process mutation authority.
        port.execute(PortContext::new(process), input)
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
    fn unrestricted_http_requires_explicit_registration_tm_cap_004() {
        assert_eq!(Ports::new().catalog(), json!({}));
        let ports = Ports::new().http_unrestricted(ReqwestHttpTransport::new().unwrap());
        assert!(ports.catalog().get("http").is_some());
    }
}
