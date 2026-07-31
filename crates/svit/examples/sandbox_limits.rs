use svit::{Error, Limits, Process, Value, value};

const INSPECT_SANDBOX: &str = r#"
function main()
    return {
        debug = type(debug),
        io = type(io),
        loadstring = type(loadstring),
        os = type(os),
        package = type(package),
        random = type(math.random),
        require = type(require),
    }
end
"#;

fn main() -> svit::Result<()> {
    let mut inspector = Process::new("svit://local/examples/sandbox-inspector")?;
    inspector.save_script("inspect", INSPECT_SANDBOX)?;
    let inspection = inspector.run("inspect", Value::Null)?;
    assert_eq!(
        inspection.output,
        value!({
            "debug": "nil",
            "io": "nil",
            "loadstring": "nil",
            "os": "nil",
            "package": "nil",
            "random": "nil",
            "require": "nil"
        })
    );

    let limits = Limits {
        max_interrupt_ticks: 10,
        ..Limits::default()
    };
    let mut bounded = Process::builder("svit://local/examples/bounded-loop")?
        .limits(limits)
        .memory(value!({"started": false}))
        .build()?;
    bounded.save_script(
        "loop",
        "function main() memory.started = true while true do end end",
    )?;
    let version_before = bounded.version();

    let failure = bounded.run("loop", Value::Null);
    assert!(matches!(failure, Err(Error::ExecutionLimitExceeded)));
    assert_eq!(bounded.version(), version_before);
    assert_eq!(bounded.read("/memory/started")?, Some(&Value::Bool(false)));

    println!("sandbox_limits ambient=denied loop=stopped state=unchanged");
    Ok(())
}
