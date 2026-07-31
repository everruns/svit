use svit::{Process, Value, value};

const COUNTER: &str = r#"
function main(input)
    memory.count = (memory.count or 0) + input.by
    return { count = memory.count }
end
"#;

fn main() -> svit::Result<()> {
    let mut process = Process::builder("svit://local/examples/counter")?
        .memory(value!({"count": 0}))
        .build()?;
    process.save_script("counter", COUNTER)?;

    let first = process.run("counter", value!({"by": 2}))?;
    let second = process.run("counter", value!({"by": 3}))?;
    assert_eq!(first.output, value!({"count": 2}));
    assert_eq!(second.output, value!({"count": 5}));

    let snapshot = process.snapshot()?;
    let mut restored = Process::restore(&snapshot)?;
    assert_eq!(restored.version(), 3);
    assert_eq!(restored.read("/memory/count")?, Some(&Value::Integer(5)));

    let third = restored.run("counter", value!({"by": 4}))?;
    assert_eq!(third.output, value!({"count": 9}));
    assert_eq!(third.version, 4);

    println!("durable_counter count=9 version=4");
    Ok(())
}
