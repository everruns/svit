use svit::{Message, OpenAI, Reasoner, Script, Svit, SvitResult, Value};

const QUESTION: &str = "Can I recover access without my authenticator?";

const COMMIT_ANSWER: &str = r#"
module support/commit_answer

fn text_or(found: Result<String, String>, fallback: String) -> String {
    match found {
        ok(text) => text,
        err(why) => fallback,
    }
}

fn nonempty(value: String) -> String
  where
    length(value) > 0,
{
    value
}

fn main(sys: System) -> Int
  uses
    Io.env,
    Io.write,
{
    let answer = nonempty(text_or(Io.env(sys, "/input/answer"), ""))
    Io.write(sys.console, "set-text\t/memory/answer\t" + answer)
    1
}
"#;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut svit = Svit::builder("svit://local/support/deed")?
        .memory("answer", Value::Null)
        .library(
            "commit_answer",
            Script::deed(COMMIT_ANSWER).with_documentation(
                "Commit the support answer before replying. Input: {answer: string}. Returns 1.",
            ),
        )
        .instructions(
            "Help the user recover account access without weakening identity verification. \
             Before replying, call commit_answer exactly once with your complete answer. \
             A result of 1 means the Deed script committed it. Return the same answer as your \
             final response.",
        )
        .reasoner(Reasoner::new("gpt-5.6-terra", OpenAI::from_env()?))
        .allow_scripts(["commit_answer"])
        .build()
        .await?;

    let inbox = svit.inbox();
    let mut outbox = svit.outbox();

    svit.start()?;
    inbox.send(Message::user(QUESTION)).await?;

    let turn = outbox.recv().await?;
    let answer = turn.text().unwrap_or_default();
    assert!(!answer.is_empty());

    drop(inbox);
    svit.block().await?;

    assert_eq!(read_text(&svit, "/memory/answer")?, answer);
    println!("answer={answer}");
    Ok(())
}

fn read_text(svit: &Svit, path: &str) -> SvitResult<String> {
    match svit.read(path)? {
        Some(Value::String(value)) => Ok(value),
        _ => panic!("expected text at {path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::COMMIT_ANSWER;
    use svit::{Process, Script, Value, value};

    #[test]
    fn deed_commit_answer_updates_memory_transactionally() {
        let mut process = Process::builder("svit://local/tests/support-deed")
            .unwrap()
            .memory("answer", Value::Null)
            .library("commit_answer", Script::deed(COMMIT_ANSWER))
            .build()
            .unwrap();

        let activation = process
            .exec(
                "/lib/commit_answer",
                value!({"answer": "Use a verified recovery method."}),
            )
            .unwrap();

        assert_eq!(activation.output, value!(1));
        assert_eq!(
            process.read("/memory/answer").unwrap(),
            Some(value!("Use a verified recovery method."))
        );

        let committed = process.snapshot().unwrap();
        assert!(
            process
                .exec("/lib/commit_answer", value!({"answer": ""}))
                .is_err()
        );
        assert_eq!(process.snapshot().unwrap(), committed);
    }
}
