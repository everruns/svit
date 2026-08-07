use std::env;
use std::error::Error;

use agentyk::{Agent, ModelSpec, OpenAiDriver};
use svit_agentyk::SvitCapability;
use svit_support_agent::{run_support_turn, support_process};

const REQUEST_ID: &str = "support-request-001";
const QUESTION: &str =
    "I lost access to my verified email and my authenticator. Can support restore access?";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let process = support_process(REQUEST_ID, QUESTION)?;

    let svit_capability =
        SvitCapability::read_exec(process, ["search_support_docs", "commit_support_result"]);
    let process_handle = svit_capability.process();
    let agent = Agent::builder()
        .system_prompt(
            "You are a support agent. Discover and execute the Svit scripts needed to answer. \
             The host owns the active question and request ID in process memory. Commit the result \
             exactly once. A ticket intent is queued, not delivered; describe it only as queued.",
        )
        .model(
            ModelSpec::openai("gpt-5.6-terra")
                .api_key(env::var("OPENAI_API_KEY")?)
                .reasoning_effort("none"),
        )
        .driver(OpenAiDriver::new())
        .capability(svit_capability)
        .max_iterations(8)
        .build()?;

    let result = run_support_turn(&agent, &process_handle, REQUEST_ID).await?;
    println!("{}", result.answer);
    println!("sources={}", result.source_ids.join(","));
    println!("ticket_queued={}", result.ticket_queued);
    let committed_process = process_handle.lock().unwrap();
    println!("svit version={}", committed_process.version());
    println!("svit root_hash={}", committed_process.root_hash());
    for message in committed_process.outbox()? {
        println!("queued={} to={}", message.message_id, message.to);
    }
    Ok(())
}
