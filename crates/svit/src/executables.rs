//! Native `/bin` executables for process-owned runtimes.
//!
//! They are invoked through the generic `exec` operation under `/bin`; they
//! are not shell commands or Svit Lisp primitives. They receive only explicit
//! process state or host capabilities.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentyk::{
    ChatDriver, ChatRequest, DriverId, FnTool, Message, ModelSpec, Tool, ToolContext, ToolOutput,
};
use async_trait::async_trait;
use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx, Vars, data};
use jaq_json::Val;
use regex::{Regex, RegexBuilder};
use serde_json::{Value as JsonValue, json};
use url::Url;

use crate::{Process, ProcessId};

const MAX_TOOL_INPUT_BYTES: usize = 256 * 1024;
const MAX_TOOL_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_SEARCH_RESULTS: usize = 100;
const MAX_JQ_RESULTS: usize = 100;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct ModelTool {
    driver: Arc<dyn ChatDriver>,
    model: ModelSpec,
}

#[derive(Clone)]
enum ChildEntry {
    Starting,
    Ready(Arc<Mutex<Process>>),
}

/// An outbound HTTP request after native-executable policy validation.
///
/// This type intentionally does not implement `Debug`: headers may contain
/// credentials supplied by the model or embedding host.
#[derive(Clone)]
pub struct HttpRequest {
    /// HTTP method.
    pub method: String,
    /// Absolute, allowlisted HTTP(S) URL.
    pub url: String,
    /// Request headers.
    pub headers: BTreeMap<String, String>,
    /// Optional request body.
    pub body: Option<Vec<u8>>,
    /// Maximum accepted response body size.
    pub max_response_bytes: usize,
}

/// A response returned by a host-owned HTTP transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: BTreeMap<String, String>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

/// Typed failure returned by a host-owned HTTP transport.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HttpTransportError {
    /// The host denied the request.
    #[error("request denied")]
    Denied,
    /// The request timed out.
    #[error("request timed out")]
    Timeout,
    /// The response exceeded its configured bound.
    #[error("response too large")]
    TooLarge,
    /// Connectivity failed without exposing dependency diagnostics.
    #[error("request failed")]
    Transport,
}

/// Host-owned connectivity for `/bin/http`.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Performs one already-validated request.
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpTransportError>;
}

/// Default-deny URL policy for `/bin/http`.
#[derive(Clone, Default)]
pub struct HttpAllowlist {
    roots: Vec<Url>,
}

impl HttpAllowlist {
    /// Creates an empty allowlist.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allows one HTTP(S) endpoint and its path descendants.
    ///
    /// Invalid URLs are ignored, so malformed host configuration fails closed.
    pub fn allow(mut self, root: impl AsRef<str>) -> Self {
        if let Ok(url) = Url::parse(root.as_ref())
            && matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
        {
            self.roots.push(url);
        }
        self
    }

    fn allows(&self, candidate: &Url) -> bool {
        self.roots.iter().any(|root| {
            root.scheme() == candidate.scheme()
                && root.host_str() == candidate.host_str()
                && root.port_or_known_default() == candidate.port_or_known_default()
                && path_is_within(root.path(), candidate.path())
        })
    }
}

/// Configuration for Svit's native executables.
///
/// `search` and `jq` are always installed. HTTP, nested model calls, and child
/// process creation appear only after the host grants their capabilities.
#[derive(Clone, Default)]
pub struct Executables {
    http: Option<(HttpAllowlist, Arc<dyn HttpTransport>)>,
    llm: Option<ModelTool>,
    llm_system_prompt: Option<String>,
    spawn: Option<ModelTool>,
    spawn_system_prompt: Option<String>,
    children: Arc<Mutex<BTreeMap<ProcessId, ChildEntry>>>,
}

impl Executables {
    /// Creates native data executables with external effects disabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `http` with a host-selected allowlist and transport.
    pub fn http(
        mut self,
        allowlist: HttpAllowlist,
        transport: impl HttpTransport + 'static,
    ) -> Self {
        self.http = Some((allowlist, Arc::new(transport)));
        self
    }

