use async_trait::async_trait;
use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx, Vars, data};
use jaq_json::Val;
use regex::Regex;
use serde_json::{Value as JsonValue, json};

use super::{
    Builtin, BuiltinContext, BuiltinManual, BuiltinResult, MAX_TOOL_INPUT_BYTES,
    MAX_TOOL_OUTPUT_BYTES,
};

const MAX_JQ_RESULTS: usize = 100;

pub(super) struct JqBuiltin;

#[async_trait]
impl Builtin for JqBuiltin {
    fn manual(&self) -> BuiltinManual {
        BuiltinManual::new(
            "Run a bounded jq filter over a supplied JSON value.",
            json!({
                "type": "object",
                "properties": {"filter": {"type": "string"}, "input": {}},
                "required": ["filter", "input"]
            }),
        )
        .effect("pure")
        .output("JSON object containing the values emitted by the filter.")
        .limits([
            "4 KiB filter and 256 KiB input/output.",
            "100 results.",
            "Recursive and generator constructs are rejected.",
        ])
    }

    async fn execute(&self, _context: BuiltinContext, arguments: JsonValue) -> BuiltinResult {
        let filter = arguments["filter"].as_str().unwrap_or_default();
        if filter.is_empty() || filter.len() > 4 * 1024 {
            return BuiltinResult::error("jq filter is empty or exceeds its limit");
        }
        if jq_filter_is_unbounded(filter) {
            return BuiltinResult::error("jq filter uses an unbounded construct");
        }
        let input = arguments.get("input").cloned().unwrap_or(JsonValue::Null);
        if serde_json::to_vec(&input).is_ok_and(|bytes| bytes.len() > MAX_TOOL_INPUT_BYTES) {
            return BuiltinResult::error("jq input limit exceeded");
        }
        match run_jq(filter, input) {
            Ok(values) => BuiltinResult::text(json!({"values": values}).to_string()),
            Err(message) => BuiltinResult::error(message),
        }
    }
}

fn jq_filter_is_unbounded(filter: &str) -> bool {
    let unsafe_names = Regex::new(
        r"(^|[^[:alnum:]_])(def|recurse|repeat|while|until|range|input|inputs)([^[:alnum:]_]|$)",
    )
    .expect("static jq policy regex");
    unsafe_names.is_match(filter)
}

fn run_jq(code: &str, input: JsonValue) -> Result<Vec<JsonValue>, &'static str> {
    // THREAT[TM-DOS-008]: jq receives bounded JSON, rejects recursive and
    // generator constructs, and caps result count and serialized output.
    let arena = Arena::default();
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let loader = Loader::new(defs);
    let modules = loader
        .load(&arena, File { path: (), code })
        .map_err(|_| "invalid jq filter")?;
    let funs = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());
    let filter = Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|_| "invalid jq filter")?;
    let input: Val = serde_json::from_value(input).map_err(|_| "invalid jq input")?;
    let context = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let mut values = Vec::new();
    let mut output_bytes = 0usize;
    for result in filter.id.run((context, input)).take(MAX_JQ_RESULTS + 1) {
        if values.len() == MAX_JQ_RESULTS {
            return Err("jq result count limit exceeded");
        }
        let value = result.map_err(|_| "jq evaluation failed")?;
        let value: JsonValue =
            serde_json::from_str(&value.to_string()).map_err(|_| "jq produced a non-JSON value")?;
        let size = serde_json::to_vec(&value)
            .map_err(|_| "jq output conversion failed")?
            .len();
        output_bytes = output_bytes
            .checked_add(size)
            .ok_or("jq output limit exceeded")?;
        if output_bytes > MAX_TOOL_OUTPUT_BYTES {
            return Err("jq output limit exceeded");
        }
        values.push(value);
    }
    Ok(values)
}
