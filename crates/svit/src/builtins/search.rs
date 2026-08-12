use async_trait::async_trait;
use regex::{Regex, RegexBuilder};
use serde_json::{Value as JsonValue, json};

use super::{Builtin, BuiltinContext, BuiltinManual, BuiltinResult, MAX_TOOL_OUTPUT_BYTES};

const MAX_SEARCH_RESULTS: usize = 100;
/// Nodes one mount search may resolve before it stops.
///
/// A mount subtree has no committed size, so the walk needs its own bound.
const MAX_SEARCH_MOUNT_NODES: usize = 2048;

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
        // THREAT[TM-DOS-008]: Search uses the linear-time Rust regex engine
        // and caps pattern size, compiled size, result count, and output.
        let mut matches = Vec::new();
        let mut output_bytes = 0;
        if path == "/mounts" || path.starts_with("/mounts/") {
            // THREAT[TM-DOS-010]: A mount subtree is resolved node by node
            // under an independent node budget, never materialized first.
            let mut budget = MAX_SEARCH_MOUNT_NODES;
            let truncated = !collect_mount_matches(
                &context,
                path,
                &regex,
                &mut matches,
                &mut output_bytes,
                &mut budget,
            );
            return BuiltinResult::text(
                json!({"matches": matches, "truncated": truncated}).to_string(),
            );
        }
        let value = match context.read(path) {
            Ok(Some(value)) => value.to_json(),
            Ok(None) => return BuiltinResult::error("path not found"),
            Err(error) => return BuiltinResult::error(error.to_string()),
        };
        collect_matches(path, &value, &regex, &mut matches, &mut output_bytes);
        BuiltinResult::text(json!({"matches": matches}).to_string())
    }
}

/// Walks a mount subtree lazily. Returns `false` when a bound stopped it.
fn collect_mount_matches(
    context: &BuiltinContext,
    path: &str,
    regex: &Regex,
    matches: &mut Vec<JsonValue>,
    output_bytes: &mut usize,
    budget: &mut usize,
) -> bool {
    if matches.len() >= MAX_SEARCH_RESULTS {
        return false;
    }
    let Some(remaining) = budget.checked_sub(1) else {
        return false;
    };
    *budget = remaining;

    let directory = match context.stat(path) {
        Ok(Some(facts)) => facts.to_json()["kind"] == JsonValue::String("directory".into()),
        // A node that vanished between listing and stat is not an error; the
        // mount source is external and may change under the walk.
        Ok(None) | Err(_) => return true,
    };
    if !directory {
        if let Ok(Some(value)) = context.read(path) {
            collect_matches(path, &value.to_json(), regex, matches, output_bytes);
        }
        return true;
    }
    let Ok(children) = context.discover(path) else {
        return true;
    };
    let mut complete = true;
    for child in children {
        let child_path = format!("{path}/{child}");
        if !collect_mount_matches(context, &child_path, regex, matches, output_bytes, budget) {
            complete = false;
            break;
        }
    }
    complete
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::{Mount, Process};

    fn mounted_context() -> BuiltinContext {
        let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/mount-data");
        let process = Process::builder("svit://local/test/search-mount")
            .unwrap()
            .mount("files", Mount::folder(folder).unwrap())
            .build()
            .unwrap();
        BuiltinContext::new(Arc::new(Mutex::new(process)))
    }

    #[tokio::test]
    async fn search_walks_a_mount_subtree_one_node_at_a_time() {
        let result = SearchBuiltin
            .execute(
                mounted_context(),
                json!({"path": "/mounts/files", "pattern": "just check"}),
            )
            .await;

        let BuiltinResult::Success(JsonValue::String(text)) = result else {
            panic!("search returned {result:?}");
        };
        let output: JsonValue = serde_json::from_str(&text).unwrap();
        assert_eq!(output["truncated"], false);
        assert_eq!(
            output["matches"][0]["path"],
            "/mounts/files/notes/checklist.md"
        );
        assert_eq!(output["matches"][0]["line"], 2);
    }

    #[tokio::test]
    // THREAT[TM-DOS-010]
    async fn a_mount_search_reports_when_a_bound_stopped_the_walk() {
        let context = mounted_context();
        let regex = Regex::new("a").unwrap();
        let mut matches = Vec::new();
        let mut output_bytes = 0;
        let mut budget = 1;

        let complete = collect_mount_matches(
            &context,
            "/mounts/files",
            &regex,
            &mut matches,
            &mut output_bytes,
            &mut budget,
        );

        assert!(!complete);
    }
}