    /// Adds `llm` using one host-selected model and driver.
    pub fn llm(mut self, model: ModelSpec, driver: impl ChatDriver + 'static) -> Self {
        self.llm = Some(ModelTool {
            driver: Arc::new(driver),
            model,
        });
        self
    }

    /// Sets the fixed system prompt used by `llm`.
    pub fn llm_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.llm_system_prompt = Some(prompt.into());
        self
    }

    /// Adds `spawn`, which forks and runs one child Svit turn.
    pub fn spawn(mut self, model: ModelSpec, driver: impl ChatDriver + 'static) -> Self {
        self.spawn = Some(ModelTool {
            driver: Arc::new(driver),
            model,
        });
        self
    }

    /// Replaces the inherited parent prompt for spawned children.
    pub fn spawn_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.spawn_system_prompt = Some(prompt.into());
        self
    }

    /// Returns completed local child addresses in deterministic order.
    pub fn child_ids(&self) -> Vec<ProcessId> {
        self.children
            .lock()
            .map(|children| {
                children
                    .iter()
                    .filter(|(_, child)| matches!(child, ChildEntry::Ready(_)))
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Snapshots one completed local child.
    pub fn child_snapshot(&self, id: &ProcessId) -> crate::Result<Option<Vec<u8>>> {
        let process = self
            .children
            .lock()
            .ok()
            .and_then(|children| match children.get(id) {
                Some(ChildEntry::Ready(process)) => Some(process.clone()),
                _ => None,
            });
        process
            .map(|process| {
                process
                    .lock()
                    .map_err(|_| crate::Error::ChildUnavailable)?
                    .snapshot()
            })
            .transpose()
    }

    pub(crate) fn build_tools(&self, process: Arc<Mutex<Process>>) -> Vec<Arc<dyn Tool>> {
        let mut tools = vec![build_search_tool(process.clone()), build_jq_tool()];
        if let Some((allowlist, transport)) = &self.http {
            tools.push(build_http_tool(allowlist.clone(), transport.clone()));
        }
        if let Some(config) = &self.llm {
            tools.push(build_llm_tool(
                config.clone(),
                self.llm_system_prompt.clone(),
            ));
        }
        if let Some(config) = &self.spawn {
            tools.push(build_spawn_tool(SpawnTool {
                parent: process,
                config: config.clone(),
                system_prompt: self.spawn_system_prompt.clone(),
                children: self.children.clone(),
            }));
        }
        tools
    }

    pub(crate) fn catalog(&self, process: Arc<Mutex<Process>>) -> JsonValue {
        let mut catalog = serde_json::Map::new();
        for executable in self.build_tools(process) {
            let definition = executable.definition();
            let (effect, output, limits) = executable_manual(&definition.name);
            catalog.insert(
                definition.name.clone(),
                json!({
                    "name": definition.name,
                    "description": definition.description,
                    "input_schema": definition.parameters,
                    "output": output,
                    "effect": effect,
                    "limits": limits,
                }),
            );
        }
        JsonValue::Object(catalog)
    }

    pub(crate) async fn execute(
        &self,
        name: &str,
        input: JsonValue,
        process: Arc<Mutex<Process>>,
    ) -> ToolOutput {
        let Some(executable) = self
            .build_tools(process)
            .into_iter()
            .find(|tool| tool.definition().name == name)
        else {
            return ToolOutput::error("executable not found");
        };
        executable.execute(input, &ToolContext::default()).await
    }
}

fn executable_manual(name: &str) -> (&'static str, &'static str, Vec<&'static str>) {
    match name {
        "search" => (
            "read",
            "JSON object with matched process paths, one-based line numbers, and text.",
            vec![
                "4 KiB pattern.",
                "100 results.",
                "256 KiB aggregate output.",
            ],
        ),
        "jq" => (
            "pure",
            "JSON object containing the values emitted by the filter.",
            vec![
                "4 KiB filter and 256 KiB input/output.",
                "100 results.",
                "Recursive and generator constructs are rejected.",
            ],
        ),
        "http" => (
            "external",
            "JSON object containing status, response headers, and UTF-8-lossy body text.",
            vec![
                "Host URL allowlist and transport apply.",
                "30 second timeout.",
                "256 KiB request and response bodies.",
            ],
        ),
        "llm" => (
            "external",
            "Text returned by the host-selected nested model.",
            vec!["256 KiB prompt.", "Host model and provider limits apply."],
        ),
        "spawn" => (
            "child-process",
            "JSON object containing the child address and completed assistant response.",
            vec![
                "One turn per unique local child address.",
                "256 KiB task.",
                "Child registry is local and non-durable.",
            ],
        ),
        _ => ("host-defined", "Executable-defined output.", Vec::new()),
    }
}

fn build_search_tool(process: Arc<Mutex<Process>>) -> Arc<dyn Tool> {
    Arc::new(FnTool::new(
        "search",
        "Search text values under a committed Svit process path with a Rust regular expression.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "pattern": {"type": "string"},
                "case_sensitive": {"type": "boolean", "default": true},
                "fixed_strings": {"type": "boolean", "default": false}
            },
            "required": ["path", "pattern"]
        }),
        move |arguments| {
            let process = process.clone();
            async move {
                let path = arguments["path"].as_str().unwrap_or_default();
                let pattern = arguments["pattern"].as_str().unwrap_or_default();
                if pattern.len() > 4 * 1024 {
                    return ToolOutput::error("search pattern limit exceeded");
                }
                let pattern = if arguments["fixed_strings"].as_bool().unwrap_or(false) {
                    regex::escape(pattern)
                } else {
                    pattern.to_owned()
                };
                let regex = match RegexBuilder::new(&pattern)
                    .case_insensitive(!arguments["case_sensitive"].as_bool().unwrap_or(true))
                    .size_limit(1 << 20)
                    .build()
                {
                    Ok(regex) => regex,
                    Err(_) => return ToolOutput::error("invalid search pattern"),
                };
                let Ok(process) = process.lock() else {
                    return ToolOutput::error("Svit process lock is unavailable");
                };
                let value = match process.read(path) {
                    Ok(Some(value)) => value.to_json(),
                    Ok(None) => return ToolOutput::error("path not found"),
                    Err(error) => return ToolOutput::error(error.to_string()),
                };
                // THREAT[TM-DOS-008]: Search uses the linear-time Rust regex
                // engine and caps pattern size, compiled size, and results.
                let mut matches = Vec::new();
                let mut output_bytes = 0;
                collect_matches(path, &value, &regex, &mut matches, &mut output_bytes);
                ToolOutput::text(json!({"matches": matches}).to_string())
            }
        },
    ))
}

