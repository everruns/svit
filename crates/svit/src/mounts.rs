//! Virtual mounts: external resources resolved lazily below `/mounts`.
//!
//! A mount is not a copy. The committed process root stores only a
//! [`MountDescriptor`] — kind, disclosed source, locality, and access — while
//! node facts and leaf content are resolved through a host-attached
//! [`MountProvider`] at the moment a path is read, discovered, stated, or
//! written.
//!
//! Providers are host-owned typed capabilities. They are never serialized, so
//! restoring a snapshot restores mount identity without restoring authority:
//! an unattached mount reads its descriptor and fails closed everywhere else.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

#[cfg(feature = "turso-mount")]
use turso::{Connection, IntoParams};

use crate::{Error, Limits, Result, Value};

const MAX_MOUNT_NAME_BYTES: usize = 64;
const MAX_MOUNT_SEGMENTS: usize = 64;
const MAX_MOUNT_SEGMENT_BYTES: usize = 255;

/// Where a mount's data lives, and therefore what touching it costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Locality {
    /// Already materialized in host memory. Reads are allocation-bound.
    Cache,
    /// Reachable through the host machine. Reads are disk-bound.
    Local,
    /// Reachable only across a network. Reads are latency-bound and fallible.
    Remote,
}

impl Locality {
    /// Returns the guest-visible discriminant.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cache => "cache",
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "cache" => Ok(Self::Cache),
            "local" => Ok(Self::Local),
            "remote" => Ok(Self::Remote),
            other => Err(Error::InvalidMount(format!("unknown locality: {other}"))),
        }
    }
}

impl fmt::Display for Locality {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The operations a host granted on one mount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountAccess {
    /// Reads, listings, and facts only.
    Read,
    /// Writes and removals only; content reads are denied.
    Write,
    /// Both reads and writes.
    ReadWrite,
}

impl MountAccess {
    /// Returns the guest-visible discriminant.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::ReadWrite => "read-write",
        }
    }

    /// Reports whether content reads are granted.
    pub fn allows_read(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    /// Reports whether writes and removals are granted.
    pub fn allows_write(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "read-write" => Ok(Self::ReadWrite),
            other => Err(Error::InvalidMount(format!(
                "unknown mount access: {other}"
            ))),
        }
    }
}

impl fmt::Display for MountAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Whether a mount node has children or content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountNodeKind {
    /// A node that can be listed with `discover`.
    Directory,
    /// A node that can be read as one persistent value.
    Leaf,
}

impl MountNodeKind {
    /// Returns the guest-visible discriminant.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::Leaf => "leaf",
        }
    }
}

/// Stable, serializable identity of one mount.
///
/// This is the only mount data that crosses a snapshot boundary. It contains
/// no handle, connection, or credential.
#[derive(Clone, Debug, PartialEq)]
pub struct MountDescriptor {
    kind: String,
    source: String,
    locality: Locality,
    access: MountAccess,
}

impl MountDescriptor {
    /// Describes a mount by provider kind and host-disclosed source.
    ///
    /// `source` is deliberately host-chosen. Providers disclose an origin the
    /// host is willing to show guests; runtime diagnostics never add one.
    pub fn new(kind: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            source: source.into(),
            locality: Locality::Local,
            access: MountAccess::Read,
        }
    }

    /// Sets the access cost class.
    pub fn locality(mut self, locality: Locality) -> Self {
        self.locality = locality;
        self
    }

    /// Sets the granted operations.
    pub fn access(mut self, access: MountAccess) -> Self {
        self.access = access;
        self
    }

    /// Returns the provider kind.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Returns the host-disclosed origin.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns the access cost class.
    pub fn locality_class(&self) -> Locality {
        self.locality
    }

    /// Returns the granted operations.
    pub fn granted_access(&self) -> MountAccess {
        self.access
    }

    pub(crate) fn to_value(&self) -> Value {
        Value::Map(BTreeMap::from([
            ("access".into(), Value::from(self.access.as_str())),
            ("kind".into(), Value::from(self.kind.clone())),
            ("locality".into(), Value::from(self.locality.as_str())),
            ("source".into(), Value::from(self.source.clone())),
        ]))
    }

    pub(crate) fn from_value(value: &Value) -> Result<Self> {
        let Value::Map(fields) = value else {
            return Err(Error::InvalidMount("mount record is not a map".into()));
        };
        if fields.keys().map(String::as_str).collect::<BTreeSet<_>>()
            != BTreeSet::from(["access", "kind", "locality", "source"])
        {
            return Err(Error::InvalidMount(
                "mount record must contain exactly access, kind, locality, and source".into(),
            ));
        }
        let text = |name: &str| match fields.get(name) {
            Some(Value::String(value)) => Ok(value.clone()),
            _ => Err(Error::InvalidMount(format!("mount {name} is not text"))),
        };
        let kind = text("kind")?;
        validate_mount_kind(&kind)?;
        Ok(Self {
            kind,
            source: text("source")?,
            locality: Locality::parse(&text("locality")?)?,
            access: MountAccess::parse(&text("access")?)?,
        })
    }
}

/// Facts about one node inside a mount.
///
/// Facts are ordinary persistent values. They are the guest-visible answer to
/// "what is this and what does reading it cost", never a handle to the source.
#[derive(Clone, Debug, PartialEq)]
pub struct MountNode {
    kind: MountNodeKind,
    facts: BTreeMap<String, Value>,
}

