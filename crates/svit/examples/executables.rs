use agentyk::{ModelSpec, Role, SimDriver, SimTurn};
use serde_json::json;
use svit::{Executables, Message, Svit, SvitResult, value};

#[tokio::main]
async fn main() -> SvitResult<()> {
    let mut svit = Svit::builder("svit://local/example/executables")?
        .memory(
            "releases",
            value!([
                {"version": "0.1", "ready": false},
                {"version": "0.2", "ready": true}
            ]),
        )
        .executables(Executables::new())
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/bin/search",
                    "input": {"path": "/memory/releases", "pattern": "^0\\.2$"}
                }),
            ),
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/bin/jq",
                    "input": {
                        "filter": ".[] | select(.ready) | .version",
                        "input": [
                            {"version": "0.1", "ready": false},
                            {"version": "0.2", "ready": true}
                        ]
                    }
                }),
            ),
            SimTurn::text("release 0.2 is ready"),
        ]))
        .build()
        .await?;

    let inbox = svit.inbox();
    svit.start()?;
    inbox.send(Message::user("Find the ready release."))?;
    drop(inbox);
    svit.block().await?;

    let tool_outputs = svit
        .messages()?
        .iter()
        .filter(|message| message.role == Role::Tool)
        .map(|message| message.text())
        .collect::<Vec<_>>();
    assert!(tool_outputs[0].contains("/memory/releases/1/version"));
    assert!(tool_outputs[1].contains("\"0.2\""));
    assert_eq!(
        svit.messages()?.last().expect("assistant response").text(),
        "release 0.2 is ready"
    );
    println!("executables ready_release=0.2");
    Ok(())
}
