use std::env;
use std::error::Error;

use agentyk::{Agent, ModelSpec, OpenAiDriver};
use serde_json::json;
use svit::{Process, Script, Value};
use svit_agentyk::SvitCapability;

const QUESTION: &str =
    "I lost access to my verified email and my authenticator. Can support restore access?";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let process = Process::builder("svit://local/support-agent")?
        .memory(
            "docs",
            Value::from_json(json!([
                {"id": "account-recovery", "content": include_str!("../docs/account-recovery.md")},
                {"id": "password-reset", "content": include_str!("../docs/password-reset.md")},
                {"id": "refunds", "content": include_str!("../docs/refunds.md")}
            ]))?,
        )
        .memory("requests", Value::empty_map())
        .library(
            "search_support_docs",
            Script::new(include_str!("../scripts/search_support_docs.svit-script"))
                .with_documentation(
                    "Search support docs. Input: {query}. Returns the two best matches.",
                ),
        )
        .library(
            "commit_support_result",
            Script::new(include_str!("../scripts/commit_support_result.svit-script"))
                .with_documentation(
                    "Commit an answer and optional ticket intent. Input: {request_id, answer, \
                     source_ids, create_ticket, ticket_title, ticket_description}.",
                ),
        )
        .build()?;

    let svit_capability = SvitCapability::new(process);
    let process_handle = svit_capability.process();
    let agent = Agent::builder()
        .system_prompt(
            "You are a support agent. Discover and execute the Svit scripts needed to answer. \
             Commit the result before replying. A ticket intent is queued, not delivered; never \
             say a ticket was opened or created.",
        )
        .model(
            ModelSpec::openai("gpt-5.6-terra")
                .api_key(env::var("OPENAI_API_KEY")?)
                .reasoning_effort("none"),
        )
        .driver(OpenAiDriver::new())
        .capability(svit_capability)
        .build()?;

    let turn = agent.run(QUESTION).await?;
    println!("{}", turn.response);
    let committed_process = process_handle.lock().unwrap();
    println!("svit version={}", committed_process.version());
    for message in committed_process.outbox()? {
        println!("queued={} to={}", message.message_id, message.to);
    }
    Ok(())
}
