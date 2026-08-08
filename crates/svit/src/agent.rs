//! Process-owned agent loop implemented by Agentyk.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use agentyk::{
    Agent as AgentykAgent, AgentBuilder as AgentykAgentBuilder, Capability, ChatDriver, Event,
    EventId, EventLog, EventRequest, ExpectedVersion, FnTool, Message, ModelSpec, Role, Session,
    SessionId, SessionPoint, SystemPromptContext, Tool, ToolOutput, TurnResult,
    messages_from_events,
};
use async_trait::async_trait;
use serde_json::{Value as JsonValue, json};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::{
    Activation, ActivationHook, Executables, Limits, MessageIntent, Process, ProcessBuilder,
    ProcessId, Script, SnapshotMount, Value,
};

const AGENT_STATE_PATH: &str = "/agent";
const AGENT_STATE_FORMAT: &str = "svit-agent@2";

/// Failure to construct or run a process-owned agent loop.
#[derive(Debug, Error)]
pub enum AgentError {
    /// The Agentyk loop or durable event protocol failed.
    #[error(transparent)]
    Engine(#[from] agentyk::Error),

    /// The shared process state could not be locked.
    #[error("Svit agent process is unavailable")]
    ProcessUnavailable,

    /// A Svit process transition failed.
    #[error(transparent)]
    Process(#[from] crate::Error),

    /// Internal loop assembly did not receive its owning process.
    #[error("Svit agent process is required")]
    MissingProcess,

    /// The inbox no longer accepts new messages.
    #[error("Svit inbox is closed")]
    InboxClosed,

    /// The process loop has already started.
    #[error("Svit process loop has already started")]
    AlreadyStarted,

    /// The process loop has not started.
    #[error("Svit process loop has not started")]
    NotStarted,

    /// A committed inbox value is not an Agentyk message.
    #[error("invalid Svit inbox message: {0}")]
    InvalidInbox(String),

    /// The independently running process task failed.
    #[error("Svit process task failed")]
    TaskFailed,

    /// A completed turn did not produce an assistant message.
    #[error("Svit turn completed without an assistant message")]
    MissingOutboxMessage,
}

/// Result returned by the process-owned agent API.
pub type AgentResult<T> = std::result::Result<T, AgentError>;

/// Result returned by the runnable Svit API.
pub type SvitResult<T> = AgentResult<T>;

/// One configured, independently running Svit process.
pub struct Svit {
    agent: Option<Agent>,
    process: Arc<Mutex<Process>>,
    inbox_state: Arc<Mutex<InboxState>>,
    command_sender: mpsc::UnboundedSender<RuntimeCommand>,
    command_receiver: Option<mpsc::UnboundedReceiver<RuntimeCommand>>,
    outbox_sender: broadcast::Sender<Message>,
    executables: Option<Executables>,
    task: Option<JoinHandle<AgentResult<Agent>>>,
}

/// Cloneable handle for committing messages to one Svit process inbox.
#[derive(Clone)]
pub struct Inbox {
    process: Arc<Mutex<Process>>,
    state: Arc<Mutex<InboxState>>,
    command_sender: mpsc::UnboundedSender<RuntimeCommand>,
}

struct InboxState {
    accepting: bool,
}

#[derive(Clone, Copy)]
enum RuntimeCommand {
    Wake,
    Stop,
}

impl Svit {
    /// Starts one process and loop definition around `id`.
    pub fn builder(id: impl Into<String>) -> crate::Result<SvitBuilder> {
        Ok(SvitBuilder {
            process: Process::builder(id)?,
            agent: AgentBuilder::detached(),
        })
    }

    /// Configures a runnable Svit around a restored or forked process.
    pub fn resume(process: Process) -> SvitResumeBuilder {
        SvitResumeBuilder {
            agent: AgentBuilder::new(process),
        }
    }

    fn from_agent(agent: Agent) -> Self {
        let process = agent.process();
        let executables = agent.executables.clone();
        let (command_sender, command_receiver) = mpsc::unbounded_channel();
        let (outbox_sender, _) = broadcast::channel(64);
        Self {
            agent: Some(agent),
            process,
            inbox_state: Arc::new(Mutex::new(InboxState { accepting: true })),
            command_sender,
            command_receiver: Some(command_receiver),
            outbox_sender,
            executables,
            task: None,
        }
    }

    /// Returns a cloneable handle to the durable process inbox.
    pub fn inbox(&self) -> Inbox {
        Inbox {
            process: self.process.clone(),
            state: self.inbox_state.clone(),
            command_sender: self.command_sender.clone(),
        }
    }

    /// Subscribes to completed turns from this process lifetime.
    ///
    /// The returned receiver may await before, during, or after a turn. It
    /// observes outputs emitted after this subscription was created.
    pub fn outbox(&self) -> broadcast::Receiver<Message> {
        self.outbox_sender.subscribe()
    }

    /// Starts the Agentyk loop as an independent Tokio task.
    pub fn start(&mut self) -> SvitResult<()> {
        if self.task.is_some() || self.command_receiver.is_none() {
            return Err(AgentError::AlreadyStarted);
        }
        let agent = self.agent.take().ok_or(AgentError::AlreadyStarted)?;
        let receiver = self
            .command_receiver
            .take()
            .ok_or(AgentError::AlreadyStarted)?;
        let outbox = self.outbox_sender.clone();
        self.task = Some(tokio::spawn(run_process_loop(agent, receiver, outbox)));
        Ok(())
    }

    /// Seals the inbox, drains committed messages, and waits for the loop.
    pub async fn block(&mut self) -> SvitResult<()> {
        let task = self.task.take().ok_or(AgentError::NotStarted)?;
        {
            let mut state = self
                .inbox_state
                .lock()
                .map_err(|_| AgentError::ProcessUnavailable)?;
            state.accepting = false;
            self.command_sender
                .send(RuntimeCommand::Stop)
                .map_err(|_| AgentError::TaskFailed)?;
        }
        self.agent = Some(task.await.map_err(|_| AgentError::TaskFailed)??);
        Ok(())
    }

    /// Returns the durable conversation projection.
    pub fn messages(&self) -> SvitResult<&[Message]> {
        self.agent
            .as_ref()
            .map(Agent::messages)
            .ok_or(AgentError::AlreadyStarted)
    }

    /// Returns the process identifier.
    pub fn id(&self) -> SvitResult<ProcessId> {
        Ok(self
            .process
            .lock()
            .map_err(|_| AgentError::ProcessUnavailable)?
            .id()
            .clone())
    }

    /// Returns the current committed process version.
    pub fn version(&self) -> SvitResult<u64> {
        Ok(self
            .process
            .lock()
            .map_err(|_| AgentError::ProcessUnavailable)?
            .version())
    }

    /// Returns the configured process limits.
    pub fn limits(&self) -> SvitResult<Limits> {
        Ok(self
            .process
            .lock()
            .map_err(|_| AgentError::ProcessUnavailable)?
            .limits()
            .clone())
    }

    /// Discovers immediate children under a process path.
    pub fn discover(&self, path: &str) -> SvitResult<Vec<String>> {
        Ok(self
            .process
            .lock()
            .map_err(|_| AgentError::ProcessUnavailable)?
            .discover(path)?)
    }

    /// Reads a cloned committed value from the process.
    pub fn read(&self, path: &str) -> SvitResult<Option<Value>> {
        Ok(self
            .process
            .lock()
            .map_err(|_| AgentError::ProcessUnavailable)?
            .read(path)?
            .cloned())
    }

    /// Commits a host write through the process path contract.
    pub fn write(&mut self, path: &str, value: Value) -> SvitResult<()> {
        Ok(self
            .process
            .lock()
            .map_err(|_| AgentError::ProcessUnavailable)?
            .write(path, value)?)
    }

    /// Commits a host removal through the process path contract.
    pub fn remove(&mut self, path: &str) -> SvitResult<()> {
        Ok(self
            .process
            .lock()
            .map_err(|_| AgentError::ProcessUnavailable)?
            .remove(path)?)
    }

    /// Executes one transactional process script by its absolute `/lib` path.
    pub fn exec(&mut self, path: &str, input: Value) -> SvitResult<Activation> {
        Ok(self
            .process
            .lock()
            .map_err(|_| AgentError::ProcessUnavailable)?
            .exec(path, input)?)
    }

    /// Returns committed Lisp message intents from process state.
    pub fn message_intents(&self) -> SvitResult<Vec<MessageIntent>> {
        Ok(self
            .process
            .lock()
            .map_err(|_| AgentError::ProcessUnavailable)?
            .outbox()?)
    }

    /// Snapshots process state and the durable agent thread together.
    pub fn snapshot(&self) -> SvitResult<Vec<u8>> {
        Ok(self
            .process
            .lock()
            .map_err(|_| AgentError::ProcessUnavailable)?
            .snapshot()?)
    }

    /// Returns the current committed process root hash.
    pub fn root_hash(&self) -> SvitResult<String> {
        Ok(self
            .process
            .lock()
            .map_err(|_| AgentError::ProcessUnavailable)?
            .root_hash())
    }

    /// Forks the complete state into an independently mutable child process.
    pub fn fork_process(&self, child_id: impl Into<String>) -> SvitResult<Process> {
        Ok(self
            .process
            .lock()
            .map_err(|_| AgentError::ProcessUnavailable)?
            .fork(child_id)?)
    }

    /// Returns a shared handle for host integrations requiring `Process`.
    pub fn process(&self) -> Arc<Mutex<Process>> {
        self.process.clone()
    }

    /// Returns completed child addresses created through `/bin/spawn`.
    pub fn child_ids(&self) -> Vec<ProcessId> {
        self.executables
            .as_ref()
            .map(Executables::child_ids)
            .unwrap_or_default()
    }

    /// Snapshots one completed child created through `/bin/spawn`.
    pub fn child_snapshot(&self, id: &ProcessId) -> SvitResult<Option<Vec<u8>>> {
        Ok(self
            .executables
            .as_ref()
            .map(|tools| tools.child_snapshot(id))
            .transpose()?
            .flatten())
    }
}

impl Inbox {
    /// Atomically commits one message, then wakes the process loop.
    ///
    /// Sending remains valid while another turn is running. The committed
    /// message stays in queue order and starts the next turn.
    pub fn send(&self, message: Message) -> SvitResult<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| AgentError::ProcessUnavailable)?;
        if !state.accepting {
            return Err(AgentError::InboxClosed);
        }
        let json = serde_json::to_value(message)
            .map_err(|error| AgentError::InvalidInbox(error.to_string()))?;
        let value = Value::from_json(json)?;
        self.process
            .lock()
            .map_err(|_| AgentError::ProcessUnavailable)?
            .enqueue_inbox(value)?;
        self.command_sender
            .send(RuntimeCommand::Wake)
            .map_err(|_| AgentError::TaskFailed)?;
        drop(state);
        Ok(())
    }
}

