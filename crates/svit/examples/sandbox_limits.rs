use svit::{Error, Limits, Process, Value, value};

fn main() -> svit::Result<()> {
    let mut inspector = Process::new("svit://local/examples/sandbox-inspector")?;
    assert!(matches!(
        inspector.save_script("inspect", "(use random) (define (main input) true)"),
        Err(Error::Script(_))
    ));

    let limits = Limits {
        max_execution_millis: 1,
        ..Limits::default()
    };
    let mut bounded = Process::builder("svit://local/examples/bounded-loop")?
        .limits(limits)
        .memory("started", value!(false))
        .build()?;
    bounded.save_script(
        "loop",
        r#"
        (define (spin) (spin))
        (define (main input) (do (memory-set! "/started" true) (spin)))
        "#,
    )?;
    let version_before = bounded.version();

    let failure = bounded.exec("loop", Value::Null);
    assert!(matches!(failure, Err(Error::ExecutionLimitExceeded)));
    assert_eq!(bounded.version(), version_before);
    assert_eq!(bounded.get("/memory/started")?, Some(&Value::Bool(false)));

    println!("sandbox_limits ambient=denied loop=stopped state=unchanged");
    Ok(())
}
