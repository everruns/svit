use svit::{Process, Value, value};

#[test]
fn lisp_activation_commits_memory_and_returns_a_map() {
    let mut process = Process::builder("svit://local/tests/lisp-counter")
        .unwrap()
        .memory("count", value!(0))
        .build()
        .unwrap();
    process
        .write(
            "/lib/counter",
            value!({"source": r#"
            (define (main input)
              (let ((count (+ (read "/memory/count")
                              (value-get input "/by"))))
                (do
                  (write "/memory/count" count)
                  (log-info! "counted" (value-map "count" count))
                  (value-map "count" count
                             "missing-is-null" (value-null? (read "/memory/missing"))))))
            "#}),
        )
        .unwrap();

    let activation = process.exec("counter", value!({"by": 2})).unwrap();

    assert_eq!(
        activation.output,
        value!({"count": 2, "missing-is-null": true})
    );
    assert_eq!(activation.logs[0].fields, value!({"count": 2}));
    assert_eq!(
        process.read("/memory/count").unwrap(),
        Some(&Value::Integer(2))
    );
}

#[test]
fn generic_operations_share_absolute_process_paths_inside_lisp() {
    let mut process = Process::builder("svit://local/tests/generic-operations")
        .unwrap()
        .memory("count", value!(0))
        .library(
            "orchestrator",
            svit::Script::new(
                r#"
                (define (main input)
                  (do
                    (write "/memory/draft" true)
                    (write
                      "/lib/increment"
                      (value-map
                        "source"
                        "(define (main input) (let ((next (+ (read \"/memory/count\") (value-get input \"/by\")))) (do (write \"/memory/count\" next) next)))"
                        "documentation"
                        "Increment the durable count"))
                    (let ((before (discover "/memory"))
                          (nested (exec "increment" input)))
                      (do
                        (remove "/memory/draft")
                        (remove "/lib/increment")
                        (value-map
                          "before" before
                          "nested" nested
                          "identity" (read "/system/identity/address"))))))
                "#,
            ),
        )
        .build()
        .unwrap();

    let activation = process.exec("orchestrator", value!({"by": 2})).unwrap();

    assert_eq!(
        activation.output,
        value!({
            "before": ["count", "draft"],
            "nested": 2,
            "identity": "svit://local/tests/generic-operations"
        })
    );
    assert_eq!(
        process.read("/memory/count").unwrap(),
        Some(&Value::Integer(2))
    );
    assert_eq!(process.read("/memory/draft").unwrap(), None);
    assert_eq!(process.read("/lib/increment").unwrap(), None);
}

#[test]
// THREAT[TM-EFF-001]
fn failed_nested_exec_rolls_back_the_complete_activation() {
    let mut process = Process::builder("svit://local/tests/nested-rollback")
        .unwrap()
        .memory("changed", value!(false))
        .library(
            "inner",
            svit::Script::new(
                r#"(define (main input)
                     (do (write "/memory/changed" true) (panic "inner failed")))"#,
            ),
        )
        .library(
            "outer",
            svit::Script::new(
                r#"(define (main input)
                     (do (write "/memory/outer" true) (exec "inner" input)))"#,
            ),
        )
        .build()
        .unwrap();
    let before = process.snapshot().unwrap();

    assert!(process.exec("outer", Value::Null).is_err());
    assert_eq!(process.snapshot().unwrap(), before);
}

#[test]
fn library_reads_can_be_written_back_through_the_generic_value_boundary() {
    let mut process = Process::builder("svit://local/tests/library-round-trip")
        .unwrap()
        .library(
            "original",
            svit::Script::new("(define (main input) input)")
                .with_documentation("Returns its input"),
        )
        .build()
        .unwrap();
    let visible = process.read("/lib/original").unwrap().unwrap().to_json();

    process
        .write("/lib/copy", Value::from_json(visible).unwrap())
        .unwrap();

    assert_eq!(
        process
            .exec("copy", value!({"copied": true}))
            .unwrap()
            .output,
        value!({"copied": true})
    );
    process.remove("/lib/copy").unwrap();
    assert!(process.read("/lib/copy").unwrap().is_none());
}
