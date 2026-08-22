use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub(crate) const DEFAULT_MAX_TEXT_BYTES: usize = 1024 * 1024;

/// Resource and persistence limits applied to every activation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Maximum wall-clock milliseconds spent in one Ketos VM entry.
    pub max_execution_millis: u64,
    /// Maximum abstract guest-memory units tracked by Ketos.
    pub max_guest_memory: usize,
    /// Maximum nested guest function calls.
    pub max_call_stack: usize,
    /// Maximum guest values held on the Ketos VM stack.
    pub max_value_stack: usize,
    /// Maximum guest-defined names in one activation.
    pub max_namespace_entries: usize,
    /// Maximum nested calls from one Svit script into another.
    pub max_exec_depth: usize,
    /// Maximum nested Lisp syntax depth.
    pub max_syntax_depth: usize,
    /// Maximum size of guest integer and ratio values in bits.
    pub max_integer_bits: usize,
    /// Maximum nesting depth of a persistent value.
    pub max_value_depth: usize,
    /// Maximum number of entries visited in one persistent value.
    pub max_value_entries: usize,
    /// Maximum UTF-8 bytes across keys and strings in one persistent value.
    pub max_text_bytes: usize,
    /// Maximum source size for one stored script.
    pub max_script_bytes: usize,
    /// Maximum log records emitted by one activation.
    pub max_logs: usize,
    /// Maximum messages emitted by one activation.
    pub max_messages: usize,
    /// Maximum scripts staged by one activation.
    pub max_staged_scripts: usize,
    /// Maximum child names returned by one lazy mount listing.
    pub max_mount_entries: usize,
    /// Maximum mount writes buffered by one activation.
    pub max_mount_writes: usize,
    /// Maximum nodes in the whole committed process root.
    ///
    /// Per-value limits bound one write; this bounds the state every write
    /// accumulates into, so a script that commits one small node per
    /// activation cannot grow the root without end.
    pub max_tree_nodes: usize,
    /// Maximum UTF-8 bytes across every key, string, and script in the whole
    /// committed process root.
    ///
    /// Paired with `max_tree_nodes` this bounds snapshot size within a constant
    /// per-node encoding factor; it is not itself a serialized-byte limit.
    pub max_tree_text_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_execution_millis: 100,
            max_guest_memory: 8 * 1024 * 1024,
            max_call_stack: 64,
            max_value_stack: 4096,
            max_namespace_entries: 256,
            max_exec_depth: 16,
            max_syntax_depth: 64,
            max_integer_bits: 64,
            max_value_depth: 32,
            max_value_entries: 10_000,
            max_text_bytes: DEFAULT_MAX_TEXT_BYTES,
            max_script_bytes: 64 * 1024,
            max_logs: 128,
            max_messages: 128,
            max_staged_scripts: 32,
            max_mount_entries: 4096,
            max_mount_writes: 32,
            max_tree_nodes: 100_000,
            max_tree_text_bytes: 16 * 1024 * 1024,
        }
    }
}

impl Limits {
    pub(crate) fn validate(&self) -> Result<()> {
        // THREAT[TM-DOS-003]: Snapshot limits are untrusted and cannot enlarge
        // the host's hard safety envelope during restore.
        let valid = self.max_execution_millis <= 60_000
            && self.max_guest_memory <= 64 * 1024 * 1024
            && self.max_call_stack <= 4096
            && self.max_value_stack <= 1_000_000
            && self.max_namespace_entries <= 10_000
            && self.max_exec_depth <= 64
            && self.max_syntax_depth <= 1024
            && self.max_integer_bits <= 4096
            && self.max_value_depth <= 128
            && self.max_value_entries <= 1_000_000
            && self.max_text_bytes <= 64 * 1024 * 1024
            && self.max_script_bytes <= 1024 * 1024
            && self.max_logs <= 10_000
            && self.max_messages <= 10_000
            && self.max_staged_scripts <= 10_000
            && self.max_mount_entries <= 1_000_000
            && self.max_mount_writes <= 10_000
            && self.max_tree_nodes <= 10_000_000
            && self.max_tree_text_bytes <= 512 * 1024 * 1024;
        if !valid {
            return Err(Error::InvalidLimits(
                "one or more limits exceed hard maxima",
            ));
        }
        Ok(())
    }
}
