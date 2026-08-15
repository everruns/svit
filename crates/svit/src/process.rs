// The process root is the only durable guest state. Activations work on Lisp
// copies and replace the Arc root only after every value and staged script has
// validated, providing rollback without a mutable undo log.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ketos::module::NullModuleLoader;
use ketos::{
    Builder as KetosBuilder, Context as KetosContext, Error as KetosError, ForeignValue, GlobalIo,
    Integer as KetosInteger, Interpreter as KetosInterpreter, RestrictConfig, RestrictError,
    Value as KetosValue,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::sanitize_diagnostic;
use crate::hooks::{
    ActivationEvent, ActivationHook, ActivationRequest, ActivationStatus, HookAction, SharedHook,
};
use crate::mounts::{Mount, MountDescriptor, MountPath, MountRegistry, MountView};
use crate::persistence::Mutation;
use crate::{Builtins, Error, Limits, Result, Script, Value};

const SNAPSHOT_FORMAT: u32 = 7;
const RUNTIME_LANGUAGE: &str = "svit-lisp@2";
const MAX_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;
const MAX_THREAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_THREAD_RECORDS: usize = 100_000;

/// Stable logical address of one Svit process.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProcessId(String);

impl ProcessId {
    /// Parses and validates a logical process address.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_process_id(&value)?;
        Ok(Self(value))
    }

    /// Returns the address string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ProcessId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<ProcessId> for String {
    fn from(value: ProcessId) -> Self {
        value.0
    }
}

impl fmt::Display for ProcessId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One structured log record emitted during an activation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogRecord {
    /// Human-readable message.
    pub message: String,
    /// Structured fields supplied by the script.
    pub fields: Value,
}

/// A committed intent to deliver a message to another process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageIntent {
    /// Deterministic identifier derived from sender, commit version, and index.
    pub message_id: String,
    /// Destination logical process address.
    pub to: ProcessId,
    /// Persistent message body.
    pub body: Value,
}

/// What one committed transition changed.
///
/// `mutations` are the replayable operations on committed state, and are what
/// the durable event store records. `paths` is the observer-facing set: the
/// same committed paths plus any mount path the transition wrote, because a
/// client caching that node must read it again even though nothing was
/// persisted for it.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    version: u64,
    paths: Vec<String>,
    mutations: Vec<Mutation>,
}

impl Change {
    /// Records a change reported to an observer.
    ///
    /// A notification carries the version and paths but no values: observers
    /// read what they need back through the process API rather than receiving
    /// committed state on a broadcast channel.
    pub fn notification(version: u64, paths: Vec<String>) -> Self {
        Self {
            version,
            paths,
            mutations: Vec::new(),
        }
    }

    /// Returns this change with its values dropped, ready to publish.
    pub fn to_notification(&self) -> Self {
        Self::notification(self.version, self.paths.clone())
    }

    /// Returns the process version after this transition.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Returns the canonical changed paths in deterministic order.
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// Returns the replayable committed operations.
    ///
    /// Empty on a change received as a notification, and on a transition that
    /// touched no committed state, such as a granted mount write.
    pub fn mutations(&self) -> &[Mutation] {
        &self.mutations
    }

    /// Reports whether a cached `path` could be stale after this transition.
    ///
    /// A path is affected when it is at, below, or above a changed path: a
    /// write below a node changes that node's value and can change its child
    /// listing. Clients share this predicate so they invalidate identically.
    pub fn touches(&self, path: &str) -> bool {
        self.paths.iter().any(|changed| overlapping(changed, path))
    }
}

fn overlapping(left: &str, right: &str) -> bool {
    fn covers(parent: &str, child: &str) -> bool {
        parent == "/"
            || child
                .strip_prefix(parent)
                .is_some_and(|rest| rest.starts_with('/'))
    }
    left == right || covers(left, right) || covers(right, left)
}

/// Result of a successfully committed activation.
///
/// This is also a control-protocol wire value. Unknown fields are deliberately
/// ignored so a known protocol major can evolve additively.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Activation {
    /// Value returned by the script's `main(input)` function.
    pub output: Value,
    /// Structured logs emitted during this activation.
    pub logs: Vec<LogRecord>,
    /// Newly committed message intents.
    pub messages: Vec<MessageIntent>,
    /// Process version after the commit.
    pub version: u64,
    /// SHA-256 of the canonical committed root encoding.
    pub root_hash: String,
    /// Canonical paths this activation changed, including mount paths it
    /// wrote. Clients use these to invalidate exactly what went stale.
    #[serde(default)]
    pub changed: Vec<String>,
}

/// Builder for a conventional process namespace with initial memory, scripts,
/// limits, generated system metadata, and frozen hooks.
pub struct ProcessBuilder {
    id: ProcessId,
    memory: BTreeMap<String, Value>,
    scripts: BTreeMap<String, Script>,
    mounts: MountRegistry,
    limits: Limits,
    hooks: Vec<SharedHook>,
}

impl ProcessBuilder {
    /// Adds or replaces a named value in the initial `/memory` map.
    pub fn memory(mut self, name: impl Into<String>, value: Value) -> Self {
        self.memory.insert(name.into(), value);
        self
    }

    /// Adds or replaces a named script in the initial `/lib` library.
    pub fn library(mut self, name: impl Into<String>, script: Script) -> Self {
        self.scripts.insert(name.into(), script);
        self
    }

    /// Attaches a named virtual mount below `/mounts`.
    ///
    /// The mount contributes a committed descriptor and a host-owned provider.
    /// No source data is copied into the process root.
    pub fn mount(mut self, name: impl Into<String>, mount: Mount) -> Self {
        // THREAT[TM-CAP-001]: Accept only the host-created mount domain type;
        // guest-visible names and values cannot construct live authority.
        self.mounts.attach(name.into(), mount);
        self
    }

    /// Replaces the process resource limits.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Registers a typed interceptor hook.
    pub fn hook(mut self, hook: impl ActivationHook + 'static) -> Self {
        self.hooks.push(Arc::new(hook));
        self
    }

    /// Validates the initial state and constructs the process.
    pub fn build(self) -> Result<Process> {
        self.limits.validate()?;
        for name in self.mounts.names() {
            crate::mounts::validate_mount_name(name)?;
        }
        // Builder scripts are version-zero state; only post-build saves commit a version.
        let root = initial_root(
            &self.id,
            &self.limits,
            self.memory,
            self.scripts,
            self.mounts.descriptors(),
        );
        validate_root(&root, &self.id, &self.limits)?;
        Ok(Process {
            id: self.id,
            version: 0,
            root: Arc::new(root),
            limits: self.limits,
            hooks: self.hooks.into(),
            mounts: Arc::new(self.mounts),
        })
    }
}

/// In-memory, serializable Svit process.
#[derive(Clone)]
pub struct Process {
    id: ProcessId,
    version: u64,
    root: Arc<Value>,
    limits: Limits,
    hooks: Arc<[SharedHook]>,
    // Host authority, deliberately outside the serialized boundary.
    mounts: Arc<MountRegistry>,
}

impl Process {
    /// Starts a process builder for a globally meaningful logical address.
    pub fn builder(id: impl Into<String>) -> Result<ProcessBuilder> {
        Ok(ProcessBuilder {
            id: ProcessId::new(id)?,
            memory: BTreeMap::new(),
            scripts: BTreeMap::new(),
            mounts: MountRegistry::default(),
            limits: Limits::default(),
            hooks: Vec::new(),
        })
    }

