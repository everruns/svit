//! Process-owned agent loop implemented by Everruns.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use everruns::core::capabilities::{Capability, SystemPromptContext};
use everruns::core::error::Result as EverrunsResult;
use everruns::core::events::{Event, EventData, EventRequest, ToolCompletedData};
use everruns::core::llmsim_driver::LlmSimConfig;
use everruns::core::message_retriever::{InputMessage, MessageRetriever};
use everruns::core::tools::{Tool, ToolExecutionResult, ToolResultImage};
use everruns::core::typed_id::{EventId, MessageId, SessionId};
use everruns::core::{
    AgentCapabilityConfig, AgentLoopError, ContentPart, DriverId, DriverRegistry, Message,
    MessageRole, ResolvedModel,
};
use everruns::runtime::{
    AgentBuilder as RuntimeAgentBuilder, EventBus, HarnessBuilder, InProcessRuntime,
    InProcessRuntimeBuilder, RuntimeBackends, RuntimeMessageStore, SessionBuilder, TurnResult,
};
use serde_json::{Value as JsonValue, json};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::tools::{FunctionTool, tool_error, tool_json};
use crate::{
    Activation, ActivationHook, Executables, Limits, MessageIntent, Process, ProcessBuilder,
    ProcessId, Script, SnapshotMount, Value,
};

const AGENT_STATE_PATH: &str = "/agent";
const AGENT_STATE_FORMAT: &str = "svit-agent@3";
const PROCESS_CAPABILITY_ID: &str = "svit";

/// Host-selected Everruns model runtime.
#[derive(Clone)]
pub struct AgentModel {
    resolved: ResolvedModel,
    drivers: DriverRegistry,
    llm_sim: Option<LlmSimConfig>,
}

/// One deterministic Everruns simulator response used by Svit tests and examples.
#[derive(Clone, Debug, PartialEq)]
pub enum SimTurn {
    /// A final agent text response.
    Text(String),
    /// One model tool call.
    ToolCall {
        /// Model-facing tool name.
        name: String,
        /// JSON tool arguments.
        arguments: JsonValue,
    },
}

impl SimTurn {
    /// Creates one final text response.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    /// Creates one tool-call response.
    pub fn tool_call(name: impl Into<String>, arguments: JsonValue) -> Self {
        Self::ToolCall {
            name: name.into(),
            arguments,
        }
    }

    fn into_everruns(self) -> everruns::core::llmsim_driver::SimTurn {
        match self {
            Self::Text(text) => everruns::core::llmsim_driver::SimTurn::Assistant(text),
            Self::ToolCall { name, arguments } => {
                everruns::core::llmsim_driver::SimTurn::ToolCalls(vec![
                    everruns::core::llmsim_driver::SimToolCall {
                        name,
                        arguments,
                        id: None,
                    },
                ])
            }
        }
    }
}

impl AgentModel {
    /// Configures Everruns' deterministic in-process simulator.
    pub fn simulated(config: LlmSimConfig) -> Self {
        Self {
            resolved: ResolvedModel {
                model: config.model_name.clone(),
                provider_type: DriverId::LlmSim,
                api_key: None,
                base_url: None,
                provider_metadata: None,
            },
            drivers: DriverRegistry::new(),
            llm_sim: Some(config),
        }
    }

    /// Configures a scripted deterministic Everruns simulator.
    pub fn scripted(turns: impl IntoIterator<Item = SimTurn>) -> Self {
        Self::simulated(LlmSimConfig::scripted(
            turns.into_iter().map(SimTurn::into_everruns).collect(),
        ))
    }

    /// Configures the Everruns OpenAI Responses driver.
    pub fn openai(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        let mut drivers = DriverRegistry::new();
        everruns::providers::openai::register_driver(&mut drivers);
        Self {
            resolved: ResolvedModel {
                model: model.into(),
                provider_type: DriverId::OpenAI,
                api_key: Some(api_key.into()),
                base_url: None,
                provider_metadata: None,
            },
            drivers,
            llm_sim: None,
        }
    }

    /// Configures an embedder-provided Everruns driver registry and model.
    pub fn custom(resolved: ResolvedModel, drivers: DriverRegistry) -> Self {
        Self {
            resolved,
            drivers,
            llm_sim: None,
        }
    }

    fn apply(&self, mut builder: InProcessRuntimeBuilder) -> InProcessRuntimeBuilder {
        builder = builder
            .default_model(self.resolved.clone())
            .driver_registry(self.drivers.clone());
        if let Some(config) = &self.llm_sim {
            builder = builder.llm_sim(config.clone());
        }
        builder
    }

