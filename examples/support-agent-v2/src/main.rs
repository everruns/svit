use std::env;

use svit::{AgentModel, Message, Script, Svit, SvitResult, Value};

const QUESTION: &str = "Can I recover access without my authenticator?";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key = env::var("OPENAI_API_KEY")?;
    let mut svit = Svit::builder("svit://local/support/main")?
        .memory("answer", Value::Null)
        .library(
            "commit_answer",
            Script::new(
                r#"
                (define (main input)
                  (let ((answer (value-get input "/answer")))
                    (do
                      (write "/memory/answer" answer)
                      answer)))
                "#,
            )
            .with_documentation(
                "Commit the support answer before replying. Input: {answer: string}.",
            ),
        )
        .name("support-agent")
        .system_prompt(
            "Help the user recover account access without weakening identity verification. \
             Before replying, call commit_answer exactly once with your complete answer. \
             Return the same answer as your final response.",
        )
        .model(AgentModel::openai("gpt-5.6-terra", api_key))
        .allow_scripts(["commit_answer"])
        .build()
        .await?;

    let inbox = svit.inbox();
    let mut outbox = svit.outbox();

    svit.start()?;
    inbox.send(Message::user(QUESTION))?;

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
