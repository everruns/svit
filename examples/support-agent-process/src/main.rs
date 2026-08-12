use std::error::Error;

use svit::{OpenAI, Reasoner, Svit};
use svit_support_agent_process::{run_support_turn, support_process};

const REQUEST_ID: &str = "support-request-001";
const QUESTION: &str =
    "I lost access to my verified email and my authenticator. Can support restore access?";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let process = support_process(REQUEST_ID, QUESTION).await?;
    let mut svit = Svit::resume(process)
        .instructions(
            "You are a support agent. A reply is invalid unless this exact Svit workflow \
             succeeds: first read /memory/request/id; then execute search_support_docs with \
             input {request_id: <the exact value read>}; use its mounted account context and \
             support documents to prepare an answer; then execute commit_support_result with \
             that request_id and answer exactly once. Do not produce final text before the \
             commit succeeds. A ticket intent is queued, not delivered; describe it only as \
             queued.",
        )
        .reasoner(Reasoner::new("gpt-5.6-terra", OpenAI::from_env()?))
        .allow_scripts(["search_support_docs", "commit_support_result"])
        .max_iterations(8)
        .build()
        .await?;

    let result = run_support_turn(&mut svit, REQUEST_ID).await?;
    println!("{}", result.answer);
    println!("sources={}", result.source_ids.join(","));
    println!("ticket_queued={}", result.ticket_queued);
    println!("svit version={}", svit.version()?);
    println!("svit root_hash={}", svit.root_hash()?);
    Ok(())
}