async fn run_process_loop(
    mut agent: Agent,
    mut commands: mpsc::UnboundedReceiver<RuntimeCommand>,
    outbox: broadcast::Sender<Message>,
) -> AgentResult<Agent> {
    loop {
        let next = {
            let process = agent.process();
            process
                .lock()
                .map_err(|_| AgentError::ProcessUnavailable)?
                .inbox_front()?
                .cloned()
        };
        if let Some(value) = next {
            let message: Message = serde_json::from_value(value.to_json())
                .map_err(|error| AgentError::InvalidInbox(error.to_string()))?;
            agent.run(message).await?;
            let response = agent
                .messages()
                .last()
                .filter(|message| message.role == Role::Assistant)
                .cloned()
                .ok_or(AgentError::MissingOutboxMessage)?;
            agent
                .process()
                .lock()
                .map_err(|_| AgentError::ProcessUnavailable)?
                .acknowledge_inbox(&value)?;
            let _ = outbox.send(response);
            continue;
        }

        match commands.recv().await {
            Some(RuntimeCommand::Wake) => {}
            Some(RuntimeCommand::Stop) | None => return Ok(agent),
        }
    }
}

/// Loop configuration for a restored or forked process.
pub struct SvitResumeBuilder {
    agent: AgentBuilder,
}

