//! Process-owned reasoning loop implemented by Everruns.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use everruns::core::capabilities::{Capability, SystemPromptContext};
use everruns::core::error::Result as EverrunsResult;
use everruns::core::events::{Event, EventData, EventRequest, ToolCompletedData};
use everruns::core::message_retriever::InputMessage;
use everruns::core::tools::{Tool, ToolExecutionResult, ToolResultImage};
use everruns::core::typed_id::{MessageId, SessionId};
use everruns::core::{
    AgentCapabilityConfig, AgentLoopError, CompactionCheckpointStore, ContentPart, DriverRegistry,
    Message, MessageRole,
};
use everruns_host::{
    EventDurability, EventLog, EventLogError, EventPage, EventReadLimit, EventReadRequest,
    EventReader, EventSink, EventSinkError, HostBackends, HostComposition, InMemoryEventLog,
    InProcessRuntime, InProcessRuntimeBuilder, TurnResult, TurnStopReason,
};
use serde_json::{Value as JsonValue, json};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::tools::{FunctionTool, tool_error, tool_json};
use crate::{
    Activation, ActivationHook, Builtins, Change, DurableProcessHandle, Limits, MessageIntent,
    Mount, Process, ProcessBuilder, ProcessId, Reasoner, Script, Value,
};

const THREAD_STATE_PATH: &str = "/thread";
const THREAD_STATE_FORMAT: &str = "svit-thread@8";
const PREVIOUS_THREAD_STATE_FORMAT: &str = "svit-thread@7";
const LEGACY_THREAD_STATE_FORMAT: &str = "svit-thread@6";
const PROCESS_CAPABILITY_ID: &str = "svit";
const SVIT_SYSTEM_PROMPT: &str = "You operate one Svit process. Use its memory tree for durable facts and working state. Svit owns the process state, conversation history, and execution environment. Use only the capabilities supplied by Svit, and treat committed process state as authoritative. The exec tool runs /bin built-ins and /lib scripts. Inside Svit Lisp, use (exec \"/bin/name\" input) or (exec \"/lib/name\" input), with the path first. A /bin call uses only the built-in authority attached by this Svit host and is an immediate external effect, even when invoked from a transactional script.";

/// Failure to construct or run a Svit process.
#[derive(Debug, Error)]
pub enum SvitError {
    /// The reasoning loop or durable event protocol failed.
    #[error("Svit reasoning failed: {0}")]
    Reasoning(String),

    /// The shared process state could not be locked.
    #[error("Svit process is unavailable")]
    ProcessUnavailable,

    /// A Svit process transition failed.
    #[error(transparent)]
    Process(#[from] crate::Error),

    /// No reasoning model and provider were configured.
    #[error("Svit reasoner is required")]
    MissingReasoner,

    /// The standard native HTTP transport could not be initialized.
    #[error("Svit HTTP transport is unavailable")]
    HttpTransportUnavailable,

    /// Internal loop assembly did not receive its owning process.
    #[error("Svit process is required")]
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

    /// A completed turn did not produce an assistant message.
    #[error("Svit turn completed without an assistant message")]
    MissingOutboxMessage,
}

/// Failure while observing a transient Svit stream.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ObserveError {
    /// No observation is currently available without waiting.
    #[error("no Svit observation is available")]
    Empty,
    /// The observed Svit instance closed this stream.
    #[error("Svit observation stream is closed")]
    Closed,
    /// The observer fell behind and missed committed observations.
    #[error("Svit observer missed {0} observations")]
    Lagged(u64),
}

impl From<AgentLoopError> for SvitError {
    fn from(error: AgentLoopError) -> Self {
        Self::Reasoning(error.to_string())
    }
}

/// Result returned by the runnable Svit API.
pub type SvitResult<T> = std::result::Result<T, SvitError>;

/// Operational event published by a running Svit instance.
#[derive(Clone, Debug)]
pub enum SvitEvent {
    /// Process state changed through a committed host or reasoning transition.
    ///
    /// `changed` lists the canonical paths the transition touched, so an
    /// observer re-reads exactly what went stale instead of everything. It is
    /// still not a state-bearing event: hosts read owned values back through
    /// [`Svit::read`] rather than retaining the mutable process tree.
    ///
    /// Mount nodes are covered only when this process wrote them. Nothing
    /// reports an external change to a mounted source, so a client caching
    /// mount content can still be stale (`L-045`).
    Committed(Change),
    /// One message projected from a newly committed canonical reasoning event.
    Message(Box<Message>),
    /// The independently running process loop stopped with a sanitized failure.
    Failed(String),
}

/// One configured, independently running Svit process.
pub struct Svit {
    reasoning: Option<ReasoningLoop>,
    process: ProcessState,
    session_id: SessionId,
    inbox_state: Arc<AsyncMutex<InboxState>>,
    command_sender: mpsc::UnboundedSender<RuntimeCommand>,
    command_receiver: Option<mpsc::UnboundedReceiver<RuntimeCommand>>,
    outbox_sender: broadcast::Sender<Message>,
    builtins: Option<Builtins>,
    event_sender: broadcast::Sender<SvitEvent>,
    task: Option<JoinHandle<SvitResult<ReasoningLoop>>>,
}

/// Cloneable handle for committing messages to one Svit process inbox.
#[derive(Clone)]
pub struct Inbox {
    process: ProcessState,
    state: Arc<AsyncMutex<InboxState>>,
    command_sender: mpsc::UnboundedSender<RuntimeCommand>,
}

/// Observer for completed messages emitted by one Svit instance.
pub struct Outbox {
    receiver: broadcast::Receiver<Message>,
}

/// Observer for operational events emitted by one Svit instance.
pub struct Events {
    receiver: broadcast::Receiver<SvitEvent>,
}

struct InboxState {
    accepting: bool,
}

#[derive(Clone, Copy)]
enum RuntimeCommand {
    Wake,
    Stop,
}

#[derive(Clone)]
struct ProcessState {
    view: Arc<Mutex<Process>>,
    owner: Arc<AsyncMutex<Box<dyn ProcessOwner>>>,
    event_sender: broadcast::Sender<SvitEvent>,
    event_log: Arc<dyn EventLog>,
    compaction_checkpoints: Option<Arc<dyn CompactionCheckpointStore>>,
    durable: bool,
}

struct SvitEventSink {
    sender: broadcast::Sender<SvitEvent>,
}

impl EventSink for SvitEventSink {
    fn try_send(&self, event: Event) -> Result<(), EventSinkError> {
        if let Some(message) = message_from_event(&event) {
            let _ = self.sender.send(SvitEvent::Message(Box::new(message)));
        }
        Ok(())
    }
}

impl ProcessState {
    fn volatile(process: Process) -> Self {
        let view = Arc::new(Mutex::new(process.clone()));
        let (event_sender, _) = broadcast::channel(64);
        Self {
            view,
            owner: Arc::new(AsyncMutex::new(Box::new(VolatileProcess { process }))),
            event_sender,
            event_log: Arc::new(InMemoryEventLog::new()),
            compaction_checkpoints: None,
            durable: false,
        }
    }

