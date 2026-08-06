use svit::{Process, Value, value};

const TEACHER: &str = r##"
(define (main input)
  (do
    (scripts-save!
      "greeter"
      "(define (main input) (value-map \"greeting\" (concat \"Hello, \" (value-get input \"/name\") \"!\") \"library\" (scripts-list)))"
      "Greets a person and reports its discoverable script library")
    "greeter saved"))
"##;

fn main() -> svit::Result<()> {
    let mut process = Process::new("svit://local/examples/self-authoring")?;
    process.save_script("teacher", TEACHER)?;

    let taught = process.exec("teacher", Value::Null)?;
    assert_eq!(taught.output, Value::String("greeter saved".into()));
    assert_eq!(process.discover("/lib")?, ["greeter", "teacher"]);
    assert_eq!(
        process
            .script("greeter")
            .expect("teacher committed the greeter")
            .documentation(),
        "Greets a person and reports its discoverable script library"
    );

    let greeting = process.exec("greeter", value!({"name": "Ada"}))?;
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
