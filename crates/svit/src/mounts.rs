//! Read-only external data imported into the process namespace.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::Path;

#[cfg(feature = "turso-mount")]
use turso::{Connection, IntoParams};

use crate::{Error, Limits, Result, Value};

const MAX_MOUNT_NAME_BYTES: usize = 64;

/// A bounded, read-only copy of external data recorded below `/mounts`.
///
/// Snapshot mounts contain data rather than live authority. Activations never
/// receive a host path, database connection, or query capability.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotMount {
    value: Value,
}

impl SnapshotMount {
    /// Imports a real directory as nested maps whose file leaves are UTF-8 text.
    ///
    /// Symbolic links and non-regular files are rejected so the configured root
    /// is the complete filesystem authority exercised by this import.
    pub fn folder(path: impl AsRef<Path>, limits: &Limits) -> Result<Self> {
        let mut budget = ImportBudget::new(limits);
        let data = import_directory(path.as_ref(), &mut budget, 0)?;
        Self::new("folder", data, limits)
    }

    /// Records the rows returned by one host-selected Turso query.
    ///
    /// Each row is a map keyed by its result-column names. Turso blobs are
    /// rejected because Svit's persistent value model does not yet include
    /// byte strings.
    #[cfg(feature = "turso-mount")]
    pub async fn turso_query(
        connection: &Connection,
        sql: impl AsRef<str>,
        params: impl IntoParams,
        limits: &Limits,
    ) -> Result<Self> {
        // THREAT[TM-DOS-007]: Bound every materialized row and value. The
        // caller still owns an outer deadline for the host-selected query.
        let mut rows = connection
            .query(sql, params)
            .await
            .map_err(|error| Error::InvalidMount(error.to_string()))?;
        let columns = rows.column_names();
        validate_columns(&columns)?;

        let mut budget = ImportBudget::new(limits);
        let mut values = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|error| Error::InvalidMount(error.to_string()))?
        {
            budget.entry()?;
            let mut fields = BTreeMap::new();
            for (index, column) in columns.iter().enumerate() {
                budget.text(column)?;
                budget.entry()?;
                let value = match row
                    .get_value(index)
                    .map_err(|error| Error::InvalidMount(error.to_string()))?
                {
                    turso::Value::Null => Value::Null,
                    turso::Value::Integer(value) => Value::Integer(value),
                    turso::Value::Real(value) => Value::Number(value),
                    turso::Value::Text(value) => {
                        budget.text(&value)?;
                        Value::String(value)
                    }
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
        Self::new("turso-query", Value::Array(values), limits)
    }

    fn new(kind: &str, data: Value, limits: &Limits) -> Result<Self> {
        let value = mount_value(kind, data);
        validate_mount(&value, limits)?;
        Ok(Self { value })
    }

    pub(crate) fn into_value(self) -> Value {
        self.value
    }
}

fn mount_value(kind: &str, data: Value) -> Value {
    Value::Map(BTreeMap::from([
        ("data".into(), data),
        ("kind".into(), Value::String(kind.into())),
        ("mode".into(), Value::String("snapshot-read-only".into())),
    ]))
}

pub(crate) fn validate_mounts(value: &Value, limits: &Limits) -> Result<()> {
    let Value::Map(mounts) = value else {
        return Err(Error::InvalidSnapshot("/mounts is not a map".into()));
    };
    value.validate(limits, false)?;
    for (name, mount) in mounts {
        validate_mount_name(name)?;
        validate_mount(mount, limits)?;
    }
    Ok(())
}

fn validate_mount(value: &Value, limits: &Limits) -> Result<()> {
    value.validate(limits, false)?;
    let Value::Map(fields) = value else {
        return Err(Error::InvalidMount("mount record is not a map".into()));
    };
    if fields.keys().map(String::as_str).collect::<BTreeSet<_>>()
        != BTreeSet::from(["data", "kind", "mode"])
    {
        return Err(Error::InvalidMount(
            "mount record must contain exactly data, kind, and mode".into(),
        ));
    }
    if !matches!(fields.get("kind"), Some(Value::String(kind)) if matches!(kind.as_str(), "folder" | "turso-query"))
    {
        return Err(Error::InvalidMount("unsupported mount kind".into()));
    }
    if fields.get("mode") != Some(&Value::String("snapshot-read-only".into())) {
        return Err(Error::InvalidMount(
            "mount mode must be snapshot-read-only".into(),
        ));
    }
    Ok(())
}

fn validate_mount_name(name: &str) -> Result<()> {
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

#[cfg(feature = "turso-mount")]
fn validate_columns(columns: &[String]) -> Result<()> {
    let unique = columns.iter().collect::<BTreeSet<_>>();
    if columns.is_empty() || unique.len() != columns.len() || columns.iter().any(String::is_empty) {
        return Err(Error::InvalidMount(
            "Turso query columns must be non-empty and unique".into(),
        ));
    }
    Ok(())
}

// THREAT[TM-CAP-003]: Reject links and special files encountered during the
// bounded import and return persistent values rather than handles.
fn import_directory(path: &Path, budget: &mut ImportBudget<'_>, depth: usize) -> Result<Value> {
    if depth > budget.limits.max_value_depth {
        return Err(Error::InvalidMount("folder nesting limit exceeded".into()));
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|error| Error::InvalidMount(error.to_string()))?;
    if !metadata.file_type().is_dir() {
        return Err(Error::InvalidMount(
            "folder mount root is not a directory".into(),
        ));
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(|error| Error::InvalidMount(error.to_string()))? {
        if entries.len() >= budget.remaining_entries() {
            return Err(Error::InvalidMount("mount entry limit exceeded".into()));
        }
        entries.push(entry.map_err(|error| Error::InvalidMount(error.to_string()))?);
    }
    entries.sort_by_key(|entry| entry.file_name());

    let mut values = BTreeMap::new();
    for entry in entries {
        budget.entry()?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::InvalidMount("folder entry name is not UTF-8".into()))?;
        budget.text(&name)?;
        let entry_path = entry.path();
        let metadata = fs::symlink_metadata(&entry_path)
            .map_err(|error| Error::InvalidMount(error.to_string()))?;
        let value = if metadata.file_type().is_dir() {
            import_directory(&entry_path, budget, depth + 1)?
        } else if metadata.file_type().is_file() {
            let remaining = budget.remaining_text();
            let mut bytes = Vec::new();
            fs::File::open(&entry_path)
                .and_then(|file| {
                    file.take((remaining as u64).saturating_add(1))
                        .read_to_end(&mut bytes)
                })
                .map_err(|error| Error::InvalidMount(error.to_string()))?;
            if bytes.len() > remaining {
                return Err(Error::InvalidMount("folder text limit exceeded".into()));
            }
            let text = String::from_utf8(bytes)
                .map_err(|_| Error::InvalidMount("folder file is not UTF-8".into()))?;
            budget.text(&text)?;
            Value::String(text)
        } else {
            return Err(Error::InvalidMount(
                "symbolic links and special files are not supported".into(),
            ));
        };
        values.insert(name, value);
    }
    Ok(Value::Map(values))
}

struct ImportBudget<'a> {
    limits: &'a Limits,
    entries: usize,
    text_bytes: usize,
}

impl<'a> ImportBudget<'a> {
    fn new(limits: &'a Limits) -> Self {
        Self {
            limits,
            entries: 1,
            text_bytes: 0,
        }
    }

    fn entry(&mut self) -> Result<()> {
        self.entries = self.entries.saturating_add(1);
        if self.entries > self.limits.max_value_entries {
            return Err(Error::InvalidMount("mount entry limit exceeded".into()));
        }
        Ok(())
    }

    fn text(&mut self, value: &str) -> Result<()> {
        self.text_bytes = self.text_bytes.saturating_add(value.len());
        if self.text_bytes > self.limits.max_text_bytes {
            return Err(Error::InvalidMount("mount text limit exceeded".into()));
        }
        Ok(())
    }

    fn remaining_text(&self) -> usize {
        self.limits.max_text_bytes.saturating_sub(self.text_bytes)
    }

    fn remaining_entries(&self) -> usize {
        self.limits.max_value_entries.saturating_sub(self.entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // THREAT[TM-CAP-003]
    #[cfg(unix)]
    fn folder_mount_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("svit-mount-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        symlink("/", root.join("outside")).unwrap();

        let result = SnapshotMount::folder(&root, &Limits::default());

        fs::remove_dir_all(&root).unwrap();
        assert!(matches!(result, Err(Error::InvalidMount(_))));
    }

    #[test]
    // THREAT[TM-DOS-007]
    fn folder_mount_stops_at_the_text_limit() {
        let root = std::env::temp_dir().join(format!("svit-mount-limit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        fs::write(root.join("large.txt"), "larger than the limit").unwrap();
        let limits = Limits {
            max_text_bytes: 12,
            ..Limits::default()
        };

        let result = SnapshotMount::folder(&root, &limits);

        fs::remove_dir_all(&root).unwrap();
        assert!(matches!(result, Err(Error::InvalidMount(_))));
    }

    #[tokio::test]
    // THREAT[TM-DOS-007]
    #[cfg(feature = "turso-mount")]
    async fn turso_query_mount_records_typed_rows() {
        let database = turso::Builder::new_local(":memory:").build().await.unwrap();
        let connection = database.connect().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE items (id INTEGER, name TEXT);\n\
                 INSERT INTO items VALUES (1, 'alpha'), (2, 'beta');",
            )
            .await
            .unwrap();

        let mount = SnapshotMount::turso_query(
            &connection,
            "SELECT id, name FROM items ORDER BY id",
            (),
            &Limits::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            mount.into_value(),
            mount_value(
                "turso-query",
                crate::value!([{"id": 1, "name": "alpha"}, {"id": 2, "name": "beta"}])
            )
        );

        let limits = Limits {
            max_value_entries: 2,
            ..Limits::default()
        };
        let bounded = SnapshotMount::turso_query(
            &connection,
            "SELECT id, name FROM items ORDER BY id",
            (),
            &limits,
        )
        .await;
        assert!(matches!(bounded, Err(Error::InvalidMount(_))));
    }
}