impl SvitResumeBuilder {
    /// Names the agent loop for host diagnostics.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.agent = self.agent.name(name);
        self
    }

    /// Sets the agent's system prompt.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.agent = self.agent.system_prompt(prompt);
        self
    }

    /// Selects the model used by the loop.
    pub fn model(mut self, model: ModelSpec) -> Self {
        self.agent = self.agent.model(model);
        self
    }

    /// Registers the driver for the configured model.
    pub fn driver(mut self, driver: impl ChatDriver + 'static) -> Self {
        self.agent = self.agent.driver(driver);
        self
    }

    /// Sets the maximum reason/act iterations for one turn.
    pub fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.agent = self.agent.max_iterations(max_iterations);
        self
    }

    /// Restricts the model to discovery, reads, and named scripts.
    pub fn allow_scripts<I, S>(mut self, scripts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.agent = self.agent.allow_scripts(scripts);
        self
    }

    /// Installs native `/bin` executables with explicit host grants.
    pub fn executables(mut self, executables: Executables) -> Self {
        self.agent = self.agent.executables(executables);
        self
    }

    /// Reopens the process and resumes its durable agent thread.
    pub async fn build(self) -> SvitResult<Svit> {
        Ok(Svit::from_agent(self.agent.build().await?))
    }
}

