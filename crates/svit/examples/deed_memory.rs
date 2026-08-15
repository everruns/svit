use svit::{Process, Script, value};

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

fn main() -> svit::Result<()> {
    let mut process = Process::builder("svit://local/examples/deed-memory")?
        .memory("count", value!(4))
        .library(
            "counter",
            Script::deed(COUNTER).with_documentation("Adds input.by to /memory/count."),
        )
        .build()?;

    let activation = process.exec("/lib/counter", value!({"by": 3}))?;
    assert_eq!(activation.output, value!(7));
    assert_eq!(activation.changed, ["/memory/count"]);
    assert_eq!(process.read("/memory/count")?, Some(value!(7)));

    let snapshot = process.snapshot()?;
    let restored = Process::restore(&snapshot)?;
    assert_eq!(restored.read("/memory/count")?, Some(value!(7)));

    println!("deed_memory output=7 count=7 snapshot=restored");
    Ok(())
}
