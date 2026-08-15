use svit::{Error, Process, Script, value};

const WRITE_THEN_FAIL: &str = r#"
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

fn main() -> svit::Result<()> {
    let mut process = Process::builder("svit://local/examples/deed-rollback")?
        .memory("changed", value!(0))
        .library("rollback", Script::deed(WRITE_THEN_FAIL))
        .build()?;
    let before = process.snapshot()?;

    let error = process.exec("/lib/rollback", value!({"value": -1}));
    assert!(matches!(
        error,
        Err(Error::Script(message)) if message.contains("DEED6002")
    ));
    assert_eq!(process.snapshot()?, before);
    assert_eq!(process.version(), 0);
    assert_eq!(process.read("/memory/changed")?, Some(value!(0)));

    println!("deed_rollback contract=failed count=unchanged version=0");
    Ok(())
}
