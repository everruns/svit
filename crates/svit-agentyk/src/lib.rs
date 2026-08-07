//! Generic Agentyk tools for one Svit process.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use agentyk::{Capability, FnTool, SystemPromptContext, Tool, ToolOutput};
use async_trait::async_trait;
use serde_json::{Value as JsonValue, json};
use svit::{Process, Value};

/// An Agentyk capability exposing Svit's five generic process operations.
#[derive(Clone)]
pub struct SvitCapability {
    process: Arc<Mutex<Process>>,
    access: Access,
}

#[derive(Clone)]
enum Access {
    Full,
    ReadExec(Arc<BTreeSet<String>>),
}

impl SvitCapability {
    /// Wraps one Svit process.
    pub fn new(process: Process) -> Self {
        Self {
            process: Arc::new(Mutex::new(process)),
            access: Access::Full,
        }
    }

    /// Wraps one process with read-only discovery plus an allowlist of scripts.
    ///
    /// The returned capability exposes only `discover`, `read`, and `exec`.
    /// Calls to `exec` fail before activation when the script is not listed.
    pub fn read_exec<I, S>(process: Process, allowed_scripts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            process: Arc::new(Mutex::new(process)),
            access: Access::ReadExec(Arc::new(
                allowed_scripts.into_iter().map(Into::into).collect(),
            )),
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
        Some(match &self.access {
            Access::Full => {
                "Use absolute Svit paths with discover, read, write, and remove. Scripts are under \
                 /lib and run through exec. Writes, removes, and script executions are transactional."
                    .into()
            }
            Access::ReadExec(allowed_scripts) => format!(
                "Use absolute Svit paths with discover and read. Run only these transactional \
                 scripts through exec: {}.",
                allowed_scripts.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
        })
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
        let allowed_scripts = match &self.access {
            Access::Full => None,
            Access::ReadExec(scripts) => Some(Arc::clone(scripts)),
        };
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
                let allowed_scripts = allowed_scripts.clone();
                async move {
                    let script = arguments["script"].as_str().unwrap_or_default();
                    // THREAT[TM-CAP-002]: Check host-selected script authority
                    // before guest input conversion or process activation.
                    if allowed_scripts
                        .as_ref()
                        .is_some_and(|scripts| !scripts.contains(script))
                    {
                        return ToolOutput::error(format!("script is not allowed: {script}"));
                    }
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

        Ok(match self.access {
            Access::Full => vec![discover, read, write, remove, exec_tool],
            Access::ReadExec(_) => vec![discover, read, exec_tool],
        })
    }
}

#[cfg(test)]
mod tests {
    use agentyk::{Capability, SessionId, ToolContext, TurnId};
    use serde_json::json;
    use svit::{Process, Script};

    use super::SvitCapability;

    #[tokio::test]
    async fn restricted_capability_omits_mutators_and_denies_unlisted_scripts() {
        // THREAT[TM-CAP-002]: An untrusted model receives only the operations
        // and script authority selected by its host.
        let process = Process::builder("svit://local/restricted-agent")
            .unwrap()
            .library("allowed", Script::new("(define (main input) input)"))
            .library("denied", Script::new("(define (main input) input)"))
            .build()
            .unwrap();
        let capability = SvitCapability::read_exec(process, ["allowed"]);
        let tools = capability.tools().await.unwrap();
        let names = tools
            .iter()
            .map(|tool| tool.definition().name)
            .collect::<Vec<_>>();

        assert_eq!(names, ["discover", "read", "exec"]);

        let exec = tools
            .iter()
            .find(|tool| tool.definition().name == "exec")
            .unwrap();
        let output = exec
            .execute(
                json!({"script": "denied", "input": null}),
                &ToolContext::new(SessionId::new(), TurnId::new()),
            )
            .await;
        assert!(output.is_error);
        assert_eq!(output.content, "script is not allowed: denied");
        assert_eq!(capability.process().lock().unwrap().version(), 0);

        let output = exec
            .execute(
                json!({"script": "allowed", "input": {"value": 1}}),
                &ToolContext::new(SessionId::new(), TurnId::new()),
            )
            .await;
        assert!(!output.is_error);
        assert_eq!(capability.process().lock().unwrap().version(), 1);
    }
}
