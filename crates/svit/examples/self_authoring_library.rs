use svit::{Process, Value, value};

const TEACHER: &str = r#"
function main()
    scripts.save("greeter", [[
function main(input)
    return {
        greeting = "Hello, " .. input.name .. "!",
        library = scripts.list(),
    }
end
]], "Greets a person and reports its discoverable script library")
    return "greeter saved"
end
"#;

fn main() -> svit::Result<()> {
    let mut process = Process::new("svit://local/examples/self-authoring")?;
    process.save_script("teacher", TEACHER)?;

    let taught = process.run("teacher", Value::Null)?;
    assert_eq!(taught.output, Value::String("greeter saved".into()));
    assert_eq!(process.script_names(), ["greeter", "teacher"]);
    assert_eq!(
        process
            .script("greeter")
            .expect("teacher committed the greeter")
            .documentation(),
        "Greets a person and reports its discoverable script library"
    );

    let greeting = process.run("greeter", value!({"name": "Ada"}))?;
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