    /// Creates a process with default limits and empty memory.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        Self::builder(id)?.build()
    }

    /// Returns this process's stable logical address.
    pub fn id(&self) -> &ProcessId {
        &self.id
    }

    /// Returns the current committed version.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Returns the configured limits.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Returns the SHA-256 integrity hash of the canonical committed root.
    ///
    /// This detects accidental or malicious byte changes but does not
    /// authenticate who created the state.
    pub fn root_hash(&self) -> String {
        root_hash(self.root.as_ref()).expect("validated process roots serialize")
    }

    /// Validates a replacement root and publishes it as one version.
    ///
    /// THREAT[TM-EFF-001]: Every host transition funnels through this single
    /// validation and assignment, so a rejected root leaves the committed
    /// state and version untouched.
    fn commit(
        &mut self,
        root: Value,
        mutations: Vec<Mutation>,
        extra_paths: Vec<String>,
    ) -> Result<Change> {
        validate_root(&root, &self.id, &self.limits)?;
        let mut paths = crate::persistence::touched_paths(&mutations)?;
        for path in extra_paths {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        paths.sort();
        let version = next_process_version(self.version)?;
        self.root = Arc::new(root);
        self.version = version;
        Ok(Change {
            version,
            paths,
            mutations,
        })
    }

    /// Attaches or replaces one mount provider on a live process.
    ///
    /// Restoring a snapshot restores mount descriptors but no provider. A host
    /// that wants the mount to resolve again reattaches it explicitly here.
    /// The committed descriptor is refreshed, so a mount whose identity
    /// changed commits one new version.
    pub fn attach_mount(&mut self, name: impl Into<String>, mount: Mount) -> Result<Change> {
        let name = name.into();
        crate::mounts::validate_mount_name(&name)?;
        let descriptor = mount.provider().descriptor().to_value();
        let mut mounts = self.mounts.as_ref().clone();
        mounts.attach(name.clone(), mount);
        self.mounts = Arc::new(mounts);

        let committed = root_map(self.root.as_ref())?;
        let recorded = root_map(committed.get("mounts").expect("validated process root"))?;
        if recorded.get(&name) == Some(&descriptor) {
            return Ok(Change {
                version: self.version,
                paths: Vec::new(),
                mutations: Vec::new(),
            });
        }
        let mut root = committed.clone();
        let mount_path = format!("/mounts/{name}");
        root_map_mut(root.get_mut("mounts").expect("validated process root"))?
            .insert(name, descriptor.clone());
        self.commit(
            Value::Map(root),
            vec![Mutation::Set {
                path: mount_path,
                value: descriptor,
            }],
            Vec::new(),
        )
    }

    /// Reads a value by absolute slash-separated process path.
    ///
    /// Paths below `/mounts/<name>` resolve through the attached provider at
    /// call time. A mount directory reads as its facts record; a mount leaf
    /// reads as its content.
    pub fn read(&self, path: &str) -> Result<Option<Value>> {
        if let Some((name, mount_path)) = mount_target(path)? {
            return self.mount_view().read(&name, &mount_path);
        }
        Ok(self.read_committed(path)?.cloned())
    }

    /// Returns the facts record describing one process path.
    ///
    /// Every node reports its kind, granted access, and locality, so a caller
    /// can weigh the cost of a read before making it. Committed process state
    /// is `cache`; mount nodes report the locality their provider declared.
    pub fn stat(&self, path: &str) -> Result<Option<Value>> {
        if let Some((name, mount_path)) = mount_target(path)? {
            return self.mount_view().stat(&name, &mount_path);
        }
        let Some(value) = self.read_committed(path)? else {
            return Ok(None);
        };
        Ok(Some(committed_stat_value(path, value)))
    }

    /// Discovers the deterministic child names below a map, array, or mount
    /// directory path.
    pub fn discover(&self, path: &str) -> Result<Vec<String>> {
        if let Some((name, mount_path)) = mount_target(path)? {
            return self.mount_view().discover(&name, &mount_path);
        }
        let value = self
            .read_committed(path)?
            .ok_or_else(|| Error::InvalidPath(path.into()))?;
        match value {
            Value::Map(values) => Ok(values.keys().cloned().collect()),
            Value::Array(values) => Ok((0..values.len()).map(|index| index.to_string()).collect()),
            _ => Err(Error::InvalidPath(path.into())),
        }
    }

    fn mount_view(&self) -> MountView<'_> {
        MountView::new(
            self.mounts.as_ref(),
            root_map(self.root.as_ref())
                .ok()
                .and_then(|root| root.get("mounts"))
                .unwrap_or(&Value::Null),
            &self.limits,
        )
    }

    fn read_committed(&self, path: &str) -> Result<Option<&Value>> {
        read_value_path(self.root.as_ref(), path)
    }

    /// Writes through the schema selected by an absolute path.
    ///
    /// Writes below `/mounts/<name>` reach the external source directly and
    /// do not change the committed process version.
    pub fn write(&mut self, path: &str, value: Value) -> Result<Change> {
        if let Some((name, mount_path)) = mount_target(path)? {
            // THREAT[TM-EFF-006]: A host mount write is an external effect. It
            // is deliberately not part of a process version transition, but it
            // is still reported so observers re-read the node.
            self.mount_view().write(&name, &mount_path, value)?;
            return Ok(Change {
                version: self.version,
                paths: vec![path.to_owned()],
                mutations: Vec::new(),
            });
        }
        let mut root = root_map(self.root.as_ref())?.clone();
        let committed = if let Some(memory_path) = memory_path(path)? {
            let memory = root
                .get_mut("memory")
                .expect("validated process root has memory");
            set_value_path(memory, memory_path, value.clone())?;
            value
        } else if let Some(name) = library_path(path)? {
            let script = script_from_write_value(value)?;
            let library = root_map_mut(root.get_mut("lib").expect("validated process root"))?;
            library.insert(name.into(), Value::Script(script.clone()));
            Value::Script(script)
        } else {
            return Err(Error::InvalidPath(path.into()));
        };
        self.commit(
            Value::Map(root),
            vec![Mutation::Set {
                path: path.into(),
                value: committed,
            }],
            Vec::new(),
        )
    }

    /// Removes through the schema selected by an absolute path.
    ///
    /// Removals below `/mounts/<name>` reach the external source directly and
    /// do not change the committed process version.
    pub fn remove(&mut self, path: &str) -> Result<Change> {
        if let Some((name, mount_path)) = mount_target(path)? {
            // THREAT[TM-EFF-006]: External removal, outside the version chain.
            self.mount_view().remove(&name, &mount_path)?;
            return Ok(Change {
                version: self.version,
                paths: vec![path.to_owned()],
                mutations: Vec::new(),
            });
        }
        let mut root = root_map(self.root.as_ref())?.clone();
        if let Some(memory_path) = memory_path(path)? {
            let memory = root
                .get_mut("memory")
                .expect("validated process root has memory");
            remove_value_path(memory, memory_path)?;
        } else if let Some(name) = library_path(path)? {
            let library = root_map_mut(root.get_mut("lib").expect("validated process root"))?;
            if library.remove(name).is_none() {
                return Err(Error::InvalidPath(path.into()));
            }
        } else {
            return Err(Error::InvalidPath(path.into()));
        }
        self.commit(
            Value::Map(root),
            vec![Mutation::Remove { path: path.into() }],
            Vec::new(),
        )
    }

    /// Appends one host-supplied value to the durable process inbox.
    pub fn enqueue_inbox(&mut self, value: Value) -> Result<Change> {
        let mut root = root_map(self.root.as_ref())?.clone();
        let Value::Array(inbox) = root
            .get_mut("inbox")
            .expect("validated process root has inbox")
        else {
            return Err(Error::InvalidSnapshot("/inbox is not an array".into()));
        };
        inbox.push(value.clone());

        // THREAT[TM-MSG-002]: Inbox delivery becomes visible only after the
        // complete replacement root validates.
        self.commit(
            Value::Map(root),
            vec![Mutation::Append {
                path: "/inbox".into(),
                values: vec![value],
            }],
            Vec::new(),
        )
    }

    /// Returns the oldest committed inbox value without removing it.
    pub fn inbox_front(&self) -> Result<Option<&Value>> {
        let Value::Array(inbox) = self
            .read_committed("/inbox")?
            .expect("validated process root has inbox")
        else {
            return Err(Error::InvalidSnapshot("/inbox is not an array".into()));
        };
        Ok(inbox.first())
    }

    /// Removes the oldest inbox value if it still matches the completed turn.
    pub fn acknowledge_inbox(&mut self, expected: &Value) -> Result<Change> {
        let mut root = root_map(self.root.as_ref())?.clone();
        let Value::Array(inbox) = root
            .get_mut("inbox")
            .expect("validated process root has inbox")
        else {
            return Err(Error::InvalidSnapshot("/inbox is not an array".into()));
        };
        if inbox.first() != Some(expected) {
            return Err(Error::InboxConflict);
        }
        inbox.remove(0);

        // THREAT[TM-MSG-002]: A completed turn acknowledges only the exact
        // inbox head it observed, preventing silent drop or reordering.
        self.commit(
            Value::Map(root),
            vec![Mutation::RemoveFront {
                path: "/inbox".into(),
                expected_value_hash: root_hash(expected)?,
            }],
            Vec::new(),
        )
    }

    /// Initializes host-managed durable reasoning state under `/thread`.
    ///
    /// Guest scripts and generic reasoning tools may inspect this node through
    /// `read` and `discover`, but cannot write or remove it. The Everruns
    /// adapter uses this boundary for replay and audit state that untrusted
    /// model output must not rewrite.
    pub(crate) fn initialize_thread_state(&mut self, value: Value) -> Result<Change> {
        if !matches!(self.read("/thread")?, Some(Value::Null)) {
            return Err(Error::InvalidPath("/thread".into()));
        }
        self.replace_thread_state(value)
    }

    /// Replaces host-managed reasoning state for restore validation and migration.
    ///
    /// Runnable Svit persistence uses append-only thread transitions instead;
    /// this trusted-host boundary is retained for explicit snapshot assembly.
    pub fn replace_thread_state(&mut self, value: Value) -> Result<Change> {
        validate_thread_value(&value, &self.limits)?;
        let mut root = root_map(self.root.as_ref())?.clone();
        root.insert("thread".into(), value.clone());

        // THREAT[TM-AUD-001]: Only the trusted host receives this mutation
        // boundary; guest write/remove paths deliberately exclude `/thread`.
        self.commit(
            Value::Map(root),
            vec![Mutation::Set {
                path: "/thread".into(),
                value,
            }],
            Vec::new(),
        )
    }

    /// Replaces the host-managed built-in catalog under `/bin`.
    pub(crate) fn replace_builtins(&mut self, value: Value) -> Result<Change> {
        let builtins = root_map(&value)?;
        for name in builtins.keys() {
            validate_script_name(name)?;
        }
        value.validate(&self.limits, false)?;
        let mut root = root_map(self.root.as_ref())?.clone();
        if root.get("bin") == Some(&value) {
            return Ok(Change::notification(self.version, Vec::new()));
        }
        root.insert("bin".into(), value.clone());
        self.commit(
            Value::Map(root),
            vec![Mutation::Set {
                path: "/bin".into(),
                value,
            }],
            Vec::new(),
        )
    }

    /// Returns every committed but not externally acknowledged message intent.
    pub fn outbox(&self) -> Result<Vec<MessageIntent>> {
        let root = root_map(self.root.as_ref())?;
        let system = root_map(root.get("system").expect("validated process root"))?;
        let Value::Array(values) = system.get("outbox").expect("validated process root") else {
            return Err(Error::InvalidSnapshot(
                "/system/outbox is not an array".into(),
            ));
        };
        values.iter().map(message_from_value).collect()
    }

    /// Invokes a script transactionally by its absolute `/lib` path.
    ///
    /// The script must define `main(input)`. Any error leaves memory, scripts,
    /// outbox, and version unchanged.
    pub fn exec(&mut self, path: &str, input: Value) -> Result<Activation> {
        let prepared = self.prepare_exec(path, input)?;
        Ok(self.commit_prepared(prepared))
    }

    pub(crate) fn prepare_exec(&self, path: &str, input: Value) -> Result<PreparedActivation> {
        let script = library_path(path)?
            .ok_or_else(|| Error::InvalidPath(path.into()))?
            .to_owned();
        let version_before = self.version;
        let mut request = Ok(ActivationRequest {
            script: script.clone(),
            input,
        });

        if !self.hooks.is_empty() {
            for hook in self.hooks.iter() {
                let current = match request {
                    Ok(current) => current,
                    Err(_) => break,
                };
                request = match hook.before_activation(current) {
                    HookAction::Continue(current) => Ok(current),
                    HookAction::Cancel(reason) => Err(Error::HookCancelled(reason)),
                };
            }
        }

        let event_script = request
            .as_ref()
            .map(|request| request.script.clone())
            .unwrap_or_else(|_| script);
        let result = request.and_then(|request| {
            let mut candidate = self.clone();
            candidate
                .exec_inner(request)
                .map(|(activation, mutations)| PreparedActivation {
                    candidate,
                    activation,
                    mutations,
                    event_script: event_script.clone(),
                    version_before,
                })
        });

        if !self.hooks.is_empty()
            && let Err(error) = &result
        {
            let event = ActivationEvent {
                process_id: self.id.clone(),
                script: event_script,
                version_before,
                status: ActivationStatus::Failed {
                    error: sanitize_diagnostic(error),
                },
            };
            for hook in self.hooks.iter() {
                hook.after_activation(&event);
            }
        }

        result
    }

    pub(crate) async fn prepare_exec_with_builtins(
        &self,
        path: &str,
        input: Value,
        builtins: &Builtins,
        context: Arc<Mutex<Process>>,
    ) -> Result<PreparedActivation> {
        let script = library_path(path)?
            .ok_or_else(|| Error::InvalidPath(path.into()))?
            .to_owned();
        let version_before = self.version;
        let mut request = Ok(ActivationRequest {
            script: script.clone(),
            input,
        });
        for hook in self.hooks.iter() {
            let current = match request {
                Ok(current) => current,
                Err(_) => break,
            };
            request = match hook.before_activation(current) {
                HookAction::Continue(current) => Ok(current),
                HookAction::Cancel(reason) => Err(Error::HookCancelled(reason)),
            };
        }

        let event_script = request
            .as_ref()
            .map(|request| request.script.clone())
            .unwrap_or_else(|_| script);
        let result = async {
            let request = request?;
            let mut replay = Vec::new();
            let mut execution_time = Duration::from_millis(self.limits.max_execution_millis);
            loop {
                let mut candidate = self.clone();
                let step =
                    candidate.exec_inner_step(request.clone(), replay.clone(), execution_time);
                match step? {
                    ExecStep::Complete(activation, mutations) => {
                        return Ok(PreparedActivation {
                            candidate,
                            activation,
                            mutations,
                            event_script: event_script.clone(),
                            version_before,
                        });
                    }
                    ExecStep::Builtin(call, elapsed) => {
                        execution_time = execution_time
                            .checked_sub(elapsed)
                            .ok_or(Error::ExecutionLimitExceeded)?;
                        let Some(name) = call.path.strip_prefix("/bin/") else {
                            return Err(Error::InvalidPath(call.path));
                        };
                        // THREAT[TM-CAP-004] THREAT[TM-EFF-005]: The guest can
                        // select only a built-in installed by this Svit host.
                        // The call is immediate and recorded for deterministic
                        // replay of the remaining pure script segments; it is
                        // never represented as committed or restorable authority.
                        let output = builtins
                            .execute_value(name, call.input.to_json(), context.clone())
                            .await
                            .map_err(|error| {
                                Error::Script(sanitize_diagnostic(format!(
                                    "built-in {} failed: {error}",
                                    call.path
                                )))
                            })?;
                        replay.push(BuiltinReplay {
                            path: call.path,
                            input: call.input,
                            output: Value::from_json(output)?,
                        });
                    }
                }
            }
        }
        .await;

        if let Err(error) = &result {
            let event = ActivationEvent {
                process_id: self.id.clone(),
                script: event_script,
                version_before,
                status: ActivationStatus::Failed {
                    error: sanitize_diagnostic(error),
                },
            };
            for hook in self.hooks.iter() {
                hook.after_activation(&event);
            }
        }

        result
    }

    pub(crate) fn commit_prepared(&mut self, prepared: PreparedActivation) -> Activation {
        self.root = prepared.candidate.root;
        self.version = prepared.candidate.version;
        if !self.hooks.is_empty() {
            let event = ActivationEvent {
                process_id: self.id.clone(),
                script: prepared.event_script,
                version_before: prepared.version_before,
                status: ActivationStatus::Committed {
                    version: prepared.activation.version,
                },
            };
            for hook in self.hooks.iter() {
                hook.after_activation(&event);
            }
        }
        prepared.activation
    }

    #[cfg(feature = "persistence-turso")]
    pub(crate) fn fail_prepared(&self, prepared: &PreparedActivation, error: &Error) {
        if self.hooks.is_empty() {
            return;
        }
        let event = ActivationEvent {
            process_id: self.id.clone(),
            script: prepared.event_script.clone(),
            version_before: prepared.version_before,
            status: ActivationStatus::Failed {
                error: sanitize_diagnostic(error),
            },
        };
        for hook in self.hooks.iter() {
            hook.after_activation(&event);
        }
    }

    /// Serializes a committed process boundary.
    pub fn snapshot(&self) -> Result<Vec<u8>> {
        let snapshot = Snapshot {
            format: SNAPSHOT_FORMAT,
            id: self.id.clone(),
            version: self.version,
            root: self.root.as_ref().clone(),
            root_hash: self.root_hash(),
            limits: self.limits.clone(),
        };
        serde_json::to_vec(&snapshot)
            .map_err(|error| Error::InvalidSnapshot(sanitize_diagnostic(error)))
    }

    /// Restores a process from a committed snapshot.
    ///
    /// Host hooks and mount providers are intentionally not serialized and
    /// must be attached by the host when constructing a new policy boundary.
    /// Until then, restored mounts report `attached: false` and every read,
    /// listing, and write below their root fails closed.
    pub fn restore(bytes: &[u8]) -> Result<Self> {
        Self::restore_snapshot(bytes).map(|(process, _)| process)
    }

    /// Restores a process and returns the already-validated root hash carried
    /// by its snapshot. Persistence uses this to check boundary metadata
    /// without serializing the complete restored root a second time.
    #[cfg(feature = "persistence-turso")]
    pub(crate) fn restore_with_declared_root_hash(bytes: &[u8]) -> Result<(Self, String)> {
        Self::restore_snapshot(bytes)
    }

    fn restore_snapshot(bytes: &[u8]) -> Result<(Self, String)> {
        // THREAT[TM-SNAP-001]: Bound untrusted bytes before the JSON decoder
        // can allocate from attacker-controlled lengths.
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(Error::InvalidSnapshot(
                "snapshot byte limit exceeded".into(),
            ));
        }
        let snapshot: Snapshot = serde_json::from_slice(bytes)
            .map_err(|error| Error::InvalidSnapshot(sanitize_diagnostic(error)))?;
        if snapshot.format != SNAPSHOT_FORMAT {
            return Err(Error::InvalidSnapshot(format!(
                "unsupported format {}",
                snapshot.format
            )));
        }
        snapshot.limits.validate()?;
        validate_root(&snapshot.root, &snapshot.id, &snapshot.limits)?;
        // THREAT[TM-SNAP-001]: Recompute integrity after decoding and complete
        // invariant validation. Hashes detect corruption, not provenance.
        if root_hash(&snapshot.root)? != snapshot.root_hash {
            return Err(Error::InvalidSnapshot("root hash mismatch".into()));
        }
        let declared_root_hash = snapshot.root_hash;
        Ok((
            Self {
                id: snapshot.id,
                version: snapshot.version,
                root: Arc::new(snapshot.root),
                limits: snapshot.limits,
                hooks: Arc::from([]),
                // THREAT[TM-CAP-008]: Restoring state never restores authority.
                mounts: Arc::new(MountRegistry::default()),
            },
            declared_root_hash,
        ))
    }

    /// Creates an independent child at the current committed boundary.
    ///
    /// The committed namespace is copied, discoverable identity and lineage
    /// are updated, and parent message intents are not duplicated into the
    /// child. Mount providers are shared with the child because the host chose
    /// to attach them; a fork that must not share an external source has to be
    /// given its own mounts through [`Self::attach_mount`].
    pub fn fork(&self, child_id: impl Into<String>) -> Result<Self> {
        // THREAT[TM-FORK-001]: Clone the committed root before clearing
        // process-local delivery state; subsequent child commits replace only
        // the child's root.
        let child_id = ProcessId::new(child_id)?;
        let mut root = root_map(self.root.as_ref())?.clone();
        let system = root_map_mut(root.get_mut("system").expect("validated process root"))?;
        system.insert("outbox".into(), Value::Array(Vec::new()));
        system.insert("identity".into(), identity_value(&child_id));
        system.insert("lineage".into(), lineage_value(Some(&self.id)));
        let root = Value::Map(root);
        validate_root(&root, &child_id, &self.limits)?;
        Ok(Self {
            id: child_id,
            version: self.version,
            root: Arc::new(root),
            limits: self.limits.clone(),
            hooks: Arc::clone(&self.hooks),
            mounts: Arc::clone(&self.mounts),
        })
    }

    pub(crate) fn apply_persisted_mutations(
        &mut self,
        version_before: u64,
        version_after: u64,
        mutations: &[Mutation],
    ) -> Result<()> {
        if self.version != version_before || version_before.checked_add(1) != Some(version_after) {
            return Err(Error::InvalidPersistence(
                "process version transition is invalid".into(),
            ));
        }
        let mut root = self.root.as_ref().clone();
        for mutation in mutations {
            apply_mutation(&mut root, mutation)?;
        }
        validate_root(&root, &self.id, &self.limits)?;
        self.root = Arc::new(root);
        self.version = version_after;
        Ok(())
    }

    /// Applies one validated event while reconstructing an unpublished owner.
    ///
    /// Unlike the public replay boundary, recovery may mutate its private root
    /// in place: an error discards the entire reconstruction, so it does not
    /// need a full rollback clone for every retained transaction.
    #[cfg(feature = "persistence-turso")]
    pub(crate) fn apply_persisted_mutations_for_recovery(
        &mut self,
        version_before: u64,
        version_after: u64,
        mutations: &[Mutation],
    ) -> Result<()> {
        if self.version != version_before || version_before.checked_add(1) != Some(version_after) {
            return Err(Error::InvalidPersistence(
                "process version transition is invalid".into(),
            ));
        }
        for mutation in mutations {
            match mutation {
                Mutation::Set { path, value } => {
                    value.validate(&self.limits, path == "/lib" || path.starts_with("/lib/"))?;
                }
                Mutation::Append { values, .. } => {
                    for value in values {
                        value.validate(&self.limits, false)?;
                    }
                }
                Mutation::Remove { .. } | Mutation::RemoveFront { .. } => {}
            }
        }
        let root = Arc::make_mut(&mut self.root);
        for mutation in mutations {
            apply_mutation(root, mutation)?;
        }
        self.version = version_after;
        Ok(())
    }

    #[cfg(feature = "persistence-turso")]
    pub(crate) fn validate_recovery_boundary(&self) -> Result<()> {
        validate_root(self.root.as_ref(), &self.id, &self.limits)
    }

    fn exec_inner(&mut self, request: ActivationRequest) -> Result<(Activation, Vec<Mutation>)> {
        match self.exec_inner_step(
            request,
            Vec::new(),
            Duration::from_millis(self.limits.max_execution_millis),
        )? {
            ExecStep::Complete(activation, mutations) => Ok((activation, mutations)),
            ExecStep::Builtin(_, _) => Err(Error::Script(
                "Svit Lisp /bin execution requires a Svit host".into(),
            )),
        }
    }

    fn exec_inner_step(
        &mut self,
        request: ActivationRequest,
        builtin_replay: Vec<BuiltinReplay>,
        execution_time: Duration,
    ) -> Result<ExecStep> {
        request.input.validate(&self.limits, false)?;
        let memory = self
            .read_committed("/memory")?
            .expect("validated process root")
            .clone();

        let state = RuntimeState::new(
            self.root.as_ref().clone(),
            memory,
            Arc::clone(&self.mounts),
            self.limits.clone(),
            execution_time,
            builtin_replay,
        );
        let execution_started = Instant::now();
        let execution = catch_unwind(AssertUnwindSafe(|| {
            run_guest_script(
                &state,
                &request.script,
                &request.input,
                &self.limits,
                self.limits.max_exec_depth,
            )
        }))
        .map_err(|_| Error::Script("Lisp interpreter failed".into()))?;
        let output = match execution {
            Ok(output) => {
                if !state.builtin_replay_complete()? {
                    return Err(Error::Script("built-in replay diverged".into()));
                }
                output
            }
            Err(error) => match state.pending_builtin()? {
                Some(call) => {
                    return Ok(ExecStep::Builtin(call, execution_started.elapsed()));
                }
                None => return Err(error),
            },
        };

        let new_memory = lock(&state.memory)?.clone();
        output.validate(&self.limits, false)?;
        new_memory.validate(&self.limits, false)?;

        let library_changes = lock(&state.library_changes)?.clone();
        for (name, script) in library_changes
            .iter()
            .filter_map(|(name, script)| script.as_ref().map(|script| (name, script)))
        {
            validate_script_name(name)?;
            script_value(script).validate(&self.limits, true)?;
            validate_script_source(name, script.source(), &self.limits)?;
        }

        let version = next_process_version(self.version)?;
        let staged_messages = lock(&state.messages)?.clone();
        let committed_messages = staged_messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| MessageIntent {
                // THREAT[TM-MSG-001]: Sender and ordering components are
                // derived by the host, never accepted from guest memory.
                message_id: format!("{}:{version}:{index}", self.id),
                to: message.to,
                body: message.body,
            })
            .collect::<Vec<_>>();

        let mut root = root_map(self.root.as_ref())?.clone();
        root.insert("memory".into(), new_memory);
        let lib = root_map_mut(root.get_mut("lib").expect("validated process root"))?;
        for (name, script) in library_changes {
            match script {
                Some(script) => {
                    lib.insert(name, Value::Script(script));
                }
                None => {
                    lib.remove(&name);
                }
            }
        }
        let system = root_map_mut(root.get_mut("system").expect("validated process root"))?;
        let Value::Array(outbox) = system.get_mut("outbox").expect("validated process root") else {
            return Err(Error::InvalidSnapshot(
                "/system/outbox is not an array".into(),
            ));
        };
        outbox.extend(committed_messages.iter().map(message_to_value));

        let new_root = Value::Map(root);
        validate_root(&new_root, &self.id, &self.limits)?;
        // Buffered mount effects run after every in-process validation and
        // before the commit, so a denied external write still rolls back.
        state.apply_mount_writes()?;
        // THREAT[TM-EFF-001]: This is the only activation commit point. Every
        // fallible guest conversion and staged-script validation is complete.
        self.root = Arc::new(new_root);
        self.version = version;

        let mut mutations = lock(&state.mutations)?.clone();
        if !committed_messages.is_empty() {
            mutations.push(Mutation::Append {
                path: "/system/outbox".into(),
                values: committed_messages.iter().map(message_to_value).collect(),
            });
        }
        // Mount effects are not replayable operations on committed state, so
        // they never enter the mutation list, but an observer caching those
        // nodes still has to read them again.
        let mut changed = crate::persistence::touched_paths(&mutations)?;
        for write in lock(&state.mount_writes)?.iter() {
            let path = format!("/mounts/{}{}", write.mount, write.path.display());
            let path = path.trim_end_matches('/').to_owned();
            if !changed.contains(&path) {
                changed.push(path);
            }
        }
        changed.sort();

        Ok(ExecStep::Complete(
            Activation {
                output,
                logs: lock(&state.logs)?.clone(),
                messages: committed_messages,
                version,
                root_hash: self.root_hash(),
                changed,
            },
            mutations,
        ))
    }
}

