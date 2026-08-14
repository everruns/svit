//! Transactional support-agent-process workflow used by the builtin example.

use serde_json::json;
use std::path::PathBuf;
use svit::{Limits, Message, MessageIntent, Mount, Process, Script, Svit, Value};
use thiserror::Error;

const TICKET_DESTINATION: &str = "svit://local/support-tickets";

/// A support result validated against the committed process state and outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedSupportResult {
    /// User-facing answer stored by the support commit activation.
    pub answer: String,
    /// Document identifiers recorded by the retrieval activation.
    pub source_ids: Vec<String>,
    /// Whether one matching ticket intent is present in the committed outbox.
    pub ticket_queued: bool,
}

/// Failure to construct, run, or validate the support workflow.
#[derive(Debug, Error)]
pub enum SupportError {
    /// The underlying Svit process rejected an operation.
    #[error(transparent)]
    Svit(#[from] svit::Error),

    /// The agent turn failed before producing a committed result.
    #[error(transparent)]
    Agent(#[from] svit::SvitError),

    /// Turso could not construct or query the account-context database.
    #[error(transparent)]
    Turso(#[from] turso::Error),

    /// The committed support record does not satisfy the host contract.
    #[error("invalid committed support result: {0}")]
    InvalidResult(String),
}

/// Builds one support process around a host-issued request identifier and question.
pub async fn support_process(request_id: &str, question: &str) -> Result<Process, SupportError> {
    let limits = Limits::default();
    let docs_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs");
    let support_docs = Mount::folder(docs_path)?;

    let database = turso::Builder::new_local(":memory:").build().await?;
    let connection = database.connect()?;
    connection
        .execute_batch(
            "CREATE TABLE account_context (\n\
                 account_id TEXT,\n\
                 email_verified INTEGER,\n\
                 authenticator_enrolled INTEGER,\n\
                 recovery_method TEXT\n\
             );\n\
             INSERT INTO account_context VALUES\n\
                 ('demo-account', 1, 1, 'identity-support-review');",
        )
        .await?;
    let account_context = Mount::turso_query(
        &connection,
        "SELECT account_id, email_verified, authenticator_enrolled, recovery_method \
         FROM account_context",
        (),
        &limits,
    )
    .await?;

    Ok(Process::builder("svit://local/support-agent-process")?
        .limits(limits)
        .mount("support_docs", support_docs)
        .mount("account_context", account_context)
        .memory(
            "request",
            Value::from_json(json!({
                "id": request_id,
                "question": question,
                "retrieval": null,
                "result": null
            }))?,
        )
        .library(
            "search_support_docs",
            Script::new(include_str!("../scripts/search_support_docs.svit-script"))
                .with_documentation(
                    "Search mounted support docs and account context for the active host request. \
                     Input: {request_id}.",
                ),
        )
        .library(
            "commit_support_result",
            Script::new(include_str!("../scripts/commit_support_result.svit-script"))
                .with_documentation(
                    "Commit the active request exactly once. Input: {request_id, answer}. \
                     Ticket policy, content, and source IDs are derived from process state.",
                ),
        )
        .build()?)
}

/// Runs one agent turn and returns only its validated committed support result.
///
/// The model's final free-form response is intentionally ignored. The host
/// returns the answer that committed atomically with any ticket intent.
pub async fn run_support_turn(
    agent: &mut Svit,
    request_id: &str,
) -> Result<CommittedSupportResult, SupportError> {
    let question = match agent.read("/memory/request/question")? {
        Some(Value::String(question)) if !question.is_empty() => question,
        _ => return Err(invalid("active question is missing or invalid")),
    };
    let inbox = agent.inbox();
    agent.start()?;
    inbox.send(Message::user(question)).await?;
    drop(inbox);
    agent.block().await?;
    let result = agent
        .read("/memory/request/result")?
        .ok_or_else(|| invalid("missing result path"))?;
    let outbox = agent.message_intents()?;
    validate_committed_support_result(&result, &outbox, request_id)
}

/// Validates and loads the authoritative support result from committed state.
pub fn committed_support_result(
    process: &Process,
    request_id: &str,
) -> Result<CommittedSupportResult, SupportError> {
    let result = process
        .read("/memory/request/result")?
        .ok_or_else(|| invalid("missing result path"))?;
    let outbox = process.outbox()?;
    validate_committed_support_result(&result, &outbox, request_id)
}

fn validate_committed_support_result(
    result: &Value,
    outbox: &[MessageIntent],
    request_id: &str,
) -> Result<CommittedSupportResult, SupportError> {
    let Value::Map(result) = result else {
        return Err(invalid("result was not committed"));
    };

    if map_text(result, "request_id")? != request_id {
        return Err(invalid("request id does not match the host request"));
    }
    let answer = map_text(result, "answer")?.to_owned();
    if answer.is_empty() {
        return Err(invalid("answer is empty"));
    }
    let source_ids = map_text_array(result, "source_ids")?;
    if source_ids.is_empty() {
        return Err(invalid("source IDs are empty"));
    }
    let ticket_queued = match result.get("ticket_queued") {
        Some(Value::Bool(value)) => *value,
        _ => return Err(invalid("ticket_queued is not boolean")),
    };

    match (ticket_queued, outbox) {
        (false, []) => {}
        (true, [message])
            if message.to.as_str() == TICKET_DESTINATION
                && message.body.to_json()["request_id"] == request_id
                && message.body.to_json()["kind"].is_string() => {}
        (false, _) => return Err(invalid("unexpected ticket intent")),
        (true, _) => return Err(invalid("matching ticket intent is not unique")),
    }

    Ok(CommittedSupportResult {
        answer,
        source_ids,
        ticket_queued,
    })
}

fn map_text<'a>(
    values: &'a std::collections::BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a str, SupportError> {
    match values.get(key) {
        Some(Value::String(value)) => Ok(value),
        _ => Err(invalid(format!("{key} is not text"))),
    }
}

fn map_text_array(
    values: &std::collections::BTreeMap<String, Value>,
    key: &str,
) -> Result<Vec<String>, SupportError> {
    let Some(Value::Array(values)) = values.get(key) else {
        return Err(invalid(format!("{key} is not an array")));
    };
    values
        .iter()
        .map(|value| match value {
            Value::String(value) => Ok(value.clone()),
            _ => Err(invalid("source ID is not text")),
        })
        .collect()
}

fn invalid(message: impl Into<String>) -> SupportError {
    SupportError::InvalidResult(message.into())
}
