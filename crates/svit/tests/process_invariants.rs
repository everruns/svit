use svit::{Error, Limits, Process, Value, value};

fn unchanged(process: &Process) -> Vec<u8> {
    process.snapshot().expect("snapshot")
}

fn write_script(process: &mut Process, name: &str, source: impl Into<String>) -> svit::Result<()> {
    process.write(
        &format!("/lib/{name}"),
        Value::from_json(serde_json::json!({"source": source.into()}))?,
    )
}

#[test]
// THREAT[TM-AUTH-001]
fn builder_exposes_the_conventional_namespace_and_non_authoritative_identity() {
    let mut process = Process::builder("svit://local/tests/namespace")
        .unwrap()
        .memory("profile", value!({"name": "Ada"}))
        .build()
        .unwrap();

    assert_eq!(
        process.discover("/").unwrap(),
        [
            "bin", "children", "inbox", "lib", "memory", "mounts", "system", "tasks", "thread"
        ]
    );
    assert_eq!(
        process.discover("/system").unwrap(),
        [
            "api",
            "capabilities",
            "identity",
            "limits",
            "lineage",
            "outbox",
            "runtime"
        ]
    );
    assert_eq!(
        process.read("/system/api/operations").unwrap(),
        Some(value!(["discover", "exec", "read", "remove", "write"]))
    );
    assert_eq!(
        process.read("/system/runtime/language").unwrap(),
        Some(Value::String("svit-lisp@2".into()))
    );
    assert_eq!(
        process.read("/system/identity/address").unwrap(),
        Some(Value::String("svit://local/tests/namespace".into()))
    );
    assert_eq!(
        process.read("/system/identity/authenticated").unwrap(),
        Some(Value::Bool(false))
    );
    assert_eq!(
        process.read("/system/capabilities").unwrap(),
        Some(Value::Array(Vec::new()))
    );
    assert!(matches!(
        process.write("/system/identity/authenticated", Value::Bool(true)),
        Err(Error::InvalidPath(_))
    ));
    assert_eq!(process.version(), 0);
}

