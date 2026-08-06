//! Generic Agentyk tools for one Svit process.

use std::sync::{Arc, Mutex};

use agentyk::{Capability, FnTool, SystemPromptContext, Tool, ToolOutput};
use async_trait::async_trait;
use serde_json::{Value as JsonValue, json};
use svit::{Process, Value};

/// An Agentyk capability exposing `discover`, `exec`, `get`, and `set` for one
/// Svit process.
#[derive(Clone)]
pub struct SvitCapability {
    process: Arc<Mutex<Process>>,
}

impl SvitCapability {
    /// Wraps one Svit process.
    pub fn new(process: Process) -> Self {
        Self {
            process: Arc::new(Mutex::new(process)),
        }
    }

    /// Returns a shared handle for host-side observation and snapshotting.
    pub fn process(&self) -> Arc<Mutex<Process>> {
        self.process.clone()
    }
}

fn exec(process: &Arc<Mutex<Process>>, script: &str, input: JsonValue) -> ToolOutput {
    let input = match Value::from_json(input) {
        Ok(input) => input,
        Err(error) => return ToolOutput::error(error.to_string()),
    };
    let Ok(mut process) = process.lock() else {
        return ToolOutput::error("Svit process lock is unavailable");
    };
    let activation = match process.exec(script, input) {
        Ok(activation) => activation,
        Err(error) => return ToolOutput::error(error.to_string()),
    };
    let messages = activation
        .messages
        .iter()
        .map(|message| {
            json!({
                "id": message.message_id,
                "to": message.to.as_str(),
                "body": message.body.to_json(),
            })
        })
        .collect::<Vec<_>>();
    ToolOutput::text(
        json!({
            "output": activation.output.to_json(),
            "version": activation.version,
            "messages": messages,
        })
        .to_string(),
    )
}

#[async_trait]
impl Capability for SvitCapability {
    fn id(&self) -> &str {
        "svit"
    }

    async fn system_prompt_contribution(&self, _context: &SystemPromptContext) -> Option<String> {
        Some(
            "Use discover to list children in the Svit tree. Scripts are under /lib; use get to \
             inspect a script and exec to run it. Use get or set for process memory. Svit script \
             executions and set operations are transactional."
                .into(),
        )
    }

    async fn tools(&self) -> agentyk::Result<Vec<Arc<dyn Tool>>> {
        let discover_process = self.process.clone();
        let discover: Arc<dyn Tool> = Arc::new(FnTool::new(
            "discover",
            "List child names under a Svit process path, like dir in Python.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            move |arguments| {
                let process = discover_process.clone();
                async move {
                    let path = arguments["path"].as_str().unwrap_or_default();
                    let Ok(process) = process.lock() else {
                        return ToolOutput::error("Svit process lock is unavailable");
                    };
                    match process.discover(path) {
                        Ok(entries) => {
                            ToolOutput::text(json!({"path": path, "entries": entries}).to_string())
                        }
                        Err(error) => ToolOutput::error(error.to_string()),
                    }
                }
            },
        ));

        let exec_process = self.process.clone();
        let exec_tool: Arc<dyn Tool> = Arc::new(FnTool::new(
            "exec",
            "Run a named Svit script transactionally.",
            json!({
                "type": "object",
                "properties": {"script": {"type": "string"}, "input": {}},
                "required": ["script", "input"]
            }),
            move |arguments| {
                let process = exec_process.clone();
                async move {
                    let script = arguments["script"].as_str().unwrap_or_default();
                    exec(&process, script, arguments["input"].clone())
                }
            },
        ));

        let get_process = self.process.clone();
        let get: Arc<dyn Tool> = Arc::new(FnTool::new(
            "get",
            "Read a value from the Svit process tree by absolute path.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            move |arguments| {
                let process = get_process.clone();
                async move {
                    let path = arguments["path"].as_str().unwrap_or_default();
                    let Ok(process) = process.lock() else {
                        return ToolOutput::error("Svit process lock is unavailable");
                    };
                    match process.get(path) {
                        Ok(value) => ToolOutput::text(
                            json!({
                                "path": path,
                                "value": value.map(Value::to_json),
                                "version": process.version(),
                            })
                            .to_string(),
                        ),
                        Err(error) => ToolOutput::error(error.to_string()),
                    }
                }
            },
        ));

        let set_process = self.process.clone();
        let set: Arc<dyn Tool> = Arc::new(FnTool::new(
            "set",
            "Transactionally set a value below /memory in Svit.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "value": {}},
                "required": ["path", "value"]
            }),
            move |arguments| {
                let process = set_process.clone();
                async move {
                    let Some(path) = arguments["path"].as_str() else {
                        return ToolOutput::error("path must be text");
                    };
                    let value = match Value::from_json(arguments["value"].clone()) {
                        Ok(value) => value,
                        Err(error) => return ToolOutput::error(error.to_string()),
                    };
                    let Ok(mut process) = process.lock() else {
                        return ToolOutput::error("Svit process lock is unavailable");
                    };
                    match process.set(path, value) {
                        Ok(()) => ToolOutput::text(
                            json!({"path": path, "version": process.version()}).to_string(),
                        ),
                        Err(error) => ToolOutput::error(error.to_string()),
                    }
                }
            },
        ));

        Ok(vec![discover, exec_tool, get, set])
    }
}
