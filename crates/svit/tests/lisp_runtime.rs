use svit::{Process, Value, value};

#[test]
fn lisp_activation_commits_memory_and_returns_a_map() {
    let mut process = Process::builder("svit://local/tests/lisp-counter")
        .unwrap()
        .memory("count", value!(0))
        .build()
        .unwrap();
    process
        .save_script(
            "counter",
            r#"
            (define (main input)
              (let ((count (+ (memory-get "/count")
                              (value-get input "/by"))))
                (do
                  (memory-set! "/count" count)
                  (log-info! "counted" (value-map "count" count))
                  (value-map "count" count
                             "missing-is-null" (value-null? (memory-get "/missing"))))))
            "#,
        )
        .unwrap();

    let activation = process.exec("counter", value!({"by": 2})).unwrap();

    assert_eq!(
        activation.output,
        value!({"count": 2, "missing-is-null": true})
    );
    assert_eq!(activation.logs[0].fields, value!({"count": 2}));
    assert_eq!(
        process.get("/memory/count").unwrap(),
        Some(&Value::Integer(2))
    );
}