enum ExecStep {
    Complete(Activation, Vec<Mutation>),
    Builtin(BuiltinCall, Duration),
}

#[cfg_attr(not(feature = "persistence-turso"), allow(dead_code))]
pub(crate) struct PreparedActivation {
    candidate: Process,
    activation: Activation,
    mutations: Vec<Mutation>,
    event_script: String,
    version_before: u64,
}

#[cfg(feature = "persistence-turso")]
impl PreparedActivation {
    pub(crate) fn candidate(&self) -> &Process {
        &self.candidate
    }

    pub(crate) fn mutations(&self) -> &[Mutation] {
        &self.mutations
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    format: u32,
    id: ProcessId,
    version: u64,
    root: Value,
    root_hash: String,
    limits: Limits,
}

#[derive(Clone)]
struct StagedMessage {
    to: ProcessId,
    body: Value,
}

fn initial_root(
    id: &ProcessId,
    limits: &Limits,
    memory: BTreeMap<String, Value>,
    scripts: BTreeMap<String, Script>,
    mounts: BTreeMap<String, MountDescriptor>,
) -> Value {
    Value::Map(BTreeMap::from([
        ("thread".into(), Value::Null),
        ("bin".into(), Value::empty_map()),
        ("children".into(), Value::empty_map()),
        ("inbox".into(), Value::Array(Vec::new())),
        (
            "lib".into(),
            Value::Map(
                scripts
                    .into_iter()
                    .map(|(name, script)| (name, Value::Script(script)))
                    .collect(),
            ),
        ),
        ("memory".into(), Value::Map(memory)),
        (
            "mounts".into(),
            Value::Map(
                mounts
                    .iter()
                    .map(|(name, descriptor)| (name.clone(), descriptor.to_value()))
                    .collect(),
            ),
        ),
        ("system".into(), system_value(id, limits, None)),
        ("tasks".into(), Value::empty_map()),
    ]))
}

fn validate_root(root: &Value, id: &ProcessId, limits: &Limits) -> Result<()> {
    let root = root_map(root)?;
    if root.keys().map(String::as_str).collect::<BTreeSet<_>>()
        != BTreeSet::from([
            "thread", "bin", "children", "inbox", "lib", "memory", "mounts", "system", "tasks",
        ])
    {
        return Err(Error::InvalidSnapshot(
            "process root does not match the conventional namespace".into(),
        ));
    }
    validate_reserved_root(root, "children", Value::empty_map())?;
    let Value::Array(inbox) = root
        .get("inbox")
        .ok_or_else(|| Error::InvalidSnapshot("missing /inbox".into()))?
    else {
        return Err(Error::InvalidSnapshot("/inbox is not an array".into()));
    };
    Value::Array(inbox.clone()).validate(limits, false)?;
    crate::mounts::validate_mounts(
        root.get("mounts")
            .ok_or_else(|| Error::InvalidSnapshot("missing /mounts".into()))?,
        limits,
    )?;
    validate_reserved_root(root, "tasks", Value::empty_map())?;

    validate_thread_value(
        root.get("thread")
            .ok_or_else(|| Error::InvalidSnapshot("missing /thread".into()))?,
        limits,
    )?;

    let bin = root_map(
        root.get("bin")
            .ok_or_else(|| Error::InvalidSnapshot("missing /bin".into()))?,
    )?;
    for name in bin.keys() {
        validate_script_name(name)?;
    }
    Value::Map(bin.clone()).validate(limits, false)?;

    let memory = root
        .get("memory")
        .ok_or_else(|| Error::InvalidSnapshot("missing /memory".into()))?;
    memory.validate(limits, false)?;

    let lib = root_map(
        root.get("lib")
            .ok_or_else(|| Error::InvalidSnapshot("missing /lib".into()))?,
    )?;
    Value::Map(lib.clone()).validate(limits, true)?;
    for (name, value) in lib {
        validate_script_name(name)?;
        let Value::Script(script) = value else {
            return Err(Error::InvalidSnapshot(format!(
                "/lib/{name} is not a script"
            )));
        };
        script_value(script).validate(limits, true)?;
        validate_script_source(name, script.source(), limits)?;
    }

    let system = root_map(
        root.get("system")
            .ok_or_else(|| Error::InvalidSnapshot("missing /system".into()))?,
    )?;
    if system.keys().map(String::as_str).collect::<BTreeSet<_>>()
        != BTreeSet::from([
            "api",
            "capabilities",
            "identity",
            "limits",
            "lineage",
            "outbox",
            "runtime",
        ])
    {
        return Err(Error::InvalidSnapshot(
            "/system does not match the runtime metadata schema".into(),
        ));
    }
    for (name, expected) in [
        ("api", api_value()),
        ("capabilities", Value::Array(Vec::new())),
        ("identity", identity_value(id)),
        ("limits", limits_value(limits)),
        ("runtime", runtime_value()),
    ] {
        if system.get(name) != Some(&expected) {
            return Err(Error::InvalidSnapshot(format!(
                "/system/{name} does not match process metadata"
            )));
        }
    }
    validate_lineage(system.get("lineage"))?;
    let Value::Array(outbox) = system
        .get("outbox")
        .ok_or_else(|| Error::InvalidSnapshot("missing /system/outbox".into()))?
    else {
        return Err(Error::InvalidSnapshot(
            "/system/outbox is not an array".into(),
        ));
    };
    Value::Array(outbox.clone()).validate(limits, false)?;
    for message in outbox {
        message_from_value(message)?;
    }
    Ok(())
}

fn validate_thread_value(value: &Value, limits: &Limits) -> Result<()> {
    let Value::Map(thread) = value else {
        return value.validate(limits, false);
    };

    // THREAT[TM-DOS-003]: `/thread` is an append-only host collection, not one
    // guest value. Validate each untrusted envelope with the process value
    // limits, then bound the collection independently. Applying one entry
    // budget to the whole history makes valid durable sessions fail merely by
    // accumulating individually valid Everruns events.
    for (name, value) in thread {
        if matches!(name.as_str(), "events" | "messages")
            && let Value::Array(records) = value
        {
            if records.len() > MAX_THREAD_RECORDS {
                return Err(Error::InvalidValue(
                    "maximum thread records exceeded".into(),
                ));
            }
            for record in records {
                record.validate(limits, false)?;
            }
        } else {
            value.validate(limits, false)?;
        }
    }

    let encoded = serde_json::to_vec(value)
        .map_err(|_| Error::InvalidValue("thread serialization failed".into()))?;
    if encoded.len() > MAX_THREAD_BYTES {
        return Err(Error::InvalidValue("maximum thread bytes exceeded".into()));
    }
    Ok(())
}

fn validate_reserved_root(
    root: &BTreeMap<String, Value>,
    name: &str,
    expected: Value,
) -> Result<()> {
    if root.get(name) != Some(&expected) {
        return Err(Error::InvalidSnapshot(format!(
            "/{name} is reserved and must be empty in the initial slice"
        )));
    }
    Ok(())
}

fn validate_lineage(value: Option<&Value>) -> Result<()> {
    let lineage = value
        .ok_or_else(|| Error::InvalidSnapshot("missing /system/lineage".into()))
        .and_then(root_map)?;
    if lineage.keys().map(String::as_str).collect::<BTreeSet<_>>() != BTreeSet::from(["parent"]) {
        return Err(Error::InvalidSnapshot(
            "/system/lineage must contain exactly parent".into(),
        ));
    }
    match lineage.get("parent") {
        Some(Value::Null) => Ok(()),
        Some(Value::String(parent)) => ProcessId::new(parent.clone())
            .map(|_| ())
            .map_err(|_| Error::InvalidSnapshot("invalid /system/lineage/parent".into())),
        _ => Err(Error::InvalidSnapshot(
            "/system/lineage/parent must be null or a process address".into(),
        )),
    }
}

fn system_value(id: &ProcessId, limits: &Limits, parent: Option<&ProcessId>) -> Value {
    Value::Map(BTreeMap::from([
        ("api".into(), api_value()),
        ("capabilities".into(), Value::Array(Vec::new())),
        ("identity".into(), identity_value(id)),
        ("limits".into(), limits_value(limits)),
        ("lineage".into(), lineage_value(parent)),
        ("outbox".into(), Value::Array(Vec::new())),
        ("runtime".into(), runtime_value()),
    ]))
}

fn api_value() -> Value {
    Value::Map(BTreeMap::from([(
        "operations".into(),
        Value::Array(
            ["discover", "exec", "read", "remove", "write"]
                .into_iter()
                .map(Value::from)
                .collect(),
        ),
    )]))
}

fn identity_value(id: &ProcessId) -> Value {
    // THREAT[TM-AUTH-001]: The discoverable address is explicitly marked as
    // unauthenticated so callers cannot mistake process metadata for authority.
    Value::Map(BTreeMap::from([
        ("address".into(), Value::String(id.to_string())),
        ("authenticated".into(), Value::Bool(false)),
    ]))
}

fn lineage_value(parent: Option<&ProcessId>) -> Value {
    Value::Map(BTreeMap::from([(
        "parent".into(),
        parent
            .map(|parent| Value::String(parent.to_string()))
            .unwrap_or(Value::Null),
    )]))
}

fn runtime_value() -> Value {
    Value::Map(BTreeMap::from([
        ("language".into(), Value::from(RUNTIME_LANGUAGE)),
        (
            "snapshot_format".into(),
            Value::Integer(i64::from(SNAPSHOT_FORMAT)),
        ),
    ]))
}

fn limits_value(limits: &Limits) -> Value {
    Value::Map(BTreeMap::from([
        (
            "max_call_stack".into(),
            integer_value(limits.max_call_stack),
        ),
        (
            "max_exec_depth".into(),
            integer_value(limits.max_exec_depth),
        ),
        (
            "max_execution_millis".into(),
            Value::Integer(limits.max_execution_millis as i64),
        ),
        (
            "max_guest_memory".into(),
            integer_value(limits.max_guest_memory),
        ),
        (
            "max_integer_bits".into(),
            integer_value(limits.max_integer_bits),
        ),
        ("max_logs".into(), integer_value(limits.max_logs)),
        ("max_messages".into(), integer_value(limits.max_messages)),
        (
            "max_mount_entries".into(),
            integer_value(limits.max_mount_entries),
        ),
        (
            "max_mount_writes".into(),
            integer_value(limits.max_mount_writes),
        ),
        (
            "max_namespace_entries".into(),
            integer_value(limits.max_namespace_entries),
        ),
        (
            "max_script_bytes".into(),
            integer_value(limits.max_script_bytes),
        ),
        (
            "max_staged_scripts".into(),
            integer_value(limits.max_staged_scripts),
        ),
        (
            "max_syntax_depth".into(),
            integer_value(limits.max_syntax_depth),
        ),
        (
            "max_text_bytes".into(),
            integer_value(limits.max_text_bytes),
        ),
        (
            "max_value_depth".into(),
            integer_value(limits.max_value_depth),
        ),
        (
            "max_value_entries".into(),
            integer_value(limits.max_value_entries),
        ),
        (
            "max_value_stack".into(),
            integer_value(limits.max_value_stack),
        ),
    ]))
}

fn integer_value(value: usize) -> Value {
    Value::Integer(value as i64)
}

fn memory_path(path: &str) -> Result<Option<&str>> {
    let Some(memory_path) = path.strip_prefix("/memory") else {
        return Ok(None);
    };
    if !memory_path.is_empty() && !memory_path.starts_with('/') {
        return Err(Error::InvalidPath(path.into()));
    }
    Ok(Some(memory_path))
}

/// Splits an absolute process path into a mount name and a mount-relative path.
///
/// `/mounts` itself stays committed state: it lists mount identity. Only
/// `/mounts/<name>` and deeper cross into a provider.
fn mount_target(path: &str) -> Result<Option<(String, MountPath)>> {
    let Some(rest) = path.strip_prefix("/mounts/") else {
        return Ok(None);
    };
    let (name, remainder) = rest.split_once('/').unwrap_or((rest, ""));
    crate::mounts::validate_mount_name(name)?;
    Ok(Some((name.to_owned(), MountPath::parse(remainder)?)))
}

/// Describes one committed node with the same facts vocabulary mounts use.
fn committed_stat_value(path: &str, value: &Value) -> Value {
    let (kind, facts) = match value {
        Value::Map(values) => (
            "directory",
            BTreeMap::from([
                ("content".into(), Value::from("object")),
                ("entries".into(), Value::Integer(values.len() as i64)),
            ]),
        ),
        Value::Array(values) => (
            "directory",
            BTreeMap::from([
                ("content".into(), Value::from("array")),
                ("entries".into(), Value::Integer(values.len() as i64)),
            ]),
        ),
        Value::String(text) => (
            "leaf",
            BTreeMap::from([
                ("bytes".into(), Value::Integer(text.len() as i64)),
                ("content".into(), Value::from("text/plain")),
            ]),
        ),
        Value::Script(script) => (
            "leaf",
            BTreeMap::from([
                ("bytes".into(), Value::Integer(script.source().len() as i64)),
                ("content".into(), Value::from("svit-script")),
            ]),
        ),
        _ => (
            "leaf",
            BTreeMap::from([("content".into(), Value::from("scalar"))]),
        ),
    };
    let writable = path == "/memory" || path.starts_with("/memory/") || path.starts_with("/lib/");
    Value::Map(BTreeMap::from([
        (
            "access".into(),
            Value::from(if writable { "read-write" } else { "read" }),
        ),
        ("attached".into(), Value::Bool(true)),
        ("facts".into(), Value::Map(facts)),
        ("kind".into(), Value::from(kind)),
        // Committed state is already resident in the process root.
        ("locality".into(), Value::from("cache")),
        ("mount".into(), Value::Null),
        ("path".into(), Value::from(path)),
        ("source".into(), Value::from("process")),
    ]))
}

fn library_path(path: &str) -> Result<Option<&str>> {
    let Some(name) = path.strip_prefix("/lib/") else {
        return Ok(None);
    };
    validate_script_name(name)?;
    Ok(Some(name))
}

fn script_from_write_value(value: Value) -> Result<Script> {
    match value {
        Value::Script(script) => Ok(script),
        Value::Map(mut fields) => {
            let source = match fields.remove("source") {
                Some(Value::String(source)) => source,
                _ => {
                    return Err(Error::InvalidValue(
                        "library writes require a text source".into(),
                    ));
                }
            };
            let documentation = match fields.remove("documentation") {
                Some(Value::String(documentation)) => documentation,
                None => String::new(),
                _ => {
                    return Err(Error::InvalidValue(
                        "library documentation must be text".into(),
                    ));
                }
            };
            if !fields.is_empty() {
                return Err(Error::InvalidValue(
                    "library writes accept only source and documentation".into(),
                ));
            }
            Ok(Script::new(source).with_documentation(documentation))
        }
        _ => Err(Error::InvalidValue(
            "library writes require a script record".into(),
        )),
    }
}

fn validate_process_id(value: &str) -> Result<()> {
    if value.len() > 256
        || !value.starts_with("svit://")
        || value[7..].is_empty()
        || value.chars().any(char::is_whitespace)
    {
        return Err(Error::InvalidValue(format!(
            "process address must be svit:// followed by a non-empty path: {value}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_script_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(Error::InvalidScriptName(name.into()));
    }
    Ok(())
}

fn validate_script_source(name: &str, source: &str, limits: &Limits) -> Result<()> {
    if source.len() > limits.max_script_bytes {
        return Err(Error::ResourceLimitExceeded("script source"));
    }
    let validation_id = ProcessId::new("svit://local/validation")?;
    let root = initial_root(
        &validation_id,
        limits,
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let state = RuntimeState::new(
        root,
        Value::empty_map(),
        Arc::new(MountRegistry::default()),
        limits.clone(),
        Duration::from_millis(limits.max_execution_millis),
        Vec::new(),
    );
    let interpreter = secure_lisp(&state, &Value::Null, limits, limits.max_exec_depth)?;
    interpreter
        .compile_exprs(source)
        .map(|_| ())
        .map_err(|error| map_ketos_error(&interpreter, error))
        .map_err(|error| match error {
            Error::Script(message) => Error::Script(sanitize_diagnostic(format!(
                "/lib/{name}.svit-script: {message}"
            ))),
            other => other,
        })
}

#[derive(Clone)]
struct RuntimeState {
    committed_root: Arc<Value>,
    memory: Arc<Mutex<Value>>,
    mutations: Arc<Mutex<Vec<Mutation>>>,
    logs: Arc<Mutex<Vec<LogRecord>>>,
    messages: Arc<Mutex<Vec<StagedMessage>>>,
    library_changes: Arc<Mutex<LibraryChanges>>,
    mount_writes: Arc<Mutex<Vec<MountWrite>>>,
    builtin_replay: Arc<Mutex<BuiltinReplayState>>,
    mounts: Arc<MountRegistry>,
    limits: Limits,
    deadline: Instant,
}

/// One mount effect an activation intends, buffered until the commit point.
#[derive(Clone)]
struct MountWrite {
    mount: String,
    path: MountPath,
    value: Option<Value>,
}

#[derive(Clone)]
struct BuiltinReplay {
    path: String,
    input: Value,
    output: Value,
}

struct BuiltinReplayState {
    completed: Vec<BuiltinReplay>,
    cursor: usize,
    pending: Option<BuiltinCall>,
}

#[derive(Clone)]
struct BuiltinCall {
    path: String,
    input: Value,
}

enum BuiltinResolution {
    Output(Value),
    Pending,
}

impl RuntimeState {
    fn new(
        committed_root: Value,
        memory: Value,
        mounts: Arc<MountRegistry>,
        limits: Limits,
        execution_time: Duration,
        builtin_replay: Vec<BuiltinReplay>,
    ) -> Self {
        Self {
            committed_root: Arc::new(committed_root),
            memory: Arc::new(Mutex::new(memory)),
            mutations: Arc::new(Mutex::new(Vec::new())),
            logs: Arc::new(Mutex::new(Vec::new())),
            messages: Arc::new(Mutex::new(Vec::new())),
            library_changes: Arc::new(Mutex::new(Vec::new())),
            mount_writes: Arc::new(Mutex::new(Vec::new())),
            builtin_replay: Arc::new(Mutex::new(BuiltinReplayState {
                completed: builtin_replay,
                cursor: 0,
                pending: None,
            })),
            mounts,
            limits,
            deadline: Instant::now() + execution_time,
        }
    }

    fn resolve_builtin(&self, path: String, input: Value) -> Result<BuiltinResolution> {
        let mut replay = lock(&self.builtin_replay)?;
        if let Some(completed) = replay.completed.get(replay.cursor).cloned() {
            if completed.path != path || completed.input != input {
                return Err(Error::Script("built-in replay diverged".into()));
            }
            replay.cursor += 1;
            return Ok(BuiltinResolution::Output(completed.output));
        }
        if replay.completed.len() >= self.limits.max_exec_depth {
            return Err(Error::ResourceLimitExceeded("built-in calls"));
        }
        replay.pending = Some(BuiltinCall { path, input });
        Ok(BuiltinResolution::Pending)
    }

    fn pending_builtin(&self) -> Result<Option<BuiltinCall>> {
        Ok(lock(&self.builtin_replay)?.pending.clone())
    }

    fn builtin_replay_complete(&self) -> Result<bool> {
        let replay = lock(&self.builtin_replay)?;
        Ok(replay.cursor == replay.completed.len())
    }

    fn mount_view(&self) -> MountView<'_> {
        MountView::new(
            self.mounts.as_ref(),
            root_map(self.committed_root.as_ref())
                .ok()
                .and_then(|root| root.get("mounts"))
                .unwrap_or(&Value::Null),
            &self.limits,
        )
    }

    /// Applies every buffered mount effect in guest order.
    ///
    /// THREAT[TM-EFF-006]: Nothing external happens while the guest runs. A
    /// failure here fails the activation, so a rejected write never coexists
    /// with a committed memory change. Mount sources are external systems, so
    /// this is ordering, not distributed atomicity.
    fn apply_mount_writes(&self) -> Result<()> {
        for write in lock(&self.mount_writes)?.iter() {
            match &write.value {
                Some(value) => self
                    .mount_view()
                    .write(&write.mount, &write.path, value.clone())?,
                None => self.mount_view().remove(&write.mount, &write.path)?,
            }
        }
        Ok(())
    }

    fn remaining_time(&self) -> Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(Error::ExecutionLimitExceeded)
    }

    fn script(&self, name: &str) -> Result<Option<Script>> {
        for (changed_name, script) in lock(&self.library_changes)?.iter().rev() {
            if changed_name == name {
                return Ok(script.clone());
            }
        }
        let root = root_map(self.committed_root.as_ref())?;
        let library = root_map(root.get("lib").expect("validated process root"))?;
        Ok(match library.get(name) {
            Some(Value::Script(script)) => Some(script.clone()),
            _ => None,
        })
    }

    fn view(&self) -> Result<Value> {
        let mut root = root_map(self.committed_root.as_ref())?.clone();
        root.insert("memory".into(), lock(&self.memory)?.clone());
        let library = root_map_mut(root.get_mut("lib").expect("validated process root"))?;
        for (name, script) in lock(&self.library_changes)?.iter() {
            match script {
                Some(script) => {
                    library.insert(name.clone(), Value::Script(script.clone()));
                }
                None => {
                    library.remove(name);
                }
            }
        }
        for value in library.values_mut() {
            let Value::Script(script) = value else {
                return Err(Error::InvalidValue("invalid library record".into()));
            };
            *value = script_metadata(script);
        }
        Ok(Value::Map(root))
    }

    fn checkpoint(&self) -> Result<RuntimeCheckpoint> {
        Ok(RuntimeCheckpoint {
            memory: lock(&self.memory)?.clone(),
            mutations: lock(&self.mutations)?.clone(),
            logs: lock(&self.logs)?.clone(),
            messages: lock(&self.messages)?.clone(),
            library_changes: lock(&self.library_changes)?.clone(),
            mount_writes: lock(&self.mount_writes)?.clone(),
        })
    }

    fn restore(&self, checkpoint: RuntimeCheckpoint) -> Result<()> {
        *lock(&self.memory)? = checkpoint.memory;
        *lock(&self.mutations)? = checkpoint.mutations;
        *lock(&self.logs)? = checkpoint.logs;
        *lock(&self.messages)? = checkpoint.messages;
        *lock(&self.library_changes)? = checkpoint.library_changes;
        *lock(&self.mount_writes)? = checkpoint.mount_writes;
        Ok(())
    }

    fn buffer_mount_write(
        &self,
        mount: String,
        path: MountPath,
        value: Option<Value>,
        limits: &Limits,
    ) -> Result<()> {
        // Reject denied or unknown mounts while the guest can still see the
        // failure, instead of discovering it at the commit point.
        let view = self.mount_view();
        if !view.grants_write(&mount)? {
            return Err(Error::MountDenied(format!("/mounts/{mount} is read-only")));
        }
        let mut writes = lock(&self.mount_writes)?;
        if writes.len() >= limits.max_mount_writes {
            return Err(Error::ResourceLimitExceeded("mount writes"));
        }
        writes.push(MountWrite { mount, path, value });
        Ok(())
    }

    fn write(&self, path: &str, value: Value, limits: &Limits) -> Result<()> {
        if let Some((mount, mount_path)) = mount_target(path)? {
            value.validate(limits, false)?;
            return self.buffer_mount_write(mount, mount_path, Some(value), limits);
        }
        if let Some(memory_path) = memory_path(path)? {
            value.validate(limits, false)?;
            let mut candidate = lock(&self.memory)?.clone();
            set_value_path(&mut candidate, memory_path, value.clone())?;
            candidate.validate(limits, false)?;
            *lock(&self.memory)? = candidate;
            lock(&self.mutations)?.push(Mutation::Set {
                path: path.into(),
                value,
            });
            return Ok(());
        }
        if let Some(name) = library_path(path)? {
            let script = script_from_write_value(value)?;
            script_value(&script).validate(limits, true)?;
            let mut changes = lock(&self.library_changes)?;
            if changes.len() >= limits.max_staged_scripts {
                return Err(Error::ResourceLimitExceeded("staged scripts"));
            }
            changes.push((name.into(), Some(script.clone())));
            lock(&self.mutations)?.push(Mutation::Set {
                path: path.into(),
                value: Value::Script(script),
            });
            return Ok(());
        }
        Err(Error::InvalidPath(path.into()))
    }

    fn remove(&self, path: &str, limits: &Limits) -> Result<()> {
        if let Some((mount, mount_path)) = mount_target(path)? {
            return self.buffer_mount_write(mount, mount_path, None, limits);
        }
        if let Some(memory_path) = memory_path(path)? {
            let mut candidate = lock(&self.memory)?.clone();
            remove_value_path(&mut candidate, memory_path)?;
            candidate.validate(limits, false)?;
            *lock(&self.memory)? = candidate;
            lock(&self.mutations)?.push(Mutation::Remove { path: path.into() });
            return Ok(());
        }
        if let Some(name) = library_path(path)? {
            if self.script(name)?.is_none() {
                return Err(Error::InvalidPath(path.into()));
            }
            let mut changes = lock(&self.library_changes)?;
            if changes.len() >= limits.max_staged_scripts {
                return Err(Error::ResourceLimitExceeded("staged scripts"));
            }
            changes.push((name.into(), None));
            lock(&self.mutations)?.push(Mutation::Remove { path: path.into() });
            return Ok(());
        }
        Err(Error::InvalidPath(path.into()))
    }
}

struct RuntimeCheckpoint {
    memory: Value,
    mutations: Vec<Mutation>,
    logs: Vec<LogRecord>,
    messages: Vec<StagedMessage>,
    library_changes: LibraryChanges,
    mount_writes: Vec<MountWrite>,
}

type LibraryChanges = Vec<(String, Option<Script>)>;

#[derive(Clone, Debug)]
struct GuestPersistent(Value);

impl ForeignValue for GuestPersistent {
    fn type_name(&self) -> &'static str {
        "svit-value"
    }

    fn size(&self) -> usize {
        serde_json::to_vec(&self.0.to_json()).map_or(1, |bytes| bytes.len().max(1))
    }
}

#[derive(Clone, Debug)]
enum GuestFailure {
    BuiltinPending,
    Execution,
    InvalidPath(String),
    InvalidValue(String),
    Resource(&'static str),
    Script(String),
}

impl fmt::Display for GuestFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuiltinPending => formatter.write_str("built-in execution suspended"),
            Self::Execution => formatter.write_str("activation execution limit exceeded"),
            Self::InvalidPath(message) | Self::InvalidValue(message) | Self::Script(message) => {
                formatter.write_str(message)
            }
            Self::Resource(resource) => write!(formatter, "{resource} limit exceeded"),
        }
    }
}

