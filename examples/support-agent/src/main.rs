use std::env;
use std::error::Error;

use svit::{AgentModel, Svit};
use svit_support_agent::{run_support_turn, support_process};

const REQUEST_ID: &str = "support-request-001";
const QUESTION: &str =
    "I lost access to my verified email and my authenticator. Can support restore access?";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let process = support_process(REQUEST_ID, QUESTION).await?;
    let mut agent = Svit::resume(process)
        .system_prompt(
            "You are a support agent. A reply is invalid unless this exact Svit workflow \
             succeeds: first read /memory/request/id; then execute search_support_docs with \
             input {request_id: <the exact value read>}; use its mounted account context and \
             support documents to prepare an answer; then execute commit_support_result with \
             that request_id and answer exactly once. Do not produce final text before the \
             commit succeeds. A ticket intent is queued, not delivered; describe it only as \
             queued.",
        )
        .model(AgentModel::openai(
            "gpt-5.6-terra",
            env::var("OPENAI_API_KEY")?,
        ))
        .allow_scripts(["search_support_docs", "commit_support_result"])
        .max_iterations(8)
        .build()
        .await?;

    let result = run_support_turn(&mut agent, REQUEST_ID).await?;
    let process = agent.process();
    let committed_process = process.lock().unwrap();
    println!("{}", result.answer);
    println!("sources={}", result.source_ids.join(","));
    println!("ticket_queued={}", result.ticket_queued);
    println!("svit version={}", committed_process.version());
    println!("svit root_hash={}", committed_process.root_hash());
    Ok(())
}