impl MountNode {
    /// Describes a listable node.
    pub fn directory() -> Self {
        Self {
            kind: MountNodeKind::Directory,
            facts: BTreeMap::new(),
        }
    }

    /// Describes a readable node.
    pub fn leaf() -> Self {
        Self {
            kind: MountNodeKind::Leaf,
            facts: BTreeMap::new(),
        }
    }

    /// Adds one provider-supplied fact.
    pub fn fact(mut self, name: impl Into<String>, value: Value) -> Self {
        self.facts.insert(name.into(), value);
        self
    }

    /// Returns the node kind.
    pub fn node_kind(&self) -> MountNodeKind {
        self.kind
    }

    /// Returns the provider-supplied facts.
    pub fn facts(&self) -> &BTreeMap<String, Value> {
        &self.facts
    }
}

/// A validated path relative to one mount root.
///
/// Segments are non-empty, contain no separator, and never include `.` or
/// `..`, so a mount path cannot address anything above its own root.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MountPath {
    segments: Vec<String>,
}

impl MountPath {
    /// Returns the mount root path.
    pub fn root() -> Self {
        Self::default()
    }

    /// Parses a slash-separated path relative to the mount root.
    pub fn parse(path: &str) -> Result<Self> {
        let trimmed = path.trim_start_matches('/');
        if trimmed.is_empty() {
            return Ok(Self::root());
        }
        // THREAT[TM-CAP-007]: Reject traversal, empty, and oversized segments
        // before any provider observes the request.
        let segments = trimmed.split('/').map(str::to_owned).collect::<Vec<_>>();
        if segments.len() > MAX_MOUNT_SEGMENTS {
            return Err(Error::InvalidPath("mount path is too deep".into()));
        }
        for segment in &segments {
            if segment.is_empty()
                || segment.len() > MAX_MOUNT_SEGMENT_BYTES
                || matches!(segment.as_str(), "." | "..")
                || segment.contains('\\')
                || segment.contains('\0')
            {
                return Err(Error::InvalidPath(format!(
                    "invalid mount path segment: {segment}"
                )));
            }
        }
        Ok(Self { segments })
    }

    /// Returns the validated segments.
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// Reports whether this path addresses the mount root.
    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    /// Returns the mount-relative display form, always starting with `/`.
    pub fn display(&self) -> String {
        format!("/{}", self.segments.join("/"))
    }
}

/// A host-owned resolver for one mount.
///
/// Implementations are trusted native code. They receive validated,
/// root-relative paths and return persistent values, never handles.
pub trait MountProvider: Send + Sync {
    /// Returns the stable identity committed under `/mounts/<name>`.
    fn descriptor(&self) -> MountDescriptor;

    /// Returns facts about one node, or `None` when it does not exist.
    fn stat(&self, path: &MountPath, limits: &Limits) -> Result<Option<MountNode>>;

    /// Returns the deterministic child names of a directory node.
    fn list(&self, path: &MountPath, limits: &Limits) -> Result<Vec<String>>;

    /// Returns the content of a leaf node.
    fn read(&self, path: &MountPath, limits: &Limits) -> Result<Option<Value>>;

    /// Replaces the content of a leaf node.
    ///
    /// The default denies the write. Read-only providers need no override.
    fn write(&self, path: &MountPath, value: Value, limits: &Limits) -> Result<()> {
        let _ = (path, value, limits);
        Err(Error::MountDenied("mount does not grant writes".into()))
    }

    /// Removes one node.
    ///
    /// The default denies the removal.
    fn remove(&self, path: &MountPath, limits: &Limits) -> Result<()> {
        let _ = (path, limits);
        Err(Error::MountDenied("mount does not grant removals".into()))
    }
}

/// A host-attached mount handle.
///
/// Cloning shares one provider; attaching the same [`Mount`] to a fork shares
/// the external resource deliberately, exactly as the host chose.
#[derive(Clone)]
pub struct Mount {
    provider: Arc<dyn MountProvider>,
}

impl Mount {
    /// Wraps a host-implemented provider.
    pub fn new(provider: impl MountProvider + 'static) -> Self {
        Self {
            provider: Arc::new(provider),
        }
    }

    /// Mounts a real directory for reading, resolved lazily per node.
    pub fn folder(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::new(FolderMount::new(path, MountAccess::Read)?))
    }

    /// Mounts a real directory for reading and writing, resolved lazily.
    pub fn writable_folder(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::new(FolderMount::new(path, MountAccess::ReadWrite)?))
    }

    /// Mounts an already materialized value with `cache` locality.
    pub fn value(source: impl Into<String>, value: Value, limits: &Limits) -> Result<Self> {
        Ok(Self::new(ValueMount::new(
            "value",
            source,
            value,
            MountAccess::Read,
            limits,
        )?))
    }

    /// Mounts a writable in-memory value with `cache` locality.
    pub fn writable_value(
        source: impl Into<String>,
        value: Value,
        limits: &Limits,
    ) -> Result<Self> {
        Ok(Self::new(ValueMount::new(
            "value",
            source,
            value,
            MountAccess::ReadWrite,
            limits,
        )?))
    }

    /// Materializes one host-selected Turso query as a `cache` mount.
    ///
    /// Rows are read once. Svit does not hold the connection, so the mount
    /// reports `cache` locality rather than claiming a live remote view.
    #[cfg(feature = "turso-mount")]
    pub async fn turso_query(
        connection: &Connection,
        sql: impl AsRef<str>,
        params: impl IntoParams,
        limits: &Limits,
    ) -> Result<Self> {
        let sql = sql.as_ref().to_owned();
        let rows = turso_rows(connection, &sql, params, limits).await?;
        Ok(Self::new(ValueMount::new(
            "turso-query",
            sql,
            rows,
            MountAccess::Read,
            limits,
        )?))
    }

    pub(crate) fn provider(&self) -> &Arc<dyn MountProvider> {
        &self.provider
    }
}

