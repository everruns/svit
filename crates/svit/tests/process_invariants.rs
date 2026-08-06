use svit::{Error, Limits, Process, Value, value};

fn unchanged(process: &Process) -> Vec<u8> {
    process.snapshot().expect("snapshot")
}

#[test]
// THREAT[TM-EFF-001]
fn invalid_staged_script_rolls_back_memory_scripts_outbox_and_version() {
    let mut process = Process::builder("svit://local/tests/staged-script")
        .unwrap()
        .memory(value!({"changed": false}))
        .build()
        .unwrap();
    process
        .save_script(
            "teacher",
            r#"
            (define (main input)
              (do
                (memory-set! "/changed" true)
                (send! "svit://local/tests/recipient" (value-map "accepted" true))
                (scripts-save! "broken" "(define (main input)")))
            "#,
        )
        .unwrap();
    let before = unchanged(&process);

    assert!(matches!(
        process.run("teacher", Value::Null),
        Err(Error::Script(_))
    ));
    assert_eq!(process.snapshot().unwrap(), before);
    assert!(process.script("broken").is_none());
    assert!(process.outbox().unwrap().is_empty());
}

#[test]
// THREAT[TM-ESC-002] THREAT[TM-EFF-001]
fn guest_function_values_are_rejected_atomically() {
    let mut process = Process::new("svit://local/tests/function-value").unwrap();
    process
        .save_script(
            "function-value",
            "(define (main input) (memory-set! \"/bad\" main))",
        )
        .unwrap();
    let before = unchanged(&process);

    assert!(matches!(
        process.run("function-value", Value::Null),
        Err(Error::InvalidValue(_))
    ));
    assert_eq!(process.snapshot().unwrap(), before);
}

#[test]
// THREAT[TM-DOS-002] THREAT[TM-EFF-001]
fn guest_memory_limit_fails_closed() {
    let limits = Limits {
        max_guest_memory: 128,
        ..Limits::default()
    };
    let mut process = Process::builder("svit://local/tests/guest-memory")
        .unwrap()
        .limits(limits)
        .build()
        .unwrap();
    let items = std::iter::repeat_n("\"xxxxxxxx\"", 256)
        .collect::<Vec<_>>()
        .join(" ");
    process
        .save_script(
            "allocate",
            format!("(define (main input) (value-array {items}))"),
        )
        .unwrap();
    let before = unchanged(&process);

    assert!(matches!(
        process.run("allocate", Value::Null),
        Err(Error::ResourceLimitExceeded("guest memory"))
    ));
    assert_eq!(process.snapshot().unwrap(), before);
}

#[test]
// THREAT[TM-DOS-001] THREAT[TM-DOS-003] THREAT[TM-EFF-001]
fn ketos_activation_limits_fail_without_committing() {
    let cases = [
        (
            "call-stack",
            Limits {
                max_call_stack: 8,
                ..Limits::default()
            },
            r#"(define (descend n)
                 (if (= n 0) 0 (+ 1 (descend (- n 1)))))
               (define (main input)
                 (do (memory-set! "/changed" true) (descend 100)))"#
                .to_owned(),
            "call stack",
        ),
        (
            "value-stack",
            Limits {
                max_value_stack: 16,
                ..Limits::default()
            },
            format!(
                "(define (main input) (do (memory-set! \"/changed\" true) (value-array {})))",
                std::iter::repeat_n("1", 64).collect::<Vec<_>>().join(" ")
            ),
            "value stack",
        ),
        (
            "namespace",
            Limits {
                max_namespace_entries: 16,
                ..Limits::default()
            },
            r#"(define a 1) (define b 2) (define c 3) (define d 4)
               (define e 5) (define f 6) (define g 7) (define h 8)
               (define (main input)
                 (do (memory-set! "/changed" true) true))"#
                .to_owned(),
            "guest namespace",
        ),
    ];

    for (name, limits, source, expected_limit) in cases {
        let mut process = Process::builder(format!("svit://local/tests/{name}"))
            .unwrap()
            .limits(limits)
            .memory(value!({"changed": false}))
            .build()
            .unwrap();
        process.save_script("exercise", source).unwrap();
        let before = unchanged(&process);

        assert!(matches!(
            process.run("exercise", Value::Null),
            Err(Error::ResourceLimitExceeded(limit)) if limit == expected_limit
        ));
        assert_eq!(process.snapshot().unwrap(), before);
    }
}

