//! Persistent values and named Svit Lisp script records.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Limits, Result};

// Content-hash type tags. These are committed contract: changing one changes
// every hash below it, so a change here bumps the snapshot format.
const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_INTEGER: u8 = 2;
const TAG_NUMBER: u8 = 3;
const TAG_STRING: u8 = 4;
const TAG_ARRAY: u8 = 5;
const TAG_MAP: u8 = 6;
const TAG_SCRIPT: u8 = 7;

/// Named executable source stored in the process state tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    source: String,
    documentation: String,
}

impl Script {
    /// Creates a script record.
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            documentation: String::new(),
        }
    }

    /// Attaches discoverable documentation to the script.
    pub fn with_documentation(mut self, documentation: impl Into<String>) -> Self {
        self.documentation = documentation.into();
        self
    }

    /// Returns the authoritative source.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns the discoverable documentation.
    pub fn documentation(&self) -> &str {
        &self.documentation
    }
}

/// Canonical values that can cross an activation commit boundary.
///
/// Functions, userdata, threads, non-text map keys, cycles, NaN, and infinity
/// are intentionally absent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Value {
    /// No value.
    Null,
    /// Boolean value.
    Bool(bool),
    /// Signed 64-bit integer.
    Integer(i64),
    /// Finite IEEE-754 double.
    Number(f64),
    /// UTF-8 text.
    String(String),
    /// Ordered collection.
    Array(Vec<Value>),
    /// String-keyed object.
    Map(BTreeMap<String, Value>),
    /// Executable source record stored below `/lib`.
    Script(Script),
}

impl Value {
    /// Creates an empty object.
    pub fn empty_map() -> Self {
        Self::Map(BTreeMap::new())
    }

    /// Converts ordinary JSON into a persistent value.
    pub fn from_json(value: serde_json::Value) -> Result<Self> {
        match value {
            serde_json::Value::Null => Ok(Self::Null),
            serde_json::Value::Bool(value) => Ok(Self::Bool(value)),
            serde_json::Value::Number(value) => {
                if let Some(value) = value.as_i64() {
                    Ok(Self::Integer(value))
                } else if let Some(value) = value.as_f64().filter(|value| value.is_finite()) {
                    Ok(Self::Number(value))
                } else {
                    Err(Error::InvalidValue(
                        "JSON number is not finite or supported".into(),
                    ))
                }
            }
            serde_json::Value::String(value) => Ok(Self::String(value)),
            serde_json::Value::Array(values) => values
                .into_iter()
                .map(Self::from_json)
                .collect::<Result<Vec<_>>>()
                .map(Self::Array),
            serde_json::Value::Object(values) => values
                .into_iter()
                .map(|(key, value)| Ok((key, Self::from_json(value)?)))
                .collect::<Result<BTreeMap<_, _>>>()
                .map(Self::Map),
        }
    }

