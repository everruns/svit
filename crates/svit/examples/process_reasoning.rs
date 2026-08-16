use everruns_test_support::{
    LLMSIM_MODEL_ID, LlmSimConfig, SimToolCall, SimTurn, llm_sim_provider,
};
use serde_json::json;
use svit::{Message, Reasoner, Script, Svit, SvitResult, value};

fn simulated_model() -> &'static str {
    LLMSIM_MODEL_ID
}

fn scripted_reasoner(turns: Vec<SimTurn>) -> Reasoner {
    Reasoner::new(
        simulated_model(),
        llm_sim_provider(LlmSimConfig::scripted(turns)),
    )
}

#[tokio::main]
async fn main() -> SvitResult<()> {
    let mut svit = Svit::builder("svit://local/example/runnable")?
        .memory("release", value!({"color": null}))
        .library(
            "remember_color",
            Script::new(
                r#"
                (define (main input)
                  (do
                    (write "/memory/release/color" (value-get input "/color"))
                    (read "/memory/release")))
                "#,
            ),
        )
        .instructions("Handle inbox messages entirely inside your Svit process.")
        .reasoner(scripted_reasoner(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "exec".into(),
                arguments: json!({"path": "/lib/remember_color", "input": {"color": "blue"}}),
                id: None,
            }]),
            SimTurn::Assistant("remembered blue".into()),
        ]))
        .build()
        .await?;

    let inbox = svit.inbox();
    let mut outbox = svit.outbox();

    svit.start()?;
    inbox
        .send(Message::user("Remember that the release color is blue."))
        .await?;
    let result = outbox
        .recv()
        .await
        .expect("the subscribed outbox receives the completed turn");
    drop(inbox);
    svit.block().await?;

    assert_eq!(result.text(), Some("remembered blue"));
    assert_eq!(svit.read("/memory/release/color")?, Some(value!("blue")));
    println!("process_reasoning color=blue version={}", svit.version()?);
    Ok(())
}
