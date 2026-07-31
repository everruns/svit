use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Resource and persistence limits applied to every activation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Maximum Luau interrupt callbacks before an activation is stopped.
    ///
    /// This is a versioned VM tick budget, not an exact source-instruction
    /// count.
    pub max_interrupt_ticks: u64,
    /// Maximum guest heap growth above the fresh VM baseline.
    pub max_heap_bytes: usize,
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
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_interrupt_ticks: 100_000,
            max_heap_bytes: 8 * 1024 * 1024,
            max_value_depth: 32,
            max_value_entries: 10_000,
            max_text_bytes: 1024 * 1024,
            max_script_bytes: 64 * 1024,
            max_logs: 128,
            max_messages: 128,
            max_staged_scripts: 32,
        }
    }
}

impl Limits {
    pub(crate) fn validate(&self) -> Result<()> {
        // THREAT[TM-DOS-003]: Snapshot limits are untrusted and cannot enlarge
        // the host's hard safety envelope during restore.
        let valid = self.max_interrupt_ticks <= 10_000_000
            && self.max_heap_bytes <= 64 * 1024 * 1024
            && self.max_value_depth <= 128
            && self.max_value_entries <= 1_000_000
            && self.max_text_bytes <= 64 * 1024 * 1024
            && self.max_script_bytes <= 1024 * 1024
            && self.max_logs <= 10_000
            && self.max_messages <= 10_000
            && self.max_staged_scripts <= 10_000;
        if !valid {
            return Err(Error::InvalidLimits(
                "one or more limits exceed hard maxima",
            ));
        }
        Ok(())
    }
}
