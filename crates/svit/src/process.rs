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
use std::time::Duration;

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
use crate::{Error, Limits, Result, Script, Value};

const SNAPSHOT_FORMAT: u32 = 2;
const MAX_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;

/// Stable logical address of one agent process.
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
}

/// Builder for a process with initial memory, limits, and frozen hooks.
pub struct ProcessBuilder {
    id: ProcessId,
    memory: Value,
    limits: Limits,
    hooks: Vec<SharedHook>,
}

impl ProcessBuilder {
    /// Replaces the initial `/memory` value.
    pub fn memory(mut self, memory: Value) -> Self {
        self.memory = memory;
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
        self.memory.validate(&self.limits, false)?;
        Ok(Process {
            id: self.id,
            version: 0,
            root: Arc::new(initial_root(self.memory)),
            limits: self.limits,
            hooks: self.hooks.into(),
        })
    }
}

/// In-memory, serializable agent process.
pub struct Process {
    id: ProcessId,
    version: u64,
    root: Arc<Value>,
    limits: Limits,
    hooks: Arc<[SharedHook]>,
}

impl Process {
    /// Starts a process builder for a globally meaningful logical address.
    pub fn builder(id: impl Into<String>) -> Result<ProcessBuilder> {
        Ok(ProcessBuilder {
            id: ProcessId::new(id)?,
            memory: Value::empty_map(),
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

    /// Reads a committed value by slash-separated path.
    pub fn read(&self, path: &str) -> Result<Option<&Value>> {
        if path.is_empty() || path == "/" {
            return Ok(Some(self.root.as_ref()));
        }
        let path = path
            .strip_prefix('/')
            .ok_or_else(|| Error::InvalidPath(path.into()))?;
        let mut current = self.root.as_ref();
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

    /// Saves or replaces a named script and commits a new process version.
    pub fn save_script(&mut self, name: &str, source: impl Into<String>) -> Result<()> {
        self.save_script_record(name, Script::new(source))
    }

    /// Saves a script record with discoverable documentation.
    pub fn save_script_record(&mut self, name: &str, script: Script) -> Result<()> {
        validate_script_name(name)?;
        script_value(&script).validate(&self.limits, true)?;
        validate_script_source(name, script.source(), &self.limits)?;

        let mut root = root_map(self.root.as_ref())?.clone();
        let lib = root_map_mut(root.get_mut("lib").expect("validated process root"))?;
        lib.insert(name.into(), Value::Script(script));
        self.root = Arc::new(Value::Map(root));
        self.version += 1;
        Ok(())
    }

    /// Returns a stored script record.
    pub fn script(&self, name: &str) -> Option<&Script> {
        let root = root_map(self.root.as_ref()).ok()?;
        let lib = root_map(root.get("lib")?).ok()?;
        match lib.get(name)? {
            Value::Script(script) => Some(script),
            _ => None,
        }
    }

    /// Lists stored script names in deterministic order.
    pub fn script_names(&self) -> Vec<String> {
        root_map(self.root.as_ref())
            .ok()
            .and_then(|root| root.get("lib"))
            .and_then(|lib| root_map(lib).ok())
            .map(|lib| lib.keys().cloned().collect())
            .unwrap_or_default()
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

    /// Invokes a named script transactionally.
    ///
    /// The script must define `main(input)`. Any error leaves memory, scripts,
    /// outbox, and version unchanged.
    pub fn run(&mut self, script: &str, input: Value) -> Result<Activation> {
        let version_before = self.version;
        let mut request = Ok(ActivationRequest {
            script: script.into(),
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
            .unwrap_or_else(|_| script.into());
        let result = request.and_then(|request| self.run_inner(request));

        if !self.hooks.is_empty() {
            let status = match &result {
                Ok(activation) => ActivationStatus::Committed {
                    version: activation.version,
                },
                Err(error) => ActivationStatus::Failed {
                    error: sanitize_diagnostic(error),
                },
            };
            let event = ActivationEvent {
                process_id: self.id.clone(),
                script: event_script,
                version_before,
                status,
            };
            for hook in self.hooks.iter() {
                hook.after_activation(&event);
            }
        }

        result
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
    /// Host hooks are intentionally not serialized and must be attached by the
    /// host when constructing a new policy boundary.
    pub fn restore(bytes: &[u8]) -> Result<Self> {
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
        validate_root(&snapshot.root, &snapshot.limits)?;
        // THREAT[TM-SNAP-001]: Recompute integrity after decoding and complete
        // invariant validation. Hashes detect corruption, not provenance.
        if root_hash(&snapshot.root)? != snapshot.root_hash {
            return Err(Error::InvalidSnapshot("root hash mismatch".into()));
        }
        Ok(Self {
            id: snapshot.id,
            version: snapshot.version,
            root: Arc::new(snapshot.root),
            limits: snapshot.limits,
            hooks: Arc::from([]),
        })
    }

    /// Creates an independent child at the current committed boundary.
    ///
    /// Memory and scripts are copied. Already-committed parent message intents
    /// are not duplicated into the child.
    pub fn fork(&self, child_id: impl Into<String>) -> Result<Self> {
        // THREAT[TM-FORK-001]: Clone the committed root before clearing
        // process-local delivery state; subsequent child commits replace only
        // the child's root.
        let mut root = root_map(self.root.as_ref())?.clone();
        let system = root_map_mut(root.get_mut("system").expect("validated process root"))?;
        system.insert("outbox".into(), Value::Array(Vec::new()));
        Ok(Self {
            id: ProcessId::new(child_id)?,
            version: self.version,
            root: Arc::new(Value::Map(root)),
            limits: self.limits.clone(),
            hooks: Arc::clone(&self.hooks),
        })
    }

    fn run_inner(&mut self, request: ActivationRequest) -> Result<Activation> {
        request.input.validate(&self.limits, false)?;
        let script = self
            .script(&request.script)
            .cloned()
            .ok_or_else(|| Error::ScriptNotFound(request.script.clone()))?;
        let memory = self
            .read("/memory")?
            .expect("validated process root")
            .clone();

        let state = RuntimeState::new(memory);
        let interpreter =
            secure_lisp(&state, &request.input, &self.script_records(), &self.limits)?;
        // Svit owns the language contract and virtual source identity; Ketos is
        // an interpreter implementation detail.
        let source_path = format!("/lib/{}.svit-script", request.script);
        let output_lisp = catch_unwind(AssertUnwindSafe(|| {
            let execution = (|| {
                interpreter
                    .run_code(script.source(), Some(source_path.clone()))
                    .map_err(|error| map_ketos_error(&interpreter, error))?;
                if interpreter.get_value("main").is_none() {
                    return Err(Error::Script("script must define main(input)".into()));
                }
                interpreter
                    .call("main", vec![persistent_to_ketos(&request.input)?])
                    .map_err(|error| map_ketos_error(&interpreter, error))
            })();
            execution.map_err(|error| add_script_path(&source_path, error))
        }))
        .map_err(|_| Error::Script("Lisp interpreter failed".into()))??;

        let output = persistent_from_ketos(&output_lisp, &self.limits)?;
        let new_memory = lock(&state.memory)?.clone();
        output.validate(&self.limits, false)?;
        new_memory.validate(&self.limits, false)?;

        let staged_scripts = lock(&state.staged_scripts)?.clone();
        for (name, script) in &staged_scripts {
            validate_script_name(name)?;
            script_value(script).validate(&self.limits, true)?;
            validate_script_source(name, script.source(), &self.limits)?;
        }

        let version = self.version + 1;
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
        for (name, script) in staged_scripts {
            lib.insert(name, Value::Script(script));
        }
        let system = root_map_mut(root.get_mut("system").expect("validated process root"))?;
        let Value::Array(outbox) = system.get_mut("outbox").expect("validated process root") else {
            return Err(Error::InvalidSnapshot(
                "/system/outbox is not an array".into(),
            ));
        };
        outbox.extend(committed_messages.iter().map(message_to_value));

        let new_root = Value::Map(root);
        validate_root(&new_root, &self.limits)?;
        // THREAT[TM-EFF-001]: This is the only activation commit point. Every
        // fallible guest conversion and staged-script validation is complete.
        self.root = Arc::new(new_root);
        self.version = version;

        Ok(Activation {
            output,
            logs: lock(&state.logs)?.clone(),
            messages: committed_messages,
            version,
            root_hash: self.root_hash(),
        })
    }

    fn script_records(&self) -> BTreeMap<String, Script> {
        self.script_names()
            .into_iter()
            .filter_map(|name| self.script(&name).cloned().map(|script| (name, script)))
            .collect()
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

fn initial_root(memory: Value) -> Value {
    Value::Map(BTreeMap::from([
        ("lib".into(), Value::empty_map()),
        ("memory".into(), memory),
        (
            "system".into(),
            Value::Map(BTreeMap::from([(
                "outbox".into(),
                Value::Array(Vec::new()),
            )])),
        ),
    ]))
}

fn validate_root(root: &Value, limits: &Limits) -> Result<()> {
    let root = root_map(root)?;
    if root.keys().map(String::as_str).collect::<BTreeSet<_>>()
        != BTreeSet::from(["lib", "memory", "system"])
    {
        return Err(Error::InvalidSnapshot(
            "process root must contain exactly lib, memory, and system".into(),
        ));
    }
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
    if system.keys().map(String::as_str).collect::<BTreeSet<_>>() != BTreeSet::from(["outbox"]) {
        return Err(Error::InvalidSnapshot(
            "/system must contain exactly outbox".into(),
        ));
    }
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
    let state = RuntimeState::new(Value::empty_map());
    let interpreter = secure_lisp(&state, &Value::Null, &BTreeMap::new(), limits)?;
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

struct RuntimeState {
    memory: Arc<Mutex<Value>>,
    logs: Arc<Mutex<Vec<LogRecord>>>,
    messages: Arc<Mutex<Vec<StagedMessage>>>,
    staged_scripts: Arc<Mutex<Vec<(String, Script)>>>,
}

impl RuntimeState {
    fn new(memory: Value) -> Self {
        Self {
            memory: Arc::new(Mutex::new(memory)),
            logs: Arc::new(Mutex::new(Vec::new())),
            messages: Arc::new(Mutex::new(Vec::new())),
            staged_scripts: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

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
    InvalidPath(String),
    InvalidValue(String),
    Resource(&'static str),
    Script(String),
}

impl fmt::Display for GuestFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(message) | Self::InvalidValue(message) | Self::Script(message) => {
                formatter.write_str(message)
            }
            Self::Resource(resource) => write!(formatter, "{resource} limit exceeded"),
        }
    }
}

impl StdError for GuestFailure {}

fn secure_lisp(
    state: &RuntimeState,
    input: &Value,
    scripts: &BTreeMap<String, Script>,
    limits: &Limits,
) -> Result<KetosInterpreter> {
    // THREAT[TM-ESC-001]: Null I/O and a null module loader leave guest code
    // without filesystem, environment, network, clock, randomness, or modules.
    // THREAT[TM-ISO-001]: Every activation receives a fresh interpreter scope.
    let restrictions = RestrictConfig {
        execution_time: Some(Duration::from_millis(limits.max_execution_millis)),
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
        .add_named_value("*svit-version*", KetosValue::from("Svit Lisp 1"));
    install_guest_api(&interpreter, state, input, scripts, limits);
    Ok(interpreter)
}

fn install_guest_api(
    interpreter: &KetosInterpreter,
    state: &RuntimeState,
    input: &Value,
    scripts: &BTreeMap<String, Script>,
    limits: &Limits,
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

    let memory_for_get = Arc::clone(&state.memory);
    install_guest_function(interpreter, "memory-get", move |_, args| {
        expect_arity(args, 1, "memory-get")?;
        let path = guest_string(&args[0], "memory-get path")?;
        let memory = memory_for_get.lock().map_err(|_| {
            KetosError::custom(GuestFailure::Script("runtime state unavailable".into()))
        })?;
        let found = read_value_path(&memory, &path)
            .map_err(guest_from_svit)?
            .cloned()
            .unwrap_or(Value::Null);
        persistent_to_ketos(&found).map_err(guest_from_svit)
    });

    let memory_for_set = Arc::clone(&state.memory);
    let set_limits = limits.clone();
    install_guest_function(interpreter, "memory-set!", move |_, args| {
        expect_arity(args, 2, "memory-set!")?;
        let path = guest_string(&args[0], "memory-set! path")?;
        let value = persistent_from_ketos(&args[1], &set_limits).map_err(guest_from_svit)?;
        value
            .validate(&set_limits, false)
            .map_err(guest_from_svit)?;
        let mut memory = memory_for_set.lock().map_err(|_| {
            KetosError::custom(GuestFailure::Script("runtime state unavailable".into()))
        })?;
        set_value_path(&mut memory, &path, value).map_err(guest_from_svit)?;
        memory
            .validate(&set_limits, false)
            .map_err(guest_from_svit)?;
        Ok(KetosValue::Unit)
    });

    let memory_for_remove = Arc::clone(&state.memory);
    install_guest_function(interpreter, "memory-remove!", move |_, args| {
        expect_arity(args, 1, "memory-remove!")?;
        let path = guest_string(&args[0], "memory-remove! path")?;
        let mut memory = memory_for_remove.lock().map_err(|_| {
            KetosError::custom(GuestFailure::Script("runtime state unavailable".into()))
        })?;
        remove_value_path(&mut memory, &path).map_err(guest_from_svit)?;
        Ok(KetosValue::Unit)
    });

    let script_names: Vec<Value> = scripts.keys().cloned().map(Value::String).collect();
    install_guest_function(interpreter, "scripts-list", move |_, args| {
        expect_arity(args, 0, "scripts-list")?;
        persistent_to_ketos(&Value::Array(script_names.clone())).map_err(guest_from_svit)
    });

    let readable_scripts = scripts.clone();
    install_guest_function(interpreter, "scripts-read", move |_, args| {
        expect_arity(args, 1, "scripts-read")?;
        let name = guest_string(&args[0], "scripts-read name")?;
        let value = readable_scripts.get(&name).map_or(Value::Null, |script| {
            Value::Map(BTreeMap::from([
                (
                    "documentation".into(),
                    Value::String(script.documentation().into()),
                ),
                ("source".into(), Value::String(script.source().into())),
            ]))
        });
        persistent_to_ketos(&value).map_err(guest_from_svit)
    });

    let staged_for_save = Arc::clone(&state.staged_scripts);
    let maximum_scripts = limits.max_staged_scripts;
    let maximum_script_bytes = limits.max_script_bytes;
    install_guest_function(interpreter, "scripts-save!", move |_, args| {
        if !(2..=3).contains(&args.len()) {
            return guest_error("scripts-save! expects name, source, and optional documentation");
        }
        let name = guest_string(&args[0], "scripts-save! name")?;
        let source = guest_string(&args[1], "scripts-save! source")?;
        let documentation = args
            .get(2)
            .map(|value| guest_string(value, "scripts-save! documentation"))
            .transpose()?
            .unwrap_or_default();
        if source.len() > maximum_script_bytes {
            return Err(KetosError::custom(GuestFailure::Resource("script source")));
        }
        let mut staged = staged_for_save.lock().map_err(|_| {
            KetosError::custom(GuestFailure::Script("runtime state unavailable".into()))
        })?;
        if staged.len() >= maximum_scripts {
            return Err(KetosError::custom(GuestFailure::Resource("staged scripts")));
        }
        staged.push((name, Script::new(source).with_documentation(documentation)));
        Ok(KetosValue::Unit)
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

fn remove_value_path(root: &mut Value, path: &str) -> Result<()> {
    let segments = path_segments(path)?;
    if segments.is_empty() {
        return Err(Error::InvalidPath("cannot remove the memory root".into()));
    }
    let (parent, leaf) = parent_at_path(root, &segments)?;
    match parent {
        Value::Map(values) => {
            values.remove(&leaf);
            Ok(())
        }
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
          (let ((count (+ (memory-get "/count") (value-get input "/by"))))
            (do
              (memory-set! "/count" count)
              (log-info! "counted" (value-map "count" count))
              (value-map "count" count))))
    "#;

    #[test]
    fn activation_commits_memory_output_logs_and_version() {
        let mut process = Process::builder("svit://local/test/counter")
            .unwrap()
            .memory(value!({"count": 0}))
            .build()
            .unwrap();
        process.save_script("counter", COUNTER).unwrap();

        let activation = process.run("counter", value!({"by": 2})).unwrap();

        assert_eq!(activation.output, value!({"count": 2}));
        assert_eq!(activation.logs[0].message, "counted");
        assert_eq!(
            process.read("/memory/count").unwrap(),
            Some(&Value::Integer(2))
        );
        assert_eq!(activation.version, 2);
    }

    #[test]
    // THREAT[TM-EFF-001]
    fn runtime_error_rolls_back_memory_and_outbox() {
        let mut process = Process::builder("svit://local/test/rollback")
            .unwrap()
            .memory(value!({"balance": 10}))
            .build()
            .unwrap();
        process
            .save_script(
                "payment",
                r#"
                (define (main input)
                  (let ((amount (value-get input "/amount")))
                    (do
                      (memory-set! "/balance" (- (memory-get "/balance") amount))
                      (send! "svit://local/test/merchant" (value-map "amount" amount))
                      (panic "declined"))))
                "#,
            )
            .unwrap();
        let version = process.version();

        assert!(process.run("payment", value!({"amount": 3})).is_err());
        assert_eq!(process.version(), version);
        assert_eq!(
            process.read("/memory/balance").unwrap(),
            Some(&Value::Integer(10))
        );
        assert!(process.outbox().unwrap().is_empty());
    }

    #[test]
    // THREAT[TM-FORK-001]
    fn snapshot_restore_and_fork_are_independent() {
        let mut parent = Process::builder("svit://local/test/parent")
            .unwrap()
            .memory(value!({"count": 0}))
            .build()
            .unwrap();
        parent.save_script("counter", COUNTER).unwrap();
        let snapshot = parent.snapshot().unwrap();
        let restored = Process::restore(&snapshot).unwrap();
        assert_eq!(
            restored.read("/memory").unwrap(),
            parent.read("/memory").unwrap()
        );

        let mut child = parent.fork("svit://local/test/child").unwrap();
        child.run("counter", value!({"by": 5})).unwrap();
        assert_eq!(
            parent.read("/memory/count").unwrap(),
            Some(&Value::Integer(0))
        );
        assert_eq!(
            child.read("/memory/count").unwrap(),
            Some(&Value::Integer(5))
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
            .memory(value!({"started": false}))
            .build()
            .unwrap();
        process
            .save_script(
                "loop",
                r#"
                (define (spin) (spin))
                (define (main input)
                  (do (memory-set! "/started" true) (spin)))
                "#,
            )
            .unwrap();
        let version = process.version();

        let error = process.run("loop", Value::Null).unwrap_err();
        assert!(matches!(error, Error::ExecutionLimitExceeded));
        assert_eq!(process.version(), version);
        assert_eq!(
            process.read("/memory/started").unwrap(),
            Some(&Value::Bool(false))
        );
    }

    #[test]
    // THREAT[TM-ESC-001]
    fn denied_ambient_modules_are_not_in_the_script_environment() {
        let mut process = Process::new("svit://local/test/sandbox").unwrap();
        assert!(matches!(
            process.save_script("inspect", "(use random) (define (main input) true)"),
            Err(Error::Script(_))
        ));
    }

    #[test]
    fn guest_can_stage_a_discoverable_script_atomically() {
        let mut process = Process::new("svit://local/test/author").unwrap();
        process
            .save_script(
                "teacher",
                r##"
                (define (main input)
                  (do
                    (scripts-save!
                      "greeter"
                      "(define (main input) (concat \"Hello, \" (value-get input \"/name\")))"
                      "Greets a person")
                    (scripts-list)))
                "##,
            )
            .unwrap();

        process.run("teacher", Value::Null).unwrap();
        assert_eq!(
            process.script("greeter").unwrap().documentation(),
            "Greets a person"
        );
        let greeting = process.run("greeter", value!({"name": "Svit"})).unwrap();
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
        process
            .save_script(
                "mutate",
                "(define (main input) (memory-set! \"/changed\" true))",
            )
            .unwrap();

        assert!(matches!(
            process.run("mutate", Value::Null),
            Err(Error::HookCancelled(_))
        ));
        assert_eq!(process.read("/memory/changed").unwrap(), None);
        assert!(matches!(
            seen.lock().unwrap()[0].status,
            ActivationStatus::Failed { .. }
        ));
    }
}