impl StdError for GuestFailure {}

fn run_guest_script(
    state: &RuntimeState,
    name: &str,
    input: &Value,
    limits: &Limits,
    remaining_exec_depth: usize,
) -> Result<Value> {
    validate_script_name(name)?;
    let script = state
        .script(name)?
        .ok_or_else(|| Error::ScriptNotFound(name.into()))?;
    let interpreter = secure_lisp(state, input, limits, remaining_exec_depth)?;
    // Svit owns the language contract and virtual source identity; Ketos is
    // an interpreter implementation detail.
    let source_path = format!("/lib/{name}.svit-script");
    let execution = (|| {
        interpreter
            .run_code(script.source(), Some(source_path.clone()))
            .map_err(|error| map_ketos_error(&interpreter, error))?;
        if interpreter.get_value("main").is_none() {
            return Err(Error::Script("script must define main(input)".into()));
        }
        let output = interpreter
            .call("main", vec![persistent_to_ketos(input)?])
            .map_err(|error| map_ketos_error(&interpreter, error))?;
        persistent_from_ketos(&output, limits)
    })();
    execution.map_err(|error| add_script_path(&source_path, error))
}

fn secure_lisp(
    state: &RuntimeState,
    input: &Value,
    limits: &Limits,
    remaining_exec_depth: usize,
) -> Result<KetosInterpreter> {
    // THREAT[TM-ESC-001]: Null I/O and a null module loader leave guest code
    // without filesystem, environment, network, clock, randomness, or modules.
    // THREAT[TM-ISO-001]: Every activation receives a fresh interpreter scope.
    let restrictions = RestrictConfig {
        // THREAT[TM-DOS-006]: Nested exec uses a fresh interpreter but shares
        // this activation's deadline, so composition cannot reset wall time.
        execution_time: Some(state.remaining_time()?),
        call_stack_size: limits.max_call_stack,
        value_stack_size: limits.max_value_stack,
        namespace_size: limits.max_namespace_entries,
        memory_limit: limits.max_guest_memory,
        max_integer_size: limits.max_integer_bits,
        max_syntax_nesting: limits.max_syntax_depth,
    };
    let interpreter = KetosBuilder::new()
        .name("svit-lisp")
        .restrict(restrictions)
        .io(Rc::new(GlobalIo::null()))
        .module_loader(Box::new(NullModuleLoader))
        .finish();
    interpreter
        .scope()
        .add_named_value("*svit-version*", KetosValue::from("Svit Lisp 2"));
    install_guest_api(&interpreter, state, input, limits, remaining_exec_depth);
    Ok(interpreter)
}

