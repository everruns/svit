use svit::{Error, Limits, Process, Value, value};

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

    let activation = process.exec("/lib/counter", value!({"by": 2})).unwrap();

    assert_eq!(
        activation.output,
        value!({"count": 2, "missing-is-null": true})
    );
    assert_eq!(activation.logs[0].fields, value!({"count": 2}));
    assert_eq!(
        process.read("/memory/count").unwrap(),
        Some(Value::Integer(2))
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
                          (nested (exec "/lib/increment" input)))
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

    let activation = process
        .exec("/lib/orchestrator", value!({"by": 2}))
        .unwrap();

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
        Some(Value::Integer(2))
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
                     (do (write "/memory/outer" true) (exec "/lib/inner" input)))"#,
            ),
        )
        .build()
        .unwrap();
    let before = process.snapshot().unwrap();

    assert!(process.exec("/lib/outer", Value::Null).is_err());
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
            .exec("/lib/copy", value!({"copied": true}))
            .unwrap()
            .output,
        value!({"copied": true})
    );
    process.remove("/lib/copy").unwrap();
    assert!(process.read("/lib/copy").unwrap().is_none());
}

#[test]
fn json_and_map_builtins_expose_structured_values() {
    let mut process = Process::builder("svit://local/tests/structured-values")
        .unwrap()
        .library(
            "structured-values",
            svit::Script::new(r#"
            (define (main input)
              (list
                (map? (json-parse "{\"type\":\"tool\"}"))
                (string? (map-get (json-parse "{\"type\":\"tool\"}") "type"))
                (map-has? (json-parse "{\"type\":\"tool\"}") "missing")
                (map-get (map-get (json-parse "{\"arguments\":{\"query\":\"billing\"}}") "arguments") "query")
                (list? (map-get (json-parse "{\"items\":[1,true,null]}") "items"))
                (number? (list-get (map-get (json-parse "{\"items\":[1,true,null]}") "items") 0))
                (boolean? (list-get (map-get (json-parse "{\"items\":[1,true,null]}") "items") 1))
                (null? (list-get (map-get (json-parse "{\"items\":[1,true,null]}") "items") 2))
                (map? (map-set (value-map "query" "billing") "limit" 3))
                (json-stringify (map-set (value-map "query" "billing") "limit" 3))))
            "#),
        )
        .build()
        .unwrap();

    let result = process.exec("/lib/structured-values", Value::Null).unwrap();
    assert_eq!(
        result.output,
        value!([
            true,
            true,
            false,
            "billing",
            true,
            true,
            true,
            true,
            true,
            r#"{"limit":3,"query":"billing"}"#
        ])
    );
}

#[test]
fn safe_builtins_return_result_values_without_aborting_activation() {
    let mut process = Process::builder("svit://local/tests/safe-results")
        .unwrap()
        .library(
            "safe-results",
            svit::Script::new(
                r#"
            (define (main input)
              (let ((parsed (json-parse-safe "not-json"))
                    (missing (map-get-safe (value-map "present" 1) "missing")))
                (list
                  (map-get parsed "ok")
                  (string? (map-get parsed "error"))
                  (map-get missing "ok")
                  (string? (map-get missing "error")))))
            "#,
            ),
        )
        .build()
        .unwrap();

    let result = process.exec("/lib/safe-results", Value::Null).unwrap();
    assert_eq!(result.output, value!([false, true, false, true]));
}

#[test]
fn explicit_function_dispatch_uses_validated_values() {
    let mut process = Process::builder("svit://local/tests/dispatch")
        .unwrap()
        .library("dispatch", svit::Script::new(r#"
            (define search (lambda (arguments) (map-get arguments "query")))
            (define finish (lambda (arguments) (map-get arguments "answer")))
            (define (main input)
              ((if (= (map-get (json-parse "{\"type\":\"search\",\"arguments\":{\"query\":\"refund\"}}") "type") "search")
                    search
                    finish)
               (map-get (json-parse "{\"type\":\"search\",\"arguments\":{\"query\":\"refund\"}}") "arguments")))
            "#))
        .build()
        .unwrap();

    let result = process.exec("/lib/dispatch", Value::Null).unwrap();
    assert_eq!(result.output, Value::String("refund".into()));
}

#[test]
fn safe_call_catches_guest_errors_and_returns_success_values() {
    let mut process = Process::builder("svit://local/tests/safe-call")
        .unwrap()
        .library(
            "safe-call",
            svit::Script::new(
                r#"
            (define (main input)
              (let ((failed (safe-call (lambda () (map-get (value-map) "missing"))))
                    (succeeded (safe-call (lambda (value) (+ value 1)) 4)))
                (list
                  (map-get failed "ok")
                  (string? (map-get failed "error"))
                  (map-get succeeded "ok")
                  (map-get succeeded "value"))))
            "#,
            ),
        )
        .build()
        .unwrap();

    let result = process.exec("/lib/safe-call", Value::Null).unwrap();
    assert_eq!(result.output, value!([false, true, true, 5]));
}

#[test]
fn safe_call_does_not_catch_resource_limits() {
    let mut process = Process::builder("svit://local/tests/safe-call-limit")
        .unwrap()
        .limits(Limits {
            max_call_stack: 8,
            ..Limits::default()
        })
        .library(
            "safe-call-limit",
            svit::Script::new(
                r#"
            (define (descend n)
              (if (= n 0) 0 (+ 1 (descend (- n 1)))))
            (define (main input)
              (safe-call descend 100))
            "#,
            ),
        )
        .build()
        .unwrap();

    assert!(matches!(
        process.exec("/lib/safe-call-limit", Value::Null),
        Err(Error::ResourceLimitExceeded(limit)) if limit == "call stack"
    ));
}

#[test]
fn runtime_builtins_catalog_describes_guest_helpers() {
    let mut process = Process::builder("svit://local/tests/runtime-builtins")
        .unwrap()
        .library(
            "runtime-builtins",
            svit::Script::new(
                r#"
            (define (main input)
              (let ((catalog (runtime-builtins)))
                (list
                  (list? catalog)
                  (map-get (list-get catalog 0) "name")
                  (string? (map-get (list-get catalog 0) "signature"))
                  (string? (map-get (list-get catalog 0) "description"))
                  (map-get (list-get catalog 0) "category")
                  (map-get (list-get catalog 30) "name"))))
            "#,
            ),
        )
        .build()
        .unwrap();

    let result = process.exec("/lib/runtime-builtins", Value::Null).unwrap();
    assert_eq!(
        result.output,
        value!([
            true,
            "runtime-builtins",
            true,
            true,
            "discovery",
            "safe-call"
        ])
    );
}
