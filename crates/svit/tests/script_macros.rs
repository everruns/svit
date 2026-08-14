use svit::{Value, svit_script, svit_script_test, value};

#[test]
fn direct_macro_preserves_lisp_names_and_executes() -> svit::Result<()> {
    let mut process = svit::Process::builder("svit://local/tests/direct-macro")?
        .library(
            "inspect",
            svit_script! {
                (define (missing?)
                  (value-null? (read "/memory/missing")))

                (define (main input)
                  (let ((missing (missing?)))
                    (do
                      (log-info! "direct source" (value-map "missing" missing))
                      (value-map "input" input "missing" missing))))
            }
            .with_documentation("Returns the activation input and a null check"),
        )
        .build()?;

    let activation = process.exec("/lib/inspect", value!({"checked": true}))?;
    assert_eq!(
        activation.output,
        value!({"input": {"checked": true}, "missing": true})
    );
    assert_eq!(activation.logs[0].fields, value!({"missing": true}));
    Ok(())
}

svit_script_test!(
    file_macro_executes_and_commits,
    svit_script!(file "tests/scripts/counter.svit-script"),
    |process, script| {
        process.write("/memory/count", value!(40))?;

        let activation = process.exec(script, value!({"by": 2}))?;

        assert_eq!(activation.output, value!({"count": 42}));
        assert_eq!(process.read("/memory/count")?, Some(Value::Integer(42)));
    },
);