    fn persisted(handle: impl DurableProcessHandle + 'static) -> crate::Result<Self> {
        let process = handle.process_projection();
        let event_log = handle.event_log();
        let compaction_checkpoints = handle.compaction_checkpoint_store();
        let (event_sender, _) = broadcast::channel(64);
        Ok(Self {
            view: Arc::new(Mutex::new(process)),
            owner: Arc::new(AsyncMutex::new(Box::new(PersistedProcess { handle }))),
            event_sender,
            event_log,
            compaction_checkpoints: Some(compaction_checkpoints),
            durable: true,
        })
    }

    fn view(&self) -> Arc<Mutex<Process>> {
        self.view.clone()
    }

    fn event_sender(&self) -> broadcast::Sender<SvitEvent> {
        self.event_sender.clone()
    }

    async fn write(&self, path: &str, value: Value) -> crate::Result<Change> {
        let mut owner = self.owner.lock().await;
        let change = owner.write(path, value).await?;
        self.refresh_view(owner.as_ref())?;
        publish_committed(&self.event_sender, &change);
        Ok(change)
    }

    async fn remove(&self, path: &str) -> crate::Result<Change> {
        let mut owner = self.owner.lock().await;
        let change = owner.remove(path).await?;
        self.refresh_view(owner.as_ref())?;
        publish_committed(&self.event_sender, &change);
        Ok(change)
    }

    async fn exec(
        &self,
        path: &str,
        input: Value,
        builtins: Option<&Builtins>,
    ) -> crate::Result<Activation> {
        let mut owner = self.owner.lock().await;
        let activation = owner.exec(path, input, builtins).await?;
        self.refresh_view(owner.as_ref())?;
        publish_committed(&self.event_sender, &activation_change(&activation));
        Ok(activation)
    }

    async fn enqueue_inbox(&self, value: Value) -> crate::Result<Change> {
        let mut owner = self.owner.lock().await;
        let change = owner.enqueue_inbox(value).await?;
        self.refresh_view(owner.as_ref())?;
        publish_committed(&self.event_sender, &change);
        Ok(change)
    }

    async fn acknowledge_inbox(&self, expected: &Value) -> crate::Result<Change> {
        let mut owner = self.owner.lock().await;
        let change = owner.acknowledge_inbox(expected).await?;
        self.refresh_view(owner.as_ref())?;
        publish_committed(&self.event_sender, &change);
        Ok(change)
    }

    async fn attach_mount(&self, name: String, mount: Mount) -> crate::Result<Change> {
        let mut owner = self.owner.lock().await;
        let change = owner.attach_mount(name, mount).await?;
        self.refresh_view(owner.as_ref())?;
        publish_committed(&self.event_sender, &change);
        Ok(change)
    }

    async fn initialize_thread_state(&self, value: Value) -> crate::Result<()> {
        let mut owner = self.owner.lock().await;
        let change = owner.initialize_thread_state(value).await?;
        self.refresh_view(owner.as_ref())?;
        publish_committed(&self.event_sender, &change);
        Ok(())
    }

    async fn replace_thread_state(&self, value: Value) -> crate::Result<()> {
        let mut owner = self.owner.lock().await;
        let change = owner.replace_thread_state(value).await?;
        self.refresh_view(owner.as_ref())?;
        publish_committed(&self.event_sender, &change);
        Ok(())
    }

    async fn replace_builtins(&self, value: Value) -> crate::Result<()> {
        let mut owner = self.owner.lock().await;
        let change = owner.replace_builtins(value).await?;
        self.refresh_view(owner.as_ref())?;
        publish_committed(&self.event_sender, &change);
        Ok(())
    }

    async fn import_thread_events(&self, events: &[Event]) -> crate::Result<()> {
        if self.durable {
            return self.owner.lock().await.import_thread_events(events).await;
        }
        for event in events {
            self.event_log
                .append(event_request_from(event))
                .await
                .map_err(|error| crate::Error::InvalidPersistence(error.to_string()))?;
        }
        Ok(())
    }

    async fn recent_thread_events(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> crate::Result<Vec<Event>> {
        if self.durable {
            return self
                .owner
                .lock()
                .await
                .recent_thread_events(session_id, limit)
                .await;
        }
        if limit == 0 || limit > 4096 {
            return Err(crate::Error::ResourceLimitExceeded("recent thread events"));
        }
        let page_limit = EventReadLimit::default();
        let mut request = EventReadRequest::new(session_id, page_limit);
        let mut recent = std::collections::VecDeque::with_capacity(limit);
        loop {
            let page = self
                .event_log
                .read_page(request)
                .await
                .map_err(|error| crate::Error::InvalidPersistence(error.to_string()))?;
            for event in page
                .events
                .into_iter()
                .filter(|event| message_from_event(event).is_some())
            {
                if recent.len() == limit {
                    recent.pop_front();
                }
                recent.push_back(event);
            }
            let Some(cursor) = page.next_cursor else {
                return Ok(recent.into());
            };
            request = EventReadRequest::from_cursor(cursor, page_limit);
        }
    }

    async fn continues_thread_history(&self, session_id: SessionId) -> crate::Result<bool> {
        if !self.durable {
            return Ok(false);
        }
        self.owner
            .lock()
            .await
            .continues_thread_history(session_id)
            .await
    }

    fn refresh_view(&self, owner: &dyn ProcessOwner) -> crate::Result<()> {
        let process = owner.process_projection();
        *self
            .view
            .lock()
            .map_err(|_| crate::Error::ControlUnavailable)? = process;
        Ok(())
    }
}

#[async_trait]
trait ProcessOwner: Send + Sync {
    fn process_projection(&self) -> Process;
    async fn write(&mut self, path: &str, value: Value) -> crate::Result<Change>;
    async fn remove(&mut self, path: &str) -> crate::Result<Change>;
    async fn exec(
        &mut self,
        path: &str,
        input: Value,
        builtins: Option<&Builtins>,
    ) -> crate::Result<Activation>;
    async fn enqueue_inbox(&mut self, value: Value) -> crate::Result<Change>;
    async fn acknowledge_inbox(&mut self, expected: &Value) -> crate::Result<Change>;
    async fn attach_mount(&mut self, name: String, mount: Mount) -> crate::Result<Change>;
    async fn initialize_thread_state(&mut self, value: Value) -> crate::Result<Change>;
    async fn replace_thread_state(&mut self, value: Value) -> crate::Result<Change>;
    async fn replace_builtins(&mut self, value: Value) -> crate::Result<Change>;
    async fn import_thread_events(&self, events: &[Event]) -> crate::Result<()>;
    async fn recent_thread_events(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> crate::Result<Vec<Event>>;
    async fn continues_thread_history(&self, session_id: SessionId) -> crate::Result<bool>;
}

struct VolatileProcess {
    process: Process,
}

#[async_trait]
impl ProcessOwner for VolatileProcess {
    fn process_projection(&self) -> Process {
        self.process.clone()
    }

    async fn write(&mut self, path: &str, value: Value) -> crate::Result<Change> {
        self.process.write(path, value)
    }

    async fn remove(&mut self, path: &str) -> crate::Result<Change> {
        self.process.remove(path)
    }

    async fn exec(
        &mut self,
        path: &str,
        input: Value,
        builtins: Option<&Builtins>,
    ) -> crate::Result<Activation> {
        match builtins {
            Some(builtins) => {
                let context = Arc::new(Mutex::new(self.process.clone()));
                let prepared = self
                    .process
                    .prepare_exec_with_builtins(path, input, builtins, context)
                    .await?;
                Ok(self.process.commit_prepared(prepared))
            }
            None => self.process.exec(path, input),
        }
    }