impl fmt::Debug for Mount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let descriptor = self.provider.descriptor();
        formatter
            .debug_struct("Mount")
            .field("kind", &descriptor.kind)
            .field("locality", &descriptor.locality)
            .field("access", &descriptor.access)
            .finish_non_exhaustive()
    }
}

/// Host-attached providers for one process.
///
/// The registry is runtime state. It is intentionally absent from snapshots
/// so restoring state never restores host authority.
#[derive(Clone, Default)]
pub(crate) struct MountRegistry {
    mounts: BTreeMap<String, Mount>,
}

impl MountRegistry {
    pub(crate) fn attach(&mut self, name: String, mount: Mount) {
        self.mounts.insert(name, mount);
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Mount> {
        self.mounts.get(name)
    }

    pub(crate) fn descriptors(&self) -> BTreeMap<String, MountDescriptor> {
        self.mounts
            .iter()
            .map(|(name, mount)| (name.clone(), mount.provider().descriptor()))
            .collect()
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &String> {
        self.mounts.keys()
    }
}

/// Resolution of `/mounts/<name>/...` against committed identity and attached
/// providers.
///
/// Committed descriptors answer "what is mounted here". Attached providers
/// answer "what is there now". Separating them is what lets a restored
/// snapshot keep the first without regaining the second.
pub(crate) struct MountView<'a> {
    registry: &'a MountRegistry,
    committed: &'a Value,
    limits: &'a Limits,
}

impl<'a> MountView<'a> {
    pub(crate) fn new(
        registry: &'a MountRegistry,
        committed: &'a Value,
        limits: &'a Limits,
    ) -> Self {
        Self {
            registry,
            committed,
            limits,
        }
    }

    fn descriptor(&self, name: &str) -> Result<MountDescriptor> {
        let Value::Map(mounts) = self.committed else {
            return Err(Error::InvalidSnapshot("/mounts is not a map".into()));
        };
        let record = mounts
            .get(name)
            .ok_or_else(|| Error::InvalidPath(format!("/mounts/{name}")))?;
        MountDescriptor::from_value(record)
    }

    /// Reads mount content, or node facts for the root and directory nodes.
    pub(crate) fn read(&self, name: &str, path: &MountPath) -> Result<Option<Value>> {
        let descriptor = self.descriptor(name)?;
        let Some(mount) = self.registry.get(name) else {
            // Mount identity survives a snapshot; mount authority does not.
            return if path.is_root() {
                Ok(Some(unattached_value(name, &descriptor)))
            } else {
                Err(Error::MountUnavailable(name.into()))
            };
        };
        if path.is_root() {
            let node = mount.provider().stat(path, self.limits)?;
            return Ok(Some(node_value(name, &descriptor, path, node.as_ref())));
        }
        // THREAT[TM-CAP-001]: Content below the root requires the read grant
        // the host recorded in the descriptor.
        if !descriptor.granted_access().allows_read() {
            return Err(Error::MountDenied(format!("/mounts/{name} is write-only")));
        }
        // A node reads as its content when the provider can produce one, and
        // as its facts otherwise. A folder directory has no content; a cached
        // value node does, and materializing it costs nothing extra.
        if let Some(value) = mount.provider().read(path, self.limits)? {
            return Ok(Some(value));
        }
        Ok(mount
            .provider()
            .stat(path, self.limits)?
            .map(|node| node_value(name, &descriptor, path, Some(&node))))
    }

    /// Returns the facts record for one mount node.
    pub(crate) fn stat(&self, name: &str, path: &MountPath) -> Result<Option<Value>> {
        let descriptor = self.descriptor(name)?;
        let Some(mount) = self.registry.get(name) else {
            return if path.is_root() {
                Ok(Some(unattached_value(name, &descriptor)))
            } else {
                Err(Error::MountUnavailable(name.into()))
            };
        };
        Ok(mount
            .provider()
            .stat(path, self.limits)?
            .map(|node| node_value(name, &descriptor, path, Some(&node)))
            .or_else(|| {
                path.is_root()
                    .then(|| node_value(name, &descriptor, path, None))
            }))
    }

    /// Lists the children of one mount directory node.
    pub(crate) fn discover(&self, name: &str, path: &MountPath) -> Result<Vec<String>> {
        let descriptor = self.descriptor(name)?;
        if !descriptor.granted_access().allows_read() {
            return Err(Error::MountDenied(format!("/mounts/{name} is write-only")));
        }
        let mount = self
            .registry
            .get(name)
            .ok_or_else(|| Error::MountUnavailable(name.into()))?;
        mount.provider().list(path, self.limits)
    }