/// Builder combining process definition and agent-loop configuration.
pub struct SvitBuilder {
    process: ProcessBuilder,
    agent: AgentBuilder,
}

impl SvitBuilder {
    /// Adds initial process memory.
    pub fn memory(mut self, name: impl Into<String>, value: Value) -> Self {
        self.process = self.process.memory(name, value);
        self
    }

    /// Adds a named process script.
    pub fn library(mut self, name: impl Into<String>, script: Script) -> Self {
        self.process = self.process.library(name, script);
        self
    }

    /// Adds a bounded read-only process mount.
    pub fn mount(mut self, name: impl Into<String>, mount: SnapshotMount) -> Self {
        self.process = self.process.mount(name, mount);
        self
    }

    /// Replaces process resource limits.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.process = self.process.limits(limits);
        self
    }

    /// Registers a typed process activation hook.
    pub fn hook(mut self, hook: impl ActivationHook + 'static) -> Self {
        self.process = self.process.hook(hook);
        self
    }

    /// Names the agent loop for host diagnostics.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.agent = self.agent.name(name);
        self
    }

    /// Sets the agent's system prompt.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.agent = self.agent.system_prompt(prompt);
        self
    }

    /// Selects the model used by the loop.
    pub fn model(mut self, model: ModelSpec) -> Self {
        self.agent = self.agent.model(model);
        self
    }

    /// Registers the driver for the configured model.
    pub fn driver(mut self, driver: impl ChatDriver + 'static) -> Self {
        self.agent = self.agent.driver(driver);
        self
    }

    /// Sets the maximum reason/act iterations for one turn.
    pub fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.agent = self.agent.max_iterations(max_iterations);
        self
    }

    /// Restricts the model to discovery, reads, and named scripts.
    pub fn allow_scripts<I, S>(mut self, scripts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.agent = self.agent.allow_scripts(scripts);
        self
    }

    /// Installs native `/bin` executables with explicit host grants.
    pub fn executables(mut self, executables: Executables) -> Self {
        self.agent = self.agent.executables(executables);
        self
    }

    /// Builds the process and its runnable loop as one Svit instance.
    pub async fn build(self) -> SvitResult<Svit> {
        let process = self.process.build()?;
        let agent = self.agent.with_process(process).build().await?;
        Ok(Svit::from_agent(agent))
    }
}

/// A running agent represented by one Svit process.
///
/// Svit owns the process, durable thread, snapshot, and fork boundary. Agentyk
/// implements the host-side reason/act loop. Every durable Agentyk event is
/// committed under the host-managed `/agent` node.
pub struct Agent {
    process: Arc<Mutex<Process>>,
    session: Session,
    executables: Option<Executables>,
}

impl Agent {
    /// Runs one turn in the process-owned thread.
    pub async fn run(&mut self, input: impl Into<Message>) -> AgentResult<TurnResult> {
        Ok(self.session.run(input).await?)
    }

    /// Returns the conversation reconstructed from committed process events.
    pub fn messages(&self) -> &[Message] {
        self.session.messages()
    }

    /// Returns a shared handle for host-side inspection of committed state.
    pub fn process(&self) -> Arc<Mutex<Process>> {
        self.process.clone()
    }
}

/// Builder for one process-owned agent loop.
pub struct AgentBuilder {
    process: Option<Arc<Mutex<Process>>>,
    access: AgentAccess,
    inner: AgentykAgentBuilder,
    system_prompt: Option<String>,
    executables: Option<Executables>,
}

impl AgentBuilder {
    fn new(process: Process) -> Self {
        Self::detached().with_process(process)
    }

    fn detached() -> Self {
        Self {
            process: None,
            access: AgentAccess::Full,
            inner: AgentykAgent::builder(),
            system_prompt: None,
            executables: None,
        }
    }

    fn with_process(mut self, process: Process) -> Self {
        self.process = Some(Arc::new(Mutex::new(process)));
        self
    }