fn collect_matches(
    path: &str,
    value: &JsonValue,
    regex: &Regex,
    matches: &mut Vec<JsonValue>,
    output_bytes: &mut usize,
) {
    if matches.len() >= MAX_SEARCH_RESULTS {
        return;
    }
    match value {
        JsonValue::String(text) => {
            for (line_index, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    let Some(next_size) = output_bytes.checked_add(path.len() + line.len()) else {
                        return;
                    };
                    if next_size > MAX_TOOL_OUTPUT_BYTES {
                        return;
                    }
                    matches.push(json!({"path": path, "line": line_index + 1, "text": line}));
                    *output_bytes = next_size;
                    if matches.len() >= MAX_SEARCH_RESULTS {
                        break;
                    }
                }
            }
        }
        JsonValue::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                collect_matches(
                    &format!("{path}/{index}"),
                    value,
                    regex,
                    matches,
                    output_bytes,
                );
            }
        }
        JsonValue::Object(values) => {
            for (name, value) in values {
                collect_matches(
                    &format!("{path}/{name}"),
                    value,
                    regex,
                    matches,
                    output_bytes,
                );
            }
        }
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) => {}
    }
}

fn build_jq_tool() -> Arc<dyn Tool> {
    Arc::new(FnTool::new(
        "jq",
        "Run a bounded jq filter over a supplied JSON value.",
        json!({
            "type": "object",
            "properties": {"filter": {"type": "string"}, "input": {}},
            "required": ["filter", "input"]
        }),
        |arguments| async move {
            let filter = arguments["filter"].as_str().unwrap_or_default();
            if filter.is_empty() || filter.len() > 4 * 1024 {
                return ToolOutput::error("jq filter is empty or exceeds its limit");
            }
            if jq_filter_is_unbounded(filter) {
                return ToolOutput::error("jq filter uses an unbounded construct");
            }
            let input = arguments.get("input").cloned().unwrap_or(JsonValue::Null);
            if serde_json::to_vec(&input).is_ok_and(|bytes| bytes.len() > MAX_TOOL_INPUT_BYTES) {
                return ToolOutput::error("jq input limit exceeded");
            }
            match run_jq(filter, input) {
                Ok(values) => ToolOutput::text(json!({"values": values}).to_string()),
                Err(message) => ToolOutput::error(message),
            }
        },
    ))
}