    /// Reports whether the named mount grants writes, failing on unknown or
    /// unattached mounts so a guest learns the outcome before the commit point.
    pub(crate) fn grants_write(&self, name: &str) -> Result<bool> {
        let descriptor = self.descriptor(name)?;
        if self.registry.get(name).is_none() {
            return Err(Error::MountUnavailable(name.into()));
        }
        Ok(descriptor.granted_access().allows_write())
    }

    /// Writes one mount leaf through the host-granted access.
    pub(crate) fn write(&self, name: &str, path: &MountPath, value: Value) -> Result<()> {
        let descriptor = self.descriptor(name)?;
        if !descriptor.granted_access().allows_write() {
            return Err(Error::MountDenied(format!("/mounts/{name} is read-only")));
        }
        let mount = self
            .registry
            .get(name)
            .ok_or_else(|| Error::MountUnavailable(name.into()))?;
        mount.provider().write(path, value, self.limits)
    }

    /// Removes one mount node through the host-granted access.
    pub(crate) fn remove(&self, name: &str, path: &MountPath) -> Result<()> {
        let descriptor = self.descriptor(name)?;
        if !descriptor.granted_access().allows_write() {
            return Err(Error::MountDenied(format!("/mounts/{name} is read-only")));
        }
        let mount = self
            .registry
            .get(name)
            .ok_or_else(|| Error::MountUnavailable(name.into()))?;
        mount.provider().remove(path, self.limits)
    }
}

fn node_value(
    name: &str,
    descriptor: &MountDescriptor,
    path: &MountPath,
    node: Option<&MountNode>,
) -> Value {
    let kind = node
        .map(|node| node.node_kind().as_str())
        .unwrap_or("missing");
    let facts = node
        .map(|node| Value::Map(node.facts().clone()))
        .unwrap_or_else(Value::empty_map);
    Value::Map(BTreeMap::from([
        (
            "access".into(),
            Value::from(descriptor.granted_access().as_str()),
        ),
        ("attached".into(), Value::Bool(true)),
        ("facts".into(), facts),
        ("kind".into(), Value::from(kind)),
        (
            "locality".into(),
            Value::from(descriptor.locality_class().as_str()),
        ),
        ("mount".into(), Value::from(name)),
        ("path".into(), Value::from(path.display())),
        ("source".into(), Value::from(descriptor.source())),
    ]))
}

fn unattached_value(name: &str, descriptor: &MountDescriptor) -> Value {
    Value::Map(BTreeMap::from([
        (
            "access".into(),
            Value::from(descriptor.granted_access().as_str()),
        ),
        ("attached".into(), Value::Bool(false)),
        ("facts".into(), Value::empty_map()),
        ("kind".into(), Value::from(descriptor.kind())),
        (
            "locality".into(),
            Value::from(descriptor.locality_class().as_str()),
        ),
        ("mount".into(), Value::from(name)),
        ("path".into(), Value::from("/")),
        ("source".into(), Value::from(descriptor.source())),
    ]))
}

pub(crate) fn validate_mounts(value: &Value, limits: &Limits) -> Result<()> {
    let Value::Map(mounts) = value else {
        return Err(Error::InvalidSnapshot("/mounts is not a map".into()));
    };
    value.validate(limits, false)?;
    for (name, descriptor) in mounts {
        validate_mount_name(name)?;
        MountDescriptor::from_value(descriptor)?;
    }
    Ok(())
}

pub(crate) fn validate_mount_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_MOUNT_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::InvalidMount(format!("invalid mount name: {name}")));
    }
    Ok(())
}

fn validate_mount_kind(kind: &str) -> Result<()> {
    if kind.is_empty()
        || kind.len() > MAX_MOUNT_NAME_BYTES
        || !kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(Error::InvalidMount(format!("invalid mount kind: {kind}")));
    }
    Ok(())
}

/// A real directory resolved one node at a time.
///
/// Nothing below the root is read until a guest or host addresses it.
struct FolderMount {
    root: PathBuf,
    source: String,
    access: MountAccess,
}

impl FolderMount {
    fn new(path: impl AsRef<Path>, access: MountAccess) -> Result<Self> {
        let root = fs::canonicalize(path.as_ref())
            .map_err(|_| Error::InvalidMount("folder mount root is unreadable".into()))?;
        if !fs::symlink_metadata(&root)
            .map_err(|_| Error::InvalidMount("folder mount root is unreadable".into()))?
            .is_dir()
        {
            return Err(Error::InvalidMount(
                "folder mount root is not a directory".into(),
            ));
        }
        let source = root.to_string_lossy().into_owned();
        Ok(Self {
            root,
            source,
            access,
        })
    }

    // THREAT[TM-CAP-003]: Reject links and special files at every segment.
    // THREAT[TM-CAP-007]: Resolve one validated segment at a time and reject
    // any symbolic link or special file, so the configured root is the whole
    // filesystem authority this mount ever exercises.
    fn resolve(&self, path: &MountPath) -> Result<Option<(PathBuf, fs::Metadata)>> {
        let mut current = self.root.clone();
        let mut metadata = fs::symlink_metadata(&current)
            .map_err(|_| Error::InvalidMount("folder mount root is unreadable".into()))?;
        for segment in path.segments() {
            if !metadata.is_dir() {
                return Ok(None);
            }
            current.push(segment);
            metadata = match fs::symlink_metadata(&current) {
                Ok(metadata) => metadata,
                Err(_) => return Ok(None),
            };
            if metadata.file_type().is_symlink() {
                return Err(Error::MountDenied(
                    "symbolic links are not resolved inside a folder mount".into(),
                ));
            }
            if !metadata.is_dir() && !metadata.is_file() {
                return Err(Error::MountDenied(
                    "special files are not readable inside a folder mount".into(),
                ));
            }
        }
        Ok(Some((current, metadata)))
    }

