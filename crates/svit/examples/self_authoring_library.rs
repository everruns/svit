use svit::{Process, Value, value};

const TEACHER: &str = r##"
(define (main input)
  (do
    (write
      "/lib/greeter"
      (value-map
        "source" "(define (main input) (value-map \"greeting\" (concat \"Hello, \" (value-get input \"/name\") \"!\") \"library\" (discover \"/lib\")))"
        "documentation" "Greets a person and reports its discoverable script library"))
    "greeter saved"))
"##;

fn main() -> svit::Result<()> {
    let mut process = Process::new("svit://local/examples/self-authoring")?;
    process.write("/lib/teacher", value!({"source": TEACHER}))?;

    let taught = process.exec("/lib/teacher", Value::Null)?;
    assert_eq!(taught.output, Value::String("greeter saved".into()));
    assert_eq!(process.discover("/lib")?, ["greeter", "teacher"]);
    assert_eq!(
        process.read("/lib/greeter")?.map(|value| value.to_json()),
        Some(value!({
            "language": "svit-lisp@2",
            "source": "(define (main input) (value-map \"greeting\" (concat \"Hello, \" (value-get input \"/name\") \"!\") \"library\" (discover \"/lib\")))",
            "documentation": "Greets a person and reports its discoverable script library"
        }).to_json())
    );

    let greeting = process.exec("/lib/greeter", value!({"name": "Ada"}))?;
    assert_eq!(
        greeting.output,
        value!({
            "greeting": "Hello, Ada!",
            "library": ["greeter", "teacher"]
        })
    );

    println!("self_authoring_library scripts=greeter,teacher greeting=Hello_Ada");
    Ok(())
}