    pub(crate) async fn run_once(
        &self,
        system_prompt: Option<String>,
        prompt: String,
    ) -> EverrunsResult<String> {
        let runtime = self
            .apply(InProcessRuntimeBuilder::new().single_session(|session| {
                session
                    .harness("svit-native-llm", system_prompt.unwrap_or_default())
                    .agent("svit-native-llm", "Complete the supplied task.")
            }))
            .build()
            .await?;
        let session_id = runtime
            .default_session_id()
            .ok_or_else(|| AgentLoopError::config("nested Everruns session is missing"))?;
        Ok(runtime
            .run_turn(session_id, InputMessage::user(prompt))
            .await?
            .response)
    }
}

impl fmt::Debug for AgentModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentModel")
            .field("model", &self.resolved.model)
            .field("provider", &self.resolved.provider_type)
            .field("simulated", &self.llm_sim.is_some())
            .finish()
    }
}

/// Failure to construct or run a process-owned agent loop.
#[derive(Debug, Error)]
pub enum AgentError {
    /// The Everruns loop or durable event protocol failed.
    #[error(transparent)]
    Engine(#[from] AgentLoopError),

    /// The shared process state could not be locked.
    #[error("Svit agent process is unavailable")]
    ProcessUnavailable,

    /// A Svit process transition failed.
    #[error(transparent)]
    Process(#[from] crate::Error),

    /// No Everruns model runtime was configured.
    #[error("Svit agent model is required")]
    MissingModel,

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

    /// A committed inbox value is not an Everruns message.
    #[error("invalid Svit inbox message: {0}")]
    InvalidInbox(String),

    /// The independently running process task failed.
    #[error("Svit process task failed")]
    TaskFailed,

    /// A completed turn did not produce an agent message.
    #[error("Svit turn completed without an agent message")]
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
    error_sender: broadcast::Sender<String>,
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
        let (error_sender, _) = broadcast::channel(16);
        Self {
            agent: Some(agent),
            process,
            inbox_state: Arc::new(Mutex::new(InboxState { accepting: true })),
            command_sender,
            command_receiver: Some(command_receiver),
            outbox_sender,
            executables,
            error_sender,
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
    pub fn outbox(&self) -> broadcast::Receiver<Message> {
        self.outbox_sender.subscribe()
    }

    /// Subscribes to sanitized terminal failures from the independently running loop.
    pub fn errors(&self) -> broadcast::Receiver<String> {
        self.error_sender.subscribe()
    }

    /// Starts the Everruns loop as an independent Tokio task.
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
        let errors = self.error_sender.clone();
        let inbox_state = self.inbox_state.clone();
        self.task = Some(tokio::spawn(async move {
            let result = run_process_loop(agent, receiver, outbox).await;
            if let Ok(mut state) = inbox_state.lock() {
                state.accepting = false;
            }
            if let Err(error) = &result {
                // THREAT[TM-INF-001]: Operational subscribers receive the same
                // bounded diagnostic surface as other runtime callers.
                let diagnostic = crate::error::sanitize_diagnostic(error);
                let _ = errors.send(diagnostic);
            }
            result
        }));
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
            let _ = self.command_sender.send(RuntimeCommand::Stop);
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
                .filter(|message| message.role == MessageRole::Agent)
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

    /// Selects the Everruns model runtime used by the loop.
    pub fn model(mut self, model: AgentModel) -> Self {
        self.agent = self.agent.model(model);
        self
    }

    /// Sets the maximum Everruns reason/act iterations for one turn.
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

    /// Selects the Everruns model runtime used by the loop.
    pub fn model(mut self, model: AgentModel) -> Self {
        self.agent = self.agent.model(model);
        self
    }

    /// Sets the maximum Everruns reason/act iterations for one turn.
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

/// A running Everruns agent represented by one Svit process.
pub struct Agent {
    process: Arc<Mutex<Process>>,
    runtime: InProcessRuntime,
    session_id: SessionId,
    messages: Vec<Message>,
    executables: Option<Executables>,
}

impl Agent {
    /// Runs one turn in the process-owned thread.
    pub async fn run(&mut self, input: Message) -> AgentResult<TurnResult> {
        let result = self
            .runtime
            .run_turn(self.session_id, InputMessage::from_message(&input))
            .await?;
        self.messages = self.runtime.messages(self.session_id).await?;
        if !result.success {
            return Err(AgentLoopError::event(
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| format!("Everruns turn stopped: {:?}", result.stop_reason)),
            )
            .into());
        }
        Ok(result)
    }

    /// Returns the conversation reconstructed from committed process events.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Returns a shared handle for host-side inspection of committed state.
    pub fn process(&self) -> Arc<Mutex<Process>> {
        self.process.clone()
    }
}

struct AgentBuilder {
    process: Option<Arc<Mutex<Process>>>,
    access: AgentAccess,
    name: String,
    system_prompt: Option<String>,
    model: Option<AgentModel>,
    max_iterations: usize,
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
            name: "svit-agent".into(),
            system_prompt: None,
            model: None,
            max_iterations: 8,
            executables: None,
        }
    }

    fn with_process(mut self, process: Process) -> Self {
        self.process = Some(Arc::new(Mutex::new(process)));
        self
    }

    fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    fn model(mut self, model: AgentModel) -> Self {
        self.model = Some(model);
        self
    }

    fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    fn allow_scripts<I, S>(mut self, scripts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.access =
            AgentAccess::ReadExec(Arc::new(scripts.into_iter().map(Into::into).collect()));
        self
    }

    fn executables(mut self, executables: Executables) -> Self {
        self.executables = Some(executables);
        self
    }

    async fn build(self) -> AgentResult<Agent> {
        let process = self.process.ok_or(AgentError::MissingProcess)?;
        let model = self.model.ok_or(AgentError::MissingModel)?;
        let stored_state = {
            let process = process.lock().map_err(|_| AgentError::ProcessUnavailable)?;
            load_agent_state(&process).map_err(AgentError::Engine)?
        };
        let system_prompt = self
            .system_prompt
            .or_else(|| {
                stored_state
                    .as_ref()
                    .map(|state| state.system_prompt.clone())
            })
            .unwrap_or_default();
        let session_id = stored_state
            .as_ref()
            .map(|state| state.session_id)
            .unwrap_or_else(SessionId::new);
        let executable_catalog = self
            .executables
            .as_ref()
            .map(|tools| tools.catalog(process.clone()))
            .unwrap_or_else(|| json!({}));
        {
            // THREAT[TM-CAP-005]: `/bin` is refreshed from current host
            // configuration before Everruns can run a turn.
            let mut owned = process.lock().map_err(|_| AgentError::ProcessUnavailable)?;
            owned.replace_executables(Value::from_json(executable_catalog)?)?;
            initialize_agent_state(&mut owned, session_id, &system_prompt)?;
        }

        let capability = ProcessCapability {
            process: process.clone(),
            access: self.access,
            executables: self.executables.clone(),
        };
        let capability_config = AgentCapabilityConfig::new(PROCESS_CAPABILITY_ID);
        let harness =
            HarnessBuilder::new(&self.name, &system_prompt).capability(capability_config.clone());
        let harness_id = harness.harness_id();
        let harness = harness.build();
        let agent = RuntimeAgentBuilder::new(&self.name, &system_prompt)
            .harness_id(harness_id)
            .capability(capability_config.clone())
            .max_iterations(self.max_iterations);
        let agent_id = agent.agent_id();
        let agent = agent.build();
        let session = SessionBuilder::new(harness_id)
            .id(session_id)
            .agent(agent_id)
            .capability(capability_config)
            .max_iterations(self.max_iterations)
            .build();
        let event_bus = Arc::new(ProcessEventBus::new(process.clone(), system_prompt.clone()));
        let message_store = Arc::new(ProcessMessageStore::new(process.clone()));
        let backends = RuntimeBackends::in_memory()
            .with_event_bus(event_bus)
            .with_message_store(message_store);
        let runtime = model
            .apply(
                InProcessRuntimeBuilder::new()
                    .harness(harness)
                    .agent(agent)
                    .session(session)
                    .capability(capability)
                    .backends(backends),
            )
            .build()
            .await?;
        let messages = runtime.messages(session_id).await?;
        Ok(Agent {
            process,
            runtime,
            session_id,
            messages,
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

#[async_trait]
impl Capability for ProcessCapability {
    fn id(&self) -> &str {
        PROCESS_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Svit process"
    }

    fn description(&self) -> &str {
        "Typed access to the owning Svit process"
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

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        process_tools(self)
            .into_iter()
            .map(|tool| Box::new(tool) as Box<dyn Tool>)
            .collect()
    }
}

fn process_tools(capability: &ProcessCapability) -> Vec<FunctionTool> {
    let discover_process = capability.process.clone();
    let discover = FunctionTool::new(
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
                    return tool_error("Svit process lock is unavailable");
                };
                match process.discover(path) {
                    Ok(entries) => tool_json(json!({"path": path, "entries": entries})),
                    Err(error) => tool_error(error.to_string()),
                }
            }
        },
    );

    let read_process = capability.process.clone();
    let read = FunctionTool::new(
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
                    return tool_error("Svit process lock is unavailable");
                };
                match process.read(path) {
                    Ok(value) => tool_json(json!({
                        "path": path,
                        "value": value.map(Value::to_json),
                        "version": process.version(),
                    })),
                    Err(error) => tool_error(error.to_string()),
                }
            }
        },
    );

    let exec_process = capability.process.clone();
    let executables = capability.executables.clone();
    let allowed_scripts = match &capability.access {
        AgentAccess::Full => None,
        AgentAccess::ReadExec(scripts) => Some(Arc::clone(scripts)),
    };
    let exec = FunctionTool::new(
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
                        return tool_error("executable not found");
                    };
                    return executables
                        .execute(name, arguments["input"].clone(), process)
                        .await;
                }
                let Some(script) = path.strip_prefix("/lib/") else {
                    return tool_error("exec path must be under /lib or /bin");
                };
                // THREAT[TM-CAP-002]: Check host-selected script authority
                // before guest input conversion or process activation.
                if allowed_scripts
                    .as_ref()
                    .is_some_and(|scripts| !scripts.contains(script))
                {
                    return tool_error(format!("script is not allowed: {script}"));
                }
                execute_script(&process, path, arguments["input"].clone())
            }
        },
    );

    if matches!(capability.access, AgentAccess::ReadExec(_)) {
        return vec![discover, read, exec];
    }

    let write_process = capability.process.clone();
    let write = FunctionTool::new(
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
                    return tool_error("path must be text");
                };
                let value = match Value::from_json(arguments["value"].clone()) {
                    Ok(value) => value,
                    Err(error) => return tool_error(error.to_string()),
                };
                let Ok(mut process) = process.lock() else {
                    return tool_error("Svit process lock is unavailable");
                };
                match process.write(path, value) {
                    Ok(()) => tool_json(json!({"path": path, "version": process.version()})),
                    Err(error) => tool_error(error.to_string()),
                }
            }
        },
    );

    let remove_process = capability.process.clone();
    let remove = FunctionTool::new(
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
                    return tool_error("path must be text");
                };
                let Ok(mut process) = process.lock() else {
                    return tool_error("Svit process lock is unavailable");
                };
                match process.remove(path) {
                    Ok(()) => tool_json(json!({"path": path, "version": process.version()})),
                    Err(error) => tool_error(error.to_string()),
                }
            }
        },
    );

    vec![discover, read, write, remove, exec]
}

