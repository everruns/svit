use serde_json::json;
use svit::{AgentModel, Message, Script, SimTurn, Svit, SvitResult, value};

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
        .system_prompt("Handle inbox messages entirely inside your Svit process.")
        .model(AgentModel::scripted([
            SimTurn::tool_call(
                "exec",
                json!({"path": "/lib/remember_color", "input": {"color": "blue"}}),
            ),
            SimTurn::text("remembered blue"),
        ]))
        .allow_scripts(["remember_color"])
        .build()
        .await?;

    let inbox = svit.inbox();
    let mut outbox = svit.outbox();

    svit.start()?;
    inbox.send(Message::user("Remember that the release color is blue."))?;
    let result = outbox
        .recv()
        .await
        .expect("the subscribed outbox receives the completed turn");
    drop(inbox);
    svit.block().await?;

    assert_eq!(result.text(), Some("remembered blue"));
    assert_eq!(svit.read("/memory/release/color")?, Some(value!("blue")));
    println!("process_owned_agent color=blue version={}", svit.version()?);
    Ok(())
}