fn install_guest_api(
    interpreter: &KetosInterpreter,
    state: &RuntimeState,
    input: &Value,
    limits: &Limits,
    remaining_exec_depth: usize,
) {
    interpreter.scope().add_named_value(
        "input",
        persistent_to_ketos(input).expect("validated input"),
    );

    let value_limits = limits.clone();
    install_guest_function(interpreter, "value-map", move |_, args| {
        if args.len() % 2 != 0 {
            return guest_error("value-map expects text/value pairs");
        }
        let mut values = BTreeMap::new();
        for pair in args.chunks(2) {
            let key = guest_string(&pair[0], "value-map key")?;
            if values.contains_key(&key) {
                return Err(KetosError::custom(GuestFailure::InvalidValue(format!(
                    "duplicate map key: {key}"
                ))));
            }
            values.insert(
                key,
                persistent_from_ketos(&pair[1], &value_limits).map_err(guest_from_svit)?,
            );
        }
        let value = Value::Map(values);
        value
            .validate(&value_limits, false)
            .map_err(guest_from_svit)?;
        persistent_to_ketos(&value).map_err(guest_from_svit)
    });

    let array_limits = limits.clone();
    install_guest_function(interpreter, "value-array", move |_, args| {
        let value = Value::Array(
            args.iter()
                .map(|value| persistent_from_ketos(value, &array_limits))
                .collect::<Result<Vec<_>>>()
                .map_err(guest_from_svit)?,
        );
        value
            .validate(&array_limits, false)
            .map_err(guest_from_svit)?;
        persistent_to_ketos(&value).map_err(guest_from_svit)
    });

    install_guest_function(interpreter, "value-null?", move |_, args| {
        expect_arity(args, 1, "value-null?")?;
        Ok(KetosValue::Bool(matches!(
            &args[0],
            KetosValue::Foreign(value)
                if matches!(value.downcast_ref::<GuestPersistent>(), Some(GuestPersistent(Value::Null)))
        )))
    });

    let get_limits = limits.clone();
    install_guest_function(interpreter, "value-get", move |_, args| {
        expect_arity(args, 2, "value-get")?;
        let value = persistent_from_ketos(&args[0], &get_limits).map_err(guest_from_svit)?;
        let path = guest_string(&args[1], "value-get path")?;
        let found = read_value_path(&value, &path)
            .map_err(guest_from_svit)?
            .cloned()
            .unwrap_or(Value::Null);
        persistent_to_ketos(&found).map_err(guest_from_svit)
    });

    let state_for_discover = state.clone();
    install_guest_function(interpreter, "discover", move |_, args| {
        expect_arity(args, 1, "discover")?;
        let path = guest_string(&args[0], "discover path")?;
        if let Some((mount, mount_path)) = mount_target(&path).map_err(guest_from_svit)? {
            let children = state_for_discover
                .mount_view()
                .discover(&mount, &mount_path)
                .map_err(guest_from_svit)?;
            let children = children.into_iter().map(Value::String).collect();
            return persistent_to_ketos(&Value::Array(children)).map_err(guest_from_svit);
        }
        let view = state_for_discover.view().map_err(guest_from_svit)?;
        let value = read_value_path(&view, &path)
            .map_err(guest_from_svit)?
            .ok_or_else(|| KetosError::custom(GuestFailure::InvalidPath(path.clone())))?;
        let children = match value {
            Value::Map(values) => values.keys().cloned().map(Value::String).collect(),
            Value::Array(values) => (0..values.len())
                .map(|index| Value::String(index.to_string()))
                .collect(),
            _ => return Err(KetosError::custom(GuestFailure::InvalidPath(path))),
        };
        persistent_to_ketos(&Value::Array(children)).map_err(guest_from_svit)
    });

    let state_for_read = state.clone();
    install_guest_function(interpreter, "read", move |_, args| {
        expect_arity(args, 1, "read")?;
        let path = guest_string(&args[0], "read path")?;
        // THREAT[TM-DOS-010]: A mount read resolves exactly one node under the
        // activation's limits; it never materializes a subtree.
        if let Some((mount, mount_path)) = mount_target(&path).map_err(guest_from_svit)? {
            let found = state_for_read
                .mount_view()
                .read(&mount, &mount_path)
                .map_err(guest_from_svit)?
                .unwrap_or(Value::Null);
            return persistent_to_ketos(&found).map_err(guest_from_svit);
        }
        let view = state_for_read.view().map_err(guest_from_svit)?;
        let found = read_value_path(&view, &path)
            .map_err(guest_from_svit)?
            .cloned()
            .unwrap_or(Value::Null);
        persistent_to_ketos(&found).map_err(guest_from_svit)
    });

    let state_for_stat = state.clone();
    install_guest_function(interpreter, "stat", move |_, args| {
        expect_arity(args, 1, "stat")?;
        let path = guest_string(&args[0], "stat path")?;
        let found =
            if let Some((mount, mount_path)) = mount_target(&path).map_err(guest_from_svit)? {
                state_for_stat
                    .mount_view()
                    .stat(&mount, &mount_path)
                    .map_err(guest_from_svit)?
            } else {
                let view = state_for_stat.view().map_err(guest_from_svit)?;
                read_value_path(&view, &path)
                    .map_err(guest_from_svit)?
                    .map(|value| committed_stat_value(&path, value))
            };
        persistent_to_ketos(&found.unwrap_or(Value::Null)).map_err(guest_from_svit)
    });

    let state_for_write = state.clone();
    let write_limits = limits.clone();
    install_guest_function(interpreter, "write", move |_, args| {
        expect_arity(args, 2, "write")?;
        let path = guest_string(&args[0], "write path")?;
        let value = persistent_from_ketos(&args[1], &write_limits).map_err(guest_from_svit)?;
        state_for_write
            .write(&path, value, &write_limits)
            .map_err(guest_from_svit)?;
        Ok(KetosValue::Unit)
    });

    let state_for_remove = state.clone();
    let remove_limits = limits.clone();
    install_guest_function(interpreter, "remove", move |_, args| {
        expect_arity(args, 1, "remove")?;
        let path = guest_string(&args[0], "remove path")?;
        state_for_remove
            .remove(&path, &remove_limits)
            .map_err(guest_from_svit)?;
        Ok(KetosValue::Unit)
    });

    let state_for_exec = state.clone();
    let exec_limits = limits.clone();
    install_guest_function(interpreter, "exec", move |_, args| {
        expect_arity(args, 2, "exec")?;
        // THREAT[TM-DOS-006]: Bound native recursion independently of the
        // Ketos call stack because nested exec creates a fresh interpreter.
        if remaining_exec_depth == 0 {
            return Err(KetosError::custom(GuestFailure::Resource(
                "nested exec depth",
            )));
        }
        let path = match guest_string(&args[0], "exec path") {
            Ok(path) => path,
            Err(_) if matches!(&args[1], KetosValue::String(_)) => {
                return guest_error("exec expects (exec path input); arguments appear reversed");
            }
            Err(error) => return Err(error),
        };
        if path.starts_with("/bin/") {
            let input = persistent_from_ketos(&args[1], &exec_limits).map_err(guest_from_svit)?;
            return match state_for_exec
                .resolve_builtin(path, input)
                .map_err(guest_from_svit)?
            {
                BuiltinResolution::Output(output) => {
                    persistent_to_ketos(&output).map_err(guest_from_svit)
                }
                BuiltinResolution::Pending => Err(KetosError::custom(GuestFailure::BuiltinPending)),
            };
        }
        let name = library_path(&path)
            .map_err(guest_from_svit)?
            .ok_or_else(|| guest_from_svit(Error::InvalidPath(path.clone())))?;
        let input = persistent_from_ketos(&args[1], &exec_limits).map_err(guest_from_svit)?;
        let checkpoint = state_for_exec.checkpoint().map_err(guest_from_svit)?;
        match run_guest_script(
            &state_for_exec,
            name,
            &input,
            &exec_limits,
            remaining_exec_depth - 1,
        ) {
            Ok(output) => persistent_to_ketos(&output).map_err(guest_from_svit),
            Err(error) => {
                state_for_exec
                    .restore(checkpoint)
                    .map_err(guest_from_svit)?;
                Err(guest_from_svit(error))
            }
        }
    });

    let logs_for_info = Arc::clone(&state.logs);
    let log_limits = limits.clone();
    let maximum_logs = limits.max_logs;
    install_guest_function(interpreter, "log-info!", move |_, args| {
        if !(1..=2).contains(&args.len()) {
            return guest_error("log-info! expects a message and optional fields");
        }
        let message = guest_string(&args[0], "log-info! message")?;
        let fields = args
            .get(1)
            .map(|value| persistent_from_ketos(value, &log_limits))
            .transpose()
            .map_err(guest_from_svit)?
            .unwrap_or(Value::Null);
        fields
            .validate(&log_limits, false)
            .map_err(guest_from_svit)?;
        let mut logs = logs_for_info.lock().map_err(|_| {
            KetosError::custom(GuestFailure::Script("runtime state unavailable".into()))
        })?;
        if logs.len() >= maximum_logs {
            return Err(KetosError::custom(GuestFailure::Resource("log records")));
        }
        logs.push(LogRecord { message, fields });
        Ok(KetosValue::Unit)
    });

    let messages_for_send = Arc::clone(&state.messages);
    let send_limits = limits.clone();
    let maximum_messages = limits.max_messages;
    install_guest_function(interpreter, "send!", move |_, args| {
        expect_arity(args, 2, "send!")?;
        let address = guest_string(&args[0], "send! address")?;
        let to = ProcessId::new(address)
            .map_err(|error| KetosError::custom(GuestFailure::Script(error.to_string())))?;
        let body = persistent_from_ketos(&args[1], &send_limits).map_err(guest_from_svit)?;
        body.validate(&send_limits, false)
            .map_err(guest_from_svit)?;
        let mut messages = messages_for_send.lock().map_err(|_| {
            KetosError::custom(GuestFailure::Script("runtime state unavailable".into()))
        })?;
        if messages.len() >= maximum_messages {
            return Err(KetosError::custom(GuestFailure::Resource(
                "message intents",
            )));
        }
        messages.push(StagedMessage { to, body });
        Ok(KetosValue::Unit)
    });
}