    /// Converts a persistent data value to ordinary JSON.
    ///
    /// Script records are represented as discoverable metadata objects.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(value) => (*value).into(),
            Self::Integer(value) => (*value).into(),
            Self::Number(value) => serde_json::Number::from_f64(*value)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Self::String(value) => value.clone().into(),
            Self::Array(values) => values.iter().map(Self::to_json).collect(),
            Self::Map(values) => values
                .iter()
                .map(|(key, value)| (key.clone(), value.to_json()))
                .collect(),
            Self::Script(script) => serde_json::json!({
                "source": script.source(),
                "documentation": script.documentation(),
            }),
        }
    }

    /// Returns the SHA-256 content hash of this value and everything below it.
    ///
    /// The hash is structural: it covers a node's own subtree and nothing
    /// above it, so an unchanged subtree keeps its hash across commits, forks,
    /// and snapshots, and a client holding the same hash holds the same
    /// content. A mount node hashes its committed descriptor, never the
    /// external source it resolves through.
    ///
    /// The encoding is canonical and part of the committed contract: a type
    /// tag, then the leaf bytes or the child digests in canonical order, with
    /// map keys length-prefixed so no key and value pair can be confused with
    /// another.
    pub fn content_hash(&self) -> String {
        self.digest()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        match self {
            Self::Null => hasher.update([TAG_NULL]),
            Self::Bool(value) => {
                hasher.update([TAG_BOOL]);
                hasher.update([u8::from(*value)]);
            }
            Self::Integer(value) => {
                hasher.update([TAG_INTEGER]);
                hasher.update(value.to_be_bytes());
            }
            Self::Number(value) => {
                hasher.update([TAG_NUMBER]);
                // Validation rejects non-finite numbers, and canonicalizing
                // negative zero keeps equal values equally hashed.
                let value = if *value == 0.0 { 0.0 } else { *value };
                hasher.update(value.to_be_bytes());
            }
            Self::String(value) => {
                hasher.update([TAG_STRING]);
                hasher.update(value.as_bytes());
            }
            Self::Array(values) => {
                hasher.update([TAG_ARRAY]);
                hasher.update((values.len() as u64).to_be_bytes());
                for value in values {
                    hasher.update(value.digest());
                }
            }
            Self::Map(values) => {
                hasher.update([TAG_MAP]);
                hasher.update((values.len() as u64).to_be_bytes());
                for (key, value) in values {
                    hasher.update((key.len() as u64).to_be_bytes());
                    hasher.update(key.as_bytes());
                    hasher.update(value.digest());
                }
            }
            Self::Script(script) => {
                hasher.update([TAG_SCRIPT]);
                hasher.update((script.source().len() as u64).to_be_bytes());
                hasher.update(script.source().as_bytes());
                hasher.update(script.documentation().as_bytes());
            }
        }
        hasher.finalize().into()
    }

    pub(crate) fn validate(&self, limits: &Limits, scripts_allowed: bool) -> Result<()> {
        let mut entries = 0usize;
        let mut text_bytes = 0usize;
        self.validate_inner(limits, scripts_allowed, 0, &mut entries, &mut text_bytes)
    }

    fn validate_inner(
        &self,
        limits: &Limits,
        scripts_allowed: bool,
        depth: usize,
        entries: &mut usize,
        text_bytes: &mut usize,
    ) -> Result<()> {
        if depth > limits.max_value_depth {
            return Err(Error::InvalidValue("maximum nesting depth exceeded".into()));
        }
        *entries = entries.saturating_add(1);
        if *entries > limits.max_value_entries {
            return Err(Error::InvalidValue("maximum value entries exceeded".into()));
        }

        match self {
            Self::Number(value) if !value.is_finite() => {
                return Err(Error::InvalidValue("numbers must be finite".into()));
            }
            Self::String(value) => add_text_bytes(text_bytes, value.len(), limits)?,
            Self::Array(values) => {
                for value in values {
                    value.validate_inner(
                        limits,
                        scripts_allowed,
                        depth + 1,
                        entries,
                        text_bytes,
                    )?;
                }
            }
            Self::Map(values) => {
                for (key, value) in values {
                    add_text_bytes(text_bytes, key.len(), limits)?;
                    value.validate_inner(
                        limits,
                        scripts_allowed,
                        depth + 1,
                        entries,
                        text_bytes,
                    )?;
                }
            }
            Self::Script(script) if !scripts_allowed => {
                return Err(Error::InvalidValue(
                    "script records are only valid below /lib".into(),
                ));
            }
            Self::Script(script) => {
                if script.source().len() > limits.max_script_bytes {
                    return Err(Error::ResourceLimitExceeded("script source"));
                }
                add_text_bytes(text_bytes, script.source().len(), limits)?;
                add_text_bytes(text_bytes, script.documentation().len(), limits)?;
            }
            Self::Null | Self::Bool(_) | Self::Integer(_) | Self::Number(_) => {}
        }
        Ok(())
    }
}

fn add_text_bytes(total: &mut usize, value: usize, limits: &Limits) -> Result<()> {
    *total = total.saturating_add(value);
    if *total > limits.max_text_bytes {
        return Err(Error::InvalidValue("maximum text bytes exceeded".into()));
    }
    Ok(())
}

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let json = serde_json::to_string(&self.to_json()).map_err(|_| fmt::Error)?;
        formatter.write_str(&json)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i32> for Value {
    fn from(value: i32) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

#[doc(hidden)]
pub mod __private {
    pub use serde_json::json;
}

/// Constructs a [`Value`] using JSON-like syntax.
#[macro_export]
macro_rules! value {
    ($($json:tt)+) => {
        $crate::Value::from_json($crate::value::__private::json!($($json)+))
            .expect("value! input is valid JSON")
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_preserves_data_values() {
        let value = crate::value!({"active": true, "count": 2, "items": ["a", "b"]});
        assert_eq!(Value::from_json(value.to_json()).unwrap(), value);
    }

    #[test]
    fn validation_rejects_non_finite_numbers() {
        let error = Value::Number(f64::NAN)
            .validate(&Limits::default(), false)
            .unwrap_err();
        assert!(matches!(error, Error::InvalidValue(_)));
    }
}
