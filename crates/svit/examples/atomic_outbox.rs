use svit::{Error, Process, Value, value};

const PAYMENT: &str = r#"
(define (main input)
  (let ((amount (value-get input "/amount"))
        (balance (- (read "/memory/balance") (value-get input "/amount"))))
    (do
      (write "/memory/balance" balance)
      (send! "svit://local/examples/merchant"
             (value-map "kind" "payment" "amount" amount))
      (if (value-get input "/fail")
          (panic "payment declined")
          balance))))
"#;

fn main() -> svit::Result<()> {
    let mut payer = Process::builder("svit://local/examples/payer")?
        .memory("balance", value!(100))
        .build()?;
    payer.write("/lib/payment", value!({"source": PAYMENT}))?;
    let version_before = payer.version();

    let failure = payer.exec("payment", value!({"amount": 25, "fail": true}));
    assert!(matches!(failure, Err(Error::Script(_))));
    assert_eq!(payer.version(), version_before);
    assert_eq!(payer.read("/memory/balance")?, Some(&Value::Integer(100)));
    assert!(payer.outbox()?.is_empty());

    let committed = payer.exec("payment", value!({"amount": 25, "fail": false}))?;
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