    /// Names the loop for host diagnostics.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.inner = self.inner.name(name);
        self
    }

    /// Sets the agent's instructions.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Selects the model used by the Agentyk loop.
    pub fn model(mut self, model: ModelSpec) -> Self {
        self.inner = self.inner.model(model);
        self
    }

    /// Registers the driver for the configured model.
    pub fn driver(mut self, driver: impl ChatDriver + 'static) -> Self {
        self.inner = self.inner.driver(driver);
        self
    }

    /// Sets the maximum Agentyk reason/act iterations per turn.
    pub fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.inner = self.inner.max_iterations(max_iterations);
        self
    }

    /// Restricts the model to discovery, reads, and named scripts.
    pub fn allow_scripts<I, S>(mut self, scripts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.access =
            AgentAccess::ReadExec(Arc::new(scripts.into_iter().map(Into::into).collect()));
        self
    }

    /// Installs native `/bin` executables with explicit host grants.
    pub fn executables(mut self, executables: Executables) -> Self {
        self.executables = Some(executables);
        self
    }

    /// Builds the loop and resumes the thread already committed in the process.
    pub async fn build(self) -> AgentResult<Agent> {
        let process = self.process.ok_or(AgentError::MissingProcess)?;
        let stored_prompt = {
            let process = process.lock().map_err(|_| AgentError::ProcessUnavailable)?;
            load_agent_state(&process)
                .map_err(AgentError::Engine)?
                .map(|state| state.system_prompt)
        };
        let system_prompt = self.system_prompt.or(stored_prompt).unwrap_or_default();
        let executable_catalog = self
            .executables
            .as_ref()
            .map(|tools| tools.catalog(process.clone()))
            .unwrap_or_else(|| json!({}));
        {
            // THREAT[TM-CAP-005]: `/bin` is refreshed from current host
            // configuration before the engine can run a turn. Descriptors do
            // not grant execution authority.
            let mut owned = process.lock().map_err(|_| AgentError::ProcessUnavailable)?;
            owned.replace_executables(Value::from_json(executable_catalog)?)?;
        }
        let capability = ProcessCapability {
            process: process.clone(),
            access: self.access,
            executables: self.executables.clone(),
        };
        let engine = self
            .inner
            .system_prompt(system_prompt.clone())
            .capability(capability)
            .build()?;
        let log = Arc::new(ProcessEventLog::new(process.clone(), system_prompt));
        let session = match log.session_id()? {
            Some(session_id) => engine.resume_session(log.clone(), session_id).await?,
            None => engine.session_with_log(log.clone()),
        };
        log.initialize(session.id())?;
        Ok(Agent {
            process,
            session,
            executables: self.executables,
        })
    }
}

#[derive(Clone)]
enum AgentAccess {
    Full,
    ReadExec(Arc<BTreeSet<String>>),
}

#[derive(Clone)]
struct ProcessCapability {
    process: Arc<Mutex<Process>>,
    access: AgentAccess,
    executables: Option<Executables>,
}

