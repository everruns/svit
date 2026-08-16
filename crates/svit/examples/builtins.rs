use everruns_test_support::{
    LLMSIM_MODEL_ID, LlmSimConfig, SimToolCall, SimTurn, llm_sim_provider,
};
use serde_json::json;
use svit::{
    Message, MessageRole, Port, PortContext, PortDescriptor, PortExtension, PortResult, Ports,
    Reasoner, Svit, SvitResult, value,
};

struct ReadCommitted;

#[svit::async_trait]
impl Port for ReadCommitted {
    fn descriptor(&self) -> PortDescriptor {
        PortDescriptor::new(
            "Read one committed process value.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        )
        .effect("read")
        .output("The committed JSON value at path.")
    }

    async fn execute(&self, context: PortContext, input: serde_json::Value) -> PortResult {
        let path = input["path"].as_str().unwrap_or_default();
        match context.read(path) {
            Ok(Some(value)) => PortResult::success(value.to_json()),
            Ok(None) => PortResult::error("path not found"),
            Err(error) => PortResult::error(error.to_string()),
        }
    }
}

struct ProcessReaders;

impl PortExtension for ProcessReaders {
    fn ports(&self) -> Vec<(String, Box<dyn Port>)> {
        vec![("read-committed".into(), Box::new(ReadCommitted))]
    }
}

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
    let mut svit = Svit::builder("svit://local/example/ports")?
        .memory(
            "releases",
            value!([
                {"version": "0.1", "ready": false},
                {"version": "0.2", "ready": true}
            ]),
        )
        .ports(Ports::new().extension(ProcessReaders))
        .reasoner(scripted_reasoner(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "exec".into(),
                arguments: json!({
                    "source": "(define (main input) (search \"/memory/releases\" \"0.2\"))",
                    "input": null
                }),
                id: None,
            }]),
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "exec".into(),
                arguments: json!({
                    "source": "(define (main input) (jq \".[] | select(.ready) | .version\" input))",
                    "input": [
                        {"version": "0.1", "ready": false},
                        {"version": "0.2", "ready": true}
                    ]
                }),
                id: None,
            }]),
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "exec".into(),
                arguments: json!({
                    "source": "(define (main input) (port-call \"read-committed\" input))",
                    "input": {"path": "/memory/releases/1/version"}
                }),
                id: None,
            }]),
            SimTurn::Assistant("release 0.2 is ready".into()),
        ]))
        .build()
        .await?;

    let inbox = svit.inbox();
    svit.start()?;
    inbox.send(Message::user("Find the ready release.")).await?;
    drop(inbox);
    svit.block().await?;

    let tool_outputs = svit
        .messages()?
        .iter()
        .filter(|message| message.role == MessageRole::ToolResult)
        .filter_map(|message| {
            message
                .tool_result_content()
                .and_then(|result| result.result.as_ref())
                .map(ToString::to_string)
        })
        .collect::<Vec<_>>();
    assert!(tool_outputs[0].contains("/memory/releases/1/version"));
    assert!(tool_outputs[1].contains("\"0.2\""));
    assert!(tool_outputs[2].contains("0.2"));
    assert_eq!(
        svit.messages()?.last().expect("assistant response").text(),
        Some("release 0.2 is ready")
    );
    println!("ports ready_release=0.2");
    Ok(())
}