fn jq_filter_is_unbounded(filter: &str) -> bool {
    let unsafe_names = Regex::new(
        r"(^|[^[:alnum:]_])(def|recurse|repeat|while|until|range|input|inputs)([^[:alnum:]_]|$)",
    )
    .expect("static jq policy regex");
    unsafe_names.is_match(filter)
}

fn run_jq(code: &str, input: JsonValue) -> Result<Vec<JsonValue>, &'static str> {
    // THREAT[TM-DOS-008]: jq receives bounded JSON, rejects recursive and
    // generator constructs, and caps result count and serialized output.
    let arena = Arena::default();
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let loader = Loader::new(defs);
    let modules = loader
        .load(&arena, File { path: (), code })
        .map_err(|_| "invalid jq filter")?;
    let funs = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());
    let filter = Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|_| "invalid jq filter")?;
    let input: Val = serde_json::from_value(input).map_err(|_| "invalid jq input")?;
    let context = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let mut values = Vec::new();
    let mut output_bytes = 0usize;
    for result in filter.id.run((context, input)).take(MAX_JQ_RESULTS + 1) {
        if values.len() == MAX_JQ_RESULTS {
            return Err("jq result count limit exceeded");
        }
        let value = result.map_err(|_| "jq evaluation failed")?;
        let value: JsonValue =
            serde_json::from_str(&value.to_string()).map_err(|_| "jq produced a non-JSON value")?;
        let size = serde_json::to_vec(&value)
            .map_err(|_| "jq output conversion failed")?
            .len();
        output_bytes = output_bytes
            .checked_add(size)
            .ok_or("jq output limit exceeded")?;
        if output_bytes > MAX_TOOL_OUTPUT_BYTES {
            return Err("jq output limit exceeded");
        }
        values.push(value);
    }
    Ok(values)
}