fn execute_script(process: &Arc<Mutex<Process>>, path: &str, input: JsonValue) -> ToolOutput {
    let input = match Value::from_json(input) {
        Ok(input) => input,
        Err(error) => return ToolOutput::error(error.to_string()),
    };
    let Ok(mut process) = process.lock() else {
        return ToolOutput::error("Svit process lock is unavailable");
    };
    let activation = match process.exec(path, input) {
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
impl Capability for ProcessCapability {
    fn id(&self) -> &str {
        "svit"
    }

    async fn system_prompt_contribution(&self, _context: &SystemPromptContext) -> Option<String> {
        let mut contribution = match &self.access {
            AgentAccess::Full => {
                "Your durable thread and workspace belong to one Svit process. Use absolute paths \
                 with discover, read, write, remove, and exec. Discover executable manuals under \
                 /bin; run /bin executables or /lib scripts through exec. /agent/system_prompt, \
                 /agent/messages, and /agent/events are durable host-managed runtime projections."
                    .into()
            }
            AgentAccess::ReadExec(allowed_scripts) => format!(
                "Your durable thread and workspace belong to one Svit process. Use absolute paths \
                 with discover, read, and exec. Discover executable manuals under /bin. Run only \
                 these /lib scripts through exec: {}. /agent/system_prompt, /agent/messages, and \
                 /agent/events are durable host-managed runtime projections.",
                allowed_scripts
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        if self.executables.is_some() {
            contribution.push_str(
                " Native executables are listed under /bin and invoked only through exec(path, input).",
            );
        }
        Some(contribution)
    }

    async fn tools(&self) -> agentyk::Result<Vec<Arc<dyn Tool>>> {
        let discover_process = self.process.clone();
        let discover: Arc<dyn Tool> = Arc::new(FnTool::new(
            "discover",
            "List immediate child names under a Svit process path.",
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

        let exec_process = self.process.clone();
        let executables = self.executables.clone();
        let allowed_scripts = match &self.access {
            AgentAccess::Full => None,
            AgentAccess::ReadExec(scripts) => Some(Arc::clone(scripts)),
        };
        let exec: Arc<dyn Tool> = Arc::new(FnTool::new(
            "exec",
            "Execute an absolute /lib script or /bin native executable path.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "input": {}},
                "required": ["path", "input"]
            }),
            move |arguments| {
                let process = exec_process.clone();
                let allowed_scripts = allowed_scripts.clone();
                let executables = executables.clone();
                async move {
                    let path = arguments["path"].as_str().unwrap_or_default();
                    if let Some(name) = path.strip_prefix("/bin/") {
                        let Some(executables) = executables else {
                            return ToolOutput::error("executable not found");
                        };
                        return executables
                            .execute(name, arguments["input"].clone(), process)
                            .await;
                    }
                    let Some(script) = path.strip_prefix("/lib/") else {
                        return ToolOutput::error("exec path must be under /lib or /bin");
                    };
                    // THREAT[TM-CAP-002]: Check host-selected script authority
                    // before guest input conversion or process activation.
                    if allowed_scripts
                        .as_ref()
                        .is_some_and(|scripts| !scripts.contains(script))
                    {
                        return ToolOutput::error(format!("script is not allowed: {script}"));
                    }
                    execute_script(&process, path, arguments["input"].clone())
                }
            },
        ));

        if matches!(self.access, AgentAccess::ReadExec(_)) {
            return Ok(vec![discover, read, exec]);
        }

        let write_process = self.process.clone();
        let write: Arc<dyn Tool> = Arc::new(FnTool::new(
            "write",
            "Transactionally write process memory or one named script.",
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
            "Transactionally remove process memory or one named script.",
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

        Ok(vec![discover, read, write, remove, exec])
    }
}

#[derive(Clone)]
struct ProcessEventLog {
    process: Arc<Mutex<Process>>,
    system_prompt: Arc<str>,
}

impl ProcessEventLog {
    fn new(process: Arc<Mutex<Process>>, system_prompt: String) -> Self {
        Self {
            process,
            system_prompt: system_prompt.into(),
        }
    }

    fn session_id(&self) -> agentyk::Result<Option<SessionId>> {
        let process = self.process.lock().map_err(|_| event_log_unavailable())?;
        Ok(load_agent_state(&process)?.map(|state| state.session_id))
    }

    fn initialize(&self, session_id: SessionId) -> agentyk::Result<()> {
        let mut process = self.process.lock().map_err(|_| event_log_unavailable())?;
        let state = load_agent_state(&process)?;
        let events = match state {
            Some(state)
                if state.session_id == session_id
                    && state.system_prompt == self.system_prompt.as_ref() =>
            {
                return Ok(());
            }
            Some(state) if state.session_id == session_id => state.events,
            Some(_) => {
                return Err(event_log_error(
                    "Svit process already owns a different agent thread",
                ));
            }
            None => Vec::new(),
        };
        persist_agent_state(
            &mut process,
            session_id,
            self.system_prompt.as_ref(),
            events,
        )
    }
}

struct AgentState {
    session_id: SessionId,
    system_prompt: String,
    events: Vec<Event>,
}

fn load_agent_state(process: &Process) -> agentyk::Result<Option<AgentState>> {
    let Some(value) = process.read(AGENT_STATE_PATH).map_err(event_log_error)? else {
        return Ok(None);
    };
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let json = value.to_json();
    if json.get("format").and_then(JsonValue::as_str) != Some(AGENT_STATE_FORMAT) {
        return Err(event_log_error("invalid /agent format"));
    }
    let session_id = json
        .get("session_id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| event_log_error("missing /agent session_id"))?
        .parse()
        .map_err(event_log_error)?;
    let system_prompt = json
        .get("system_prompt")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| event_log_error("missing /agent system_prompt"))?
        .to_owned();
    let messages: Vec<Message> = serde_json::from_value(
        json.get("messages")
            .cloned()
            .ok_or_else(|| event_log_error("missing /agent messages"))?,
    )
    .map_err(event_log_error)?;
    let events: Vec<Event> = serde_json::from_value(
        json.get("events")
            .cloned()
            .ok_or_else(|| event_log_error("missing /agent events"))?,
    )
    .map_err(event_log_error)?;
    if messages_from_events(&events) != messages {
        return Err(event_log_error(
            "/agent messages do not match the event projection",
        ));
    }
    Ok(Some(AgentState {
        session_id,
        system_prompt,
        events,
    }))
}

fn persist_agent_state(
    process: &mut Process,
    session_id: SessionId,
    system_prompt: &str,
    events: Vec<Event>,
) -> agentyk::Result<()> {
    // THREAT[TM-AUD-001]: Events are canonical. The guest-readable message
    // history is derived at this host-only commit boundary and revalidated on
    // restore so the two representations cannot diverge silently.
    let messages = messages_from_events(&events);
    let value = Value::from_json(json!({
        "format": AGENT_STATE_FORMAT,
        "session_id": session_id.to_string(),
        "system_prompt": system_prompt,
        "messages": messages,
        "events": events,
    }))
    .map_err(event_log_error)?;
    process.replace_agent_state(value).map_err(event_log_error)
}

fn event_log_error(error: impl std::fmt::Display) -> agentyk::Error {
    agentyk::Error::EventLog(error.to_string())
}

fn event_log_unavailable() -> agentyk::Error {
    event_log_error("Svit process lock is unavailable")
}

#[async_trait]
impl EventLog for ProcessEventLog {
    async fn append_batch(
        &self,
        session_id: SessionId,
        expected: ExpectedVersion,
        requests: Vec<EventRequest>,
    ) -> agentyk::Result<Vec<Event>> {
        if requests
            .iter()
            .any(|request| request.session_id != session_id)
        {
            return Err(event_log_error("event batch contains a different session"));
        }

        let mut process = self.process.lock().map_err(|_| event_log_unavailable())?;
        let state = load_agent_state(&process)?;
        let (mut events, actual) = match state {
            Some(state) if state.session_id == session_id => {
                let actual = state.events.len() as u64;
                (state.events, actual)
            }
            Some(_) => {
                return Err(event_log_error(
                    "Svit process already owns a different agent thread",
                ));
            }
            None => (Vec::new(), 0),
        };
        if let ExpectedVersion::Exact(expected) = expected
            && expected != actual
        {
            return Err(agentyk::Error::EventConflict { expected, actual });
        }

        let emitted_at = std::time::SystemTime::now().into();
        let appended = requests
            .into_iter()
            .enumerate()
            .map(|(index, request)| {
                request.into_event(EventId::new(), emitted_at, actual + index as u64 + 1)
            })
            .collect::<Vec<_>>();
        events.extend(appended.iter().cloned());

        // THREAT[TM-DOS-003]: Prompts, model output, and tool values remain
        // untrusted; Svit value validation and process limits fail closed.
        persist_agent_state(
            &mut process,
            session_id,
            self.system_prompt.as_ref(),
            events,
        )?;
        Ok(appended)
    }

    async fn read_after(
        &self,
        session_id: SessionId,
        sequence: Option<u64>,
    ) -> agentyk::Result<Vec<Event>> {
        let process = self.process.lock().map_err(|_| event_log_unavailable())?;
        let Some(state) = load_agent_state(&process)? else {
            return Ok(Vec::new());
        };
        if state.session_id != session_id {
            return Ok(Vec::new());
        }
        let after = sequence.unwrap_or(0);
        Ok(state
            .events
            .into_iter()
            .filter(|event| event.sequence.is_some_and(|value| value > after))
            .collect())
    }

    async fn head(&self, session_id: SessionId) -> agentyk::Result<SessionPoint> {
        let process = self.process.lock().map_err(|_| event_log_unavailable())?;
        let sequence = load_agent_state(&process)?
            .filter(|state| state.session_id == session_id)
            .and_then(|state| state.events.last().and_then(|event| event.sequence))
            .unwrap_or(0);
        Ok(SessionPoint::new(session_id, sequence))
    }
}
