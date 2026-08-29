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
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::error::sanitize_diagnostic;
use crate::hooks::{
    ActivationEvent, ActivationHook, ActivationRequest, ActivationStatus, HookAction, SharedHook,
};
use crate::mounts::{Mount, MountDescriptor, MountPath, MountRegistry, MountView};
use crate::persistence::Mutation;
use crate::{Error, Limits, Ports, Result, Script, Value};

#[derive(Clone, Copy)]
struct RuntimeBuiltinDescriptor {
    name: &'static str,
    signature: &'static str,
    description: &'static str,
    category: &'static str,
}

const RUNTIME_BUILTINS: &[RuntimeBuiltinDescriptor] = &[
    RuntimeBuiltinDescriptor {
        name: "runtime-builtins",
        signature: "(runtime-builtins)",
        description: "Return metadata for Svit-provided Lisp runtime helpers.",
        category: "discovery",
    },
    RuntimeBuiltinDescriptor {
        name: "value-map",
        signature: "(value-map key value ...)",
        description: "Construct a validated persistent map from text/value pairs.",
        category: "persistent-value",
    },
    RuntimeBuiltinDescriptor {
        name: "value-array",
        signature: "(value-array value ...)",
        description: "Construct a validated persistent array.",
        category: "persistent-value",
    },
    RuntimeBuiltinDescriptor {
        name: "value-null?",
        signature: "(value-null? value)",
        description: "Return whether value is persistent Svit null.",
        category: "predicate",
    },
    RuntimeBuiltinDescriptor {
        name: "value-get",
        signature: "(value-get value path)",
        description: "Read a path from a structured value, returning null when absent.",
        category: "persistent-value",
    },
    RuntimeBuiltinDescriptor {
        name: "jq",
        signature: "(jq filter value)",
        description: "Apply a bounded jq filter to explicit structured data.",
        category: "structured-data",
    },
    RuntimeBuiltinDescriptor {
        name: "search",
        signature: "(search path pattern)",
        description: "Search bounded text under a process-tree path.",
        category: "memory-tree",
    },
    RuntimeBuiltinDescriptor {
        name: "discover",
        signature: "(discover path)",
        description: "List immediate children under a memory-tree path.",
        category: "memory-tree",
    },
    RuntimeBuiltinDescriptor {
        name: "read",
        signature: "(read path)",
        description: "Read one value from the transactional memory tree.",
        category: "memory-tree",
    },
    RuntimeBuiltinDescriptor {
        name: "stat",
        signature: "(stat path)",
        description: "Return metadata for one memory-tree node.",
        category: "memory-tree",
    },
    RuntimeBuiltinDescriptor {
        name: "write",
        signature: "(write path value)",
        description: "Stage a validated value at a writable process-tree path.",
        category: "memory-tree",
    },
    RuntimeBuiltinDescriptor {
        name: "remove",
        signature: "(remove path)",
        description: "Stage removal of a writable process-tree node.",
        category: "memory-tree",
    },
    RuntimeBuiltinDescriptor {
        name: "exec",
        signature: "(exec path input)",
        description: "Execute a named library script transactionally with bounded nesting.",
        category: "scripts",
    },
    RuntimeBuiltinDescriptor {
        name: "port-call",
        signature: "(port-call name input)",
        description: "Invoke an explicitly attached host port through activation suspension.",
        category: "ports",
    },
    RuntimeBuiltinDescriptor {
        name: "log-info!",
        signature: "(log-info! message [fields])",
        description: "Stage a bounded informational log record.",
        category: "effects",
    },
    RuntimeBuiltinDescriptor {
        name: "send!",
        signature: "(send! address body)",
        description: "Stage a validated message intent for atomic commit.",
        category: "effects",
    },
    RuntimeBuiltinDescriptor {
        name: "json-parse",
        signature: "(json-parse text)",
        description: "Parse bounded JSON into a structured Svit value; invalid JSON fails.",
        category: "structured-data",
    },
    RuntimeBuiltinDescriptor {
        name: "json-stringify",
        signature: "(json-stringify value)",
        description: "Encode a JSON-compatible Lisp or Svit value as canonical compact JSON.",
        category: "structured-data",
    },
    RuntimeBuiltinDescriptor {
        name: "json-parse-safe",
        signature: "(json-parse-safe text)",
        description: "Parse JSON and return an ok/value or ok/error result map.",
        category: "recoverable",
    },
    RuntimeBuiltinDescriptor {
        name: "map?",
        signature: "(map? value)",
        description: "Return whether value is a persistent Svit map.",
        category: "predicate",
    },
    RuntimeBuiltinDescriptor {
        name: "map-get",
        signature: "(map-get map key)",
        description: "Return a map entry; an absent key fails.",
        category: "structured-data",
    },
    RuntimeBuiltinDescriptor {
        name: "map-get-safe",
        signature: "(map-get-safe map key)",
        description: "Read a map entry and return an ok/value or ok/error result map.",
        category: "recoverable",
    },
    RuntimeBuiltinDescriptor {
        name: "map-has?",
        signature: "(map-has? map key)",
        description: "Return whether a map contains key.",
        category: "structured-data",
    },
    RuntimeBuiltinDescriptor {
        name: "map-set",
        signature: "(map-set map key value)",
        description: "Return a new persistent map with key set to value.",
        category: "structured-data",
    },
    RuntimeBuiltinDescriptor {
        name: "list?",
        signature: "(list? value)",
        description: "Return whether value is a native Lisp list or persistent Svit array.",
        category: "predicate",
    },
    RuntimeBuiltinDescriptor {
        name: "list-get",
        signature: "(list-get list index)",
        description: "Return an item from a persistent Svit array by zero-based index.",
        category: "structured-data",
    },
    RuntimeBuiltinDescriptor {
        name: "string?",
        signature: "(string? value)",
        description: "Return whether value is a string.",
        category: "predicate",
    },
    RuntimeBuiltinDescriptor {
        name: "number?",
        signature: "(number? value)",
        description: "Return whether value is an integer or finite float.",
        category: "predicate",
    },
    RuntimeBuiltinDescriptor {
        name: "boolean?",
        signature: "(boolean? value)",
        description: "Return whether value is a Boolean.",
        category: "predicate",
    },
    RuntimeBuiltinDescriptor {
        name: "null?",
        signature: "(null? value)",
        description: "Return whether value is Svit null.",
        category: "predicate",
    },
    RuntimeBuiltinDescriptor {
        name: "result-ok",
        signature: "(result-ok value)",
        description: "Construct a successful result map.",
        category: "result",
    },
    RuntimeBuiltinDescriptor {
        name: "result-error",
        signature: "(result-error message)",
        description: "Construct a failed result map with a bounded message.",
        category: "result",
    },
    RuntimeBuiltinDescriptor {
        name: "result-ok?",
        signature: "(result-ok? result)",
        description: "Return whether a result map is successful.",
        category: "result",
    },
    RuntimeBuiltinDescriptor {
        name: "result-value",
        signature: "(result-value result)",
        description: "Read the value from a successful result map.",
        category: "result",
    },
    RuntimeBuiltinDescriptor {
        name: "result-error-message",
        signature: "(result-error-message result)",
        description: "Read the message from a failed result map.",
        category: "result",
    },
    RuntimeBuiltinDescriptor {
        name: "result-map",
        signature: "(result-map function result)",
        description: "Map a function over a successful result; failures pass through.",
        category: "result",
    },
    RuntimeBuiltinDescriptor {
        name: "result-and-then",
        signature: "(result-and-then function result)",
        description: "Chain a result-returning function after a successful result.",
        category: "result",
    },
    RuntimeBuiltinDescriptor {
        name: "result-or-else",
        signature: "(result-or-else function result)",
        description: "Recover a failed result with a result-returning function.",
        category: "result",
    },
    RuntimeBuiltinDescriptor {
        name: "value-at",
        signature: "(value-at value path)",
        description: "Traverse maps and arrays using a list of string keys and integer indices.",
        category: "structured-data",
    },
    RuntimeBuiltinDescriptor {
        name: "value-at-safe",
        signature: "(value-at-safe value path)",
        description: "Traverse a structured path and return a result map.",
        category: "recoverable",
    },
    RuntimeBuiltinDescriptor {
        name: "value-has-path?",
        signature: "(value-has-path? value path)",
        description: "Return whether a structured value contains a path.",
        category: "structured-data",
    },
    RuntimeBuiltinDescriptor {
        name: "dispatch-table",
        signature: "(dispatch-table name function ...)",
        description: "Construct an ephemeral table of explicitly supplied functions.",
        category: "dispatch",
    },
    RuntimeBuiltinDescriptor {
        name: "dispatch",
        signature: "(dispatch table name arguments)",
        description: "Call one explicitly registered handler; unknown names fail closed.",
        category: "dispatch",
    },
    RuntimeBuiltinDescriptor {
        name: "dispatch-safe",
        signature: "(dispatch-safe table name arguments)",
        description: "Dispatch and return a result map for recoverable failures.",
        category: "dispatch",
    },
    RuntimeBuiltinDescriptor {
        name: "safe-call",
        signature: "(safe-call function argument ...)",
        description: "Call a function and return a result map for recoverable guest errors; hard failures propagate.",
        category: "recoverable",
    },
];
const SNAPSHOT_FORMAT: u32 = 10;
const INLINE_SCRIPT_PREFIX: &str = "\0svit:inline:";
const RUNTIME_LANGUAGE: &str = "svit-lisp@2";
const MAX_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;
const MAX_THREAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_THREAD_RECORDS: usize = 100_000;
const MAX_SEARCH_PATTERN_BYTES: usize = 4 * 1024;
const MAX_SEARCH_RESULTS: usize = 100;
const MAX_SEARCH_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_SEARCH_MOUNT_NODES: usize = 2048;

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
/// persisted for it. Committed paths and their ancestors also carry the
/// content hash they now have, so a client revalidates what it holds instead
/// of discarding everything the paths reach.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    version: u64,
    paths: Vec<String>,
    hashes: BTreeMap<String, Option<String>>,
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
            hashes: BTreeMap::new(),
            mutations: Vec::new(),
        }
    }

    /// Records a change reported to an observer with the content hash each
    /// changed path and its ancestors now have.
    ///
    /// A path whose node no longer exists carries `None`.
    pub fn notification_with_hashes(
        version: u64,
        paths: Vec<String>,
        hashes: BTreeMap<String, Option<String>>,
    ) -> Self {
        Self {
            version,
            paths,
            hashes,
            mutations: Vec::new(),
        }
    }

    /// Returns this change with its values dropped, ready to publish.
    pub fn to_notification(&self) -> Self {
        Self::notification_with_hashes(self.version, self.paths.clone(), self.hashes.clone())
    }

    /// Returns the content hash this transition left at `path`.
    ///
    /// `Some(None)` reports a path the transition removed. `None` means the
    /// transition carries no hash for it, either because the path is outside
    /// the reported set or because the change was published without hashes;
    /// a client must then re-read the node rather than assume it is current.
    /// Mount paths never carry a hash: their content lives outside the
    /// committed root.
    pub fn hash(&self, path: &str) -> Option<Option<&str>> {
        self.hashes
            .get(path)
            .map(|hash| hash.as_ref().map(String::as_str))
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
    /// Structural SHA-256 content hash of the committed root.
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

    /// Returns the content hash of one committed node.
    ///
    /// A client caches this alongside the node and compares it against the
    /// hash a later [`Change`] publishes, keeping what still matches instead
    /// of re-reading everything a commit could have touched. Mount paths have
    /// no committed content and return `None`.
    pub fn node_hash(&self, path: &str) -> Result<Option<String>> {
        if mount_target(path)?.is_some() {
            return Ok(None);
        }
        Ok(self.read_committed(path)?.map(Value::content_hash))
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
        // A client revalidates by content, not by path overlap: a changed
        // path reaches every ancestor, so each one publishes the hash it now
        // has and a client keeps whatever still matches.
        let hashes = self.published_hashes(&paths);
        Ok(Change {
            version,
            paths,
            hashes,
            mutations,
        })
    }

    /// Collects the content hash of every reported path and its ancestors.
    ///
    /// Mount paths are deliberately absent: their content is external, so a
    /// client must re-read them rather than compare a committed hash.
    fn published_hashes(&self, paths: &[String]) -> BTreeMap<String, Option<String>> {
        let mut hashes = BTreeMap::new();
        for path in paths {
            for path in ancestors_inclusive(path) {
                if mount_target(&path).ok().flatten().is_some() {
                    continue;
                }
                hashes.entry(path).or_insert_with_key(|path| {
                    self.read_committed(path)
                        .ok()
                        .flatten()
                        .map(Value::content_hash)
                });
            }
        }
        hashes
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
                hashes: BTreeMap::new(),
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
                hashes: BTreeMap::new(),
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
                hashes: BTreeMap::new(),
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

    /// Replaces the host-managed port catalog under `/ports`.
    pub(crate) fn replace_ports(&mut self, value: Value) -> Result<Change> {
        let ports = root_map(&value)?;
        for name in ports.keys() {
            validate_script_name(name)?;
        }
        value.validate(&self.limits, false)?;
        let mut root = root_map(self.root.as_ref())?.clone();
        if root.get("ports") == Some(&value) {
            return Ok(Change::notification(self.version, Vec::new()));
        }
        root.insert("ports".into(), value.clone());
        self.commit(
            Value::Map(root),
            vec![Mutation::Set {
                path: "/ports".into(),
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
        let (script, event_script) = activation_script(path, &self.limits)?;
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
            .map(|request| display_script(&request.script))
            .unwrap_or(event_script);
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

    pub(crate) async fn prepare_exec_with_ports(
        &self,
        path: &str,
        input: Value,
        ports: &Ports,
        context: Arc<Mutex<Process>>,
    ) -> Result<PreparedActivation> {
        let (script, event_script) = activation_script(path, &self.limits)?;
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
            .map(|request| display_script(&request.script))
            .unwrap_or(event_script);
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
                    ExecStep::Port(call, elapsed) => {
                        execution_time = execution_time
                            .checked_sub(elapsed)
                            .ok_or(Error::ExecutionLimitExceeded)?;
                        let Some(name) = call.path.strip_prefix("/ports/") else {
                            return Err(Error::InvalidPath(call.path));
                        };
                        // THREAT[TM-CAP-004] THREAT[TM-EFF-005]: The guest can
                        // select only a port installed by this Svit host.
                        // The call is immediate and recorded for deterministic
                        // replay of the remaining pure script segments; it is
                        // never represented as committed or restorable authority.
                        let output = ports
                            .execute_value(name, call.input.to_json(), context.clone())
                            .await
                            .map_err(|error| {
                                Error::Script(sanitize_diagnostic(format!(
                                    "port {} failed: {error}",
                                    call.path
                                )))
                            })?;
                        replay.push(PortReplay {
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
            ExecStep::Port(_, _) => Err(Error::Script(
                "Svit Lisp port calls require a Svit host".into(),
            )),
        }
    }

    fn exec_inner_step(
        &mut self,
        request: ActivationRequest,
        port_replay: Vec<PortReplay>,
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
            port_replay,
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
                if !state.port_replay_complete()? {
                    return Err(Error::Script("port replay diverged".into()));
                }
                output
            }
            Err(error) => match state.pending_port()? {
                Some(call) => {
                    return Ok(ExecStep::Port(call, execution_started.elapsed()));
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
    Port(PortCall, Duration),
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
        ("ports".into(), Value::empty_map()),
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
    validate_tree_size(root, limits)?;
    let root = root_map(root)?;
    if root.keys().map(String::as_str).collect::<BTreeSet<_>>()
        != BTreeSet::from([
            "thread", "ports", "children", "inbox", "lib", "memory", "mounts", "system", "tasks",
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

    let ports = root_map(
        root.get("ports")
            .ok_or_else(|| Error::InvalidSnapshot("missing /ports".into()))?,
    )?;
    for name in ports.keys() {
        validate_script_name(name)?;
    }
    Value::Map(ports.clone()).validate(limits, false)?;

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

// THREAT[TM-DOS-012]: Per-value limits bound one write but never the state a
// process accumulates. Measuring the whole committed root at the same boundary
// that validates it keeps unbounded growth from surviving a commit, a restore,
// or a fork, and keeps every snapshot of that root bounded too.
fn validate_tree_size(root: &Value, limits: &Limits) -> Result<()> {
    let mut nodes = 0usize;
    let mut text_bytes = 0usize;
    root.measure(&mut nodes, &mut text_bytes);
    if nodes > limits.max_tree_nodes {
        return Err(Error::ResourceLimitExceeded("process tree nodes"));
    }
    if text_bytes > limits.max_tree_text_bytes {
        return Err(Error::ResourceLimitExceeded("process tree text bytes"));
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
        (
            "max_tree_nodes".into(),
            integer_value(limits.max_tree_nodes),
        ),
        (
            "max_tree_text_bytes".into(),
            integer_value(limits.max_tree_text_bytes),
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
        ("hash".into(), Value::from(value.content_hash())),
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

/// Resolves either a durable `/lib` entry or transient source passed through
/// the Svit execution boundary. Inline source never enters the process root.
fn activation_script(path: &str, limits: &Limits) -> Result<(String, String)> {
    if let Some(source) = path.strip_prefix(INLINE_SCRIPT_PREFIX) {
        validate_script_source("inline", source, limits)?;
        return Ok((format!("{INLINE_SCRIPT_PREFIX}{source}"), "<inline>".into()));
    }
    let script = library_path(path)?
        .ok_or_else(|| Error::InvalidPath(path.into()))?
        .to_owned();
    Ok((script.clone(), script))
}

pub(crate) fn inline_script_path(source: &str) -> String {
    format!("{INLINE_SCRIPT_PREFIX}{source}")
}

fn display_script(script: &str) -> String {
    if script.starts_with(INLINE_SCRIPT_PREFIX) {
        "<inline>".into()
    } else {
        script.into()
    }
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
    port_replay: Arc<Mutex<PortReplayState>>,
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
struct PortReplay {
    path: String,
    input: Value,
    output: Value,
}

struct PortReplayState {
    completed: Vec<PortReplay>,
    cursor: usize,
    pending: Option<PortCall>,
}

#[derive(Clone)]
struct PortCall {
    path: String,
    input: Value,
}

enum PortResolution {
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
        port_replay: Vec<PortReplay>,
    ) -> Self {
        Self {
            committed_root: Arc::new(committed_root),
            memory: Arc::new(Mutex::new(memory)),
            mutations: Arc::new(Mutex::new(Vec::new())),
            logs: Arc::new(Mutex::new(Vec::new())),
            messages: Arc::new(Mutex::new(Vec::new())),
            library_changes: Arc::new(Mutex::new(Vec::new())),
            mount_writes: Arc::new(Mutex::new(Vec::new())),
            port_replay: Arc::new(Mutex::new(PortReplayState {
                completed: port_replay,
                cursor: 0,
                pending: None,
            })),
            mounts,
            limits,
            deadline: Instant::now() + execution_time,
        }
    }

    fn resolve_port(&self, path: String, input: Value) -> Result<PortResolution> {
        let mut replay = lock(&self.port_replay)?;
        if let Some(completed) = replay.completed.get(replay.cursor).cloned() {
            if completed.path != path || completed.input != input {
                return Err(Error::Script("port replay diverged".into()));
            }
            replay.cursor += 1;
            return Ok(PortResolution::Output(completed.output));
        }
        if replay.completed.len() >= self.limits.max_exec_depth {
            return Err(Error::ResourceLimitExceeded("port calls"));
        }
        replay.pending = Some(PortCall { path, input });
        Ok(PortResolution::Pending)
    }

    fn pending_port(&self) -> Result<Option<PortCall>> {
        Ok(lock(&self.port_replay)?.pending.clone())
    }

    fn port_replay_complete(&self) -> Result<bool> {
        let replay = lock(&self.port_replay)?;
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
    PortPending,
    Execution,
    InvalidPath(String),
    InvalidValue(String),
    Resource(&'static str),
    Script(String),
}

impl fmt::Display for GuestFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PortPending => formatter.write_str("port execution suspended"),
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
    let (source, source_path) = match name.strip_prefix(INLINE_SCRIPT_PREFIX) {
        Some(source) => (source.to_owned(), "/activation/inline.svit-script".into()),
        None => {
            validate_script_name(name)?;
            let script = state
                .script(name)?
                .ok_or_else(|| Error::ScriptNotFound(name.into()))?;
            (
                script.source().to_owned(),
                format!("/lib/{name}.svit-script"),
            )
        }
    };
    let interpreter = secure_lisp(state, input, limits, remaining_exec_depth)?;
    // Svit owns the language contract and virtual source identity; Ketos is
    // an interpreter implementation detail.
    let execution = (|| {
        interpreter
            .run_code(&source, Some(source_path.clone()))
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

    install_structured_value_functions(interpreter, limits);

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

    let jq_limits = limits.clone();
    install_guest_function(interpreter, "jq", move |_, args| {
        expect_arity(args, 2, "jq")?;
        let filter = guest_string(&args[0], "jq filter")?;
        // A port response can be larger than the durable value envelope. Jq is
        // a reduction boundary: it may inspect that activation-local value,
        // but its result still validates before it can cross into guest state.
        let input = jq_input_from_ketos(&args[1], &jq_limits).map_err(guest_from_svit)?;
        let output = crate::stdlib::jq(&filter, &input, &jq_limits).map_err(guest_from_svit)?;
        persistent_to_ketos(&output).map_err(guest_from_svit)
    });

    let search_state = state.clone();
    let search_limits = limits.clone();
    install_guest_function(interpreter, "search", move |_, args| {
        expect_arity(args, 2, "search")?;
        let path = guest_string(&args[0], "search path")?;
        let pattern = guest_string(&args[1], "search pattern")?;
        let output = search_runtime(&search_state, &path, &pattern, &search_limits)
            .map_err(guest_from_svit)?;
        persistent_to_ketos(&output).map_err(guest_from_svit)
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

    let state_for_port = state.clone();
    let port_limits = limits.clone();
    install_guest_function(interpreter, "port-call", move |_, args| {
        expect_arity(args, 2, "port-call")?;
        let name = guest_string(&args[0], "port-call name")?;
        if name.is_empty() || name.contains('/') {
            return guest_error("port-call name must name one port");
        }
        let input = persistent_from_ketos(&args[1], &port_limits).map_err(guest_from_svit)?;
        match state_for_port
            .resolve_port(format!("/ports/{name}"), input)
            .map_err(guest_from_svit)?
        {
            PortResolution::Output(output) => persistent_to_ketos(&output).map_err(guest_from_svit),
            PortResolution::Pending => Err(KetosError::custom(GuestFailure::PortPending)),
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

fn runtime_builtin_catalog() -> Value {
    Value::Array(
        RUNTIME_BUILTINS
            .iter()
            .map(|builtin| {
                Value::Map(BTreeMap::from([
                    ("category".into(), Value::String(builtin.category.into())),
                    (
                        "description".into(),
                        Value::String(builtin.description.into()),
                    ),
                    ("name".into(), Value::String(builtin.name.into())),
                    ("signature".into(), Value::String(builtin.signature.into())),
                ]))
            })
            .collect(),
    )
}

fn install_structured_value_functions(interpreter: &KetosInterpreter, limits: &Limits) {
    install_guest_function(interpreter, "runtime-builtins", |_, args| {
        expect_arity(args, 0, "runtime-builtins")?;
        persistent_to_ketos(&runtime_builtin_catalog()).map_err(guest_from_svit)
    });
    let parse_limits = limits.clone();
    install_guest_function(interpreter, "json-parse", move |_, args| {
        expect_arity(args, 1, "json-parse")?;
        json_parse_value(&guest_string(&args[0], "json-parse")?, &parse_limits)
    });
    let stringify_limits = limits.clone();
    install_guest_function(interpreter, "json-stringify", move |_, args| {
        expect_arity(args, 1, "json-stringify")?;
        let value = persistent_from_ketos(&args[0], &stringify_limits).map_err(guest_from_svit)?;
        let text = serde_json::to_string(&value.to_json())
            .map_err(|_| guest_failure("json stringify failed"))?;
        Ok(KetosValue::String(text.into()))
    });
    let safe_parse_limits = limits.clone();
    install_guest_function(interpreter, "json-parse-safe", move |_, args| {
        expect_arity(args, 1, "json-parse-safe")?;
        let result = guest_string(&args[0], "json-parse-safe")
            .and_then(|text| json_parse_value(&text, &safe_parse_limits))
            .and_then(|value| {
                persistent_from_ketos(&value, &safe_parse_limits).map_err(guest_from_svit)
            });
        safe_result(recoverable_result(result)?, &safe_parse_limits)
    });
    install_guest_function(interpreter, "map?", |_, args| {
        expect_arity(args, 1, "map?")?;
        Ok(KetosValue::Bool(matches!(
            guest_persistent(&args[0]),
            Some(Value::Map(_))
        )))
    });
    install_guest_function(interpreter, "list?", |_, args| {
        expect_arity(args, 1, "list?")?;
        Ok(KetosValue::Bool(
            matches!(args[0], KetosValue::List(_))
                || matches!(guest_persistent(&args[0]), Some(Value::Array(_))),
        ))
    });
    install_guest_function(interpreter, "string?", |_, args| {
        expect_arity(args, 1, "string?")?;
        Ok(KetosValue::Bool(matches!(args[0], KetosValue::String(_))))
    });
    install_guest_function(interpreter, "number?", |_, args| {
        expect_arity(args, 1, "number?")?;
        Ok(KetosValue::Bool(matches!(
            args[0],
            KetosValue::Integer(_) | KetosValue::Float(_)
        )))
    });
    install_guest_function(interpreter, "boolean?", |_, args| {
        expect_arity(args, 1, "boolean?")?;
        Ok(KetosValue::Bool(matches!(args[0], KetosValue::Bool(_))))
    });
    install_guest_function(interpreter, "null?", |_, args| {
        expect_arity(args, 1, "null?")?;
        Ok(KetosValue::Bool(matches!(
            guest_persistent(&args[0]),
            Some(Value::Null)
        )))
    });
    install_guest_function(interpreter, "map-get", |_, args| map_get(args));
    install_guest_function(interpreter, "list-get", |_, args| {
        expect_arity(args, 2, "list-get")?;
        let list = match guest_persistent(&args[0]) {
            Some(Value::Array(values)) => values,
            _ => return guest_error("list-get expects a structured list"),
        };
        let index = match &args[1] {
            KetosValue::Integer(value) => value
                .to_usize()
                .ok_or_else(|| guest_failure("list-get expects a non-negative index"))?,
            _ => return guest_error("list-get expects an integer index"),
        };
        list.get(index)
            .ok_or_else(|| guest_failure("list index is out of bounds"))
            .and_then(|value| persistent_to_ketos(value).map_err(guest_from_svit))
    });
    let safe_get_limits = limits.clone();
    install_guest_function(interpreter, "map-get-safe", move |_, args| {
        let result = map_get(args).and_then(|value| {
            persistent_from_ketos(&value, &safe_get_limits).map_err(guest_from_svit)
        });
        safe_result(recoverable_result(result)?, &safe_get_limits)
    });
    install_guest_function(interpreter, "map-has?", |_, args| {
        expect_arity(args, 2, "map-has?")?;
        let map = guest_map(&args[0], "map-has?")?;
        let key = guest_string(&args[1], "map-has?")?;
        Ok(KetosValue::Bool(map.contains_key(&key)))
    });
    let set_limits = limits.clone();
    install_guest_function(interpreter, "map-set", move |_, args| {
        expect_arity(args, 3, "map-set")?;
        let mut map = guest_map(&args[0], "map-set")?.clone();
        let key = guest_string(&args[1], "map-set")?;
        let value = persistent_from_ketos(&args[2], &set_limits).map_err(guest_from_svit)?;
        map.insert(key, value);
        persistent_to_ketos(&Value::Map(map)).map_err(guest_from_svit)
    });
    let result_ok_limits = limits.clone();
    install_guest_function(interpreter, "result-ok", move |_, args| {
        expect_arity(args, 1, "result-ok")?;
        result_to_ketos(true, "value", args[0].clone(), &result_ok_limits)
    });
    let result_error_limits = limits.clone();
    install_guest_function(interpreter, "result-error", move |_, args| {
        expect_arity(args, 1, "result-error")?;
        let message = guest_string(&args[0], "result-error")?;
        result_to_ketos(
            false,
            "error",
            KetosValue::from(message),
            &result_error_limits,
        )
    });
    install_guest_function(interpreter, "result-ok?", |_, args| {
        expect_arity(args, 1, "result-ok?")?;
        Ok(KetosValue::Bool(result_parts(&args[0])?.0))
    });
    install_guest_function(interpreter, "result-value", |_, args| {
        expect_arity(args, 1, "result-value")?;
        let (ok, payload) = result_parts(&args[0])?;
        if ok {
            persistent_to_ketos(payload).map_err(guest_from_svit)
        } else {
            guest_error("result-value expects a successful result")
        }
    });
    install_guest_function(interpreter, "result-error-message", |_, args| {
        expect_arity(args, 1, "result-error-message")?;
        let (ok, payload) = result_parts(&args[0])?;
        if ok {
            guest_error("result-error-message expects a failed result")
        } else {
            persistent_to_ketos(payload).map_err(guest_from_svit)
        }
    });
    let result_limits = limits.clone();
    install_guest_function(interpreter, "result-map", move |ctx, args| {
        result_transform(ctx, args, &result_limits, ResultTransform::Map)
    });
    let and_then_limits = limits.clone();
    install_guest_function(interpreter, "result-and-then", move |ctx, args| {
        result_transform(ctx, args, &and_then_limits, ResultTransform::AndThen)
    });
    let or_else_limits = limits.clone();
    install_guest_function(interpreter, "result-or-else", move |ctx, args| {
        result_transform(ctx, args, &or_else_limits, ResultTransform::OrElse)
    });
    let path_limits = limits.clone();
    install_guest_function(interpreter, "value-at", move |_, args| {
        expect_arity(args, 2, "value-at")?;
        let value = persistent_from_ketos(&args[0], &path_limits).map_err(guest_from_svit)?;
        let path = path_components(&args[1])?;
        persistent_to_ketos(value_at_path(&value, &path)?).map_err(guest_from_svit)
    });
    let safe_path_limits = limits.clone();
    install_guest_function(interpreter, "value-at-safe", move |_, args| {
        expect_arity(args, 2, "value-at-safe")?;
        let result = persistent_from_ketos(&args[0], &safe_path_limits)
            .map_err(guest_from_svit)
            .and_then(|value| {
                path_components(&args[1]).and_then(|path| value_at_path(&value, &path).cloned())
            });
        safe_result(recoverable_result(result)?, &safe_path_limits)
    });
    let has_path_limits = limits.clone();
    install_guest_function(interpreter, "value-has-path?", move |_, args| {
        expect_arity(args, 2, "value-has-path?")?;
        let value = persistent_from_ketos(&args[0], &has_path_limits).map_err(guest_from_svit)?;
        let path = path_components(&args[1])?;
        Ok(KetosValue::Bool(value_at_path(&value, &path).is_ok()))
    });
    // THREAT[TM-ESC-005]: Dispatch authority is the explicit ephemeral table;
    // names cannot resolve arbitrary guest or host functions.
    install_guest_function(interpreter, "dispatch-table", |_, args| {
        if args.len() % 2 != 0 {
            return guest_error("dispatch-table expects name/function pairs");
        }
        let mut entries = Vec::with_capacity(args.len() / 2);
        for pair in args.chunks_exact(2) {
            let name = guest_string(&pair[0], "dispatch-table")?;
            if entries.iter().any(|(existing, _)| existing == &name) {
                return guest_error("dispatch-table handler names must be unique");
            }
            if !matches!(pair[1], KetosValue::Function(_) | KetosValue::Lambda(_)) {
                return guest_error("dispatch-table values must be functions");
            }
            entries.push((name, pair[1].clone()));
        }
        Ok(KetosValue::new_foreign(GuestDispatchTable(entries)))
    });
    let dispatch_limits = limits.clone();
    install_guest_function(interpreter, "dispatch", move |ctx, args| {
        dispatch_call(ctx, args, &dispatch_limits, false)
    });
    let safe_dispatch_limits = limits.clone();
    install_guest_function(interpreter, "dispatch-safe", move |ctx, args| {
        dispatch_call(ctx, args, &safe_dispatch_limits, true)
    });
    let call_limits = limits.clone();
    install_guest_function(interpreter, "safe-call", move |ctx, args| {
        let Some((function, call_args)) = args.split_first() else {
            return guest_error("safe-call expects at least one argument");
        };
        let result = ketos::exec::call_function(ctx, function.clone(), call_args.to_vec())
            .and_then(|value| persistent_from_ketos(&value, &call_limits).map_err(guest_from_svit));
        safe_result(recoverable_result(result)?, &call_limits)
    });
}

fn json_parse_value(text: &str, limits: &Limits) -> std::result::Result<KetosValue, KetosError> {
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|_| guest_failure("invalid JSON"))?;
    let value = Value::from_json(json).map_err(guest_from_svit)?;
    value.validate(limits, false).map_err(guest_from_svit)?;
    persistent_to_ketos(&value).map_err(guest_from_svit)
}

fn guest_persistent(value: &KetosValue) -> Option<&Value> {
    match value {
        KetosValue::Foreign(value) => value
            .downcast_ref::<GuestPersistent>()
            .map(|value| &value.0),
        _ => None,
    }
}

fn guest_map<'a>(
    value: &'a KetosValue,
    function: &str,
) -> std::result::Result<&'a BTreeMap<String, Value>, KetosError> {
    match guest_persistent(value) {
        Some(Value::Map(map)) => Ok(map),
        _ => guest_error(format!("{function} expects a map")),
    }
}

fn map_get(args: &[KetosValue]) -> std::result::Result<KetosValue, KetosError> {
    expect_arity(args, 2, "map-get")?;
    let map = guest_map(&args[0], "map-get")?;
    let key = guest_string(&args[1], "map-get")?;
    map.get(&key)
        .ok_or_else(|| guest_failure("map key is absent"))
        .and_then(|value| persistent_to_ketos(value).map_err(guest_from_svit))
}

#[derive(Clone, Debug)]
struct GuestDispatchTable(Vec<(String, KetosValue)>);

impl ForeignValue for GuestDispatchTable {
    fn type_name(&self) -> &'static str {
        "svit-dispatch-table"
    }

    fn size(&self) -> usize {
        self.0.len().max(1)
    }
}

#[derive(Clone, Copy)]
enum ResultTransform {
    Map,
    AndThen,
    OrElse,
}

#[derive(Clone, Debug)]
enum PathComponent {
    Key(String),
    Index(usize),
}

fn result_to_ketos(
    ok: bool,
    payload_name: &str,
    payload: KetosValue,
    limits: &Limits,
) -> std::result::Result<KetosValue, KetosError> {
    let payload = persistent_from_ketos(&payload, limits).map_err(guest_from_svit)?;
    let result = Value::Map(BTreeMap::from([
        ("ok".into(), Value::Bool(ok)),
        (payload_name.into(), payload),
    ]));
    result.validate(limits, false).map_err(guest_from_svit)?;
    persistent_to_ketos(&result).map_err(guest_from_svit)
}

fn result_parts(value: &KetosValue) -> std::result::Result<(bool, &Value), KetosError> {
    let map = guest_map(value, "result helper")?;
    let ok = match map.get("ok") {
        Some(Value::Bool(ok)) => *ok,
        _ => return guest_error("result map requires a Boolean ok field"),
    };
    let payload = map
        .get(if ok { "value" } else { "error" })
        .ok_or_else(|| guest_failure("result map is missing its payload"))?;
    Ok((ok, payload))
}

fn result_transform(
    ctx: &KetosContext,
    args: &mut [KetosValue],
    limits: &Limits,
    transform: ResultTransform,
) -> std::result::Result<KetosValue, KetosError> {
    expect_arity(args, 2, "result transform")?;
    let (ok, payload) = result_parts(&args[1])?;
    let should_call = match transform {
        ResultTransform::Map | ResultTransform::AndThen => ok,
        ResultTransform::OrElse => !ok,
    };
    if !should_call {
        return Ok(args[1].clone());
    }
    let argument = persistent_to_ketos(payload).map_err(guest_from_svit)?;
    let value = ketos::exec::call_function(ctx, args[0].clone(), vec![argument])?;
    match transform {
        ResultTransform::Map => {
            let value = persistent_from_ketos(&value, limits).map_err(guest_from_svit)?;
            safe_result(Ok(value), limits)
        }
        ResultTransform::AndThen | ResultTransform::OrElse => {
            result_parts(&value)?;
            Ok(value)
        }
    }
}

fn path_components(value: &KetosValue) -> std::result::Result<Vec<PathComponent>, KetosError> {
    let KetosValue::List(values) = value else {
        return guest_error("value path must be a Lisp list");
    };
    values
        .iter()
        .map(|component| match component {
            KetosValue::String(key) => Ok(PathComponent::Key(key.to_string())),
            KetosValue::Integer(index) => index
                .to_usize()
                .map(PathComponent::Index)
                .ok_or_else(|| guest_failure("path index must be non-negative")),
            _ => guest_error("path components must be strings or non-negative integers"),
        })
        .collect()
}

fn value_at_path<'a>(
    mut value: &'a Value,
    path: &[PathComponent],
) -> std::result::Result<&'a Value, KetosError> {
    for component in path {
        value = match (value, component) {
            (Value::Map(map), PathComponent::Key(key)) => map
                .get(key)
                .ok_or_else(|| guest_failure("value path key is absent"))?,
            (Value::Array(values), PathComponent::Index(index)) => values
                .get(*index)
                .ok_or_else(|| guest_failure("value path index is out of bounds"))?,
            _ => return guest_error("value path component does not match its container"),
        };
    }
    Ok(value)
}

fn dispatch_call(
    ctx: &KetosContext,
    args: &mut [KetosValue],
    limits: &Limits,
    safe: bool,
) -> std::result::Result<KetosValue, KetosError> {
    expect_arity(args, 3, if safe { "dispatch-safe" } else { "dispatch" })?;
    let table = match &args[0] {
        KetosValue::Foreign(value) => value.downcast_ref::<GuestDispatchTable>(),
        _ => None,
    }
    .ok_or_else(|| guest_failure("dispatch expects a dispatch table"))?;
    let name = guest_string(&args[1], "dispatch")?;
    let result = table
        .0
        .iter()
        .find(|(candidate, _)| candidate == &name)
        .ok_or_else(|| guest_failure("dispatch handler is not registered"))
        .and_then(|(_, function)| {
            ketos::exec::call_function(ctx, function.clone(), vec![args[2].clone()])
        })
        .and_then(|value| persistent_from_ketos(&value, limits).map_err(guest_from_svit));
    if safe {
        safe_result(recoverable_result(result)?, limits)
    } else {
        let value = result?;
        persistent_to_ketos(&value).map_err(guest_from_svit)
    }
}

fn recoverable_result(
    result: std::result::Result<Value, KetosError>,
) -> std::result::Result<std::result::Result<Value, KetosError>, KetosError> {
    match result {
        Err(error) if is_hard_guest_error(&error) => Err(error),
        result => Ok(result),
    }
}

fn is_hard_guest_error(error: &KetosError) -> bool {
    match error {
        KetosError::RestrictError(_) => true,
        KetosError::Custom(error) => matches!(
            error.downcast_ref::<GuestFailure>(),
            Some(GuestFailure::PortPending | GuestFailure::Execution | GuestFailure::Resource(_))
        ),
        _ => false,
    }
}

fn safe_result(
    result: std::result::Result<Value, KetosError>,
    limits: &Limits,
) -> std::result::Result<KetosValue, KetosError> {
    let value = match result {
        Ok(value) => Value::Map(BTreeMap::from([
            ("ok".into(), Value::Bool(true)),
            ("value".into(), value),
        ])),
        Err(error) => Value::Map(BTreeMap::from([
            ("error".into(), Value::String(sanitize_diagnostic(error))),
            ("ok".into(), Value::Bool(false)),
        ])),
    };
    value.validate(limits, false).map_err(guest_from_svit)?;
    persistent_to_ketos(&value).map_err(guest_from_svit)
}

fn guest_failure(message: impl Into<String>) -> KetosError {
    KetosError::custom(GuestFailure::Script(message.into()))
}

fn install_guest_function<F>(interpreter: &KetosInterpreter, name: &str, function: F)
where
    F: Any + Fn(&KetosContext, &mut [KetosValue]) -> std::result::Result<KetosValue, KetosError>,
{
    interpreter
        .scope()
        .add_value_with_name(name, |name| KetosValue::new_foreign_fn(name, function));
}

/// Searches the activation's process view without crossing the host boundary.
fn search_runtime(
    state: &RuntimeState,
    path: &str,
    pattern: &str,
    limits: &Limits,
) -> Result<Value> {
    if pattern.len() > MAX_SEARCH_PATTERN_BYTES {
        return Err(Error::Script("search pattern limit exceeded".into()));
    }
    let regex = RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .build()
        .map_err(|_| Error::Script("invalid search pattern".into()))?;
    // THREAT[TM-DOS-008] THREAT[TM-DOS-010]: Search uses Rust's linear-time
    // regex engine and bounds pattern size, output, results, and lazy mount
    // traversal. It is a runtime function, never a host port.
    let mut matches = Vec::new();
    let mut output_bytes = 0;
    let complete = if path == "/mounts" || path.starts_with("/mounts/") {
        let mut budget = MAX_SEARCH_MOUNT_NODES;
        collect_mount_search_matches(
            state,
            path,
            &regex,
            &mut matches,
            &mut output_bytes,
            &mut budget,
        )?
    } else {
        let view = state.view()?;
        let value = read_value_path(&view, path)
            .map_err(|_| Error::Script("invalid search path".into()))?
            .ok_or_else(|| Error::InvalidPath(path.into()))?;
        collect_search_matches(path, value, &regex, &mut matches, &mut output_bytes);
        matches.len() < MAX_SEARCH_RESULTS && output_bytes < MAX_SEARCH_OUTPUT_BYTES
    };
    let output = Value::Map(BTreeMap::from([
        ("matches".into(), Value::Array(matches)),
        ("truncated".into(), Value::Bool(!complete)),
    ]));
    output.validate(limits, false)?;
    Ok(output)
}

fn collect_mount_search_matches(
    state: &RuntimeState,
    path: &str,
    regex: &Regex,
    matches: &mut Vec<Value>,
    output_bytes: &mut usize,
    budget: &mut usize,
) -> Result<bool> {
    if matches.len() >= MAX_SEARCH_RESULTS {
        return Ok(false);
    }
    let Some(remaining) = budget.checked_sub(1) else {
        return Ok(false);
    };
    *budget = remaining;
    let Some((mount, mount_path)) = mount_target(path)? else {
        return Err(Error::InvalidPath(path.into()));
    };
    let facts = match state.mount_view().stat(&mount, &mount_path) {
        Ok(Some(facts)) => facts,
        // A node may disappear between listing and stat because the source is
        // external. It is absent from this search rather than a process error.
        Ok(None) | Err(_) => return Ok(true),
    };
    let directory = matches!(
        facts,
        Value::Map(ref values) if values.get("kind") == Some(&Value::String("directory".into()))
    );
    if !directory {
        if let Ok(Some(value)) = state.mount_view().read(&mount, &mount_path) {
            collect_search_matches(path, &value, regex, matches, output_bytes);
        }
        return Ok(matches.len() < MAX_SEARCH_RESULTS && *output_bytes < MAX_SEARCH_OUTPUT_BYTES);
    }
    let children = match state.mount_view().discover(&mount, &mount_path) {
        Ok(children) => children,
        Err(_) => return Ok(true),
    };
    for child in children {
        let child_path = format!("{path}/{child}");
        if !collect_mount_search_matches(state, &child_path, regex, matches, output_bytes, budget)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn collect_search_matches(
    path: &str,
    value: &Value,
    regex: &Regex,
    matches: &mut Vec<Value>,
    output_bytes: &mut usize,
) {
    if matches.len() >= MAX_SEARCH_RESULTS {
        return;
    }
    match value {
        Value::String(text) => {
            for (line_index, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    let Some(next_size) = output_bytes.checked_add(path.len() + line.len()) else {
                        return;
                    };
                    if next_size > MAX_SEARCH_OUTPUT_BYTES {
                        return;
                    }
                    matches.push(Value::Map(BTreeMap::from([
                        ("path".into(), Value::String(path.into())),
                        ("line".into(), Value::Integer((line_index + 1) as i64)),
                        ("text".into(), Value::String(line.into())),
                    ])));
                    *output_bytes = next_size;
                    if matches.len() >= MAX_SEARCH_RESULTS {
                        return;
                    }
                }
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                collect_search_matches(
                    &format!("{path}/{index}"),
                    value,
                    regex,
                    matches,
                    output_bytes,
                );
            }
        }
        Value::Map(values) => {
            for (name, value) in values {
                collect_search_matches(
                    &format!("{path}/{name}"),
                    value,
                    regex,
                    matches,
                    output_bytes,
                );
            }
        }
        Value::Null | Value::Bool(_) | Value::Integer(_) | Value::Number(_) | Value::Script(_) => {}
    }
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

fn jq_input_from_ketos(value: &KetosValue, limits: &Limits) -> Result<Value> {
    if let KetosValue::Foreign(value) = value
        && let Some(value) = value.downcast_ref::<GuestPersistent>()
    {
        return Ok(value.0.clone());
    }
    persistent_from_ketos(value, limits)
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
            Some(GuestFailure::PortPending) => Error::Script("port execution suspended".into()),
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
    // The root hash is the root of the same content-hash tree every node
    // publishes, so a client that holds a subtree hash holds that subtree.
    Ok(root.content_hash())
}

/// Returns `path` and every ancestor above it, root last.
fn ancestors_inclusive(path: &str) -> Vec<String> {
    let mut paths = vec![path.to_owned()];
    let mut current = path;
    while let Some((parent, _)) = current.rsplit_once('/') {
        let parent = if parent.is_empty() { "/" } else { parent };
        paths.push(parent.to_owned());
        if parent == "/" {
            break;
        }
        current = parent;
    }
    paths
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
    use std::path::PathBuf;
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
    // THREAT[TM-DOS-008] THREAT[TM-DOS-010]
    fn standard_library_search_walks_a_mount_one_node_at_a_time() {
        let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/mount-data");
        let mut process = Process::builder("svit://local/test/search-mount")
            .unwrap()
            .mount("files", Mount::folder(folder).unwrap())
            .build()
            .unwrap();
        write_script(
            &mut process,
            "find-checklist",
            r#"(define (main input) (search "/mounts/files" "just check"))"#,
        )
        .unwrap();

        let output = process
            .exec("/lib/find-checklist", Value::Null)
            .unwrap()
            .output
            .to_json();
        assert_eq!(output["truncated"], false);
        assert_eq!(
            output["matches"][0]["path"],
            "/mounts/files/notes/checklist.md"
        );
        assert_eq!(output["matches"][0]["line"], 2);
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