    fn git_facts(&self) -> Option<Value> {
        let git = self.root.join(".git");
        if !fs::symlink_metadata(&git).is_ok_and(|metadata| metadata.is_dir()) {
            return None;
        }
        let head = fs::read_to_string(git.join("HEAD")).ok()?;
        let head = head.trim();
        let mut facts = BTreeMap::new();
        if let Some(reference) = head.strip_prefix("ref: ") {
            let branch = reference
                .rsplit_once('/')
                .map(|(_, branch)| branch)
                .unwrap_or(reference);
            facts.insert("branch".into(), Value::from(branch));
            if let Ok(commit) = fs::read_to_string(git.join(reference)) {
                facts.insert("commit".into(), Value::from(commit.trim()));
            }
        } else {
            facts.insert("branch".into(), Value::Null);
            facts.insert("commit".into(), Value::from(head));
        }
        Some(Value::Map(facts))
    }
}

impl MountProvider for FolderMount {
    fn descriptor(&self) -> MountDescriptor {
        MountDescriptor::new("folder", self.source.clone())
            .locality(Locality::Local)
            .access(self.access)
    }

    fn stat(&self, path: &MountPath, _limits: &Limits) -> Result<Option<MountNode>> {
        let Some((_, metadata)) = self.resolve(path)? else {
            return Ok(None);
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| Value::Integer(elapsed.as_secs() as i64))
            .unwrap_or(Value::Null);
        let node = if metadata.is_dir() {
            let mut node = MountNode::directory()
                .fact("content", Value::from("object"))
                .fact("modified_unix", modified);
            if path.is_root()
                && let Some(git) = self.git_facts()
            {
                node = node.fact("git", git);
            }
            node
        } else {
            MountNode::leaf()
                .fact("bytes", Value::Integer(metadata.len() as i64))
                .fact("modified_unix", modified)
                .fact("content", Value::from("text/plain"))
        };
        Ok(Some(node))
    }

    fn list(&self, path: &MountPath, limits: &Limits) -> Result<Vec<String>> {
        let Some((resolved, metadata)) = self.resolve(path)? else {
            return Err(Error::InvalidPath(path.display()));
        };
        if !metadata.is_dir() {
            return Err(Error::InvalidPath(path.display()));
        }
        // THREAT[TM-DOS-010]: One listing is bounded independently of the
        // committed-value limits, so a hostile directory fails closed.
        let mut names = Vec::new();
        for entry in fs::read_dir(&resolved)
            .map_err(|_| Error::InvalidMount("folder mount directory is unreadable".into()))?
        {
            let entry = entry
                .map_err(|_| Error::InvalidMount("folder mount entry is unreadable".into()))?;
            if names.len() >= limits.max_mount_entries {
                return Err(Error::ResourceLimitExceeded("mount entries"));
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| Error::InvalidMount("folder entry name is not UTF-8".into()))?;
            names.push(name);
        }
        names.sort();
        Ok(names)
    }