#[test]
// THREAT[TM-SNAP-001]
fn restore_rejects_lua_format_unknown_format_tampering_and_trailing_data() {
    let process = Process::builder("svit://local/tests/snapshot")
        .unwrap()
        .memory(value!({"count": 1}))
        .build()
        .unwrap();
    let snapshot = process.snapshot().unwrap();

    for format in [1, 999] {
        let mut unsupported: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
        unsupported["format"] = serde_json::json!(format);
        assert!(Process::restore(&serde_json::to_vec(&unsupported).unwrap()).is_err());
    }

    let mut tampered: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
    tampered["root"]["value"]["memory"]["value"]["count"]["value"] = serde_json::json!(2);
    let error = match Process::restore(&serde_json::to_vec(&tampered).unwrap()) {
        Ok(_) => panic!("tampered root must fail"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::InvalidSnapshot(message) if message == "root hash mismatch"));

    let mut trailing = snapshot;
    trailing.extend_from_slice(b"not-json");
    assert!(Process::restore(&trailing).is_err());
}

#[test]
// THREAT[TM-SNAP-001] THREAT[TM-MSG-001]
fn replay_from_the_same_snapshot_is_deterministic() {
    let mut original = Process::builder("svit://local/tests/replay")
        .unwrap()
        .memory(value!({"total": 0}))
        .build()
        .unwrap();
    original
        .save_script(
            "add",
            r#"
            (define (main input)
              (let ((total (+ (memory-get "/total") (value-get input "/amount"))))
                (do
                  (memory-set! "/total" total)
                  (log-info! "added" (value-map "total" total))
                  (send! "svit://local/tests/sink" (value-map "total" total))
                  total)))
            "#,
        )
        .unwrap();
    let snapshot = original.snapshot().unwrap();
    let mut first = Process::restore(&snapshot).unwrap();
    let mut second = Process::restore(&snapshot).unwrap();

    let first_result = first.run("add", value!({"amount": 4})).unwrap();
    let second_result = second.run("add", value!({"amount": 4})).unwrap();

    assert_eq!(first_result.output, second_result.output);
    assert_eq!(first_result.logs, second_result.logs);
    assert_eq!(first_result.messages, second_result.messages);
    assert_eq!(first_result.root_hash, second_result.root_hash);
    assert_eq!(first.snapshot().unwrap(), second.snapshot().unwrap());
}

#[test]
// THREAT[TM-ISO-001]
fn lisp_globals_do_not_cross_activation_boundaries() {
    let mut process = Process::new("svit://local/tests/globals").unwrap();
    process
        .save_script(
            "write",
            "(define rogue \"write\") (define (main input) rogue)",
        )
        .unwrap();
    process
        .save_script(
            "read",
            "(define rogue \"read\") (define (main input) rogue)",
        )
        .unwrap();

    process.run("write", Value::Null).unwrap();
    let observed = process.run("read", Value::Null).unwrap();
    assert_eq!(observed.output, Value::String("read".into()));
}

#[test]
// THREAT[TM-FORK-001]
fn fork_does_not_duplicate_parent_outbox_or_share_future_mutations() {
    let mut parent = Process::builder("svit://local/tests/parent")
        .unwrap()
        .memory(value!({"value": 0}))
        .build()
        .unwrap();
    parent
        .save_script(
            "emit",
            r#"
            (define (main input)
              (let ((value (value-get input "/value")))
                (do
                  (memory-set! "/value" value)
                  (send! "svit://local/tests/sink" (value-map "value" value)))))
            "#,
        )
        .unwrap();
    parent.run("emit", value!({"value": 1})).unwrap();

    let mut child = parent.fork("svit://local/tests/child").unwrap();
    assert_eq!(parent.outbox().unwrap().len(), 1);
    assert!(child.outbox().unwrap().is_empty());

    child.run("emit", value!({"value": 2})).unwrap();
    assert_eq!(
        parent.read("/memory/value").unwrap(),
        Some(&Value::Integer(1))
    );
    assert_eq!(
        child.read("/memory/value").unwrap(),
        Some(&Value::Integer(2))
    );
    assert_eq!(parent.outbox().unwrap().len(), 1);
    assert_eq!(child.outbox().unwrap().len(), 1);
}

#[test]
// THREAT[TM-INF-001]
fn diagnostics_are_capped_and_use_virtual_source_paths() {
    let mut process = Process::new("svit://local/tests/diagnostic").unwrap();
    let message = "x".repeat(4096);
    process
        .save_script(
            "fail",
            format!("(define (main input) (panic \"{message}\"))"),
        )
        .unwrap();

    let diagnostic = process.run("fail", Value::Null).unwrap_err().to_string();
    assert!(diagnostic.len() <= 1100);
    assert!(diagnostic.contains("/lib/fail.ket"));
    assert!(!diagnostic.contains(env!("CARGO_MANIFEST_DIR")));
    assert!(!diagnostic.contains("backtrace"));
}

#[test]
// THREAT[TM-DOS-003]
fn untrusted_snapshot_limits_cannot_exceed_hard_maxima() {
    let process = Process::new("svit://local/tests/limits").unwrap();
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&process.snapshot().unwrap()).unwrap();
    snapshot["limits"]["max_value_depth"] = serde_json::json!(10000);

    assert!(matches!(
        Process::restore(&serde_json::to_vec(&snapshot).unwrap()),
        Err(Error::InvalidLimits(_))
    ));
}

