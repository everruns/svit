use svit::{Error, Process, Script, Value, value};

const COUNTER: &str = r#"
module demos/counter

fn integer_or(found: Result<String, String>, fallback: Int) -> Int {
    match found {
        ok(text) => match to_int(text) {
            ok(value) => value,
            err(why) => fallback,
        },
        err(why) => fallback,
    }
}

fn main(sys: System) -> Int
  uses
    Io.env,
    Io.write,
{
    let before = integer_or(Io.env(sys, "/memory/count"), 0)
    let by = integer_or(Io.env(sys, "/input/by"), 0)
    let count = before + by
    Io.write(sys.console, "set-integer\t/memory/count\t" + to_string(count))
    count
}
"#;

const FAILING_AFTER_WRITE: &str = r#"
module demos/rollback

fn integer_or(found: Result<String, String>, fallback: Int) -> Int {
    match found {
        ok(text) => match to_int(text) {
            ok(value) => value,
            err(why) => fallback,
        },
        err(why) => fallback,
    }
}

fn positive(value: Int) -> Int
  where
    value > 0,
{
    value
}

fn main(sys: System) -> Int
  uses
    Io.env,
    Io.write,
{
    Io.write(sys.console, "set-integer\t/memory/changed\t1")
    positive(integer_or(Io.env(sys, "/input/value"), 0))
}
"#;

#[test]
fn deed_activation_commits_memory_once() {
    let mut process = Process::builder("svit://local/tests/deed-counter")
        .unwrap()
        .memory("count", value!(4))
        .library("counter", Script::deed(COUNTER))
        .build()
        .unwrap();

    let activation = process.exec("/lib/counter", value!({"by": 3})).unwrap();

    assert_eq!(activation.output, value!(7));
    assert_eq!(activation.version, 1);
    assert_eq!(activation.changed, ["/memory/count"]);
    assert_eq!(process.read("/memory/count").unwrap(), Some(value!(7)));
}

#[test]
fn deed_contract_failure_rolls_back_memory_and_version() {
    let mut process = Process::builder("svit://local/tests/deed-rollback")
        .unwrap()
        .memory("changed", value!(0))
        .library("rollback", Script::deed(FAILING_AFTER_WRITE))
        .build()
        .unwrap();
    let before = process.snapshot().unwrap();

    let error = process
        .exec("/lib/rollback", value!({"value": -1}))
        .unwrap_err();

    assert!(
        matches!(&error, Error::Script(message) if message.contains("DEED6002")),
        "unexpected Deed failure: {error:?}"
    );
    assert_eq!(process.snapshot().unwrap(), before);
    assert_eq!(process.version(), 0);
    assert_eq!(process.read("/memory/changed").unwrap(), Some(value!(0)));
}

#[test]
fn deed_source_can_be_staged_as_a_named_script() {
    let mut process = Process::new("svit://local/tests/deed-staged").unwrap();

    process
        .write(
            "/lib/counter",
            value!({
                "language": "deed@0.2.12",
                "source": COUNTER,
                "documentation": "Adds input.by to /memory/count."
            }),
        )
        .unwrap();
    process.write("/memory/count", value!(10)).unwrap();

    let activation = process.exec("/lib/counter", value!({"by": 2})).unwrap();

    assert_eq!(activation.output, value!(12));
    assert_eq!(process.read("/memory/count").unwrap(), Some(value!(12)));
    assert_eq!(
        process.read("/lib/counter").unwrap(),
        Some(Value::Script(
            Script::deed(COUNTER).with_documentation("Adds input.by to /memory/count.")
        ))
    );
}

#[test]
fn deed_text_and_remove_intents_commit_together() {
    let source = r#"
module demos/text

fn text_or(found: Result<String, String>, fallback: String) -> String {
    match found {
        ok(text) => text,
        err(why) => fallback,
    }
}

fn main(sys: System) -> Int
  uses
    Io.env,
    Io.write,
{
    let name = text_or(Io.env(sys, "/input/name"), "unknown")
    Io.write(sys.console, "set-text\t/memory/name\t" + name)
    Io.write(sys.console, "remove\t/memory/old")
    1
}
"#;
    let mut process = Process::builder("svit://local/tests/deed-text-remove")
        .unwrap()
        .memory("old", value!(true))
        .library("text", Script::deed(source))
        .build()
        .unwrap();

    let activation = process.exec("/lib/text", value!({"name": "Ada"})).unwrap();

    assert_eq!(activation.output, value!(1));
    assert_eq!(
        activation.changed,
        ["/memory/name".to_string(), "/memory/old".to_string()]
    );
    assert_eq!(process.read("/memory/name").unwrap(), Some(value!("Ada")));
    assert_eq!(process.read("/memory/old").unwrap(), None);
}

#[test]
fn malformed_deed_output_rolls_back_the_activation() {
    let source = r#"
module demos/output

fn main(sys: System) -> Int
  uses
    Io.write,
{
    Io.write(sys.console, "ordinary console output")
    1
}
"#;
    let mut process = Process::builder("svit://local/tests/deed-output")
        .unwrap()
        .memory("kept", value!(true))
        .library("output", Script::deed(source))
        .build()
        .unwrap();
    let before = process.snapshot().unwrap();

    let error = process.exec("/lib/output", Value::Null).unwrap_err();

    assert!(matches!(error, Error::Script(message) if message.contains("mutation intent")));
    assert_eq!(process.snapshot().unwrap(), before);
}

#[test]
fn invalid_deed_source_is_rejected_before_it_enters_the_library() {
    let mut process = Process::new("svit://local/tests/deed-invalid").unwrap();
    let before = process.snapshot().unwrap();

    let error = process
        .write(
            "/lib/broken",
            value!({
                "language": "deed@0.2.12",
                "source": "module broken\n\nfn main( -> Int { 1 }"
            }),
        )
        .unwrap_err();

    assert!(matches!(error, Error::Script(message) if message.contains("/lib/broken.deed")));
    assert_eq!(process.snapshot().unwrap(), before);
    assert!(process.read("/lib/broken").unwrap().is_none());
}

#[test]
fn deed_imports_outside_the_svit_grants_are_rejected() {
    // THREAT[TM-ESC-005]: Deed source cannot obtain clock authority merely by
    // naming the standard operation in its effect row.
    let clock = r#"
module demos/clock

fn main(sys: System) -> Int
  uses
    Io.now,
{
    Io.now(sys.clock)
}
"#;
    let mut process = Process::new("svit://local/tests/deed-clock-denied").unwrap();
    let before = process.snapshot().unwrap();

    let error = process
        .write(
            "/lib/clock",
            value!({"language": "deed@0.2.12", "source": clock}),
        )
        .unwrap_err();

    assert!(
        matches!(error, Error::Script(message) if message.contains("host import") && message.contains("not available"))
    );
    assert_eq!(process.snapshot().unwrap(), before);
}