fn install_guest_function<F>(interpreter: &KetosInterpreter, name: &str, function: F)
where
    F: Any + Fn(&KetosContext, &mut [KetosValue]) -> std::result::Result<KetosValue, KetosError>,
{
    interpreter
        .scope()
        .add_value_with_name(name, |name| KetosValue::new_foreign_fn(name, function));
}

fn persistent_to_ketos(value: &Value) -> Result<KetosValue> {
    match value {
        Value::Bool(value) => Ok(KetosValue::Bool(*value)),
        Value::Integer(value) => Ok(KetosValue::Integer(KetosInteger::from_i64(*value))),
        Value::Number(value) if value.is_finite() => Ok(KetosValue::Float(*value)),
        Value::Number(_) => Err(Error::InvalidValue("numbers must be finite".into())),
        Value::String(value) => Ok(KetosValue::from(value.clone())),
        Value::Null | Value::Array(_) | Value::Map(_) => {
            Ok(KetosValue::new_foreign(GuestPersistent(value.clone())))
        }
        Value::Script(_) => Err(Error::InvalidValue(
            "script records cannot be passed to Lisp as data".into(),
        )),
    }
}

fn persistent_from_ketos(value: &KetosValue, limits: &Limits) -> Result<Value> {
    let result = match value {
        KetosValue::Unit => Value::Null,
        KetosValue::Bool(value) => Value::Bool(*value),
        KetosValue::Float(value) if value.is_finite() => Value::Number(*value),
        KetosValue::Float(_) => return Err(Error::InvalidValue("numbers must be finite".into())),
        KetosValue::Integer(value) => {
            Value::Integer(value.to_i64().ok_or_else(|| {
                Error::InvalidValue("integer is outside signed 64-bit range".into())
            })?)
        }
        KetosValue::String(value) => Value::String(value.to_string()),
        KetosValue::List(values) => Value::Array(
            values
                .iter()
                .map(|value| persistent_from_ketos(value, limits))
                .collect::<Result<Vec<_>>>()?,
        ),
        KetosValue::Foreign(value) => value
            .downcast_ref::<GuestPersistent>()
            .map(|value| value.0.clone())
            .ok_or_else(|| Error::InvalidValue("foreign Lisp values cannot be persisted".into()))?,
        other => {
            return Err(Error::InvalidValue(format!(
                "Lisp {} values cannot be persisted",
                other.type_name()
            )));
        }
    };
    result.validate(limits, false)?;
    Ok(result)
}