    async fn enqueue_inbox(&mut self, value: Value) -> crate::Result<Change> {
        self.process.enqueue_inbox(value)
    }

    async fn acknowledge_inbox(&mut self, expected: &Value) -> crate::Result<Change> {
        self.process.acknowledge_inbox(expected)
    }

    async fn attach_mount(&mut self, name: String, mount: Mount) -> crate::Result<Change> {
        self.process.attach_mount(name, mount)
    }

    async fn initialize_thread_state(&mut self, value: Value) -> crate::Result<Change> {
        self.process.initialize_thread_state(value)
    }

    async fn replace_thread_state(&mut self, value: Value) -> crate::Result<Change> {
        self.process.replace_thread_state(value)
    }

    async fn replace_builtins(&mut self, value: Value) -> crate::Result<Change> {
        self.process.replace_builtins(value)
    }

    async fn import_thread_events(&self, _events: &[Event]) -> crate::Result<()> {
        Err(crate::Error::PersistenceUnavailable)
    }

    async fn recent_thread_events(
        &self,
        _session_id: SessionId,
        _limit: usize,
    ) -> crate::Result<Vec<Event>> {
        Err(crate::Error::PersistenceUnavailable)
    }

    async fn continues_thread_history(&self, _session_id: SessionId) -> crate::Result<bool> {
        Ok(false)
    }
}

struct PersistedProcess<H> {
    handle: H,
}

#[async_trait]
impl<H> ProcessOwner for PersistedProcess<H>
where
    H: DurableProcessHandle + 'static,
{
    fn process_projection(&self) -> Process {
        self.handle.process_projection()
    }

    async fn write(&mut self, path: &str, value: Value) -> crate::Result<Change> {
        self.handle.write(path, value).await
    }

    async fn remove(&mut self, path: &str) -> crate::Result<Change> {
        self.handle.remove(path).await
    }

    async fn exec(
        &mut self,
        path: &str,
        input: Value,
        builtins: Option<&Builtins>,
    ) -> crate::Result<Activation> {
        match builtins {
            Some(builtins) => self.handle.exec_with_builtins(path, input, builtins).await,
            None => self.handle.exec(path, input).await,
        }
    }

    async fn enqueue_inbox(&mut self, value: Value) -> crate::Result<Change> {
        self.handle.enqueue_inbox(value).await
    }

    async fn acknowledge_inbox(&mut self, expected: &Value) -> crate::Result<Change> {
        self.handle.acknowledge_inbox(expected).await
    }

    async fn attach_mount(&mut self, name: String, mount: Mount) -> crate::Result<Change> {
        self.handle.attach_mount(name, mount).await
    }

    async fn initialize_thread_state(&mut self, value: Value) -> crate::Result<Change> {
        self.handle.initialize_thread_state(value).await
    }

    async fn replace_thread_state(&mut self, value: Value) -> crate::Result<Change> {
        self.handle.replace_thread_state(value).await
    }

    async fn replace_builtins(&mut self, value: Value) -> crate::Result<Change> {
        self.handle.replace_builtins(value).await
    }

    async fn import_thread_events(&self, events: &[Event]) -> crate::Result<()> {
        self.handle.import_thread_events(events).await
    }

    async fn recent_thread_events(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> crate::Result<Vec<Event>> {
        self.handle.recent_thread_events(session_id, limit).await
    }

    async fn continues_thread_history(&self, session_id: SessionId) -> crate::Result<bool> {
        self.handle.continues_thread_history(session_id).await
    }
}

impl Svit {
    /// Starts one process and loop definition around `id`.
    pub fn builder(id: impl Into<String>) -> crate::Result<SvitBuilder> {
        Ok(SvitBuilder {
            process: Process::builder(id)?,
            reasoning: ReasoningLoopBuilder::detached(),
        })
    }

    /// Configures a runnable Svit around a restored or forked process.
    pub fn resume(process: Process) -> SvitResumeBuilder {
        SvitResumeBuilder {
            reasoning: ReasoningLoopBuilder::new(process),
        }
    }

    /// Configures a runnable Svit around an adapter-owned durable process.
    pub fn persisted(handle: impl DurableProcessHandle + 'static) -> SvitResult<SvitResumeBuilder> {
        Ok(SvitResumeBuilder {
            reasoning: ReasoningLoopBuilder::persisted(handle)?,
        })
    }

    fn from_reasoning(reasoning: ReasoningLoop) -> Self {
        let process = reasoning.process();
        let session_id = reasoning.session_id;
        let builtins = reasoning.builtins.clone();
        let (command_sender, command_receiver) = mpsc::unbounded_channel();
        let (outbox_sender, _) = broadcast::channel(64);
        let event_sender = process.event_sender();
        Self {
            reasoning: Some(reasoning),
            process,
            session_id,
            inbox_state: Arc::new(AsyncMutex::new(InboxState { accepting: true })),
            command_sender,
            command_receiver: Some(command_receiver),
            outbox_sender,
            builtins,
            event_sender,
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
    pub fn outbox(&self) -> Outbox {
        Outbox {
            receiver: self.outbox_sender.subscribe(),
        }
    }

    /// Subscribes to commit notifications and sanitized terminal failures.
    pub fn events(&self) -> Events {
        Events {
            receiver: self.event_sender.subscribe(),
        }
    }

    /// Starts the Everruns loop as an independent Tokio task.
    pub fn start(&mut self) -> SvitResult<()> {
        if self.task.is_some() || self.command_receiver.is_none() {
            return Err(SvitError::AlreadyStarted);
        }
        let reasoning = self.reasoning.take().ok_or(SvitError::AlreadyStarted)?;
        let receiver = self
            .command_receiver
            .take()
            .ok_or(SvitError::AlreadyStarted)?;
        let outbox = self.outbox_sender.clone();
        let events = self.event_sender.clone();
        let inbox_state = self.inbox_state.clone();
        self.task = Some(tokio::spawn(async move {
            let result = run_process_loop(reasoning, receiver, outbox).await;
            inbox_state.lock().await.accepting = false;
            if let Err(error) = &result {
                // THREAT[TM-INF-001]: Operational subscribers receive the same
                // bounded diagnostic surface as other runtime callers.
                let diagnostic = crate::error::sanitize_diagnostic(error);
                let _ = events.send(SvitEvent::Failed(diagnostic));
            }
            result
        }));
        Ok(())
    }

    /// Seals the inbox, drains committed messages, and waits for the loop.
    pub async fn block(&mut self) -> SvitResult<()> {
        let task = self.task.take().ok_or(SvitError::NotStarted)?;
        {
            let mut state = self.inbox_state.lock().await;
            state.accepting = false;
            let _ = self.command_sender.send(RuntimeCommand::Stop);
        }
        self.reasoning = Some(task.await.map_err(|_| SvitError::TaskFailed)??);
        Ok(())
    }

    /// Returns the durable conversation projection.
    pub fn messages(&self) -> SvitResult<&[Message]> {
        self.reasoning
            .as_ref()
            .map(ReasoningLoop::messages)
            .ok_or(SvitError::AlreadyStarted)
    }

    /// Reads a bounded window of the newest durable conversation messages.
    pub async fn recent_messages(&self, limit: usize) -> SvitResult<Vec<Message>> {
        let events = self
            .process
            .recent_thread_events(self.session_id, limit)
            .await?;
        Ok(events.iter().filter_map(message_from_event).collect())
    }

    /// Returns the process identifier.
    pub fn id(&self) -> SvitResult<ProcessId> {
        Ok(self
            .process
            .view
            .lock()
            .map_err(|_| SvitError::ProcessUnavailable)?
            .id()
            .clone())
    }

    /// Returns the current committed process version.
    pub fn version(&self) -> SvitResult<u64> {
        Ok(self
            .process
            .view
            .lock()
            .map_err(|_| SvitError::ProcessUnavailable)?
            .version())
    }

    /// Returns the configured process limits.
    pub fn limits(&self) -> SvitResult<Limits> {
        Ok(self
            .process
            .view
            .lock()
            .map_err(|_| SvitError::ProcessUnavailable)?
            .limits()
            .clone())
    }

    /// Discovers immediate children under a process path.
    pub fn discover(&self, path: &str) -> SvitResult<Vec<String>> {
        Ok(self
            .process
            .view
            .lock()
            .map_err(|_| SvitError::ProcessUnavailable)?
            .discover(path)?)
    }

    /// Reads one owned value from the process, resolving mounts lazily.
    pub fn read(&self, path: &str) -> SvitResult<Option<Value>> {
        Ok(self
            .process
            .view
            .lock()
            .map_err(|_| SvitError::ProcessUnavailable)?
            .read(path)?)
    }

    /// Returns the facts record describing one process or mount path.
    pub fn stat(&self, path: &str) -> SvitResult<Option<Value>> {
        Ok(self
            .process
            .view
            .lock()
            .map_err(|_| SvitError::ProcessUnavailable)?
            .stat(path)?)
    }

    /// Attaches or replaces one mount provider on the running process.
    pub async fn attach_mount(
        &mut self,
        name: impl Into<String>,
        mount: Mount,
    ) -> SvitResult<Change> {
        Ok(self.process.attach_mount(name.into(), mount).await?)
    }

    /// Reads one owned value and its committed process version atomically.
    pub fn read_versioned(&self, path: &str) -> SvitResult<(Option<Value>, u64)> {
        let process = self
            .process
            .view
            .lock()
            .map_err(|_| SvitError::ProcessUnavailable)?;
        Ok((process.read(path)?, process.version()))
    }

    /// Commits a host write through the process path contract.
    pub async fn write(&mut self, path: &str, value: Value) -> SvitResult<Change> {
        Ok(self.process.write(path, value).await?)
    }

    /// Commits a host removal through the process path contract.
    pub async fn remove(&mut self, path: &str) -> SvitResult<Change> {
        Ok(self.process.remove(path).await?)
    }

    /// Executes one process script, including its host-attached built-in calls.
    pub async fn exec(&mut self, path: &str, input: Value) -> SvitResult<Activation> {
        Ok(self
            .process
            .exec(path, input, self.builtins.as_ref())
            .await?)
    }

    /// Returns committed Lisp message intents from process state.
    pub fn message_intents(&self) -> SvitResult<Vec<MessageIntent>> {
        Ok(self
            .process
            .view
            .lock()
            .map_err(|_| SvitError::ProcessUnavailable)?
            .outbox()?)
    }

    /// Snapshots process state and the durable reasoning thread together.
    pub fn snapshot(&self) -> SvitResult<Vec<u8>> {
        Ok(self
            .process
            .view
            .lock()
            .map_err(|_| SvitError::ProcessUnavailable)?
            .snapshot()?)
    }

    /// Returns the current committed process root hash.
    pub fn root_hash(&self) -> SvitResult<String> {
        Ok(self
            .process
            .view
            .lock()
            .map_err(|_| SvitError::ProcessUnavailable)?
            .root_hash())
    }

    /// Forks the complete state into an independently mutable child process.
    pub fn fork_process(&self, child_id: impl Into<String>) -> SvitResult<Process> {
        Ok(self
            .process
            .view
            .lock()
            .map_err(|_| SvitError::ProcessUnavailable)?
            .fork(child_id)?)
    }

    /// Returns completed child addresses created through `/bin/spawn`.
    pub fn child_ids(&self) -> Vec<ProcessId> {
        self.builtins
            .as_ref()
            .map(Builtins::child_ids)
            .unwrap_or_default()
    }

    /// Snapshots one completed child created through `/bin/spawn`.
    pub fn child_snapshot(&self, id: &ProcessId) -> SvitResult<Option<Vec<u8>>> {
        Ok(self
            .builtins
            .as_ref()
            .map(|tools| tools.child_snapshot(id))
            .transpose()?
            .flatten())
    }
}

impl Outbox {
    /// Waits for the next completed message.
    pub async fn recv(&mut self) -> Result<Message, ObserveError> {
        self.receiver.recv().await.map_err(ObserveError::from)
    }

    /// Receives the next completed message without waiting.
    pub fn try_recv(&mut self) -> Result<Message, ObserveError> {
        self.receiver.try_recv().map_err(ObserveError::from)
    }
}

impl Events {
    /// Waits for the next operational event.
    pub async fn recv(&mut self) -> Result<SvitEvent, ObserveError> {
        self.receiver.recv().await.map_err(ObserveError::from)
    }

    /// Receives the next operational event without waiting.
    pub fn try_recv(&mut self) -> Result<SvitEvent, ObserveError> {
        self.receiver.try_recv().map_err(ObserveError::from)
    }
}

impl From<broadcast::error::RecvError> for ObserveError {
    fn from(error: broadcast::error::RecvError) -> Self {
        match error {
            broadcast::error::RecvError::Closed => Self::Closed,
            broadcast::error::RecvError::Lagged(count) => Self::Lagged(count),
        }
    }
}

impl From<broadcast::error::TryRecvError> for ObserveError {
    fn from(error: broadcast::error::TryRecvError) -> Self {
        match error {
            broadcast::error::TryRecvError::Empty => Self::Empty,
            broadcast::error::TryRecvError::Closed => Self::Closed,
            broadcast::error::TryRecvError::Lagged(count) => Self::Lagged(count),
        }
    }
}

impl Inbox {
    /// Atomically commits one message, then wakes the process loop.
    pub async fn send(&self, message: Message) -> SvitResult<()> {
        let state = self.state.lock().await;
        if !state.accepting {
            return Err(SvitError::InboxClosed);
        }
        let json = serde_json::to_value(message)
            .map_err(|error| SvitError::InvalidInbox(error.to_string()))?;
        let value = Value::from_json(json)?;
        self.process.enqueue_inbox(value).await?;
        self.command_sender
            .send(RuntimeCommand::Wake)
            .map_err(|_| SvitError::TaskFailed)?;
        drop(state);
        Ok(())
    }
}

async fn run_process_loop(
    mut reasoning: ReasoningLoop,
    mut commands: mpsc::UnboundedReceiver<RuntimeCommand>,
    outbox: broadcast::Sender<Message>,
) -> SvitResult<ReasoningLoop> {
    loop {
        let next = {
            let process = reasoning.process().view();
            process
                .lock()
                .map_err(|_| SvitError::ProcessUnavailable)?
                .inbox_front()?
                .cloned()
        };
        if let Some(value) = next {
            let message: Message = serde_json::from_value(value.to_json())
                .map_err(|error| SvitError::InvalidInbox(error.to_string()))?;
            reasoning.run(message).await?;
            let response = reasoning
                .messages()
                .last()
                .filter(|message| message.role == MessageRole::Agent)
                .cloned()
                .ok_or(SvitError::MissingOutboxMessage)?;
            reasoning.process().acknowledge_inbox(&value).await?;
            let _ = outbox.send(response);
            continue;
        }

        match commands.recv().await {
            Some(RuntimeCommand::Wake) => {}
            Some(RuntimeCommand::Stop) | None => return Ok(reasoning),
        }
    }
}

/// Projects one activation as the change its commit published.
fn activation_change(activation: &Activation) -> Change {
    Change::notification(activation.version, activation.changed.clone())
}

fn publish_committed(events: &broadcast::Sender<SvitEvent>, change: &Change) {
    let _ = events.send(SvitEvent::Committed(change.to_notification()));
}

/// Loop configuration for a restored or forked process.
pub struct SvitResumeBuilder {
    reasoning: ReasoningLoopBuilder,
}

impl SvitResumeBuilder {
    /// Adds optional host instructions to Svit's system prompt.
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.reasoning = self.reasoning.instructions(instructions);
        self
    }

    /// Supplies the model and provider used by the reasoning loop.
    pub fn reasoner(mut self, reasoner: Reasoner) -> Self {
        self.reasoning = self.reasoning.reasoner(reasoner);
        self
    }

    /// Overrides Everruns' maximum reason/act iterations for one turn.
    ///
    /// Without this override, the configured Everruns host default applies.
    pub fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.reasoning = self.reasoning.max_iterations(max_iterations);
        self
    }

    /// Restricts the model to discovery, reads, and named scripts.
    pub fn allow_scripts<I, S>(mut self, scripts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.reasoning = self.reasoning.allow_scripts(scripts);
        self
    }

    /// Installs host-provided `/bin` built-ins with explicit grants.
    pub fn builtins(mut self, builtins: Builtins) -> Self {
        self.reasoning = self.reasoning.builtins(builtins);
        self
    }

    /// Reopens the process and resumes its durable reasoning thread.
    pub async fn build(self) -> SvitResult<Svit> {
        Ok(Svit::from_reasoning(self.reasoning.build().await?))
    }
}

/// Builder combining process definition and reasoning-loop configuration.
pub struct SvitBuilder {
    process: ProcessBuilder,
    reasoning: ReasoningLoopBuilder,
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

    /// Attaches a named virtual mount below `/mounts`.
    pub fn mount(mut self, name: impl Into<String>, mount: Mount) -> Self {
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

    /// Adds optional host instructions to Svit's system prompt.
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.reasoning = self.reasoning.instructions(instructions);
        self
    }

    /// Supplies the model and provider used by the reasoning loop.
    pub fn reasoner(mut self, reasoner: Reasoner) -> Self {
        self.reasoning = self.reasoning.reasoner(reasoner);
        self
    }

    /// Overrides Everruns' maximum reason/act iterations for one turn.
    ///
    /// Without this override, the configured Everruns host default applies.
    pub fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.reasoning = self.reasoning.max_iterations(max_iterations);
        self
    }

    /// Restricts the model to discovery, reads, and named scripts.
    pub fn allow_scripts<I, S>(mut self, scripts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.reasoning = self.reasoning.allow_scripts(scripts);
        self
    }

    /// Installs host-provided `/bin` built-ins with explicit grants.
    pub fn builtins(mut self, builtins: Builtins) -> Self {
        self.reasoning = self.reasoning.builtins(builtins);
        self
    }

    /// Builds the process and its runnable loop as one Svit instance.
    pub async fn build(self) -> SvitResult<Svit> {
        let process = self.process.build()?;
        let reasoning = self.reasoning.with_process(process).build().await?;
        Ok(Svit::from_reasoning(reasoning))
    }
}

/// A running Everruns reasoning represented by one Svit process.
struct ReasoningLoop {
    process: ProcessState,
    runtime: InProcessRuntime,
    session_id: SessionId,
    messages: Vec<Message>,
    builtins: Option<Builtins>,
}

impl ReasoningLoop {
    /// Runs one turn in the process-owned thread.
    pub async fn run(&mut self, input: Message) -> SvitResult<TurnResult> {
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
        if result.stop_reason == TurnStopReason::MaxTurnRequests {
            return Err(AgentLoopError::event(
                "maximum reason/act iterations reached before a final answer",
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
    fn process(&self) -> ProcessState {
        self.process.clone()
    }
}

struct ReasoningLoopBuilder {
    process: Option<ProcessState>,
    access: ReasoningAccess,
    instructions: Option<String>,
    reasoner: Option<Reasoner>,
    max_iterations: Option<usize>,
    builtins: Option<Builtins>,
}

impl ReasoningLoopBuilder {
    fn new(process: Process) -> Self {
        Self::detached().with_process(process)
    }

    fn detached() -> Self {
        Self {
            process: None,
            access: ReasoningAccess::Full,
            instructions: None,
            reasoner: None,
            max_iterations: None,
            builtins: None,
        }
    }

    fn with_process(mut self, process: Process) -> Self {
        self.process = Some(ProcessState::volatile(process));
        self
    }

    fn persisted(handle: impl DurableProcessHandle + 'static) -> crate::Result<Self> {
        Ok(Self {
            process: Some(ProcessState::persisted(handle)?),
            ..Self::detached()
        })
    }

    fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    fn reasoner(mut self, reasoner: Reasoner) -> Self {
        self.reasoner = Some(reasoner);
        self
    }

    fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = Some(max_iterations);
        self
    }

    fn allow_scripts<I, S>(mut self, scripts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.access =
            ReasoningAccess::ReadExec(Arc::new(scripts.into_iter().map(Into::into).collect()));
        self
    }

    fn builtins(mut self, builtins: Builtins) -> Self {
        self.builtins = Some(builtins);
        self
    }

    async fn build(self) -> SvitResult<ReasoningLoop> {
        let process = self.process.ok_or(SvitError::MissingProcess)?;
        let reasoner = self.reasoner.ok_or(SvitError::MissingReasoner)?;
        let model = reasoner.model_spec();
        let provider = reasoner.provider().clone();
        let builtins = self
            .builtins
            .map(|builtins| {
                builtins
                    .resolve(&reasoner)
                    .map_err(|_| SvitError::HttpTransportUnavailable)
            })
            .transpose()?;
        let initial_state = {
            let view = process.view();
            let process = view.lock().map_err(|_| SvitError::ProcessUnavailable)?;
            load_thread_state(&process).map_err(SvitError::from)?
        };
        let process_id = thread_process_id(&process)?;
        migrate_legacy_thread_state(&process, &process_id, initial_state.as_ref()).await?;
        let stored_state = {
            let view = process.view();
            let process = view.lock().map_err(|_| SvitError::ProcessUnavailable)?;
            load_thread_state(&process).map_err(SvitError::from)?
        };
        let instructions = self
            .instructions
            .or_else(|| {
                stored_state
                    .as_ref()
                    .and_then(|state| state.instructions.clone())
            })
            .filter(|instructions| !instructions.trim().is_empty());
        let system_prompt = compose_system_prompt(&process_id, instructions.as_deref());
        let session_id = match stored_state.as_ref() {
            None => SessionId::new(),
            Some(state) if !is_process_fork(&process)? => state.session_id,
            Some(state) if process.continues_thread_history(state.session_id).await? => {
                state.session_id
            }
            // A process-only fork carries no event-log authority. Starting a
            // fresh session prevents a reused session ID from naming an empty
            // or unrelated history; durable forks retain their immutable prefix.
            Some(_) => SessionId::new(),
        };
        let builtin_catalog = builtins
            .as_ref()
            .map(Builtins::catalog)
            .unwrap_or_else(|| json!({}));
        // THREAT[TM-CAP-005]: `/bin` is refreshed from current host
        // configuration before Everruns can run a turn. Durable owners commit
        // both this projection and thread initialization before publication.
        process
            .replace_builtins(Value::from_json(builtin_catalog)?)
            .await?;
        initialize_thread_state_from(
            &process,
            &process_id,
            session_id,
            instructions.as_deref(),
            &system_prompt,
            stored_state.as_ref(),
        )
        .await?;

        let capability = ProcessCapability {
            process: process.clone(),
            access: self.access,
            builtins: builtins.clone(),
        };
        let capability_config = AgentCapabilityConfig::new(PROCESS_CAPABILITY_ID);
        let compaction_config = AgentCapabilityConfig::new("compaction").config(json!({
            "strategy": "auto",
            "proactive": true,
            "budget_percent": 0.85,
        }));
        let event_log = Arc::new(ProcessEventLog::seeded(
            process.clone(),
            stored_state.as_ref(),
        )?);
        let mut backends = HostBackends::in_memory()
            .with_event_log(event_log)
            .with_event_sink(Arc::new(SvitEventSink {
                sender: process.event_sender(),
            }));
        if let Some(checkpoints) = process.compaction_checkpoints.clone() {
            backends = backends.with_compaction_checkpoint_store(checkpoints);
        }
        let mut drivers = DriverRegistry::new();
        drivers.register_provider(provider)?;
        // Svit is an advanced Everruns host because it owns both the execution
        // surface and canonical event log. HostComposition selects the only
        // capability and provider driver exposed to this loop; HostBackends
        // separately installs the process-backed EventLog used for replay.
        let host_composition = HostComposition::builder()
            .capability(capability)
            .capability(everruns_builtins::CompactionCapability)
            .driver_registry(drivers)
            .build();
        let max_iterations = self.max_iterations;
        let runtime = InProcessRuntimeBuilder::new()
            .host_composition(host_composition)
            .single_session(|session| {
                let session = session
                    .harness(process_id.as_str(), &system_prompt)
                    .agent(process_id.as_str(), &system_prompt)
                    .session_id(session_id)
                    .harness_capability(capability_config.clone())
                    .agent_capability(capability_config.clone())
                    .session_capability(capability_config)
                    .harness_capability(compaction_config.clone())
                    .agent_capability(compaction_config.clone())
                    .session_capability(compaction_config);
                match max_iterations {
                    Some(max_iterations) => session
                        .agent_max_iterations(max_iterations)
                        .session_max_iterations(max_iterations),
                    // Everruns owns loop policy. Svit only attenuates it when
                    // the embedding host explicitly requests an override.
                    None => session,
                }
            })
            .backends(backends)
            .model_spec(model)
            .build()
            .await?;
        Ok(ReasoningLoop {
            process,
            runtime,
            session_id,
            messages: Vec::new(),
            builtins,
        })
    }
}

fn compose_system_prompt(process_id: &ProcessId, instructions: Option<&str>) -> String {
    let base = format!(
        "{SVIT_SYSTEM_PROMPT}\n\nProcess address: {}",
        process_id.as_str()
    );
    match instructions {
        Some(instructions) => {
            format!("{base}\n\n<instructions>\n{instructions}\n</instructions>")
        }
        None => base,
    }
}

#[derive(Clone)]
enum ReasoningAccess {
    Full,
    ReadExec(Arc<BTreeSet<String>>),
}

#[derive(Clone)]
struct ProcessCapability {
    process: ProcessState,
    access: ReasoningAccess,
    builtins: Option<Builtins>,
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
            ReasoningAccess::Full => {
                "Your durable thread and workspace belong to one Svit process. Use absolute paths \
                 with discover, read, stat, write, remove, and exec. Discover built-in manuals \
                 under /bin; run /bin built-ins or /lib scripts through exec. External resources \
                 appear under /mounts and are resolved one node at a time: discover a mount \
                 directory, stat a node to learn its kind, granted access, and locality (cache, \
                 local, or remote), then read the leaf you need. /thread contains bounded \
                 host-managed session metadata; canonical conversation history is not mutable \
                 process memory."
                    .into()
            }
            ReasoningAccess::ReadExec(allowed_scripts) => format!(
                "Your durable thread and workspace belong to one Svit process. Use absolute paths \
                 with discover, read, stat, and exec. Discover built-in manuals under /bin. \
                 External resources appear under /mounts and are resolved one node at a time \
                 through discover, stat, and read. Run only these /lib scripts through exec: {}. \
                 /thread contains bounded host-managed session metadata; canonical conversation \
                 history is not mutable process memory.",
                allowed_scripts
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        if self.builtins.is_some() {
            contribution.push_str(
                " Built-ins are listed under /bin and invoked only through exec(path, input).",
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
                let view = process.view();
                let Ok(process) = view.lock() else {
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
                let view = process.view();
                let Ok(process) = view.lock() else {
                    return tool_error("Svit process lock is unavailable");
                };
                match process.read(path) {
                    Ok(value) => tool_json(json!({
                        "path": path,
                        "value": value.map(|value| value.to_json()),
                        "version": process.version(),
                    })),
                    Err(error) => tool_error(error.to_string()),
                }
            }
        },
    );

    let stat_process = capability.process.clone();
    let stat = FunctionTool::new(
        "stat",
        "Describe one Svit process or mount path: kind, granted access, \
         locality (cache, local, or remote), and source facts. Use it before \
         reading an unfamiliar mount node.",
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
        move |arguments| {
            let process = stat_process.clone();
            async move {
                let path = arguments["path"].as_str().unwrap_or_default();
                let view = process.view();
                let Ok(process) = view.lock() else {
                    return tool_error("Svit process lock is unavailable");
                };
                match process.stat(path) {
                    Ok(value) => tool_json(json!({
                        "path": path,
                        "value": value.map(|value| value.to_json()),
                    })),
                    Err(error) => tool_error(error.to_string()),
                }
            }
        },
    );

    let exec_process = capability.process.clone();
    let builtins = capability.builtins.clone();
    let allowed_scripts = match &capability.access {
        ReasoningAccess::Full => None,
        ReasoningAccess::ReadExec(scripts) => Some(Arc::clone(scripts)),
    };
    let exec = FunctionTool::new(
        "exec",
        "Execute an absolute /lib script or host-attached /bin built-in path. Inside Svit Lisp, \
         use `(exec \"/bin/name\" input)` or `(exec \"/lib/name\" input)`, with the path first.",
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}, "input": {}},
            "required": ["path", "input"]
        }),
        move |arguments| {
            let process = exec_process.clone();
            let allowed_scripts = allowed_scripts.clone();
            let builtins = builtins.clone();
            async move {
                let path = arguments["path"].as_str().unwrap_or_default();
                if let Some(name) = path.strip_prefix("/bin/") {
                    let Some(builtins) = builtins else {
                        return tool_error("built-in not found");
                    };
                    return builtins
                        .execute(name, arguments["input"].clone(), process.view())
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
                execute_script(
                    &process,
                    path,
                    arguments["input"].clone(),
                    builtins.as_ref(),
                )
                .await
            }
        },
    );

    if matches!(capability.access, ReasoningAccess::ReadExec(_)) {
        return vec![discover, read, stat, exec];
    }

    let write_process = capability.process.clone();
    let write = FunctionTool::new(
        "write",
        "Transactionally write process memory or one named Svit Lisp script. A /lib value is \
         `{\"source\": \"(define (main input) ...)\", \"documentation\": \"...\"}`. \
         Scripts compose executables with `(exec \"/bin/name\" input)` or \
         `(exec \"/lib/name\" input)`, with the path first.",
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
                match process.write(path, value).await {
                    Ok(change) => tool_json(json!({
                        "path": path,
                        "version": change.version(),
                        "changed": change.paths(),
                    })),
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
                match process.remove(path).await {
                    Ok(change) => tool_json(json!({
                        "path": path,
                        "version": change.version(),
                        "changed": change.paths(),
                    })),
                    Err(error) => tool_error(error.to_string()),
                }
            }
        },
    );

