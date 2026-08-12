use async_trait::async_trait;
use regex::{Regex, RegexBuilder};
use serde_json::{Value as JsonValue, json};

use super::{Builtin, BuiltinContext, BuiltinManual, BuiltinResult, MAX_TOOL_OUTPUT_BYTES};

const MAX_SEARCH_RESULTS: usize = 100;

pub(super) struct SearchBuiltin;

#[async_trait]
impl Builtin for SearchBuiltin {
    fn manual(&self) -> BuiltinManual {
        BuiltinManual::new(
            "Search text values under a committed Svit process path with a Rust regular expression.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "pattern": {"type": "string"},
                    "case_sensitive": {"type": "boolean", "default": true},
                    "fixed_strings": {"type": "boolean", "default": false}
                },
                "required": ["path", "pattern"]
            }),
        )
        .effect("read")
        .output("JSON object with matched process paths, one-based line numbers, and text.")
        .limits([
            "4 KiB pattern.",
            "100 results.",
            "256 KiB aggregate output.",
        ])
    }

    async fn execute(&self, context: BuiltinContext, arguments: JsonValue) -> BuiltinResult {
        let path = arguments["path"].as_str().unwrap_or_default();
        let pattern = arguments["pattern"].as_str().unwrap_or_default();
        if pattern.len() > 4 * 1024 {
            return BuiltinResult::error("search pattern limit exceeded");
        }
        let pattern = if arguments["fixed_strings"].as_bool().unwrap_or(false) {
            regex::escape(pattern)
        } else {
            pattern.to_owned()
        };
        let regex = match RegexBuilder::new(&pattern)
            .case_insensitive(!arguments["case_sensitive"].as_bool().unwrap_or(true))
            .size_limit(1 << 20)
            .build()
        {
            Ok(regex) => regex,
            Err(_) => return BuiltinResult::error("invalid search pattern"),
        };
        let value = match context.read(path) {
            Ok(Some(value)) => value.to_json(),
            Ok(None) => return BuiltinResult::error("path not found"),
            Err(error) => return BuiltinResult::error(error.to_string()),
        };
        // THREAT[TM-DOS-008]: Search uses the linear-time Rust regex engine
        // and caps pattern size, compiled size, result count, and output.
        let mut matches = Vec::new();
        let mut output_bytes = 0;
        collect_matches(path, &value, &regex, &mut matches, &mut output_bytes);
        BuiltinResult::text(json!({"matches": matches}).to_string())
    }
}

fn collect_matches(
    path: &str,
    value: &JsonValue,
    regex: &Regex,
    matches: &mut Vec<JsonValue>,
    output_bytes: &mut usize,
) {
    if matches.len() >= MAX_SEARCH_RESULTS {
        return;
    }
    match value {
        JsonValue::String(text) => {
            for (line_index, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    let Some(next_size) = output_bytes.checked_add(path.len() + line.len()) else {
                        return;
                    };
                    if next_size > MAX_TOOL_OUTPUT_BYTES {
                        return;
                    }
                    matches.push(json!({"path": path, "line": line_index + 1, "text": line}));
                    *output_bytes = next_size;
                    if matches.len() >= MAX_SEARCH_RESULTS {
                        break;
                    }
                }
            }
        }
        JsonValue::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                collect_matches(
                    &format!("{path}/{index}"),
                    value,
                    regex,
                    matches,
                    output_bytes,
                );
            }
        }
        JsonValue::Object(values) => {
            for (name, value) in values {
                collect_matches(
                    &format!("{path}/{name}"),
                    value,
                    regex,
                    matches,
                    output_bytes,
                );
            }
        }
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) => {}
    }
}
