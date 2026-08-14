use serde_json::json;
use svit::{
    Builtin, BuiltinContext, BuiltinExtension, BuiltinManual, BuiltinResult, Builtins,
    LLMSIM_MODEL_ID, LlmSimConfig, Message, MessageRole, Reasoner, SimToolCall, SimTurn, Svit,
    SvitResult, llm_sim_provider, value,
};

struct ReadCommitted;

#[svit::async_trait]
impl Builtin for ReadCommitted {
    fn manual(&self) -> BuiltinManual {
        BuiltinManual::new(
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

    async fn execute(&self, context: BuiltinContext, input: serde_json::Value) -> BuiltinResult {
        let path = input["path"].as_str().unwrap_or_default();
        match context.read(path) {
            Ok(Some(value)) => BuiltinResult::success(value.to_json()),
            Ok(None) => BuiltinResult::error("path not found"),
            Err(error) => BuiltinResult::error(error.to_string()),
        }
    }
}

struct ProcessReaders;

impl BuiltinExtension for ProcessReaders {
    fn builtins(&self) -> Vec<(String, Box<dyn Builtin>)> {
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
    let mut svit = Svit::builder("svit://local/example/builtins")?
        .memory(
            "releases",
            value!([
                {"version": "0.1", "ready": false},
                {"version": "0.2", "ready": true}
            ]),
        )
        .builtins(Builtins::new().extension(ProcessReaders))
        .reasoner(scripted_reasoner(vec![
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "exec".into(),
                arguments: json!({
                    "path": "/bin/search",
                    "input": {"path": "/memory/releases", "pattern": "^0\\.2$"}
                }),
                id: None,
            }]),
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "exec".into(),
                arguments: json!({
                    "path": "/bin/jq",
                    "input": {
                        "filter": ".[] | select(.ready) | .version",
                        "input": [
                            {"version": "0.1", "ready": false},
                            {"version": "0.2", "ready": true}
                        ]
                    }
                }),
                id: None,
            }]),
            SimTurn::ToolCalls(vec![SimToolCall {
                name: "exec".into(),
                arguments: json!({
                    "path": "/bin/read-committed",
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
    println!("builtins ready_release=0.2");
    Ok(())
}