#[test]
// THREAT[TM-DOS-003] THREAT[TM-EFF-001]
fn buffered_resource_limits_fail_without_committing() {
    let cases = [
        (
            "logs",
            Limits {
                max_logs: 0,
                ..Limits::default()
            },
            "(define (main input) (do (memory-set! \"/changed\" true) (log-info! \"x\")))",
            "log records",
        ),
        (
            "messages",
            Limits {
                max_messages: 0,
                ..Limits::default()
            },
            r#"(define (main input)
                (do
                  (memory-set! "/changed" true)
                  (send! "svit://local/tests/sink" (value-map))))"#,
            "message intents",
        ),
        (
            "scripts",
            Limits {
                max_staged_scripts: 0,
                ..Limits::default()
            },
            r#"(define (main input)
                (do
                  (memory-set! "/changed" true)
                  (scripts-save! "child" "(define (main input) ())")))"#,
            "staged scripts",
        ),
    ];

    for (name, limits, source, expected_limit) in cases {
        let mut process = Process::builder(format!("svit://local/tests/{name}"))
            .unwrap()
            .limits(limits)
            .memory(value!({"changed": false}))
            .build()
            .unwrap();
        process.save_script("exercise", source).unwrap();
        let before = unchanged(&process);

        assert!(matches!(
            process.run("exercise", Value::Null),
            Err(Error::ResourceLimitExceeded(limit)) if limit == expected_limit
        ));
        assert_eq!(process.snapshot().unwrap(), before);
    }
}

#[test]
// THREAT[TM-DOS-003]
fn persistent_value_and_script_limits_fail_at_the_host_boundary() {
    let text_limits = Limits {
        max_text_bytes: 3,
        ..Limits::default()
    };
    assert!(matches!(
        Process::builder("svit://local/tests/text")
            .unwrap()
            .limits(text_limits)
            .memory(value!({"key": "too long"}))
            .build(),
        Err(Error::InvalidValue(_))
    ));

    let entry_limits = Limits {
        max_value_entries: 2,
        ..Limits::default()
    };
    assert!(matches!(
        Process::builder("svit://local/tests/entries")
            .unwrap()
            .limits(entry_limits)
            .memory(value!({"a": 1, "b": 2}))
            .build(),
        Err(Error::InvalidValue(_))
    ));

    let depth_limits = Limits {
        max_value_depth: 2,
        ..Limits::default()
    };
    assert!(matches!(
        Process::builder("svit://local/tests/depth")
            .unwrap()
            .limits(depth_limits)
            .memory(value!({"a": {"b": {"c": true}}}))
            .build(),
        Err(Error::InvalidValue(_))
    ));

    let script_limits = Limits {
        max_script_bytes: 8,
        ..Limits::default()
    };
    let mut process = Process::builder("svit://local/tests/script-size")
        .unwrap()
        .limits(script_limits)
        .build()
        .unwrap();
    assert!(matches!(
        process.save_script("large", "(define (main input) ())"),
        Err(Error::ResourceLimitExceeded("script source"))
    ));
    assert_eq!(process.version(), 0);
}

#[test]
// THREAT[TM-DOS-003]
fn ketos_parse_limits_fail_at_the_script_boundary() {
    let cases = [
        (
            "syntax-depth",
            Limits {
                max_syntax_depth: 4,
                ..Limits::default()
            },
            "(define (main input) ((((((identity true)))))))",
            "syntax depth",
        ),
        (
            "integer-bits",
            Limits {
                max_integer_bits: 8,
                ..Limits::default()
            },
            "(define (main input) 65536)",
            "integer bits",
        ),
    ];

    for (name, limits, source, expected_limit) in cases {
        let mut process = Process::builder(format!("svit://local/tests/{name}"))
            .unwrap()
            .limits(limits)
            .build()
            .unwrap();

        assert!(matches!(
            process.save_script("rejected", source),
            Err(Error::ResourceLimitExceeded(limit)) if limit == expected_limit
        ));
        assert_eq!(process.version(), 0);
        assert!(process.script("rejected").is_none());
    }
}

#[test]
// THREAT[TM-ESC-001] THREAT[TM-EFF-001]
fn activation_input_has_no_mutation_primitive() {
    let mut process = Process::new("svit://local/tests/input").unwrap();
    process
        .save_script(
            "mutate_input",
            "(define (main input) (value-set! input \"/value\" 2))",
        )
        .unwrap();
    let before = unchanged(&process);

    assert!(matches!(
        process.run("mutate_input", value!({"value": 1})),
        Err(Error::Script(_))
    ));
    assert_eq!(process.snapshot().unwrap(), before);
}
