use everruns_test_support::{
    LLMSIM_MODEL_ID, LlmSimConfig, SimToolCall, SimTurn, llm_sim_provider,
};
use serde_json::json;
use svit::{Reasoner, Svit, value};
use svit_support_agent_process::{committed_support_result, run_support_turn, support_process};

const REQUEST_ID: &str = "support-request-001";
const QUESTION: &str =
    "I lost access to my verified email and my authenticator. Can support restore access?";

fn simulated_model() -> &'static str {
    LLMSIM_MODEL_ID
}

fn scripted_reasoner(turns: impl IntoIterator<Item = SimTurn>) -> Reasoner {
    Reasoner::new(
        simulated_model(),
        llm_sim_provider(LlmSimConfig::scripted(turns.into_iter().collect())),
    )
}

trait SimTurnExt {
    fn text(value: impl Into<String>) -> Self;
    fn tool_call(name: impl Into<String>, arguments: serde_json::Value) -> Self;
}

impl SimTurnExt for SimTurn {
    fn text(value: impl Into<String>) -> Self {
        Self::Assistant(value.into())
    }

    fn tool_call(name: impl Into<String>, arguments: serde_json::Value) -> Self {
        Self::ToolCalls(vec![SimToolCall {
            name: name.into(),
            arguments,
            id: None,
        }])
    }
}

#[tokio::test]
async fn commit_is_idempotent_and_uses_retrieved_sources() {
    let mut process = support_process(REQUEST_ID, QUESTION).await.unwrap();
    process
        .exec(
            "/lib/search_support_docs",
            value!({"request_id": REQUEST_ID}),
        )
        .unwrap();
    assert_eq!(
        process
            .read("/memory/request/retrieval/account/recovery_method")
            .unwrap(),
        Some(value!("identity-support-review"))
    );
    assert!(matches!(
        process
            .read("/mounts/support_docs/account-recovery.md")
            .unwrap(),
        Some(svit::Value::String(document))
            if document.contains("identity verification is required")
    ));
    let root_hash = process.root_hash();
    let version = process.version();
    let mismatched_request = process.exec(
        "/lib/commit_support_result",
        value!({
            "request_id": "support-request-002",
            "answer": "Wrong request"
        }),
    );
    assert!(mismatched_request.is_err());
    assert_eq!(process.root_hash(), root_hash);
    assert_eq!(process.version(), version);
    assert!(process.outbox().unwrap().is_empty());

    process
        .exec(
            "/lib/commit_support_result",
            value!({
                "request_id": REQUEST_ID,
                "answer": "Identity verification is required. Your request is queued for review."
            }),
        )
        .unwrap();

    let result = committed_support_result(&process, REQUEST_ID).unwrap();
    assert_eq!(
        result.answer,
        "Identity verification is required. Your request is queued for review."
    );
    assert_eq!(result.source_ids, ["account-recovery", "password-reset"]);
    assert!(result.ticket_queued);
    assert_eq!(process.outbox().unwrap().len(), 1);

    let root_hash = process.root_hash();
    let version = process.version();
    let duplicate = process.exec(
        "/lib/commit_support_result",
        value!({
            "request_id": REQUEST_ID,
            "answer": "A different answer"
        }),
    );
    assert!(duplicate.is_err());
    assert_eq!(process.root_hash(), root_hash);
    assert_eq!(process.version(), version);
    assert_eq!(process.outbox().unwrap().len(), 1);
}

#[tokio::test]
async fn ticket_policy_does_not_queue_an_unneeded_intent() {
    let mut process = support_process(
        REQUEST_ID,
        "How do I reset my password using my verified email?",
    )
    .await
    .unwrap();
    process
        .exec(
            "/lib/search_support_docs",
            value!({"request_id": REQUEST_ID}),
        )
        .unwrap();
    process
        .exec(
            "/lib/commit_support_result",
            value!({
                "request_id": REQUEST_ID,
                "answer": "Use Forgot password."
            }),
        )
        .unwrap();
    let result = committed_support_result(&process, REQUEST_ID).unwrap();
    assert!(!result.ticket_queued);
    assert_eq!(result.source_ids[0], "password-reset");
}

#[tokio::test]
async fn user_reply_is_loaded_from_the_committed_result() {
    let process = support_process(REQUEST_ID, QUESTION).await.unwrap();
    let mut agent = Svit::resume(process)
        .instructions("Test support agent")
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/lib/search_support_docs",
                    "input": {"request_id": REQUEST_ID}
                }),
            ),
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/lib/commit_support_result",
                    "input": {
                        "request_id": REQUEST_ID,
                        "answer": "Use the approved identity-verification process."
                    }
                }),
            ),
            SimTurn::text("I opened a ticket and bypassed verification."),
        ]))
        .build()
        .await
        .unwrap();

    let result = run_support_turn(&mut agent, REQUEST_ID).await.unwrap();

    assert_eq!(
        result.answer,
        "Use the approved identity-verification process."
    );
    assert!(result.ticket_queued);
}