fn read_value_path<'a>(value: &'a Value, path: &str) -> Result<Option<&'a Value>> {
    if path.is_empty() || path == "/" {
        return Ok(Some(value));
    }
    let path = path
        .strip_prefix('/')
        .ok_or_else(|| Error::InvalidPath(path.into()))?;
    let mut current = value;
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(Error::InvalidPath(path.into()));
        }
        current = match current {
            Value::Map(values) => match values.get(segment) {
                Some(value) => value,
                None => return Ok(None),
            },
            Value::Array(values) => {
                let index = segment
                    .parse::<usize>()
                    .map_err(|_| Error::InvalidPath(path.into()))?;
                match values.get(index) {
                    Some(value) => value,
                    None => return Ok(None),
                }
            }
            _ => return Err(Error::InvalidPath(path.into())),
        };
    }
    Ok(Some(current))
}

fn set_value_path(root: &mut Value, path: &str, value: Value) -> Result<()> {
    let segments = path_segments(path)?;
    if segments.is_empty() {
        *root = value;
        return Ok(());
    }
    let (parent, leaf) = parent_at_path(root, &segments)?;
    match parent {
        Value::Map(values) => {
            values.insert(leaf, value);
            Ok(())
        }
        Value::Array(values) => {
            let index = leaf
                .parse::<usize>()
                .map_err(|_| Error::InvalidPath(path.into()))?;
            let target = values
                .get_mut(index)
                .ok_or_else(|| Error::InvalidPath(path.into()))?;
            *target = value;
            Ok(())
        }
        _ => Err(Error::InvalidPath(path.into())),
    }
}

fn apply_mutation(root: &mut Value, mutation: &Mutation) -> Result<()> {
    match mutation {
        Mutation::Set { path, value } => {
            persisted_path(path)?;
            set_value_path(root, path, value.clone())
        }
        Mutation::Remove { path } => {
            persisted_path(path)?;
            remove_value_path(root, path)
        }
        Mutation::Append { path, values } => {
            let target = value_path_mut(root, persisted_path(path)?)?;
            let Value::Array(target) = target else {
                return Err(Error::InvalidPersistence(
                    "append target is not an array".into(),
                ));
            };
            target.extend(values.iter().cloned());
            Ok(())
        }
        Mutation::RemoveFront {
            path,
            expected_value_hash,
        } => {
            let target = value_path_mut(root, persisted_path(path)?)?;
            let Value::Array(target) = target else {
                return Err(Error::InvalidPersistence(
                    "remove-front target is not an array".into(),
                ));
            };
            let first = target
                .first()
                .ok_or_else(|| Error::InvalidPersistence("remove-front target is empty".into()))?;
            if root_hash(first)? != *expected_value_hash {
                return Err(Error::InvalidPersistence(
                    "remove-front precondition failed".into(),
                ));
            }
            target.remove(0);
            Ok(())
        }
    }
}

fn persisted_path(path: &str) -> Result<&str> {
    let path = path
        .strip_prefix('/')
        .ok_or_else(|| Error::InvalidPersistence("event path is not absolute".into()))?;
    if path.is_empty() {
        return Err(Error::InvalidPersistence(
            "event cannot replace the complete root".into(),
        ));
    }
    Ok(path)
}

fn value_path_mut<'a>(root: &'a mut Value, path: &str) -> Result<&'a mut Value> {
    let mut current = root;
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(Error::InvalidPersistence("event path is invalid".into()));
        }
        current = match current {
            Value::Map(values) => values
                .get_mut(segment)
                .ok_or_else(|| Error::InvalidPersistence("event path is missing".into()))?,
            Value::Array(values) => {
                let index = segment
                    .parse::<usize>()
                    .map_err(|_| Error::InvalidPersistence("event array path is invalid".into()))?;
                values
                    .get_mut(index)
                    .ok_or_else(|| Error::InvalidPersistence("event path is missing".into()))?
            }
            _ => {
                return Err(Error::InvalidPersistence(
                    "event path crosses a scalar".into(),
                ));
            }
        };
    }
    Ok(current)
}

fn remove_value_path(root: &mut Value, path: &str) -> Result<()> {
    let segments = path_segments(path)?;
    if segments.is_empty() {
        return Err(Error::InvalidPath("cannot remove a namespace root".into()));
    }
    let (parent, leaf) = parent_at_path(root, &segments)?;
    match parent {
        Value::Map(values) => values
            .remove(&leaf)
            .map(|_| ())
            .ok_or_else(|| Error::InvalidPath(path.into())),
        Value::Array(values) => {
            let index = leaf
                .parse::<usize>()
                .map_err(|_| Error::InvalidPath(path.into()))?;
            if index >= values.len() {
                return Err(Error::InvalidPath(path.into()));
            }
            values.remove(index);
            Ok(())
        }
        _ => Err(Error::InvalidPath(path.into())),
    }
}

fn path_segments(path: &str) -> Result<Vec<&str>> {
    if path.is_empty() || path == "/" {
        return Ok(Vec::new());
    }
    let path = path
        .strip_prefix('/')
        .ok_or_else(|| Error::InvalidPath(path.into()))?;
    let segments = path.split('/').collect::<Vec<_>>();
    if segments
        .iter()
        .any(|segment| segment.is_empty() || matches!(*segment, "." | ".."))
    {
        return Err(Error::InvalidPath(path.into()));
    }
    Ok(segments)
}

fn parent_at_path<'a>(root: &'a mut Value, segments: &[&str]) -> Result<(&'a mut Value, String)> {
    let (leaf, parents) = segments
        .split_last()
        .ok_or_else(|| Error::InvalidPath("missing path segment".into()))?;
    let mut current = root;
    for segment in parents {
        current = match current {
            Value::Map(values) => values
                .get_mut(*segment)
                .ok_or_else(|| Error::InvalidPath(segments.join("/")))?,
            Value::Array(values) => {
                let index = segment
                    .parse::<usize>()
                    .map_err(|_| Error::InvalidPath(segments.join("/")))?;
                values
                    .get_mut(index)
                    .ok_or_else(|| Error::InvalidPath(segments.join("/")))?
            }
            _ => return Err(Error::InvalidPath(segments.join("/"))),
        };
    }
    Ok((current, (*leaf).to_owned()))
}

fn guest_string(value: &KetosValue, context: &str) -> std::result::Result<String, KetosError> {
    match value {
        KetosValue::String(value) => Ok(value.to_string()),
        _ => guest_error(format!("{context} must be text")),
    }
}

fn expect_arity(
    args: &[KetosValue],
    expected: usize,
    name: &str,
) -> std::result::Result<(), KetosError> {
    if args.len() == expected {
        Ok(())
    } else {
        guest_error(format!("{name} expects {expected} arguments"))
    }
}

fn guest_error<T>(message: impl Into<String>) -> std::result::Result<T, KetosError> {
    Err(KetosError::custom(GuestFailure::Script(message.into())))
}

fn guest_from_svit(error: Error) -> KetosError {
    let failure = match error {
        Error::ExecutionLimitExceeded => GuestFailure::Execution,
        Error::InvalidPath(message) => GuestFailure::InvalidPath(message),
        Error::InvalidValue(message) => GuestFailure::InvalidValue(message),
        Error::ResourceLimitExceeded(resource) => GuestFailure::Resource(resource),
        other => GuestFailure::Script(other.to_string()),
    };
    KetosError::custom(failure)
}

fn map_ketos_error(interpreter: &KetosInterpreter, error: KetosError) -> Error {
    match error {
        KetosError::RestrictError(RestrictError::ExecutionTimeExceeded) => {
            Error::ExecutionLimitExceeded
        }
        KetosError::RestrictError(RestrictError::MemoryLimitExceeded) => {
            Error::ResourceLimitExceeded("guest memory")
        }
        KetosError::RestrictError(RestrictError::CallStackExceeded) => {
            Error::ResourceLimitExceeded("call stack")
        }
        KetosError::RestrictError(RestrictError::ValueStackExceeded) => {
            Error::ResourceLimitExceeded("value stack")
        }
        KetosError::RestrictError(RestrictError::NamespaceSizeExceeded) => {
            Error::ResourceLimitExceeded("guest namespace")
        }
        KetosError::RestrictError(RestrictError::IntegerLimitExceeded) => {
            Error::ResourceLimitExceeded("integer bits")
        }
        KetosError::RestrictError(RestrictError::MaxSyntaxNestingExceeded) => {
            Error::ResourceLimitExceeded("syntax depth")
        }
        KetosError::Custom(error) => match error.downcast_ref::<GuestFailure>() {
            Some(GuestFailure::BuiltinPending) => {
                Error::Script("built-in execution suspended".into())
            }
            Some(GuestFailure::Execution) => Error::ExecutionLimitExceeded,
            Some(GuestFailure::InvalidPath(message)) => Error::InvalidPath(message.clone()),
            Some(GuestFailure::InvalidValue(message)) => Error::InvalidValue(message.clone()),
            Some(GuestFailure::Resource(resource)) => Error::ResourceLimitExceeded(resource),
            Some(GuestFailure::Script(message)) => Error::Script(sanitize_diagnostic(message)),
            None => Error::Script("guest function failed".into()),
        },
        error => Error::Script(sanitize_diagnostic(interpreter.format_error(&error))),
    }
}

