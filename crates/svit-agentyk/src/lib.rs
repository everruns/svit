//! Generic Agentyk tools for one Svit process.

use std::sync::{Arc, Mutex};

use agentyk::{Capability, FnTool, SystemPromptContext, Tool, ToolOutput};
use async_trait::async_trait;
use serde_json::{Value as JsonValue, json};
use svit::{Process, Value};

/// An Agentyk capability exposing Svit's five generic process operations.
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
            "Use absolute Svit paths with discover, read, write, and remove. Scripts are under \
             /lib and run through exec. Writes, removes, and script executions are transactional."
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

        let read_process = self.process.clone();
        let read: Arc<dyn Tool> = Arc::new(FnTool::new(
            "read",
            "Read a value from the Svit process tree by absolute path.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            move |arguments| {
                let process = read_process.clone();
                async move {
                    let path = arguments["path"].as_str().unwrap_or_default();
                    let Ok(process) = process.lock() else {
                        return ToolOutput::error("Svit process lock is unavailable");
                    };
                    match process.read(path) {
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

        let write_process = self.process.clone();
        let write: Arc<dyn Tool> = Arc::new(FnTool::new(
            "write",
            "Transactionally write a value by absolute Svit process path.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "value": {}},
                "required": ["path", "value"]
            }),
            move |arguments| {
                let process = write_process.clone();
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
                    match process.write(path, value) {
                        Ok(()) => ToolOutput::text(
                            json!({"path": path, "version": process.version()}).to_string(),
                        ),
                        Err(error) => ToolOutput::error(error.to_string()),
                    }
                }
            },
        ));

        let remove_process = self.process.clone();
        let remove: Arc<dyn Tool> = Arc::new(FnTool::new(
            "remove",
            "Transactionally remove a value by absolute Svit process path.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            move |arguments| {
                let process = remove_process.clone();
                async move {
                    let Some(path) = arguments["path"].as_str() else {
                        return ToolOutput::error("path must be text");
                    };
                    let Ok(mut process) = process.lock() else {
                        return ToolOutput::error("Svit process lock is unavailable");
                    };
                    match process.remove(path) {
                        Ok(()) => ToolOutput::text(
                            json!({"path": path, "version": process.version()}).to_string(),
                        ),
                        Err(error) => ToolOutput::error(error.to_string()),
                    }
                }
            },
        ));

        Ok(vec![discover, read, write, remove, exec_tool])
    }
}