#[test]
// THREAT[TM-AUD-001]
fn thread_state_is_host_managed_and_guest_read_only() {
    let mut process = Process::builder("svit://local/tests/thread-state")
        .unwrap()
        .library(
            "forge-history",
            svit::Script::new(r#"(define (main input) (write "/thread/events" input))"#),
        )
        .build()
        .unwrap();

    process
        .replace_thread_state(value!({"format": "test@1", "events": []}))
        .unwrap();
    let committed = unchanged(&process);
    let version = process.version();

    assert!(
        process
            .exec("/lib/forge-history", value!(["forged"]))
            .is_err()
    );
    assert_eq!(process.snapshot().unwrap(), committed);
    assert_eq!(process.version(), version);
    assert_eq!(
        process.read("/thread/format").unwrap(),
        Some(value!("test@1"))
    );

    let oversized = Value::String("x".repeat(process.limits().max_text_bytes + 1));
    assert!(matches!(
        process.replace_thread_state(oversized),
        Err(Error::InvalidValue(message)) if message == "maximum text bytes exceeded"
    ));
    assert_eq!(process.snapshot().unwrap(), committed);
    assert_eq!(process.version(), version);
}

#[test]
fn discovery_traverses_nested_memory_arrays_and_reserved_nodes() {
    let process = Process::builder("svit://local/tests/discovery")
        .unwrap()
        .memory(
            "cases",
            value!([
                {"events": [{"kind": "opened"}]},
                {"events": []}
            ]),
        )
        .build()
        .unwrap();

    assert_eq!(process.discover("/memory").unwrap(), ["cases"]);
    assert_eq!(process.discover("/memory/cases").unwrap(), ["0", "1"]);
    assert_eq!(process.discover("/memory/cases/0").unwrap(), ["events"]);
    assert_eq!(
        process.discover("/memory/cases/0/events/0").unwrap(),
        ["kind"]
    );
    assert!(process.discover("/tasks").unwrap().is_empty());
    assert!(process.discover("/inbox").unwrap().is_empty());
    assert!(process.discover("/children").unwrap().is_empty());
    assert!(process.discover("/mounts").unwrap().is_empty());
}

#[test]
// THREAT[TM-FORK-001] THREAT[TM-AUTH-001]
fn fork_updates_discoverable_identity_and_lineage_without_changing_parent() {
    let parent = Process::new("svit://local/tests/lineage-parent").unwrap();
    let child = parent.fork("svit://local/tests/lineage-child").unwrap();

    assert_eq!(
        parent.read("/system/lineage/parent").unwrap(),
        Some(Value::Null)
    );
    assert_eq!(
        child.read("/system/lineage/parent").unwrap(),
        Some(Value::String("svit://local/tests/lineage-parent".into()))
    );
    assert_eq!(
        child.read("/system/identity/address").unwrap(),
        Some(Value::String("svit://local/tests/lineage-child".into()))
    );
}

#[test]
// THREAT[TM-EFF-001]
fn invalid_staged_script_rolls_back_memory_scripts_outbox_and_version() {
    let mut process = Process::builder("svit://local/tests/staged-script")
        .unwrap()
        .memory("changed", value!(false))
        .build()
        .unwrap();
    write_script(
        &mut process,
        "teacher",
        r#"
            (define (main input)
              (do
                (write "/memory/changed" true)
                (send! "svit://local/tests/recipient" (value-map "accepted" true))
                (write "/lib/broken" (value-map "source" "(define (main input)"))))
            "#,
    )
    .unwrap();
    let before = unchanged(&process);

    assert!(matches!(
        process.exec("/lib/teacher", Value::Null),
        Err(Error::Script(_))
    ));
    assert_eq!(process.snapshot().unwrap(), before);
    assert!(process.read("/lib/broken").unwrap().is_none());
    assert!(process.outbox().unwrap().is_empty());
}

#[test]
// THREAT[TM-ESC-002] THREAT[TM-EFF-001]
fn guest_function_values_are_rejected_atomically() {
    let mut process = Process::new("svit://local/tests/function-value").unwrap();
    write_script(
        &mut process,
        "function-value",
        "(define (main input) (write \"/memory/bad\" main))",
    )
    .unwrap();
    let before = unchanged(&process);

    assert!(matches!(
        process.exec("/lib/function-value", Value::Null),
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
    write_script(
        &mut process,
        "allocate",
        format!("(define (main input) (value-array {items}))"),
    )
    .unwrap();
    let before = unchanged(&process);

    assert!(matches!(
        process.exec("/lib/allocate", Value::Null),
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
                 (do (write "/memory/changed" true) (descend 100)))"#
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
                "(define (main input) (do (write \"/memory/changed\" true) (value-array {})))",
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
                 (do (write "/memory/changed" true) true))"#
                .to_owned(),
            "guest namespace",
        ),
    ];

    for (name, limits, source, expected_limit) in cases {
        let mut process = Process::builder(format!("svit://local/tests/{name}"))
            .unwrap()
            .limits(limits)
            .memory("changed", value!(false))
            .build()
            .unwrap();
        write_script(&mut process, "exercise", source).unwrap();
        let before = unchanged(&process);

        assert!(matches!(
            process.exec("/lib/exercise", Value::Null),
            Err(Error::ResourceLimitExceeded(limit)) if limit == expected_limit
        ));
        assert_eq!(process.snapshot().unwrap(), before);
    }
}

#[test]
// THREAT[TM-SNAP-001]
fn restore_rejects_old_or_unknown_formats_schema_tampering_and_trailing_data() {
    let process = Process::builder("svit://local/tests/snapshot")
        .unwrap()
        .memory("count", value!(1))
        .build()
        .unwrap();
    let snapshot = process.snapshot().unwrap();

    for format in [1, 2, 3, 999] {
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

    let mut forged_identity: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
    forged_identity["root"]["value"]["system"]["value"]["identity"]["value"]["address"]["value"] =
        serde_json::json!("svit://local/tests/other");
    assert!(matches!(
        Process::restore(&serde_json::to_vec(&forged_identity).unwrap()),
        Err(Error::InvalidSnapshot(message)) if message.contains("/system/identity")
    ));

    let mut populated_reserved: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
    populated_reserved["root"]["value"]["tasks"]["value"]["unexpected"] =
        serde_json::json!({"type": "integer", "value": 1});
    assert!(matches!(
        Process::restore(&serde_json::to_vec(&populated_reserved).unwrap()),
        Err(Error::InvalidSnapshot(message)) if message.contains("/tasks is reserved")
    ));

    let mut trailing = snapshot;
    trailing.extend_from_slice(b"not-json");
    assert!(Process::restore(&trailing).is_err());
}

#[test]
// THREAT[TM-SNAP-001] THREAT[TM-MSG-001]
fn replay_from_the_same_snapshot_is_deterministic() {
    let mut original = Process::builder("svit://local/tests/replay")
        .unwrap()
        .memory("total", value!(0))
        .build()
        .unwrap();
    write_script(
        &mut original,
        "add",
        r#"
            (define (main input)
              (let ((total (+ (read "/memory/total") (value-get input "/amount"))))
                (do
                  (write "/memory/total" total)
                  (log-info! "added" (value-map "total" total))
                  (send! "svit://local/tests/sink" (value-map "total" total))
                  total)))
            "#,
    )
    .unwrap();
    let snapshot = original.snapshot().unwrap();
    let mut first = Process::restore(&snapshot).unwrap();
    let mut second = Process::restore(&snapshot).unwrap();

    let first_result = first.exec("/lib/add", value!({"amount": 4})).unwrap();
    let second_result = second.exec("/lib/add", value!({"amount": 4})).unwrap();

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
    write_script(
        &mut process,
        "write",
        "(define rogue \"write\") (define (main input) rogue)",
    )
    .unwrap();
    write_script(
        &mut process,
        "read",
        "(define rogue \"read\") (define (main input) rogue)",
    )
    .unwrap();

    process.exec("/lib/write", Value::Null).unwrap();
    let observed = process.exec("/lib/read", Value::Null).unwrap();
    assert_eq!(observed.output, Value::String("read".into()));
}

#[test]
// THREAT[TM-FORK-001]
fn fork_does_not_duplicate_parent_outbox_or_share_future_mutations() {
    let mut parent = Process::builder("svit://local/tests/parent")
        .unwrap()
        .memory("value", value!(0))
        .build()
        .unwrap();
    write_script(
        &mut parent,
        "emit",
        r#"
            (define (main input)
              (let ((value (value-get input "/value")))
                (do
                  (write "/memory/value" value)
                  (send! "svit://local/tests/sink" (value-map "value" value)))))
            "#,
    )
    .unwrap();
    parent.exec("/lib/emit", value!({"value": 1})).unwrap();

    let mut child = parent.fork("svit://local/tests/child").unwrap();
    assert_eq!(parent.outbox().unwrap().len(), 1);
    assert!(child.outbox().unwrap().is_empty());

    child.exec("/lib/emit", value!({"value": 2})).unwrap();
    assert_eq!(
        parent.read("/memory/value").unwrap(),
        Some(Value::Integer(1))
    );
    assert_eq!(
        child.read("/memory/value").unwrap(),
        Some(Value::Integer(2))
    );
    assert_eq!(parent.outbox().unwrap().len(), 1);
    assert_eq!(child.outbox().unwrap().len(), 1);
}

#[test]
// THREAT[TM-INF-001]
fn diagnostics_are_capped_and_use_virtual_source_paths() {
    let mut process = Process::new("svit://local/tests/diagnostic").unwrap();
    let message = "x".repeat(4096);
    write_script(
        &mut process,
        "fail",
        format!("(define (main input) (panic \"{message}\"))"),
    )
    .unwrap();

    let diagnostic = process
        .exec("/lib/fail", Value::Null)
        .unwrap_err()
        .to_string();
    assert!(diagnostic.len() <= 1100);
    assert!(diagnostic.contains("/lib/fail.svit-script"));
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

    let process = Process::new("svit://local/tests/exec-depth-limit").unwrap();
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&process.snapshot().unwrap()).unwrap();
    snapshot["limits"]["max_exec_depth"] = serde_json::json!(65);
    assert!(matches!(
        Process::restore(&serde_json::to_vec(&snapshot).unwrap()),
        Err(Error::InvalidLimits(_))
    ));
}

#[test]
// THREAT[TM-DOS-003] THREAT[TM-DOS-006] THREAT[TM-EFF-001]
fn nested_exec_depth_is_bounded_without_resetting_the_transaction() {
    let script = |source| Value::Script(svit::Script::new(source));

    let mut allowed = Process::builder("svit://local/tests/exec-depth-allowed")
        .unwrap()
        .limits(Limits {
            max_exec_depth: 1,
            ..Limits::default()
        })
        .library("inner", svit::Script::new("(define (main input) true)"))
        .library(
            "outer",
            svit::Script::new("(define (main input) (exec \"/lib/inner\" input))"),
        )
        .build()
        .unwrap();
    assert_eq!(
        allowed.exec("/lib/outer", Value::Null).unwrap().output,
        Value::Bool(true)
    );

    let mut denied = Process::builder("svit://local/tests/exec-depth-denied")
        .unwrap()
        .limits(Limits {
            max_exec_depth: 0,
            ..Limits::default()
        })
        .library("inner", svit::Script::new("(define (main input) true)"))
        .library(
            "outer",
            svit::Script::new("(define (main input) (exec \"/lib/inner\" input))"),
        )
        .build()
        .unwrap();
    let before = denied.snapshot().unwrap();
    assert!(matches!(
        denied.exec("/lib/outer", Value::Null),
        Err(Error::ResourceLimitExceeded("nested exec depth"))
    ));
    assert_eq!(denied.snapshot().unwrap(), before);

    // Keep host library writes on the same generic path contract.
    denied
        .write("/lib/replacement", script("(define (main input) ())"))
        .unwrap();
    denied.remove("/lib/replacement").unwrap();
    assert!(denied.read("/lib/replacement").unwrap().is_none());
}

#[test]
// THREAT[TM-DOS-001] THREAT[TM-DOS-006] THREAT[TM-EFF-001]
fn nested_exec_shares_the_activation_deadline() {
    let mut process = Process::builder("svit://local/tests/nested-deadline")
        .unwrap()
        .limits(Limits {
            max_execution_millis: 1,
            ..Limits::default()
        })
        .memory("changed", value!(false))
        .library(
            "inner",
            svit::Script::new(
                "(define (spin) (spin)) (define (main input) (spin))",
            ),
        )
        .library(
            "outer",
            svit::Script::new(
                "(define (main input) (do (write \"/memory/changed\" true) (exec \"/lib/inner\" input)))",
            ),
        )
        .build()
        .unwrap();
    let before = process.snapshot().unwrap();

    assert!(matches!(
        process.exec("/lib/outer", Value::Null),
        Err(Error::ExecutionLimitExceeded)
    ));
    assert_eq!(process.snapshot().unwrap(), before);
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
            "(define (main input) (do (write \"/memory/changed\" true) (log-info! \"x\")))",
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
                  (write "/memory/changed" true)
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
                  (write "/memory/changed" true)
                  (write "/lib/child" (value-map "source" "(define (main input) ())"))))"#,
            "staged scripts",
        ),
    ];

    for (name, limits, source, expected_limit) in cases {
        let mut process = Process::builder(format!("svit://local/tests/{name}"))
            .unwrap()
            .limits(limits)
            .memory("changed", value!(false))
            .build()
            .unwrap();
        write_script(&mut process, "exercise", source).unwrap();
        let before = unchanged(&process);

        assert!(matches!(
            process.exec("/lib/exercise", Value::Null),
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
            .memory("key", value!("too long"))
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
            .memory("a", value!(1))
            .memory("b", value!(2))
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
            .memory("a", value!({"b": {"c": true}}))
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
        write_script(&mut process, "large", "(define (main input) ())"),
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
            write_script(&mut process, "rejected", source),
            Err(Error::ResourceLimitExceeded(limit)) if limit == expected_limit
        ));
        assert_eq!(process.version(), 0);
        assert!(process.read("/lib/rejected").unwrap().is_none());
    }
}

#[test]
// THREAT[TM-ESC-001] THREAT[TM-EFF-001]
fn activation_input_has_no_mutation_primitive() {
    let mut process = Process::new("svit://local/tests/input").unwrap();
    write_script(
        &mut process,
        "mutate_input",
        "(define (main input) (value-set! input \"/value\" 2))",
    )
    .unwrap();
    let before = unchanged(&process);

    assert!(matches!(
        process.exec("/lib/mutate_input", value!({"value": 1})),
        Err(Error::Script(_))
    ));
    assert_eq!(process.snapshot().unwrap(), before);
}