    fn read(&self, path: &MountPath, limits: &Limits) -> Result<Option<Value>> {
        let Some((resolved, metadata)) = self.resolve(path)? else {
            return Ok(None);
        };
        if metadata.is_dir() {
            // A directory has no content. The caller falls back to its facts.
            return Ok(None);
        }
        // THREAT[TM-DOS-010]: Read at most one text budget plus one byte so an
        // oversized file is rejected instead of materialized.
        let mut bytes = Vec::new();
        fs::File::open(&resolved)
            .and_then(|file| {
                file.take(limits.max_text_bytes as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .map_err(|_| Error::InvalidMount("folder mount file is unreadable".into()))?;
        if bytes.len() > limits.max_text_bytes {
            return Err(Error::ResourceLimitExceeded("mount leaf bytes"));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| Error::InvalidMount("folder mount file is not UTF-8".into()))?;
        Ok(Some(Value::String(text)))
    }

    fn write(&self, path: &MountPath, value: Value, limits: &Limits) -> Result<()> {
        if !self.access.allows_write() {
            return Err(Error::MountDenied("mount does not grant writes".into()));
        }
        if path.is_root() {
            return Err(Error::InvalidPath("cannot replace a mount root".into()));
        }
        let Value::String(text) = value else {
            return Err(Error::InvalidValue(
                "folder mount writes require text".into(),
            ));
        };
        if text.len() > limits.max_text_bytes {
            return Err(Error::ResourceLimitExceeded("mount leaf bytes"));
        }
        let parent = MountPath {
            segments: path.segments()[..path.segments().len() - 1].to_vec(),
        };
        let Some((resolved_parent, metadata)) = self.resolve(&parent)? else {
            return Err(Error::InvalidPath(path.display()));
        };
        if !metadata.is_dir() {
            return Err(Error::InvalidPath(path.display()));
        }
        // Reject an existing non-regular target through the same link rules
        // that govern reads before replacing any bytes.
        if let Some((_, existing)) = self.resolve(path)?
            && existing.is_dir()
        {
            return Err(Error::InvalidPath(path.display()));
        }
        let target = resolved_parent.join(
            path.segments()
                .last()
                .expect("non-root mount paths have a last segment"),
        );
        fs::write(&target, text)
            .map_err(|_| Error::InvalidMount("folder mount write failed".into()))
    }

    fn remove(&self, path: &MountPath, _limits: &Limits) -> Result<()> {
        if !self.access.allows_write() {
            return Err(Error::MountDenied("mount does not grant removals".into()));
        }
        let Some((resolved, metadata)) = self.resolve(path)? else {
            return Err(Error::InvalidPath(path.display()));
        };
        if path.is_root() || metadata.is_dir() {
            return Err(Error::InvalidPath(path.display()));
        }
        fs::remove_file(&resolved)
            .map_err(|_| Error::InvalidMount("folder mount removal failed".into()))
    }
}

/// An already materialized value exposed through the mount contract.
struct ValueMount {
    kind: String,
    source: String,
    access: MountAccess,
    value: Mutex<Value>,
}

impl ValueMount {
    fn new(
        kind: impl Into<String>,
        source: impl Into<String>,
        value: Value,
        access: MountAccess,
        limits: &Limits,
    ) -> Result<Self> {
        value.validate(limits, false)?;
        Ok(Self {
            kind: kind.into(),
            source: source.into(),
            access,
            value: Mutex::new(value),
        })
    }

    fn with_value<T>(&self, apply: impl FnOnce(&Value) -> T) -> Result<T> {
        let value = self
            .value
            .lock()
            .map_err(|_| Error::InvalidMount("mount state is unavailable".into()))?;
        Ok(apply(&value))
    }
}

fn value_at<'a>(root: &'a Value, path: &MountPath) -> Option<&'a Value> {
    let mut current = root;
    for segment in path.segments() {
        current = match current {
            Value::Map(values) => values.get(segment)?,
            Value::Array(values) => values.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

impl MountProvider for ValueMount {
    fn descriptor(&self) -> MountDescriptor {
        MountDescriptor::new(self.kind.clone(), self.source.clone())
            .locality(Locality::Cache)
            .access(self.access)
    }

    fn stat(&self, path: &MountPath, _limits: &Limits) -> Result<Option<MountNode>> {
        self.with_value(|root| {
            value_at(root, path).map(|value| match value {
                Value::Map(values) => MountNode::directory()
                    .fact("entries", Value::Integer(values.len() as i64))
                    .fact("content", Value::from("object")),
                Value::Array(values) => MountNode::directory()
                    .fact("entries", Value::Integer(values.len() as i64))
                    .fact("content", Value::from("array")),
                Value::String(text) => MountNode::leaf()
                    .fact("bytes", Value::Integer(text.len() as i64))
                    .fact("content", Value::from("text/plain")),
                _ => MountNode::leaf().fact("content", Value::from("scalar")),
            })
        })
    }

    fn list(&self, path: &MountPath, limits: &Limits) -> Result<Vec<String>> {
        let names = self.with_value(|root| match value_at(root, path) {
            Some(Value::Map(values)) => Some(values.keys().cloned().collect::<Vec<_>>()),
            Some(Value::Array(values)) => {
                Some((0..values.len()).map(|index| index.to_string()).collect())
            }
            _ => None,
        })?;
        let names = names.ok_or_else(|| Error::InvalidPath(path.display()))?;
        if names.len() > limits.max_mount_entries {
            return Err(Error::ResourceLimitExceeded("mount entries"));
        }
        Ok(names)
    }

    fn read(&self, path: &MountPath, limits: &Limits) -> Result<Option<Value>> {
        let value = self.with_value(|root| value_at(root, path).cloned())?;
        if let Some(value) = &value {
            value.validate(limits, false)?;
        }
        Ok(value)
    }

    fn write(&self, path: &MountPath, value: Value, limits: &Limits) -> Result<()> {
        if !self.access.allows_write() {
            return Err(Error::MountDenied("mount does not grant writes".into()));
        }
        if path.is_root() {
            return Err(Error::InvalidPath("cannot replace a mount root".into()));
        }
        value.validate(limits, false)?;
        let mut root = self
            .value
            .lock()
            .map_err(|_| Error::InvalidMount("mount state is unavailable".into()))?;
        let mut candidate = root.clone();
        let (leaf, parents) = path
            .segments()
            .split_last()
            .expect("non-root mount paths have a last segment");
        let parent_path = MountPath {
            segments: parents.to_vec(),
        };
        match value_at_mut(&mut candidate, &parent_path) {
            Some(Value::Map(values)) => {
                values.insert(leaf.clone(), value);
            }
            Some(Value::Array(values)) => {
                let index = leaf
                    .parse::<usize>()
                    .map_err(|_| Error::InvalidPath(path.display()))?;
                let target = values
                    .get_mut(index)
                    .ok_or_else(|| Error::InvalidPath(path.display()))?;
                *target = value;
            }
            _ => return Err(Error::InvalidPath(path.display())),
        }
        candidate.validate(limits, false)?;
        *root = candidate;
        Ok(())
    }

    fn remove(&self, path: &MountPath, limits: &Limits) -> Result<()> {
        if !self.access.allows_write() {
            return Err(Error::MountDenied("mount does not grant removals".into()));
        }
        let (leaf, parents) = path
            .segments()
            .split_last()
            .ok_or_else(|| Error::InvalidPath("cannot remove a mount root".into()))?;
        let mut root = self
            .value
            .lock()
            .map_err(|_| Error::InvalidMount("mount state is unavailable".into()))?;
        let mut candidate = root.clone();
        let parent_path = MountPath {
            segments: parents.to_vec(),
        };
        match value_at_mut(&mut candidate, &parent_path) {
            Some(Value::Map(values)) => {
                values
                    .remove(leaf)
                    .ok_or_else(|| Error::InvalidPath(path.display()))?;
            }
            Some(Value::Array(values)) => {
                let index = leaf
                    .parse::<usize>()
                    .map_err(|_| Error::InvalidPath(path.display()))?;
                if index >= values.len() {
                    return Err(Error::InvalidPath(path.display()));
                }
                values.remove(index);
            }
            _ => return Err(Error::InvalidPath(path.display())),
        }
        candidate.validate(limits, false)?;
        *root = candidate;
        Ok(())
    }
}

fn value_at_mut<'a>(root: &'a mut Value, path: &MountPath) -> Option<&'a mut Value> {
    let mut current = root;
    for segment in path.segments() {
        current = match current {
            Value::Map(values) => values.get_mut(segment)?,
            Value::Array(values) => values.get_mut(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

#[cfg(feature = "turso-mount")]
async fn turso_rows(
    connection: &Connection,
    sql: &str,
    params: impl IntoParams,
    limits: &Limits,
) -> Result<Value> {
    // THREAT[TM-DOS-007]: Bound every materialized row and value. The caller
    // still owns an outer deadline for the host-selected query.
    let mut rows = connection
        .query(sql, params)
        .await
        .map_err(|_| Error::InvalidMount("Turso query failed".into()))?;
    let columns = rows.column_names();
    let unique = columns.iter().collect::<BTreeSet<_>>();
    if columns.is_empty() || unique.len() != columns.len() || columns.iter().any(String::is_empty) {
        return Err(Error::InvalidMount(
            "Turso query columns must be non-empty and unique".into(),
        ));
    }

    let mut values = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|_| Error::InvalidMount("Turso query failed".into()))?
    {
        if values.len() >= limits.max_mount_entries {
            return Err(Error::ResourceLimitExceeded("mount entries"));
        }
        let mut fields = BTreeMap::new();
        for (index, column) in columns.iter().enumerate() {
            let value = match row
                .get_value(index)
                .map_err(|_| Error::InvalidMount("Turso row is unreadable".into()))?
            {
                turso::Value::Null => Value::Null,
                turso::Value::Integer(value) => Value::Integer(value),
                turso::Value::Real(value) => Value::Number(value),
                turso::Value::Text(value) => Value::String(value),
                turso::Value::Blob(_) => {
                    return Err(Error::InvalidMount(
                        "Turso blob columns are not supported".into(),
                    ));
                }
            };
            fields.insert(column.clone(), value);
        }
        values.push(Value::Map(fields));
    }
    let value = Value::Array(values);
    value.validate(limits, false)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("svit-mount-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    // THREAT[TM-CAP-007]
    fn mount_paths_reject_traversal_and_separators() {
        assert!(MountPath::parse("../secret").is_err());
        assert!(MountPath::parse("a//b").is_err());
        assert!(MountPath::parse("a/./b").is_err());
        assert!(MountPath::parse("a\\b").is_err());
        assert_eq!(MountPath::parse("/").unwrap(), MountPath::root());
        assert_eq!(
            MountPath::parse("/a/b").unwrap().segments(),
            ["a".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    // THREAT[TM-CAP-003]
    #[cfg(unix)]
    fn folder_mounts_refuse_to_resolve_symbolic_links() {
        use std::os::unix::fs::symlink;

        let root = scratch("symlink");
        symlink("/", root.join("outside")).unwrap();
        let mount = FolderMount::new(&root, MountAccess::Read).unwrap();

        let result = mount.stat(&MountPath::parse("outside").unwrap(), &Limits::default());

        fs::remove_dir_all(&root).unwrap();
        assert!(matches!(result, Err(Error::MountDenied(_))));
    }

    #[test]
    // THREAT[TM-DOS-010]
    fn folder_leaf_reads_stop_at_the_text_limit() {
        let root = scratch("leaf-limit");
        fs::write(root.join("large.txt"), "larger than the limit").unwrap();
        let limits = Limits {
            max_text_bytes: 12,
            ..Limits::default()
        };
        let mount = FolderMount::new(&root, MountAccess::Read).unwrap();

        let result = mount.read(&MountPath::parse("large.txt").unwrap(), &limits);

        fs::remove_dir_all(&root).unwrap();
        assert!(matches!(
            result,
            Err(Error::ResourceLimitExceeded("mount leaf bytes"))
        ));
    }

    #[test]
    // THREAT[TM-DOS-010]
    fn folder_listings_stop_at_the_entry_limit() {
        let root = scratch("list-limit");
        for index in 0..4 {
            fs::write(root.join(format!("file-{index}")), "x").unwrap();
        }
        let limits = Limits {
            max_mount_entries: 2,
            ..Limits::default()
        };
        let mount = FolderMount::new(&root, MountAccess::Read).unwrap();

        let result = mount.list(&MountPath::root(), &limits);

        fs::remove_dir_all(&root).unwrap();
        assert!(matches!(
            result,
            Err(Error::ResourceLimitExceeded("mount entries"))
        ));
    }

    #[test]
    fn folder_mounts_are_lazy_and_expose_node_facts() {
        let root = scratch("facts");
        fs::create_dir(root.join("docs")).unwrap();
        fs::write(root.join("docs/readme.md"), "hello").unwrap();
        let mount = FolderMount::new(&root, MountAccess::Read).unwrap();
        let limits = Limits::default();

        let directory = mount
            .stat(&MountPath::parse("docs").unwrap(), &limits)
            .unwrap()
            .unwrap();
        let leaf = mount
            .stat(&MountPath::parse("docs/readme.md").unwrap(), &limits)
            .unwrap()
            .unwrap();
        let listing = mount
            .list(&MountPath::parse("docs").unwrap(), &limits)
            .unwrap();
        let content = mount
            .read(&MountPath::parse("docs/readme.md").unwrap(), &limits)
            .unwrap();
        let missing = mount
            .stat(&MountPath::parse("docs/absent.md").unwrap(), &limits)
            .unwrap();

        fs::remove_dir_all(&root).unwrap();
        assert_eq!(directory.node_kind(), MountNodeKind::Directory);
        assert_eq!(leaf.node_kind(), MountNodeKind::Leaf);
        assert_eq!(leaf.facts()["bytes"], Value::Integer(5));
        assert_eq!(listing, ["readme.md".to_owned()]);
        assert_eq!(content, Some(Value::from("hello")));
        assert_eq!(missing, None);
    }

    #[test]
    // THREAT[TM-CAP-001]
    fn read_only_folder_mounts_deny_writes_and_removals() {
        let root = scratch("read-only");
        fs::write(root.join("note.txt"), "original").unwrap();
        let mount = FolderMount::new(&root, MountAccess::Read).unwrap();
        let limits = Limits::default();
        let path = MountPath::parse("note.txt").unwrap();

        let write = mount.write(&path, Value::from("replaced"), &limits);
        let remove = mount.remove(&path, &limits);
        let content = fs::read_to_string(root.join("note.txt")).unwrap();

        fs::remove_dir_all(&root).unwrap();
        assert!(matches!(write, Err(Error::MountDenied(_))));
        assert!(matches!(remove, Err(Error::MountDenied(_))));
        assert_eq!(content, "original");
    }

    #[test]
    fn writable_folder_mounts_replace_and_remove_leaves() {
        let root = scratch("writable");
        fs::write(root.join("note.txt"), "original").unwrap();
        let mount = FolderMount::new(&root, MountAccess::ReadWrite).unwrap();
        let limits = Limits::default();
        let path = MountPath::parse("note.txt").unwrap();

        mount
            .write(&path, Value::from("replaced"), &limits)
            .unwrap();
        let replaced = fs::read_to_string(root.join("note.txt")).unwrap();
        mount.remove(&path, &limits).unwrap();
        let exists = root.join("note.txt").exists();

        fs::remove_dir_all(&root).unwrap();
        assert_eq!(replaced, "replaced");
        assert!(!exists);
    }

    #[test]
    fn descriptor_records_round_trip_through_the_committed_value() {
        let descriptor = MountDescriptor::new("folder", "/tmp/example")
            .locality(Locality::Local)
            .access(MountAccess::ReadWrite);

        let value = descriptor.to_value();

        assert_eq!(
            value,
            value!({
                "access": "read-write",
                "kind": "folder",
                "locality": "local",
                "source": "/tmp/example"
            })
        );
        assert_eq!(MountDescriptor::from_value(&value).unwrap(), descriptor);
        assert!(MountDescriptor::from_value(&value!({"kind": "folder"})).is_err());
        assert!(
            MountDescriptor::from_value(&value!({
                "access": "read", "kind": "folder", "locality": "moon", "source": ""
            }))
            .is_err()
        );
    }

    #[test]
    fn value_mounts_report_cache_locality_and_resolve_nodes() {
        let limits = Limits::default();
        let mount = ValueMount::new(
            "value",
            "example",
            value!({"rows": [{"name": "alpha"}]}),
            MountAccess::ReadWrite,
            &limits,
        )
        .unwrap();

        assert_eq!(mount.descriptor().locality_class(), Locality::Cache);
        assert_eq!(
            mount
                .list(&MountPath::parse("rows").unwrap(), &limits)
                .unwrap(),
            ["0".to_owned()]
        );
        assert_eq!(
            mount
                .read(&MountPath::parse("rows/0/name").unwrap(), &limits)
                .unwrap(),
            Some(Value::from("alpha"))
        );
        mount
            .write(
                &MountPath::parse("rows/0/name").unwrap(),
                Value::from("beta"),
                &limits,
            )
            .unwrap();
        assert_eq!(
            mount
                .read(&MountPath::parse("rows/0/name").unwrap(), &limits)
                .unwrap(),
            Some(Value::from("beta"))
        );
    }
}