    vec![discover, read, stat, write, remove, exec]
}

async fn execute_script(
    process: &ProcessState,
    path: &str,
    input: JsonValue,
    builtins: Option<&Builtins>,
) -> ToolExecutionResult {
    let input = match Value::from_json(input) {
        Ok(input) => input,
        Err(error) => return tool_error(error.to_string()),
    };
    let activation = match process.exec(path, input, builtins).await {
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
// Svit owns the EventLog composition boundary, while the selected process
// adapter owns paged storage. Canonical events are deliberately not copied
// into the materialized process memory tree.
struct ProcessEventLog {
    process: ProcessState,
}

impl ProcessEventLog {
    #[cfg(test)]
    fn new(process: ProcessState) -> Self {
        Self { process }
    }

    fn seeded(process: ProcessState, _state: Option<&ThreadState>) -> SvitResult<Self> {
        Ok(Self { process })
    }
}

#[async_trait]
impl EventReader for ProcessEventLog {
    async fn read_page(&self, request: EventReadRequest) -> Result<EventPage, EventLogError> {
        self.process.event_log.read_page(request).await
    }
}

#[async_trait]
impl EventLog for ProcessEventLog {
    async fn append(&self, request: EventRequest) -> Result<Event, EventLogError> {
        if request.is_ephemeral() {
            return Err(EventLogError::InvalidAppend {
                detail: "ephemeral events are sink-only".into(),
            });
        }
        let state = {
            let view = self.process.view();
            let process = view.lock().map_err(|_| EventLogError::Backend {
                detail: "Svit process lock is unavailable".into(),
            })?;
            load_thread_state(&process).map_err(event_log_corruption)?
        };
        if state.as_ref().map(|state| state.session_id) != Some(request.session_id) {
            return Err(EventLogError::InvalidAppend {
                detail: "event belongs to a different Everruns session".into(),
            });
        }
        self.process.event_log.append(request).await
    }

    fn durability(&self) -> EventDurability {
        self.process.event_log.durability()
    }
}

#[derive(Clone)]
struct ThreadState {
    session_id: SessionId,
    process_id: Option<ProcessId>,
    instructions: Option<String>,
    system_prompt: String,
    legacy_events: Option<Vec<Event>>,
}

#[cfg(test)]
async fn initialize_thread_state(
    process: &ProcessState,
    session_id: SessionId,
    instructions: Option<&str>,
    system_prompt: &str,
) -> SvitResult<()> {
    let state = {
        let view = process.view();
        let process = view.lock().map_err(|_| SvitError::ProcessUnavailable)?;
        load_thread_state(&process).map_err(SvitError::from)?
    };
    initialize_thread_state_from(
        process,
        &thread_process_id(process)?,
        session_id,
        instructions,
        system_prompt,
        state.as_ref(),
    )
    .await
}

async fn initialize_thread_state_from(
    process: &ProcessState,
    process_id: &ProcessId,
    session_id: SessionId,
    instructions: Option<&str>,
    system_prompt: &str,
    state: Option<&ThreadState>,
) -> SvitResult<()> {
    match state {
        Some(state)
            if state.process_id.as_ref() == Some(process_id)
                && state.instructions.as_deref() == instructions
                && state.system_prompt == system_prompt =>
        {
            Ok(())
        }
        Some(_) => {
            let value = thread_state_value(session_id, process_id, instructions, system_prompt)
                .map_err(SvitError::from)?;
            process.replace_thread_state(value).await?;
            Ok(())
        }
        None => {
            let value = thread_state_value(session_id, process_id, instructions, system_prompt)
                .map_err(SvitError::from)?;
            process.initialize_thread_state(value).await?;
            Ok(())
        }
    }
}

async fn migrate_legacy_thread_state(
    process: &ProcessState,
    process_id: &ProcessId,
    state: Option<&ThreadState>,
) -> SvitResult<()> {
    let Some(state) = state else {
        return Ok(());
    };
    let Some(legacy_events) = state.legacy_events.as_deref() else {
        return Ok(());
    };

    // The adapter imports the complete canonical prefix atomically and treats
    // an identical existing row as success, so crash recovery cannot duplicate
    // the former materialized history.
    process.import_thread_events(legacy_events).await?;
    let bounded = thread_state_value(
        state.session_id,
        process_id,
        state.instructions.as_deref(),
        &state.system_prompt,
    )
    .map_err(SvitError::from)?;
    process.replace_thread_state(bounded).await?;
    Ok(())
}

fn event_request_from(event: &Event) -> EventRequest {
    EventRequest {
        event_type: event.event_type.clone(),
        ts: event.ts,
        session_id: event.session_id,
        context: event.context.clone(),
        data: event.data.clone(),
        metadata: event.metadata.clone(),
        tags: event.tags.clone(),
    }
}

fn load_thread_state(process: &Process) -> EverrunsResult<Option<ThreadState>> {
    let Some(value) = process.read(THREAD_STATE_PATH).map_err(event_error)? else {
        return Ok(None);
    };
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let json = value.to_json();
    let format = json
        .get("format")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| event_error("missing /thread format"))?;
    if !matches!(
        format,
        THREAD_STATE_FORMAT | PREVIOUS_THREAD_STATE_FORMAT | LEGACY_THREAD_STATE_FORMAT
    ) {
        return Err(event_error("invalid /thread format"));
    }
    let session_id = json
        .get("session_id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| event_error("missing /thread session_id"))?
        .parse()
        .map_err(event_error)?;
    let process_id = match (format, json.get("process_id")) {
        (THREAD_STATE_FORMAT, Some(JsonValue::String(process_id))) => {
            Some(ProcessId::new(process_id.clone()).map_err(event_error)?)
        }
        (THREAD_STATE_FORMAT, _) => return Err(event_error("missing /thread process_id")),
        (_, Some(JsonValue::String(process_id))) => {
            Some(ProcessId::new(process_id.clone()).map_err(event_error)?)
        }
        _ => None,
    };
    let system_prompt = json
        .get("system_prompt")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| event_error("missing /thread system_prompt"))?
        .to_owned();
    let instructions = match json.get("instructions") {
        Some(JsonValue::String(instructions)) => Some(instructions.clone()),
        Some(JsonValue::Null) => None,
        _ => return Err(event_error("invalid /thread instructions")),
    };
    let legacy_events = if format == LEGACY_THREAD_STATE_FORMAT {
        let messages: Vec<Message> = serde_json::from_value(
            json.get("messages")
                .cloned()
                .ok_or_else(|| event_error("missing legacy /thread messages"))?,
        )
        .map_err(event_error)?;
        let events: Vec<Event> = serde_json::from_value(
            json.get("events")
                .cloned()
                .ok_or_else(|| event_error("missing legacy /thread events"))?,
        )
        .map_err(event_error)?;
        validate_thread_events(session_id, &events)?;
        let derived = messages_from_events(&events);
        if serde_json::to_value(&messages).map_err(event_error)?
            != serde_json::to_value(&derived).map_err(event_error)?
        {
            return Err(event_error(
                "legacy /thread messages do not match the Everruns event projection",
            ));
        }
        Some(events)
    } else {
        if json.get("events").is_some() || json.get("messages").is_some() {
            return Err(event_error(
                "bounded /thread metadata contains materialized history",
            ));
        }
        None
    };
    Ok(Some(ThreadState {
        session_id,
        process_id,
        instructions,
        system_prompt,
        legacy_events,
    }))
}

fn validate_thread_events(session_id: SessionId, events: &[Event]) -> EverrunsResult<()> {
    // THREAT[TM-AUD-001]: The process-backed EventLog accepts only one
    // session with unique IDs and a contiguous canonical sequence.
    let mut event_ids = HashSet::with_capacity(events.len());
    for (index, event) in events.iter().enumerate() {
        if event.session_id != session_id {
            return Err(event_error(
                "Svit thread state contains an event for another session",
            ));
        }
        let expected = i32::try_from(index + 1)
            .map_err(|_| event_error("Everruns event sequence limit exceeded"))?;
        if event.sequence != Some(expected) {
            return Err(event_error("Svit thread event sequence is invalid"));
        }
        if !event_ids.insert(event.id) {
            return Err(event_error("Svit thread event ID is duplicated"));
        }
    }
    Ok(())
}

fn thread_state_value(
    session_id: SessionId,
    process_id: &ProcessId,
    instructions: Option<&str>,
    system_prompt: &str,
) -> EverrunsResult<Value> {
    // THREAT[TM-AUD-001]: `/thread` is bounded host metadata. Canonical event
    // history remains in the paged EventLog selected by the Svit owner.
    let value = Value::from_json(json!({
        "format": THREAD_STATE_FORMAT,
        "session_id": session_id.to_string(),
        "process_id": process_id.as_str(),
        "instructions": instructions,
        "system_prompt": system_prompt,
    }))
    .map_err(event_error)?;
    Ok(value)
}

fn thread_process_id(process: &ProcessState) -> SvitResult<ProcessId> {
    Ok(process
        .view()
        .lock()
        .map_err(|_| SvitError::ProcessUnavailable)?
        .id()
        .clone())
}

fn is_process_fork(process: &ProcessState) -> SvitResult<bool> {
    Ok(matches!(
        process
            .view()
            .lock()
            .map_err(|_| SvitError::ProcessUnavailable)?
            .read("/system/lineage/parent")?,
        Some(Value::String(_))
    ))
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

fn event_error(error: impl fmt::Display) -> AgentLoopError {
    AgentLoopError::event(error.to_string())
}

fn event_log_corruption(error: impl fmt::Display) -> EventLogError {
    EventLogError::Corruption {
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use everruns::core::events::{EventContext, InputMessageData};

    use super::*;

    #[tokio::test]
    async fn concurrent_event_appends_receive_distinct_sequences() {
        let session_id = SessionId::new();
        let process = Process::builder("svit://local/concurrent-events")
            .unwrap()
            .build()
            .unwrap();
        let process = ProcessState::volatile(process);
        initialize_thread_state(&process, session_id, None, "system")
            .await
            .unwrap();
        let event_log = ProcessEventLog::new(process.clone());
        let first = EventRequest::new(
            session_id,
            EventContext::empty(),
            InputMessageData::new(Message::user("first")),
        );
        let second = EventRequest::new(
            session_id,
            EventContext::empty(),
            InputMessageData::new(Message::user("second")),
        );

        let (first, second) = tokio::join!(event_log.append(first), event_log.append(second));

        first.unwrap();
        second.unwrap();
        let page = event_log
            .read_page(EventReadRequest::new(
                session_id,
                everruns_host::EventReadLimit::default(),
            ))
            .await
            .unwrap();
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![Some(1), Some(2)]
        );
    }

    #[tokio::test]
    async fn thread_history_does_not_grow_the_process_snapshot() {
        let session_id = SessionId::new();
        let process = Process::builder("svit://local/growing-thread")
            .unwrap()
            .limits(Limits {
                max_value_entries: 80,
                ..Limits::default()
            })
            .build()
            .unwrap();
        let process = ProcessState::volatile(process);
        initialize_thread_state(&process, session_id, None, "system")
            .await
            .unwrap();
        let event_log = ProcessEventLog::new(process.clone());
        let snapshot_bytes = process.view().lock().unwrap().snapshot().unwrap().len();

        for index in 0..12 {
            event_log
                .append(EventRequest::new(
                    session_id,
                    EventContext::empty(),
                    InputMessageData::new(Message::user(format!("message {index}"))),
                ))
                .await
                .unwrap();
        }

        let view = process.view();
        let process = view.lock().unwrap();
        let thread = process.read("/thread").unwrap().unwrap().to_json();
        assert!(thread.get("events").is_none());
        assert!(thread.get("messages").is_none());
        assert_eq!(process.snapshot().unwrap().len(), snapshot_bytes);
    }

    #[tokio::test]
    async fn paged_event_log_observes_later_appends() {
        let session_id = SessionId::new();
        let process = Process::builder("svit://local/seeded-event-cache")
            .unwrap()
            .build()
            .unwrap();
        let process = ProcessState::volatile(process);
        initialize_thread_state(&process, session_id, None, "system")
            .await
            .unwrap();
        let initial_log = ProcessEventLog::new(process.clone());
        initial_log
            .append(EventRequest::new(
                session_id,
                EventContext::empty(),
                InputMessageData::new(Message::user("first")),
            ))
            .await
            .unwrap();
        let state = {
            let view = process.view();
            let process = view.lock().unwrap();
            load_thread_state(&process).unwrap().unwrap()
        };
        let event_log = ProcessEventLog::seeded(process, Some(&state)).unwrap();
        let request = EventReadRequest::new(session_id, everruns_host::EventReadLimit::default());
        assert_eq!(
            event_log
                .read_page(request.clone())
                .await
                .unwrap()
                .events
                .len(),
            1
        );

        event_log
            .append(EventRequest::new(
                session_id,
                EventContext::empty(),
                InputMessageData::new(Message::user("second")),
            ))
            .await
            .unwrap();

        assert_eq!(event_log.read_page(request).await.unwrap().events.len(), 2);
    }
}
