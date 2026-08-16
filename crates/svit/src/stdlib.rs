//! Pure Svit Lisp standard-library functions.

use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx, Vars, data};
use jaq_json::Val;
use regex::Regex;
use serde_json::Value as JsonValue;

use crate::limits::DEFAULT_MAX_TEXT_BYTES;
use crate::{Error, Limits, Result, Value};

const MAX_JQ_FILTER_BYTES: usize = 4 * 1024;
const MAX_JQ_VALUE_BYTES: usize = DEFAULT_MAX_TEXT_BYTES;
const MAX_JQ_RESULTS: usize = 100;

/// Evaluates a bounded jq filter within the Svit Lisp runtime.
///
/// This is deliberately a runtime library function, not a port: it has no
/// host authority and only transforms the explicit Svit value it receives.
pub(crate) fn jq(filter: &str, input: &Value, limits: &Limits) -> Result<Value> {
    if filter.is_empty() || filter.len() > MAX_JQ_FILTER_BYTES {
        return Err(Error::Script(
            "jq filter is empty or exceeds its limit".into(),
        ));
    }
    if jq_filter_is_unbounded(filter) {
        return Err(Error::Script(
            "jq filter uses an unbounded construct".into(),
        ));
    }

    let input = decode_jq_input(input.to_json());

    let values = run_jq(filter, input)?;
    let output = Value::from_json(JsonValue::Array(values))?;
    output.validate(limits, false)?;
    Ok(output)
}

/// Decodes JSON text and HTTP's JSON response envelope before jq evaluates it.
fn decode_jq_input(input: JsonValue) -> JsonValue {
    let input = decode_json_text(input);
    let JsonValue::Object(response) = &input else {
        return input;
    };
    let Some(body) = response.get("body").and_then(JsonValue::as_str) else {
        return input;
    };
    if !response.get("status").is_some_and(JsonValue::is_u64)
        || !response.get("headers").is_some_and(JsonValue::is_object)
    {
        return input;
    }
    decode_json_text(JsonValue::String(body.to_owned()))
}

fn decode_json_text(input: JsonValue) -> JsonValue {
    match input {
        JsonValue::String(text) => serde_json::from_str(&text).unwrap_or(JsonValue::String(text)),
        value => value,
    }
}

fn jq_filter_is_unbounded(filter: &str) -> bool {
    let unsafe_names = Regex::new(
        r"(^|[^[:alnum:]_])(def|recurse|repeat|while|until|range|input|inputs)([^[:alnum:]_]|$)",
    )
    .expect("static jq policy regex");
    unsafe_names.is_match(filter)
}

fn run_jq(code: &str, input: JsonValue) -> Result<Vec<JsonValue>> {
    // THREAT[TM-DOS-008]: jq only receives bounded values, rejects recursive
    // and generator constructs, and caps the count and size of emitted values.
    let arena = Arena::default();
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let loader = Loader::new(defs);
    let modules = loader
        .load(&arena, File { path: (), code })
        .map_err(|_| Error::Script("invalid jq filter".into()))?;
    let funs = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());
    let filter = Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|_| Error::Script("invalid jq filter".into()))?;
    let input: Val =
        serde_json::from_value(input).map_err(|_| Error::Script("invalid jq input".into()))?;
    let context = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let mut values = Vec::new();
    let mut output_bytes = 0usize;
    for result in filter.id.run((context, input)).take(MAX_JQ_RESULTS + 1) {
        if values.len() == MAX_JQ_RESULTS {
            return Err(Error::Script("jq result count limit exceeded".into()));
        }
        let value = result.map_err(|_| Error::Script("jq evaluation failed".into()))?;
        let value: JsonValue = serde_json::from_str(&value.to_string())
            .map_err(|_| Error::Script("jq produced a non-JSON value".into()))?;
        let size = serde_json::to_vec(&value)
            .map_err(|_| Error::Script("jq output conversion failed".into()))?
            .len();
        output_bytes = output_bytes
            .checked_add(size)
            .ok_or_else(|| Error::Script("jq output limit exceeded".into()))?;
        if output_bytes > MAX_JQ_VALUE_BYTES {
            return Err(Error::Script("jq output limit exceeded".into()));
        }
        values.push(value);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn jq_supports_common_array_selection_and_counting() {
        let result = jq(
            "[.tree[] | select(.type == \"blob\")] | length",
            &Value::from_json(json!({
                "tree": [
                    {"path": "README.md", "type": "blob"},
                    {"path": "src", "type": "tree"},
                    {"path": "src/lib.rs", "type": "blob"}
                ]
            }))
            .unwrap(),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(result, Value::from_json(json!([2])).unwrap());
    }

    #[test]
    fn jq_decodes_an_http_json_response_before_filtering() {
        let response = json!({
            "status": 200,
            "headers": {"content-type": "application/json"},
            "body": r#"{"tree":[{"path":"README.md","type":"blob"},{"path":"src","type":"tree"},{"path":"src/lib.rs","type":"blob"}]}"#,
        });

        let result = jq(
            "[.tree[] | select(.type == \"blob\")] | length",
            &Value::String(response.to_string()),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(result, Value::from_json(json!([2])).unwrap());
    }
}