fn execute_script(
    process: &Arc<Mutex<Process>>,
    path: &str,
    input: JsonValue,
) -> ToolExecutionResult {
    let input = match Value::from_json(input) {
        Ok(input) => input,
        Err(error) => return tool_error(error.to_string()),
    };
    let Ok(mut process) = process.lock() else {
        return tool_error("Svit process lock is unavailable");
    };
    let activation = match process.exec(path, input) {
        Ok(activation) => activation,
        Err(error) => return tool_error(error.to_string()),
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
    tool_json(json!({
        "output": activation.output.to_json(),
        "version": activation.version,
        "messages": messages,
    }))
}

#[derive(Clone)]
// Svit owns durability: Everruns emits through this bus, and one process commit
// appends the canonical event plus its derived guest-readable message projection.
struct ProcessEventBus {
    process: Arc<Mutex<Process>>,
    system_prompt: Arc<str>,
}

impl ProcessEventBus {
    fn new(process: Arc<Mutex<Process>>, system_prompt: String) -> Self {
        Self {
            process,
            system_prompt: system_prompt.into(),
        }
    }
}

#[async_trait]
impl everruns::core::EventEmitter for ProcessEventBus {
    async fn emit(&self, request: EventRequest) -> EverrunsResult<Event> {
        let mut process = self
            .process
            .lock()
            .map_err(|_| event_error("Svit process lock is unavailable"))?;
        let mut state = load_agent_state(&process)?.ok_or_else(|| event_error("missing /agent"))?;
        if state.session_id != request.session_id {
            return Err(event_error("event belongs to a different Everruns session"));
        }
        let sequence = i32::try_from(state.events.len() + 1)
            .map_err(|_| event_error("Everruns event sequence limit exceeded"))?;
        let event = request.into_event(EventId::new(), sequence);
        state.events.push(event.clone());
        state.messages = messages_from_events(&state.events);

        // THREAT[TM-DOS-003]: Prompts, model output, and tool values remain
        // untrusted; Svit value validation and process limits fail closed.
        persist_agent_state(
            &mut process,
            state.session_id,
            self.system_prompt.as_ref(),
            state.events,
            state.messages,
        )?;
        Ok(event)
    }
}

#[async_trait]
impl EventBus for ProcessEventBus {
    async fn collected_events(&self) -> Vec<Event> {
        self.process
            .lock()
            .ok()
            .and_then(|process| load_agent_state(&process).ok().flatten())
            .map(|state| state.events)
            .unwrap_or_default()
    }
}

#[derive(Clone)]
struct ProcessMessageStore {
    process: Arc<Mutex<Process>>,
}

impl ProcessMessageStore {
    fn new(process: Arc<Mutex<Process>>) -> Self {
        Self { process }
    }

    fn state(&self) -> EverrunsResult<AgentState> {
        let process = self
            .process
            .lock()
            .map_err(|_| store_error("Svit process lock is unavailable"))?;
        load_agent_state(&process)?.ok_or_else(|| store_error("missing /agent"))
    }
}

#[async_trait]
impl MessageRetriever for ProcessMessageStore {
    async fn get(
        &self,
        session_id: SessionId,
        message_id: MessageId,
    ) -> EverrunsResult<Option<Message>> {
        let state = self.state()?;
        if state.session_id != session_id {
            return Ok(None);
        }
        Ok(state
            .messages
            .into_iter()
            .find(|message| message.id == message_id))
    }

    async fn load(&self, session_id: SessionId) -> EverrunsResult<Vec<Message>> {
        let state = self.state()?;
        Ok(if state.session_id == session_id {
            state.messages
        } else {
            Vec::new()
        })
    }
}

#[async_trait]
impl RuntimeMessageStore for ProcessMessageStore {
    async fn add_input_message(
        &self,
        session_id: SessionId,
        input: InputMessage,
    ) -> EverrunsResult<Message> {
        if self.state()?.session_id != session_id {
            return Err(store_error(
                "message belongs to a different Everruns session",
            ));
        }
        Ok(Message {
            id: MessageId::new(),
            role: input.role,
            content: input.content,
            phase: None,
            thinking: None,
            thinking_signature: None,
            controls: input.controls,
            metadata: input.metadata,
            external_actor: None,
            created_at: std::time::SystemTime::now().into(),
        })
    }

    async fn store_message(&self, session_id: SessionId, message: Message) -> EverrunsResult<()> {
        let state = self.state()?;
        if state.session_id != session_id {
            return Err(store_error(
                "message belongs to a different Everruns session",
            ));
        }
        if !state
            .messages
            .iter()
            .any(|stored| message_semantically_matches(stored, &message))
        {
            return Err(store_error(
                "message is missing from the canonical event projection",
            ));
        }
        Ok(())
    }
}

struct AgentState {
    session_id: SessionId,
    system_prompt: String,
    messages: Vec<Message>,
    events: Vec<Event>,
}

fn initialize_agent_state(
    process: &mut Process,
    session_id: SessionId,
    system_prompt: &str,
) -> EverrunsResult<()> {
    match load_agent_state(process)? {
        Some(state) if state.session_id != session_id => Err(event_error(
            "Svit process already owns a different Everruns session",
        )),
        Some(state) if state.system_prompt == system_prompt => Ok(()),
        Some(state) => persist_agent_state(
            process,
            session_id,
            system_prompt,
            state.events,
            state.messages,
        ),
        None => persist_agent_state(process, session_id, system_prompt, Vec::new(), Vec::new()),
    }
}

fn load_agent_state(process: &Process) -> EverrunsResult<Option<AgentState>> {
    let Some(value) = process.read(AGENT_STATE_PATH).map_err(event_error)? else {
        return Ok(None);
    };
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let json = value.to_json();
    if json.get("format").and_then(JsonValue::as_str) != Some(AGENT_STATE_FORMAT) {
        return Err(event_error("invalid /agent format"));
    }
    let session_id = json
        .get("session_id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| event_error("missing /agent session_id"))?
        .parse()
        .map_err(event_error)?;
    let system_prompt = json
        .get("system_prompt")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| event_error("missing /agent system_prompt"))?
        .to_owned();
    let messages: Vec<Message> = serde_json::from_value(
        json.get("messages")
            .cloned()
            .ok_or_else(|| event_error("missing /agent messages"))?,
    )
    .map_err(event_error)?;
    let events: Vec<Event> = serde_json::from_value(
        json.get("events")
            .cloned()
            .ok_or_else(|| event_error("missing /agent events"))?,
    )
    .map_err(event_error)?;
    let derived = messages_from_events(&events);
    if serde_json::to_value(&messages).map_err(event_error)?
        != serde_json::to_value(&derived).map_err(event_error)?
    {
        return Err(event_error(
            "/agent messages do not match the Everruns event projection",
        ));
    }
    Ok(Some(AgentState {
        session_id,
        system_prompt,
        messages,
        events,
    }))
}

fn persist_agent_state(
    process: &mut Process,
    session_id: SessionId,
    system_prompt: &str,
    events: Vec<Event>,
    messages: Vec<Message>,
) -> EverrunsResult<()> {
    // THREAT[TM-AUD-001]: Everruns events are canonical. The guest-readable
    // history is derived and committed at this host-only boundary.
    let value = Value::from_json(json!({
        "format": AGENT_STATE_FORMAT,
        "session_id": session_id.to_string(),
        "system_prompt": system_prompt,
        "messages": messages,
        "events": events,
    }))
    .map_err(event_error)?;
    process.replace_agent_state(value).map_err(event_error)
}

fn messages_from_events(events: &[Event]) -> Vec<Message> {
    events.iter().filter_map(message_from_event).collect()
}

fn message_from_event(event: &Event) -> Option<Message> {
    match &event.data {
        EventData::InputMessage(data) => Some(data.message.clone()),
        EventData::OutputMessageCompleted(data) => Some(data.message.clone()),
        EventData::ToolCompleted(data) => Some(tool_completed_to_message(event, data)),
        _ => None,
    }
}

fn tool_completed_to_message(event: &Event, data: &ToolCompletedData) -> Message {
    let mut images = Vec::new();
    let result = data.result.as_ref().map(|parts| {
        for part in parts {
            if let ContentPart::Image(image) = part
                && let (Some(base64), Some(media_type)) = (&image.base64, &image.media_type)
            {
                images.push(ToolResultImage {
                    base64: base64.clone(),
                    media_type: media_type.clone(),
                });
            }
        }
        let text_parts = parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        if let [text] = text_parts.as_slice() {
            return parse_structured_tool_result_text(&text.text);
        }
        if text_parts.is_empty() {
            JsonValue::Null
        } else {
            serde_json::to_value(text_parts).unwrap_or_default()
        }
    });
    let mut message = if images.is_empty() {
        Message::tool_result(&data.tool_call_id, result, data.error.clone())
    } else {
        Message::tool_result_with_images(&data.tool_call_id, result, images)
    };
    message.id = MessageId::from_uuid(event.id.uuid());
    message.created_at = event.ts;
    let mut metadata = HashMap::from([("tool_name".to_string(), json!(data.tool_name))]);
    if let Some(fingerprint) = &data.tool_call_fingerprint {
        metadata.insert("tool_call_fingerprint".to_string(), json!(fingerprint));
    }
    if let Some(fingerprint) = &data.tool_result_fingerprint {
        metadata.insert("tool_result_fingerprint".to_string(), json!(fingerprint));
    }
    message.metadata = Some(metadata);
    message
}

fn parse_structured_tool_result_text(text: &str) -> JsonValue {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        return JsonValue::String(text.to_string());
    }
    match serde_json::from_str(text) {
        Ok(value @ (JsonValue::Object(_) | JsonValue::Array(_))) => value,
        _ => JsonValue::String(text.to_string()),
    }
}

fn message_semantically_matches(left: &Message, right: &Message) -> bool {
    left.role == right.role
        && left.content == right.content
        && left.phase == right.phase
        && left.thinking == right.thinking
        && left.thinking_signature == right.thinking_signature
        && left.controls == right.controls
        && left.metadata == right.metadata
        && left.external_actor == right.external_actor
}

fn event_error(error: impl fmt::Display) -> AgentLoopError {
    AgentLoopError::event(error.to_string())
}

fn store_error(error: impl fmt::Display) -> AgentLoopError {
    AgentLoopError::store(error.to_string())
}