fn add_script_path(path: &str, error: Error) -> Error {
    match error {
        Error::Script(message) => Error::Script(sanitize_diagnostic(format!("{path}: {message}"))),
        other => other,
    }
}

fn root_map(value: &Value) -> Result<&BTreeMap<String, Value>> {
    match value {
        Value::Map(value) => Ok(value),
        _ => Err(Error::InvalidSnapshot("process root is not a map".into())),
    }
}

fn root_map_mut(value: &mut Value) -> Result<&mut BTreeMap<String, Value>> {
    match value {
        Value::Map(value) => Ok(value),
        _ => Err(Error::InvalidSnapshot("process node is not a map".into())),
    }
}

fn script_value(script: &Script) -> Value {
    Value::Script(script.clone())
}

fn script_metadata(script: &Script) -> Value {
    Value::Map(BTreeMap::from([
        (
            "documentation".into(),
            Value::String(script.documentation().into()),
        ),
        ("source".into(), Value::String(script.source().into())),
    ]))
}

fn message_to_value(message: &MessageIntent) -> Value {
    Value::Map(BTreeMap::from([
        ("body".into(), message.body.clone()),
        (
            "message_id".into(),
            Value::String(message.message_id.clone()),
        ),
        ("to".into(), Value::String(message.to.to_string())),
    ]))
}

fn message_from_value(value: &Value) -> Result<MessageIntent> {
    let value = root_map(value)?;
    let string = |name: &str| match value.get(name) {
        Some(Value::String(value)) => Ok(value.clone()),
        _ => Err(Error::InvalidSnapshot(format!(
            "message field {name} is not text"
        ))),
    };
    Ok(MessageIntent {
        message_id: string("message_id")?,
        to: ProcessId::new(string("to")?)?,
        body: value
            .get("body")
            .cloned()
            .ok_or_else(|| Error::InvalidSnapshot("message body is missing".into()))?,
    })
}

fn root_hash(root: &Value) -> Result<String> {
    let bytes = serde_json::to_vec(root)
        .map_err(|error| Error::InvalidSnapshot(sanitize_diagnostic(error)))?;
    let digest = Sha256::digest(bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn next_process_version(version: u64) -> Result<u64> {
    version
        .checked_add(1)
        .ok_or(Error::ResourceLimitExceeded("process version"))
}

fn lock<T>(value: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    value
        .lock()
        .map_err(|_| Error::Script("runtime buffer lock poisoned".into()))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::value;

    const COUNTER: &str = r#"
        (define (main input)
          (let ((count (+ (read "/memory/count") (value-get input "/by"))))
            (do
              (write "/memory/count" count)
              (log-info! "counted" (value-map "count" count))
              (value-map "count" count))))
    "#;

    fn write_script(process: &mut Process, name: &str, source: impl Into<String>) -> Result<()> {
        process.write(
            &format!("/lib/{name}"),
            Value::Map(BTreeMap::from([(
                "source".into(),
                Value::String(source.into()),
            )])),
        )?;
        Ok(())
    }

    #[test]
    fn activation_commits_memory_output_logs_and_version() {
        let mut process = Process::builder("svit://local/test/counter")
            .unwrap()
            .memory("count", value!(0))
            .build()
            .unwrap();
        write_script(&mut process, "counter", COUNTER).unwrap();

        let activation = process.exec("/lib/counter", value!({"by": 2})).unwrap();

        assert_eq!(activation.output, value!({"count": 2}));
        assert_eq!(activation.logs[0].message, "counted");
        assert_eq!(
            process.read("/memory/count").unwrap(),
            Some(Value::Integer(2))
        );
        assert_eq!(activation.version, 2);
    }

    #[test]
    // THREAT[TM-EFF-001]
    fn host_write_commits_once() {
        let mut process = Process::builder("svit://local/test/write")
            .unwrap()
            .memory("count", value!(0))
            .build()
            .unwrap();

        process.write("/memory/count", value!(2)).unwrap();

        assert_eq!(process.version(), 1);
        assert_eq!(
            process.read("/memory/count").unwrap(),
            Some(Value::Integer(2))
        );
    }

    #[test]
    // THREAT[TM-EFF-001]
    fn rejected_host_write_or_remove_preserves_committed_root() {
        let mut process = Process::builder("svit://local/test/write-rollback")
            .unwrap()
            .memory("count", value!(0))
            .build()
            .unwrap();
        let before = process.snapshot().unwrap();

        assert!(matches!(
            process.write(
                "/memory/count",
                Value::Script(Script::new("(define (main input) input)")),
            ),
            Err(Error::InvalidValue(_))
        ));
        assert_eq!(process.version(), 0);
        assert_eq!(process.snapshot().unwrap(), before);

        assert!(matches!(
            process.remove("/memory/missing"),
            Err(Error::InvalidPath(_))
        ));
        assert_eq!(process.version(), 0);
        assert_eq!(process.snapshot().unwrap(), before);
    }

    #[test]
    // THREAT[TM-MSG-002]
    fn inbox_enqueue_and_acknowledgement_are_atomic() {
        let mut process = Process::new("svit://local/test/inbox").unwrap();
        let message = value!({"role": "user", "content": "hello"});

        process.enqueue_inbox(message.clone()).unwrap();
        assert_eq!(process.version(), 1);
        assert_eq!(process.inbox_front().unwrap(), Some(&message));

        process.acknowledge_inbox(&message).unwrap();
        assert_eq!(process.version(), 2);
        assert_eq!(process.inbox_front().unwrap(), None);
    }

    #[test]
    // THREAT[TM-MSG-002]
    fn rejected_inbox_transition_preserves_committed_state() {
        let mut process = Process::new("svit://local/test/inbox-rollback").unwrap();
        let message = value!({"role": "user", "content": "hello"});
        process.enqueue_inbox(message).unwrap();
        let before = process.snapshot().unwrap();

        assert!(matches!(
            process.acknowledge_inbox(&value!("different")),
            Err(Error::InboxConflict)
        ));
        assert_eq!(process.version(), 1);
        assert_eq!(process.snapshot().unwrap(), before);

        assert!(matches!(
            process.enqueue_inbox(Value::Number(f64::NAN)),
            Err(Error::InvalidValue(_))
        ));
        assert_eq!(process.version(), 1);
        assert_eq!(process.snapshot().unwrap(), before);
        assert!(matches!(
            process.write("/inbox/0", value!("tampered")),
            Err(Error::InvalidPath(_))
        ));
        assert_eq!(process.snapshot().unwrap(), before);
    }

    #[test]
    // THREAT[TM-EFF-001]
    fn runtime_error_rolls_back_memory_and_outbox() {
        let mut process = Process::builder("svit://local/test/rollback")
            .unwrap()
            .memory("balance", value!(10))
            .build()
            .unwrap();
        write_script(
            &mut process,
            "payment",
            r#"
                (define (main input)
                  (let ((amount (value-get input "/amount")))
                    (do
                      (write "/memory/balance" (- (read "/memory/balance") amount))
                      (send! "svit://local/test/merchant" (value-map "amount" amount))
                      (panic "declined"))))
                "#,
        )
        .unwrap();
        let version = process.version();

        assert!(process.exec("/lib/payment", value!({"amount": 3})).is_err());
        assert_eq!(process.version(), version);
        assert_eq!(
            process.read("/memory/balance").unwrap(),
            Some(Value::Integer(10))
        );
        assert!(process.outbox().unwrap().is_empty());
    }

    #[test]
    // THREAT[TM-FORK-001]
    fn snapshot_restore_and_fork_are_independent() {
        let mut parent = Process::builder("svit://local/test/parent")
            .unwrap()
            .memory("count", value!(0))
            .build()
            .unwrap();
        write_script(&mut parent, "counter", COUNTER).unwrap();
        let snapshot = parent.snapshot().unwrap();
        let restored = Process::restore(&snapshot).unwrap();
        assert_eq!(
            restored.read("/memory").unwrap(),
            parent.read("/memory").unwrap()
        );

        let mut child = parent.fork("svit://local/test/child").unwrap();
        child.exec("/lib/counter", value!({"by": 5})).unwrap();
        assert_eq!(
            parent.read("/memory/count").unwrap(),
            Some(Value::Integer(0))
        );
        assert_eq!(
            child.read("/memory/count").unwrap(),
            Some(Value::Integer(5))
        );
    }

    #[test]
    // THREAT[TM-DOS-001]
    fn infinite_loop_exhausts_execution_time_without_committing() {
        let limits = Limits {
            max_execution_millis: 1,
            ..Limits::default()
        };
        let mut process = Process::builder("svit://local/test/limits")
            .unwrap()
            .limits(limits)
            .memory("started", value!(false))
            .build()
            .unwrap();
        write_script(
            &mut process,
            "loop",
            r#"
                (define (spin) (spin))
                (define (main input)
                  (do (write "/memory/started" true) (spin)))
                "#,
        )
        .unwrap();
        let version = process.version();

        let error = process.exec("/lib/loop", Value::Null).unwrap_err();
        assert!(matches!(error, Error::ExecutionLimitExceeded));
        assert_eq!(process.version(), version);
        assert_eq!(
            process.read("/memory/started").unwrap(),
            Some(Value::Bool(false))
        );
    }

    #[test]
    // THREAT[TM-ESC-001]
    fn denied_ambient_modules_are_not_in_the_script_environment() {
        let mut process = Process::new("svit://local/test/sandbox").unwrap();
        assert!(matches!(
            write_script(
                &mut process,
                "inspect",
                "(use random) (define (main input) true)"
            ),
            Err(Error::Script(_))
        ));
    }

    #[test]
    fn guest_can_stage_a_discoverable_script_atomically() {
        let mut process = Process::builder("svit://local/test/author")
            .unwrap()
            .library(
                "teacher",
                Script::new(
                r##"
                (define (main input)
                  (do
                    (write
                      "/lib/greeter"
                      (value-map
                        "source" "(define (main input) (concat \"Hello, \" (value-get input \"/name\")))"
                        "documentation" "Greets a person"))
                    (discover "/lib")))
                "##,
                ),
            )
            .build()
            .unwrap();

        process.exec("/lib/teacher", Value::Null).unwrap();
        assert_eq!(
            process.read("/lib/greeter").unwrap().unwrap().to_json()["documentation"],
            "Greets a person",
        );
        let greeting = process
            .exec("/lib/greeter", value!({"name": "Svit"}))
            .unwrap();
        assert_eq!(greeting.output, Value::String("Hello, Svit".into()));
    }

    struct DenyScript {
        seen: Arc<Mutex<Vec<ActivationEvent>>>,
    }

    impl ActivationHook for DenyScript {
        fn before_activation(&self, request: ActivationRequest) -> HookAction<ActivationRequest> {
            HookAction::Cancel(format!("{} is disabled", request.script))
        }

        fn after_activation(&self, event: &ActivationEvent) {
            self.seen.lock().unwrap().push(event.clone());
        }
    }

    #[test]
    fn hooks_can_deny_and_observe_without_running_guest_code() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut process = Process::builder("svit://local/test/hooks")
            .unwrap()
            .hook(DenyScript {
                seen: Arc::clone(&seen),
            })
            .build()
            .unwrap();
        write_script(
            &mut process,
            "mutate",
            "(define (main input) (write \"/memory/changed\" true))",
        )
        .unwrap();

        assert!(matches!(
            process.exec("/lib/mutate", Value::Null),
            Err(Error::HookCancelled(_))
        ));
        assert_eq!(process.read("/memory/changed").unwrap(), None);
        assert!(matches!(
            seen.lock().unwrap()[0].status,
            ActivationStatus::Failed { .. }
        ));
    }
}
