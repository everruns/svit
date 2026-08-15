//! Experimental Deed guest boundary over the Svit activation working copy.
//!
//! Svit grants a synthetic environment containing the activation input and
//! `/memory` snapshot, then captures Deed console lines as mutation intents.
//! Intents are applied only after `main` succeeds. No OS environment or
//! console is exposed. The compiled-code runner is integration evidence, not
//! a production WebAssembly isolation boundary.

use std::cell::RefCell;
use std::rc::Rc;

use deed_codegen::{Grants, Module, Trap, Value as DeedValue};
use deed_diagnostics::{SourceMap, render_human};

use crate::error::sanitize_diagnostic;
use crate::process::RuntimeState;
use crate::{Error, Limits, Result, Value};

const STEPS_PER_MILLISECOND: u64 = 50_000;
const ALLOWED_IMPORTS: &[(&str, &str)] = &[
    ("deed:io", "env"),
    ("deed:io", "write"),
    ("deed:sys", "console"),
];

#[derive(Default)]
struct IntentCapture {
    intents: Vec<String>,
    bytes: usize,
    overflowed: bool,
}

impl IntentCapture {
    fn push(&mut self, line: &str, limits: &Limits) {
        let Some(bytes) = self.bytes.checked_add(line.len()) else {
            self.overflowed = true;
            return;
        };
        if self.intents.len() >= limits.max_value_entries || bytes > limits.max_text_bytes {
            self.overflowed = true;
            return;
        }
        self.bytes = bytes;
        self.intents.push(line.into());
    }
}

pub(crate) fn validate(name: &str, source: &str) -> Result<()> {
    compile(name, source).map(|_| ())
}

pub(crate) fn run(
    state: &RuntimeState,
    name: &str,
    source: &str,
    input: &Value,
    limits: &Limits,
) -> Result<Value> {
    state.remaining_time()?;
    let module = compile(name, source)?;
    let initial_memory = module.memory_pages.unwrap_or(0) as usize * 64 * 1024;
    if initial_memory > limits.max_guest_memory {
        return Err(Error::ResourceLimitExceeded("guest memory"));
    }

    let intents = Rc::new(RefCell::new(IntentCapture::default()));
    let captured = Rc::clone(&intents);
    let capture_limits = limits.clone();
    let environment = activation_environment(state, input)?;
    // THREAT[TM-ESC-005]: These are synthetic, activation-local grants. The
    // compiled module is also rejected if it imports anything else.
    let granted = Grants::none()
        .environment(environment)
        .console(move |line| captured.borrow_mut().push(line, &capture_limits))
        .into_host();
    let linked = granted
        .host
        .link(&module)
        .map_err(|error| Error::Script(sanitize_diagnostic(error)))?;
    let budget = limits
        .max_execution_millis
        .saturating_mul(STEPS_PER_MILLISECOND)
        .max(1);
    let output = linked
        .call_within("main", &[granted.system], budget)
        .map_err(map_trap)?;
    state.remaining_time()?;

    let intents = intents.borrow();
    if intents.overflowed {
        return Err(Error::ResourceLimitExceeded("Deed mutation intents"));
    }
    for intent in &intents.intents {
        apply_intent(state, intent)?;
    }

    match output {
        Some(DeedValue::I64(value)) => Ok(Value::Integer(value)),
        Some(DeedValue::I32(value)) => Ok(Value::Bool(value != 0)),
        None => Ok(Value::Null),
    }
}

fn compile(name: &str, source: &str) -> Result<Module> {
    let mut sources = SourceMap::new();
    let file = sources.add(format!("/lib/{name}.deed"), source.to_string());
    let mut checks = deed_driver::check_all(&sources, &[file]);
    let checked = checks.pop().expect("one Deed source produces one check");
    if let Some(diagnostic) = checked.diagnostics.iter().find(|item| item.is_error()) {
        return Err(Error::Script(sanitize_diagnostic(render_human(
            &sources, diagnostic,
        ))));
    }

    let lowered = deed_mir::lower(&checked.module, &checked.resolutions, &checked.types)
        .map_err(|error| Error::Script(sanitize_diagnostic(error)))?;
    let entry = lowered
        .entry
        .ok_or_else(|| Error::Script(format!("/lib/{name}.deed: script must define `main`")))?;
    let main = lowered.function(entry);
    if main.params.as_slice() != [deed_mir::Ty::Capability] || main.ret != deed_mir::Ty::Int {
        return Err(Error::Script(format!(
            "/lib/{name}.deed: `main` must take one `System` parameter and return `Int`"
        )));
    }

    let module = deed_codegen::compile(&lowered)
        .map_err(|error| Error::Script(sanitize_diagnostic(error)))?;
    if let Some(import) = module
        .imports
        .iter()
        .find(|import| !ALLOWED_IMPORTS.contains(&(import.module.as_str(), import.name.as_str())))
    {
        return Err(Error::Script(format!(
            "/lib/{name}.deed: host import `{}.{}` is not available to Svit scripts",
            import.module, import.name
        )));
    }
    Ok(module)
}

fn activation_environment(state: &RuntimeState, input: &Value) -> Result<Vec<(String, String)>> {
    let memory = state
        .read_memory("/memory")?
        .expect("the activation working copy always has /memory");
    let mut environment = Vec::new();
    flatten_value("/input", input, &mut environment);
    flatten_value("/memory", &memory, &mut environment);
    Ok(environment)
}

fn flatten_value(path: &str, value: &Value, output: &mut Vec<(String, String)>) {
    match value {
        Value::Map(fields) => {
            for (name, value) in fields {
                flatten_value(&format!("{path}/{name}"), value, output);
            }
        }
        Value::Array(items) => {
            for (index, value) in items.iter().enumerate() {
                flatten_value(&format!("{path}/{index}"), value, output);
            }
        }
        Value::Null => output.push((path.into(), "null".into())),
        Value::Bool(value) => output.push((path.into(), value.to_string())),
        Value::Integer(value) => output.push((path.into(), value.to_string())),
        Value::Number(value) => output.push((path.into(), value.to_string())),
        Value::String(value) => output.push((path.into(), value.clone())),
        Value::Script(_) => {}
    }
}

fn apply_intent(state: &RuntimeState, intent: &str) -> Result<()> {
    if let Some(rest) = intent.strip_prefix("set-integer\t") {
        let (path, value) = rest.split_once('\t').ok_or_else(|| {
            Error::Script("Deed set-integer intent needs a path and value".into())
        })?;
        let value = value
            .parse::<i64>()
            .map_err(|_| Error::Script("Deed set-integer intent has an invalid value".into()))?;
        return state.write_memory(path, Value::Integer(value));
    }
    if let Some(rest) = intent.strip_prefix("set-text\t") {
        let (path, value) = rest
            .split_once('\t')
            .ok_or_else(|| Error::Script("Deed set-text intent needs a path and value".into()))?;
        return state.write_memory(path, Value::String(value.into()));
    }
    if let Some(path) = intent.strip_prefix("remove\t") {
        return state.remove_memory(path);
    }
    Err(Error::Script(
        "Deed output must be a Svit mutation intent".into(),
    ))
}

fn map_trap(trap: Trap) -> Error {
    match trap {
        Trap::TooLong => Error::ExecutionLimitExceeded,
        Trap::OutOfBounds => Error::ResourceLimitExceeded("guest memory"),
        Trap::Failed { code, message, .. } => {
            Error::Script(sanitize_diagnostic(format!("{code}: {message}")))
        }
        other => Error::Script(sanitize_diagnostic(other)),
    }
}
