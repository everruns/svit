use svit::{ControlOutcome, ControlRequest, Process, ProcessController, ProcessId, Value, value};

fn main() -> svit::Result<()> {
    let process_id = "svit://local/example/shared-counter";
    let mut process = Process::builder(process_id)?
        .memory("count", value!(0))
        .build()?;
    process.write(
        "/lib/counter",
        value!({"source": r#"
        (define (main input)
          (let ((count (+ (read "/memory/count") (value-get input "/by"))))
            (do
              (write "/memory/count" count)
              (value-map "count" count))))
        "#}),
    )?;
    let controller = ProcessController::new(process);
    let process_id = ProcessId::new(process_id)?;

    let first = controller.execute(ControlRequest::activate(
        "agent-a",
        "a-1",
        process_id.clone(),
        1,
        "counter",
        value!({"by": 1}),
    )?)?;
    assert!(matches!(first.outcome, ControlOutcome::Committed { .. }));

    let stale = controller.execute(ControlRequest::activate(
        "agent-b",
        "b-1",
        process_id.clone(),
        1,
        "counter",
        value!({"by": 2}),
    )?)?;
    assert!(matches!(stale.outcome, ControlOutcome::Conflict { .. }));

    let retry = controller.execute(ControlRequest::activate(
        "agent-b",
        "b-2",
        process_id,
        2,
        "counter",
        value!({"by": 2}),
    )?)?;
    assert!(matches!(retry.outcome, ControlOutcome::Committed { .. }));

    let observation = controller.observe()?;
    let restored = Process::restore(&controller.snapshot()?)?;
    assert_eq!(restored.read("/memory/count")?, Some(Value::Integer(3)));
    assert_eq!(observation.version, 3);

    println!(
        "multi_client_control committed=2 conflict=1 count=3 version={}",
        observation.version
    );
    Ok(())
}
