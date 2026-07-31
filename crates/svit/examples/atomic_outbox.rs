use svit::{Error, Process, Value, value};

const PAYMENT: &str = r#"
function main(input)
    memory.balance = memory.balance - input.amount
    send("svit://local/examples/merchant", {
        kind = "payment",
        amount = input.amount,
    })
    if input.fail then
        error("payment declined")
    end
    return memory.balance
end
"#;

fn main() -> svit::Result<()> {
    let mut payer = Process::builder("svit://local/examples/payer")?
        .memory(value!({"balance": 100}))
        .build()?;
    payer.save_script("payment", PAYMENT)?;
    let version_before = payer.version();

    let failure = payer.run("payment", value!({"amount": 25, "fail": true}));
    assert!(matches!(failure, Err(Error::Script(_))));
    assert_eq!(payer.version(), version_before);
    assert_eq!(payer.read("/memory/balance")?, Some(&Value::Integer(100)));
    assert!(payer.outbox()?.is_empty());

    let committed = payer.run("payment", value!({"amount": 25, "fail": false}))?;
    assert_eq!(committed.output, Value::Integer(75));
    assert_eq!(committed.messages.len(), 1);
    assert_eq!(
        committed.messages[0].message_id,
        "svit://local/examples/payer:2:0"
    );
    assert_eq!(
        committed.messages[0].body,
        value!({"amount": 25, "kind": "payment"})
    );
    assert_eq!(payer.outbox()?, committed.messages);

    println!("atomic_outbox balance=75 messages=1 version=2");
    Ok(())
}