fn build_http_tool(allowlist: HttpAllowlist, transport: Arc<dyn HttpTransport>) -> Arc<dyn Tool> {
    Arc::new(FnTool::new(
        "http",
        "Make one host-allowlisted HTTP request through the configured transport.",
        json!({
            "type": "object",
            "properties": {
                "method": {"type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"]},
                "url": {"type": "string"},
                "headers": {"type": "object", "additionalProperties": {"type": "string"}},
                "body": {"type": "string"}
            },
            "required": ["method", "url"]
        }),
        move |arguments| {
            let allowlist = allowlist.clone();
            let transport = transport.clone();
            async move {
                let method = arguments["method"].as_str().unwrap_or_default();
                if !matches!(method, "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD") {
                    return ToolOutput::error("unsupported HTTP method");
                }
                let raw_url = arguments["url"].as_str().unwrap_or_default();
                let url = match Url::parse(raw_url) {
                    Ok(url) if matches!(url.scheme(), "http" | "https") => url,
                    _ => return ToolOutput::error("invalid HTTP URL"),
                };
                // THREAT[TM-CAP-004]: A model URL grants no authority. The
                // host allowlist is checked before the host transport runs.
                if !allowlist.allows(&url) {
                    return ToolOutput::error("HTTP URL is not allowed");
                }
                let headers = match parse_headers(arguments.get("headers")) {
                    Ok(headers) => headers,
                    Err(error) => return ToolOutput::error(error),
                };
                let body = arguments["body"]
                    .as_str()
                    .map(|body| body.as_bytes().to_vec());
                if body
                    .as_ref()
                    .is_some_and(|body| body.len() > MAX_TOOL_INPUT_BYTES)
                {
                    return ToolOutput::error("HTTP request body limit exceeded");
                }
                let request = HttpRequest {
                    method: method.to_owned(),
                    url: url.to_string(),
                    headers,
                    body,
                    max_response_bytes: MAX_TOOL_OUTPUT_BYTES,
                };
                let response = match tokio::time::timeout(HTTP_TIMEOUT, transport.send(request))
                    .await
                {
                    Ok(Ok(response)) => response,
                    Ok(Err(error)) => return ToolOutput::error(error.to_string()),
                    Err(_) => return ToolOutput::error(HttpTransportError::Timeout.to_string()),
                };
                if response.body.len() > MAX_TOOL_OUTPUT_BYTES {
                    return ToolOutput::error(HttpTransportError::TooLarge.to_string());
                }
                let response_header_bytes = response
                    .headers
                    .iter()
                    .try_fold(0usize, |size, (name, value)| {
                        size.checked_add(name.len() + value.len())
                    });
                if response.headers.len() > 64
                    || response_header_bytes.is_none_or(|size| size > 64 * 1024)
                {
                    return ToolOutput::error(HttpTransportError::TooLarge.to_string());
                }
                let body = String::from_utf8_lossy(&response.body);
                ToolOutput::text(
                    json!({"status": response.status, "headers": response.headers, "body": body})
                        .to_string(),
                )
            }
        },
    ))
}

fn parse_headers(value: Option<&JsonValue>) -> Result<BTreeMap<String, String>, &'static str> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let Some(object) = value.as_object() else {
        return Err("HTTP headers must be an object");
    };
    if object.len() > 64 {
        return Err("HTTP header count limit exceeded");
    }
    let mut headers = BTreeMap::new();
    let mut total_bytes = 0usize;
    for (name, value) in object {
        let Some(value) = value.as_str() else {
            return Err("HTTP header values must be text");
        };
        if name.len() + value.len() > 8 * 1024
            || name.contains(['\r', '\n'])
            || value.contains(['\r', '\n'])
        {
            return Err("invalid HTTP header");
        }
        total_bytes = total_bytes
            .checked_add(name.len() + value.len())
            .ok_or("HTTP header size limit exceeded")?;
        if total_bytes > 64 * 1024 {
            return Err("HTTP header size limit exceeded");
        }
        headers.insert(name.clone(), value.to_owned());
    }
    Ok(headers)
}

fn build_llm_tool(config: ModelTool, system_prompt: Option<String>) -> Arc<dyn Tool> {
    Arc::new(FnTool::new(
        "llm",
        "Call the host-selected nested model with one prompt.",
        json!({
            "type": "object",
            "properties": {"prompt": {"type": "string"}},
            "required": ["prompt"]
        }),
        move |arguments| {
            let config = config.clone();
            let system_prompt = system_prompt.clone();
            async move {
                let prompt = arguments["prompt"].as_str().unwrap_or_default();
                if prompt.is_empty() || prompt.len() > MAX_TOOL_INPUT_BYTES {
                    return ToolOutput::error("llm prompt is empty or exceeds its limit");
                }
                // THREAT[TM-EFF-005]: Model calls require an explicit
                // host-selected driver and remain outside Svit transactions.
                let request =
                    ChatRequest::new(config.model.clone(), vec![Message::user(prompt.to_owned())])
                        .maybe_system_prompt(system_prompt);
                match config.driver.complete(request).await {
                    Ok(response) => ToolOutput::text(response.message.text()),
                    Err(_) => ToolOutput::error("llm request failed"),
                }
            }
        },
    ))
}

