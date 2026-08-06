use svit::{Process, Value, value};

const COUNTER: &str = r#"
(define (main input)
  (let ((count (+ (read "/memory/count") (value-get input "/by"))))
    (do
      (write "/memory/count" count)
      (value-map "count" count))))
"#;

fn main() -> svit::Result<()> {
    let mut process = Process::builder("svit://local/examples/counter")?
        .memory("count", value!(0))
        .build()?;
    process.write("/lib/counter", value!({"source": COUNTER}))?;

    let first = process.exec("counter", value!({"by": 2}))?;
    let second = process.exec("counter", value!({"by": 3}))?;
    assert_eq!(first.output, value!({"count": 2}));
    assert_eq!(second.output, value!({"count": 5}));

    let snapshot = process.snapshot()?;
    let mut restored = Process::restore(&snapshot)?;
    assert_eq!(restored.version(), 3);
    assert_eq!(restored.read("/memory/count")?, Some(&Value::Integer(5)));

    let third = restored.exec("counter", value!({"by": 4}))?;
    assert_eq!(third.output, value!({"count": 9}));
    assert_eq!(third.version, 4);

    println!("durable_counter count=9 version=4");
    Ok(())
}