struct SpawnTool {
    parent: Arc<Mutex<Process>>,
    config: ModelTool,
    system_prompt: Option<String>,
    children: Arc<Mutex<BTreeMap<ProcessId, ChildEntry>>>,
}

fn build_spawn_tool(tool: SpawnTool) -> Arc<dyn Tool> {
    let tool = Arc::new(tool);
    Arc::new(FnTool::new(
        "spawn",
        "Fork the committed process, run one child Svit turn, and retain its snapshot.",
        json!({
            "type": "object",
            "properties": {"id": {"type": "string"}, "task": {"type": "string"}},
            "required": ["id", "task"]
        }),
        move |arguments| {
            let tool = tool.clone();
            async move { tool.execute(arguments).await }
        },
    ))
}

impl SpawnTool {
    async fn execute(&self, arguments: JsonValue) -> ToolOutput {
        let child_id = match arguments["id"]
            .as_str()
            .and_then(|id| ProcessId::new(id).ok())
        {
            Some(id) => id,
            None => return ToolOutput::error("invalid child address"),
        };
        let task = arguments["task"].as_str().unwrap_or_default();
        if task.is_empty() || task.len() > MAX_TOOL_INPUT_BYTES {
            return ToolOutput::error("spawn task is empty or exceeds its limit");
        }
        {
            let Ok(mut children) = self.children.lock() else {
                return ToolOutput::error("child registry unavailable");
            };
            if children.contains_key(&child_id) {
                return ToolOutput::error("child address already exists");
            }
            children.insert(child_id.clone(), ChildEntry::Starting);
        }
        let child_process = match self.parent.lock() {
            Ok(parent) => match parent.fork(child_id.as_str()) {
                Ok(child) => child,
                Err(_) => return self.fail(&child_id, "child fork failed"),
            },
            Err(_) => return self.fail(&child_id, "parent unavailable"),
        };

        // THREAT[TM-FORK-002]: A child starts from Process::fork. Model
        // authority is supplied separately and never inferred from state.
        let mut builder = crate::Svit::resume(child_process)
            .model(self.config.model.clone())
            .driver(SharedDriver(self.config.driver.clone()));
        if let Some(system_prompt) = &self.system_prompt {
            builder = builder.system_prompt(system_prompt.clone());
        }
        let mut child = match builder.build().await {
            Ok(child) => child,
            Err(_) => return self.fail(&child_id, "child construction failed"),
        };
        let inbox = child.inbox();
        let mut outbox = child.outbox();
        if child.start().is_err() || inbox.send(Message::user(task.to_owned())).is_err() {
            return self.fail(&child_id, "child start failed");
        }
        drop(inbox);
        if child.block().await.is_err() {
            return self.fail(&child_id, "child turn failed");
        }
        let response = match outbox.try_recv() {
            Ok(response) => response,
            Err(_) => return self.fail(&child_id, "child produced no response"),
        };
        let process = child.process();
        match self.children.lock() {
            Ok(mut children) if matches!(children.get(&child_id), Some(ChildEntry::Starting)) => {
                children.insert(child_id.clone(), ChildEntry::Ready(process));
            }
            _ => return ToolOutput::error("child address already exists"),
        }
        ToolOutput::text(json!({"id": child_id.as_str(), "response": response.text()}).to_string())
    }

    fn fail(&self, child_id: &ProcessId, message: &'static str) -> ToolOutput {
        if let Ok(mut children) = self.children.lock()
            && matches!(children.get(child_id), Some(ChildEntry::Starting))
        {
            children.remove(child_id);
        }
        ToolOutput::error(message)
    }
}

fn path_is_within(root: &str, candidate: &str) -> bool {
    root == "/"
        || candidate == root
        || candidate
            .strip_prefix(root.trim_end_matches('/'))
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[derive(Clone)]
struct SharedDriver(Arc<dyn ChatDriver>);

#[async_trait]
impl ChatDriver for SharedDriver {
    fn id(&self) -> DriverId {
        self.0.id()
    }

    async fn complete(&self, request: ChatRequest) -> agentyk::Result<agentyk::ChatResponse> {
        self.0.complete(request).await
    }
}
