use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use everruns_test_support::{
    LLMSIM_MODEL_ID, LlmSimConfig, SimToolCall, SimTurn, llm_sim_provider,
};
use serde_json::json;
use svit::{
    Change, ContentPart, HttpAllowlist, HttpRequest, HttpResponse, HttpTransport,
    HttpTransportError, Limits, Message, MessageRole, ObserveError, Port, PortContext,
    PortDescriptor, PortExtension, PortResult, Ports, Process, Reasoner, Script, Svit, SvitError,
    SvitEvent, Value, value,
};
#[cfg(feature = "persistence-turso")]
use svit::{TransactionQuery, TursoProcessStore};

struct FixtureHttp;

struct CountingFixtureHttp(Arc<AtomicUsize>);

struct TreeFixtureHttp;

struct ModelCatalogFixtureHttp;

struct ReadValue;

#[async_trait]
impl Port for ReadValue {
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
        .limits(["Read-only process context."])
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

struct FixturePorts;

impl PortExtension for FixturePorts {
    fn ports(&self) -> Vec<(String, Box<dyn Port>)> {
        vec![("read-value".into(), Box::new(ReadValue))]
    }
}

struct OversizedOutput;

#[async_trait]
impl Port for OversizedOutput {
    fn descriptor(&self) -> PortDescriptor {
        PortDescriptor::new("Return an oversized fixture.", json!({"type": "object"}))
    }

    async fn execute(&self, _context: PortContext, _input: serde_json::Value) -> PortResult {
        PortResult::text("x".repeat(2 * 1024 * 1024))
    }
}

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

fn message_text(message: &Message) -> String {
    if let Some(text) = message.text() {
        return text.to_owned();
    }
    message
        .tool_result_content()
        .and_then(|result| {
            result
                .result
                .as_ref()
                .map(ToString::to_string)
                .or_else(|| result.error.clone())
        })
        .unwrap_or_default()
}

#[async_trait]
impl HttpTransport for FixtureHttp {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpTransportError> {
        assert_eq!(request.url, "https://8.8.8.8/data");
        Ok(HttpResponse {
            status: 200,
            headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
            body: br#"{"source":"fixture"}"#.to_vec(),
        })
    }
}

#[async_trait]
impl HttpTransport for CountingFixtureHttp {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpTransportError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        FixtureHttp.send(request).await
    }
}

#[async_trait]
impl HttpTransport for TreeFixtureHttp {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpTransportError> {
        assert_eq!(request.url, "https://8.8.8.8/tree");
        Ok(HttpResponse {
            status: 200,
            headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
            body: br#"{"tree":[{"path":"README.md","type":"blob"},{"path":"src","type":"tree"},{"path":"src/lib.rs","type":"blob"}]}"#.to_vec(),
        })
    }
}

#[async_trait]
impl HttpTransport for ModelCatalogFixtureHttp {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpTransportError> {
        assert_eq!(request.url, "https://models.dev/models.json");
        let body = json!({
            "openai/gpt-4.1": {
                "id": "openai/gpt-4.1",
                "name": "GPT-4.1",
                "release_date": "2025-04-14"
            },
            "openai/gpt-5.6": {
                "id": "openai/gpt-5.6",
                "name": "GPT-5.6",
                "release_date": "2026-08-01"
            },
            "openai/gpt-5.4": {
                "id": "openai/gpt-5.4",
                "name": "GPT-5.4",
                "release_date": "2026-06-01"
            },
            "anthropic/large-catalog-entry": {
                "id": "anthropic/large-catalog-entry",
                "name": "Large catalog entry",
                "description": "x".repeat(2 * 1024 * 1024)
            }
        });
        let body = serde_json::to_vec(&body).unwrap();
        assert!(body.len() > 1024 * 1024);
        Ok(HttpResponse {
            status: 200,
            headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
            body,
        })
    }
}

#[tokio::test]
async fn svit_builder_requires_a_reasoner() {
    let result = Svit::builder("svit://local/missing-provider")
        .unwrap()
        .build()
        .await;

    assert!(matches!(result, Err(SvitError::MissingReasoner)));
}

#[tokio::test]
async fn svit_owns_the_system_prompt_without_host_instructions() {
    let svit = Svit::builder("svit://local/base-prompt")
        .unwrap()
        .reasoner(scripted_reasoner([]))
        .build()
        .await
        .unwrap();

    assert_eq!(
        svit.read("/thread/instructions").unwrap(),
        Some(Value::Null)
    );
    let system_prompt = match svit.read("/thread/system_prompt").unwrap() {
        Some(Value::String(prompt)) => prompt,
        other => panic!("expected projected system prompt, got {other:?}"),
    };
    assert!(system_prompt.contains("svit://local/base-prompt"));
    assert!(system_prompt.contains("Use its memory tree for durable facts and working state."));
    assert!(system_prompt.contains("(port-call \"name\" input)"));
    assert!(system_prompt.contains("(exec \"/lib/name\" input)"));
    assert!(system_prompt.contains("(value-map \"key\" value ...)"));
    assert!(system_prompt.contains("immediate external effect"));
    assert!(!system_prompt.contains("<instructions>"));
}

#[tokio::test]
async fn svit_builder_combines_process_definition_and_blocking_inbox_loop() {
    let mut svit = Svit::builder("svit://local/runnable")
        .unwrap()
        .memory("status", value!("idle"))
        .instructions("Handle inbox messages inside your Svit process.")
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "write",
                json!({"path": "/memory/status", "value": "complete"}),
            ),
            SimTurn::text("work complete"),
        ]))
        .build()
        .await
        .unwrap();

    let inbox = svit.inbox();
    let mut outbox = svit.outbox();
    svit.start().unwrap();
    assert_eq!(svit.read("/memory/status").unwrap(), Some(value!("idle")));
    inbox
        .send(Message::user("Do the configured work."))
        .await
        .unwrap();
    svit.block().await.unwrap();
    let result = outbox.recv().await.unwrap();

    assert_eq!(result.role, MessageRole::Agent);
    assert_eq!(result.content, vec![ContentPart::text("work complete")]);
    assert_eq!(result.text(), Some("work complete"));
    assert_eq!(
        svit.read("/memory/status").unwrap(),
        Some(value!("complete"))
    );
    assert_eq!(svit.read("/inbox").unwrap(), Some(value!([])));
    assert_eq!(
        svit.messages().unwrap()[0].content,
        vec![ContentPart::text("Do the configured work.")]
    );
    assert!(matches!(
        inbox.send(Message::user("too late")).await,
        Err(svit::SvitError::InboxClosed)
    ));
}

#[tokio::test]
async fn svit_uses_the_everruns_iteration_default() {
    let turns = (0..9)
        .map(|_| SimTurn::tool_call("discover", json!({"path": "/memory"})))
        .chain([SimTurn::text("finished after discovery")]);
    let mut svit = Svit::builder("svit://local/default-turn-iterations")
        .unwrap()
        .reasoner(scripted_reasoner(turns))
        .build()
        .await
        .unwrap();
    let inbox = svit.inbox();
    let mut outbox = svit.outbox();

    svit.start().unwrap();
    inbox.send(Message::user("finish the task")).await.unwrap();
    svit.block().await.unwrap();

    assert_eq!(
        outbox.recv().await.unwrap().text(),
        Some("finished after discovery")
    );
    assert_eq!(svit.read("/inbox").unwrap(), Some(value!([])));
}

#[tokio::test]
async fn iteration_limit_does_not_publish_a_tool_call_as_a_completed_turn() {
    let mut svit = Svit::builder("svit://local/limited-turn")
        .unwrap()
        .max_iterations(1)
        .reasoner(scripted_reasoner([
            SimTurn::tool_call("discover", json!({"path": "/memory"})),
            SimTurn::text("unreached answer"),
        ]))
        .build()
        .await
        .unwrap();
    let inbox = svit.inbox();
    let mut outbox = svit.outbox();

    svit.start().unwrap();
    inbox.send(Message::user("finish the task")).await.unwrap();
    let error = svit.block().await.unwrap_err();

    assert!(error.to_string().contains("maximum reason/act iterations"));
    assert!(matches!(outbox.try_recv(), Err(ObserveError::Empty)));
    let queued = svit.read("/inbox").unwrap().unwrap().to_json();
    assert_eq!(queued.as_array().unwrap().len(), 1);
}

#[tokio::test]
#[cfg(feature = "persistence-turso")]
// THREAT[TM-AUD-001] THREAT[TM-MSG-002] THREAT[TM-PERS-002]
async fn persisted_svit_resumes_memory_and_conversation_without_rerunning_reasoning() {
    let store = TursoProcessStore::memory().await.unwrap();
    let process = Process::builder("svit://local/persisted-reasoning")
        .unwrap()
        .memory("status", value!("idle"))
        .build()
        .unwrap();
    let durable = store.create(process).await.unwrap();
    let mut svit = Svit::persisted(durable)
        .unwrap()
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "write",
                json!({"path": "/memory/status", "value": "complete"}),
            ),
            SimTurn::text("persisted answer"),
        ]))
        .ports(Ports::standard())
        .build()
        .await
        .unwrap();

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox
        .send(Message::user("persist this turn"))
        .await
        .unwrap();
    svit.block().await.unwrap();
    let expected_messages = serde_json::to_value(svit.messages().unwrap()).unwrap();
    let expected_version = svit.version().unwrap();
    drop(svit);

    let durable = store
        .resume("svit://local/persisted-reasoning")
        .await
        .unwrap();
    let events = durable.transactions(TransactionQuery::new()).await.unwrap();
    let sources = events
        .iter()
        .map(|event| event.source().to_owned())
        .collect::<Vec<_>>();
    assert!(sources.iter().any(|source| source == "ports.refresh"));
    assert!(sources.iter().any(|source| source == "inbox.enqueue"));
    assert!(sources.iter().any(|source| source == "inbox.acknowledge"));
    assert!(!sources.iter().any(|source| source == "reasoning"));
    let resumed = Svit::persisted(durable)
        .unwrap()
        .reasoner(scripted_reasoner([]))
        .ports(Ports::standard())
        .build()
        .await
        .unwrap();

    assert_eq!(
        resumed.read("/memory/status").unwrap(),
        Some(value!("complete"))
    );
    assert!(resumed.messages().unwrap().is_empty());
    assert_eq!(
        serde_json::to_value(resumed.recent_messages(128).await.unwrap()).unwrap(),
        expected_messages
    );
    assert_eq!(
        resumed.recent_messages(128).await.unwrap()[0].text(),
        Some("persist this turn")
    );
    assert_eq!(
        resumed
            .recent_messages(128)
            .await
            .unwrap()
            .last()
            .unwrap()
            .text(),
        Some("persisted answer")
    );
    let events = resumed.recent_events(128).await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "input.message")
    );
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "output.message.completed")
    );
    assert_eq!(resumed.read("/inbox").unwrap(), Some(value!([])));
    assert_eq!(resumed.version().unwrap(), expected_version);
}

#[tokio::test]
#[cfg(feature = "persistence-turso")]
// THREAT[TM-AUD-001] THREAT[TM-FORK-001]
async fn durable_fork_shares_an_immutable_event_prefix_and_diverges_afterward() {
    let store = TursoProcessStore::memory().await.unwrap();
    let parent = store
        .create(Process::new("svit://local/persisted-thread-parent").unwrap())
        .await
        .unwrap();
    let mut parent = Svit::persisted(parent)
        .unwrap()
        .reasoner(scripted_reasoner([SimTurn::text("parent answer")]))
        .build()
        .await
        .unwrap();
    let inbox = parent.inbox();
    parent.start().unwrap();
    inbox.send(Message::user("parent question")).await.unwrap();
    parent.block().await.unwrap();
    let parent_history = parent.recent_messages(32).await.unwrap();
    drop(parent);

    let parent = store
        .resume("svit://local/persisted-thread-parent")
        .await
        .unwrap();
    let child = parent
        .fork("svit://local/persisted-thread-child")
        .await
        .unwrap();
    let mut child = Svit::persisted(child)
        .unwrap()
        .reasoner(scripted_reasoner([SimTurn::text("child answer")]))
        .build()
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(child.recent_messages(32).await.unwrap()).unwrap(),
        serde_json::to_value(&parent_history).unwrap()
    );

    let inbox = child.inbox();
    child.start().unwrap();
    inbox.send(Message::user("child question")).await.unwrap();
    child.block().await.unwrap();
    let child_history = child.recent_messages(32).await.unwrap();
    assert_eq!(child_history.len(), 4);
    assert_eq!(child_history[2].text(), Some("child question"));
    assert_eq!(child_history[3].text(), Some("child answer"));

    let parent = store
        .resume("svit://local/persisted-thread-parent")
        .await
        .unwrap();
    let parent = Svit::persisted(parent)
        .unwrap()
        .reasoner(scripted_reasoner([]))
        .build()
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(parent.recent_messages(32).await.unwrap()).unwrap(),
        serde_json::to_value(parent_history).unwrap()
    );
}

#[tokio::test]
async fn svit_standard_builtin_setup_derives_model_tools() {
    let svit = Svit::builder("svit://local/port-setup")
        .unwrap()
        .reasoner(scripted_reasoner([]))
        .ports(Ports::standard())
        .build()
        .await
        .unwrap();

    assert_eq!(
        svit.discover("/ports").unwrap(),
        vec!["http", "llm", "spawn"]
    );
}

#[tokio::test]
async fn http_allowlist_layers_onto_a_custom_builtin_set() {
    let svit = Svit::builder("svit://local/http-ports")
        .unwrap()
        .reasoner(scripted_reasoner([]))
        .ports(Ports::new().with_http_allowlist(HttpAllowlist::new()))
        .build()
        .await
        .unwrap();

    assert_eq!(svit.discover("/ports").unwrap(), vec!["http"]);
}

#[tokio::test]
async fn svit_commit_notifications_are_observed_through_the_contract() {
    async fn next_commit(events: &mut svit::Events) -> Change {
        loop {
            match events.recv().await.unwrap() {
                SvitEvent::Committed(change) => return change,
                SvitEvent::CanonicalEvent(_) => {}
                SvitEvent::Message(_) => {}
                SvitEvent::Failed(error) => panic!("Svit failed: {error}"),
            }
        }
    }

    let mut svit = Svit::builder("svit://local/state-events")
        .unwrap()
        .reasoner(scripted_reasoner([SimTurn::text("done")]))
        .build()
        .await
        .unwrap();
    let inbox = svit.inbox();
    let mut events = svit.events();
    let mut second_events = svit.events();
    let mut outbox = svit.outbox();
    assert!(matches!(events.try_recv(), Err(ObserveError::Empty)));
    assert!(matches!(outbox.try_recv(), Err(ObserveError::Empty)));

    svit.start().unwrap();
    inbox.send(Message::user("run")).await.unwrap();
    next_commit(&mut events).await;
    next_commit(&mut second_events).await;
    outbox.recv().await.unwrap();
    let completed = next_commit(&mut events).await;
    next_commit(&mut second_events).await;
    svit.block().await.unwrap();

    assert!(completed.touches("/inbox"));
    let (root, version) = svit.read_versioned("/").unwrap();
    assert_eq!(root, svit.read("/").unwrap());
    assert_eq!(version, svit.version().unwrap());
}

#[tokio::test]
async fn canonical_events_are_observed_without_materializing_thread_history() {
    let mut svit = Svit::builder("svit://local/canonical-event-observation")
        .unwrap()
        .reasoner(scripted_reasoner([SimTurn::text("done")]))
        .build()
        .await
        .unwrap();
    let inbox = svit.inbox();
    let mut events = svit.events();

    svit.start().unwrap();
    inbox.send(Message::user("run")).await.unwrap();
    let observed = loop {
        if let SvitEvent::CanonicalEvent(event) = events.recv().await.unwrap()
            && event.event_type == "input.message"
        {
            break event;
        }
    };
    svit.block().await.unwrap();

    assert_eq!(observed.event_type, "input.message");
    assert_eq!(
        svit.discover("/thread").unwrap(),
        vec![
            "format",
            "instructions",
            "process_id",
            "session_id",
            "system_prompt"
        ]
    );
}

#[tokio::test]
async fn reasoning_tool_commits_are_observed_through_the_contract() {
    let mut svit = Svit::builder("svit://local/reasoning-tool-events")
        .unwrap()
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "write",
                json!({"path": "/memory/research", "value": "complete"}),
            ),
            SimTurn::text("done"),
        ]))
        .build()
        .await
        .unwrap();
    let inbox = svit.inbox();
    let mut outbox = svit.outbox();
    let mut events = svit.events();

    svit.start().unwrap();
    inbox.send(Message::user("remember this")).await.unwrap();
    outbox.recv().await.unwrap();
    let changes = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            SvitEvent::Committed(change) => Some(change),
            SvitEvent::CanonicalEvent(_) | SvitEvent::Message(_) | SvitEvent::Failed(_) => None,
        })
        .collect::<Vec<_>>();
    svit.block().await.unwrap();

    assert!(
        changes
            .iter()
            .any(|change| change.paths() == ["/memory/research"])
    );
}

#[tokio::test]
async fn started_svit_processes_inbox_messages_in_commit_order() {
    let mut svit = Svit::builder("svit://local/ordered-inbox")
        .unwrap()
        .reasoner(scripted_reasoner([
            SimTurn::text("first answer"),
            SimTurn::text("second answer"),
        ]))
        .build()
        .await
        .unwrap();
    let inbox = svit.inbox();
    let mut outbox = svit.outbox();

    svit.start().unwrap();
    inbox.send(Message::user("first question")).await.unwrap();
    assert_eq!(outbox.recv().await.unwrap().text(), Some("first answer"));

    // The process remains live after a completed turn. A message committed
    // while it is waiting becomes the next turn without restarting the loop.
    inbox.send(Message::user("second question")).await.unwrap();
    assert_eq!(outbox.recv().await.unwrap().text(), Some("second answer"));

    svit.block().await.unwrap();
    assert_eq!(svit.messages().unwrap().len(), 4);
    assert_eq!(svit.read("/inbox").unwrap(), Some(value!([])));
}

#[tokio::test]
async fn reasoning_loop_exposes_bounded_thread_metadata_and_host_history() {
    // THREAT[TM-CAP-005]: The port catalog comes from the exact host
    // runtime attached to this process and remains read-only to guest code.
    let instructions = "Inspect your projected runtime state.";
    let mut svit = Svit::builder("svit://local/thread-projection")
        .unwrap()
        .instructions(instructions)
        .reasoner(scripted_reasoner([
            SimTurn::tool_call("read", json!({"path": "/thread/system_prompt"})),
            SimTurn::tool_call("discover", json!({"path": "/ports"})),
            SimTurn::text("projection inspected"),
        ]))
        .build()
        .await
        .unwrap();

    let system_prompt = match svit.read("/thread/system_prompt").unwrap() {
        Some(Value::String(prompt)) => prompt,
        other => panic!("expected projected system prompt, got {other:?}"),
    };
    assert!(system_prompt.contains("svit://local/thread-projection"));
    assert!(system_prompt.ends_with(&format!("<instructions>\n{instructions}\n</instructions>")));
    assert_eq!(
        svit.read("/thread/instructions").unwrap(),
        Some(value!(instructions))
    );
    let thread = svit.read("/thread").unwrap().unwrap().to_json();
    assert_eq!(thread["format"], "svit-thread@8");
    assert!(thread.get("messages").is_none());
    assert!(thread.get("events").is_none());
    assert!(svit.discover("/ports").unwrap().is_empty());

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox
        .send(Message::user("inspect projection"))
        .await
        .unwrap();
    svit.block().await.unwrap();

    let session_messages = serde_json::to_value(svit.messages().unwrap()).unwrap();
    let history = serde_json::to_value(svit.recent_messages(32).await.unwrap()).unwrap();
    assert_eq!(history, session_messages);
    let tool_results = svit
        .messages()
        .unwrap()
        .iter()
        .filter(|message| message.role == MessageRole::ToolResult)
        .map(message_text)
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 2);
    assert!(tool_results[0].contains(instructions));
    assert!(tool_results[1].contains("\"entries\":[]"));
}

#[tokio::test]
async fn resumed_svit_refreshes_builtin_discovery_to_current_host_grants() {
    // THREAT[TM-CAP-005]: Snapshot metadata cannot preserve an absent host
    // grant when a restored process is attached to a new agent runtime.
    let source = Svit::builder("svit://local/tool-projection-source")
        .unwrap()
        .ports(Ports::new().llm(scripted_reasoner([SimTurn::text("nested")])))
        .reasoner(scripted_reasoner([]))
        .build()
        .await
        .unwrap();
    assert!(source.discover("/ports").unwrap().contains(&"llm".into()));
    let snapshot = source.snapshot().unwrap();

    let restored = Process::restore(&snapshot).unwrap();
    let resumed = Svit::resume(restored)
        .reasoner(scripted_reasoner([]))
        .build()
        .await
        .unwrap();

    let tools = resumed.discover("/ports").unwrap();
    assert!(!tools.contains(&"llm".into()));
    assert!(tools.is_empty());
}

#[tokio::test]
async fn reasoning_resume_rejects_materialized_thread_history() {
    // THREAT[TM-AUD-001]: Canonical events are external, paged host storage.
    // A process snapshot may not smuggle a second mutable history projection.
    let svit = Svit::builder("svit://local/projection-source")
        .unwrap()
        .reasoner(scripted_reasoner([]))
        .build()
        .await
        .unwrap();
    let mut process = svit
        .fork_process("svit://local/projection-tampered")
        .unwrap();
    let mut state = process.read("/thread").unwrap().unwrap().to_json();
    state["messages"] = json!([]);
    process
        .replace_thread_state(svit::Value::from_json(state).unwrap())
        .unwrap();

    let result = Svit::resume(process)
        .reasoner(scripted_reasoner([]))
        .build()
        .await;
    let error = match result {
        Ok(_) => panic!("divergent projection resumed"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("bounded /thread metadata contains materialized history"),
        "{error}"
    );
}

#[tokio::test]
async fn reasoning_resume_rejects_thread_metadata_without_its_owner() {
    let svit = Svit::builder("svit://local/event-sequence-source")
        .unwrap()
        .reasoner(scripted_reasoner([]))
        .build()
        .await
        .unwrap();
    let mut process = svit
        .fork_process("svit://local/event-sequence-tampered")
        .unwrap();
    let mut state = process.read("/thread").unwrap().unwrap().to_json();
    state.as_object_mut().unwrap().remove("process_id");
    process
        .replace_thread_state(svit::Value::from_json(state).unwrap())
        .unwrap();

    let result = Svit::resume(process)
        .reasoner(scripted_reasoner([]))
        .build()
        .await;
    let error = match result {
        Ok(_) => panic!("invalid canonical sequence resumed"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("missing /thread process_id"),
        "{error}"
    );
}

#[tokio::test]
async fn process_snapshot_restores_thread_metadata_but_not_event_history() {
    let prompt = "Keep support answers concise.";
    let mut agent = Svit::builder("svit://local/durable-agent")
        .unwrap()
        .instructions(prompt)
        .reasoner(scripted_reasoner([SimTurn::text("first answer")]))
        .build()
        .await
        .unwrap();

    let inbox = agent.inbox();
    agent.start().unwrap();
    inbox.send(Message::user("first question")).await.unwrap();
    agent.block().await.unwrap();
    let snapshot = agent.snapshot().unwrap();
    drop(agent);

    let restored = Process::restore(&snapshot).unwrap();
    let mut resumed = Svit::resume(restored)
        .reasoner(scripted_reasoner([SimTurn::text("second answer")]))
        .build()
        .await
        .unwrap();

    assert!(resumed.messages().unwrap().is_empty());
    assert!(resumed.recent_messages(32).await.unwrap().is_empty());
    assert_eq!(
        resumed.read("/thread/instructions").unwrap(),
        Some(value!(prompt))
    );
    let resumed_prompt = resumed
        .read("/thread/system_prompt")
        .unwrap()
        .unwrap()
        .to_json();
    assert!(
        resumed_prompt
            .as_str()
            .unwrap()
            .contains("svit://local/durable-agent")
    );
    let inbox = resumed.inbox();
    resumed.start().unwrap();
    inbox.send(Message::user("second question")).await.unwrap();
    resumed.block().await.unwrap();
    assert_eq!(resumed.messages().unwrap().len(), 2);
    assert_eq!(
        resumed.messages().unwrap()[0].text(),
        Some("second question")
    );
    assert_eq!(resumed.messages().unwrap()[1].text(), Some("second answer"));
}

#[tokio::test]
async fn child_process_owns_an_isolated_forked_process() {
    // THREAT[TM-FORK-001]: A process-only fork copies process state but starts
    // a new isolated event session.
    let mut parent = Svit::builder("svit://local/parent-agent")
        .unwrap()
        .instructions("Keep inherited context.")
        .reasoner(scripted_reasoner([SimTurn::text("parent answer")]))
        .build()
        .await
        .unwrap();
    let inbox = parent.inbox();
    parent.start().unwrap();
    inbox.send(Message::user("parent question")).await.unwrap();
    parent.block().await.unwrap();

    let parent_hash = parent.root_hash().unwrap();
    let child_process = parent.fork_process("svit://local/child-agent").unwrap();
    let mut child = Svit::resume(child_process)
        .reasoner(scripted_reasoner([SimTurn::text("child answer")]))
        .build()
        .await
        .unwrap();
    assert_eq!(
        child.read("/thread/instructions").unwrap(),
        Some(value!("Keep inherited context."))
    );
    let child_prompt = child
        .read("/thread/system_prompt")
        .unwrap()
        .unwrap()
        .to_json();
    assert!(
        child_prompt
            .as_str()
            .unwrap()
            .contains("svit://local/child-agent")
    );
    assert!(
        !child_prompt
            .as_str()
            .unwrap()
            .contains("svit://local/parent-agent")
    );
    let inbox = child.inbox();
    child.start().unwrap();
    inbox.send(Message::user("child question")).await.unwrap();
    child.block().await.unwrap();

    assert_eq!(parent.root_hash().unwrap(), parent_hash);
    assert_eq!(parent.messages().unwrap().len(), 2);
    assert_eq!(child.messages().unwrap().len(), 2);
}

#[tokio::test]
async fn process_reasoning_executes_any_process_script() {
    let mut agent = Svit::builder("svit://local/process-agent")
        .unwrap()
        .memory("changed", value!(false))
        .library(
            "any_script",
            Script::new(r#"(define (main input) (do (write "/memory/changed" true) input))"#),
        )
        .reasoner(scripted_reasoner([
            SimTurn::tool_call("exec", json!({"path": "/lib/any_script", "input": null})),
            SimTurn::text("script completed"),
        ]))
        .build()
        .await
        .unwrap();

    assert!(agent.discover("/ports").unwrap().is_empty());

    let inbox = agent.inbox();
    agent.start().unwrap();
    inbox.send(Message::user("run the script")).await.unwrap();
    agent.block().await.unwrap();

    assert_eq!(agent.read("/memory/changed").unwrap(), Some(value!(true)));
}

#[tokio::test]
async fn system_prompt_teaches_the_script_library_lifecycle() {
    let svit = Svit::builder("svit://local/script-guidance")
        .unwrap()
        .reasoner(scripted_reasoner([]))
        .build()
        .await
        .unwrap();

    let prompt = svit
        .read("/thread/system_prompt")
        .unwrap()
        .unwrap()
        .to_json();
    let prompt = prompt.as_str().unwrap();
    assert!(prompt.contains("Scripts are first-class process state under /lib."));
    assert!(prompt.contains("For an operation you expect to reuse"));
    assert!(prompt.contains("exec with path /lib/name"));
    assert!(prompt.contains("For a one-off operation, provide source directly to exec"));
    assert!(prompt.contains("Either form can call ports and write durable state"));
    assert!(prompt.contains("A port response is activation-local"));
    assert!(prompt.contains("(jq filter value)"));
    assert!(prompt.contains("(search path pattern)"));
}

#[tokio::test]
async fn reasoning_event_growth_fails_closed_at_the_process_limit() {
    // THREAT[TM-DOS-003]: Model output enters the durable event stream only
    // through bounded Svit value validation.
    // THREAT[TM-INF-001]: The terminal failure also crosses the bounded
    // operational event stream without being replaced by a task error.
    let limits = Limits {
        max_text_bytes: 4 * 1024,
        ..Limits::default()
    };
    let mut agent = Svit::builder("svit://local/bounded-agent")
        .unwrap()
        .limits(limits)
        .reasoner(scripted_reasoner([SimTurn::text("x".repeat(8 * 1024))]))
        .build()
        .await
        .unwrap();

    let inbox = agent.inbox();
    let mut events = agent.events();
    agent.start().unwrap();
    inbox
        .send(Message::user("produce too much output"))
        .await
        .unwrap();
    let reported = loop {
        if let SvitEvent::Failed(error) = events.recv().await.unwrap() {
            break error;
        }
    };
    let error = agent.block().await.unwrap_err();

    assert!(
        reported.contains("maximum text bytes exceeded"),
        "{reported}"
    );
    assert!(
        error.to_string().contains("maximum text bytes exceeded"),
        "{error}"
    );
    let committed = agent.read("/thread").unwrap().unwrap().to_json();
    assert!(!committed.to_string().contains(&"x".repeat(8 * 1024)));
    assert!(!agent.discover("/inbox").unwrap().is_empty());
}

#[tokio::test]
async fn host_extension_registers_discoverable_process_builtin() {
    // THREAT[TM-CAP-006]: An extension receives the same explicit input and
    // read-only process context as a standard port.
    let mut svit = Svit::builder("svit://local/port-extension")
        .unwrap()
        .memory("color", value!("blue"))
        .ports(Ports::new().extension(FixturePorts))
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "source": "(define (main input) (port-call \"read-value\" input))",
                    "input": {"path": "/memory/color"}
                }),
            ),
            SimTurn::text("read extension value"),
        ]))
        .build()
        .await
        .unwrap();

    assert!(
        svit.discover("/ports")
            .unwrap()
            .contains(&"read-value".into())
    );
    let descriptor = svit.read("/ports/read-value").unwrap().unwrap().to_json();
    assert_eq!(descriptor["version"], "v1");
    assert_eq!(descriptor["effect"], "read");
    assert_eq!(descriptor["output"], "The committed JSON value at path.");

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox.send(Message::user("read the color")).await.unwrap();
    svit.block().await.unwrap();

    let tool_result = svit
        .messages()
        .unwrap()
        .iter()
        .find(|message| message.role == MessageRole::ToolResult)
        .map(message_text)
        .unwrap();
    assert!(tool_result.contains("blue"));
}

#[tokio::test]
async fn host_port_registration_exposes_its_own_descriptor() {
    let svit = Svit::builder("svit://local/port-override")
        .unwrap()
        .ports(
            Ports::standard()
                .with_http_allowlist(HttpAllowlist::new())
                .port("read-value", Box::new(ReadValue)),
        )
        .reasoner(scripted_reasoner([]))
        .build()
        .await
        .unwrap();

    let descriptor = svit.read("/ports/read-value").unwrap().unwrap().to_json();
    assert_eq!(
        descriptor["description"],
        "Read one committed process value."
    );
    assert_eq!(descriptor["effect"], "read");
}

#[tokio::test]
async fn port_output_cannot_cross_the_persistent_value_boundary() {
    // THREAT[TM-DOS-008]: A port may return activation-local data, but a script
    // cannot publish an oversized result into memory or model-visible output.
    let mut svit = Svit::builder("svit://local/port-output-limit")
        .unwrap()
        .ports(Ports::new().port("oversized", Box::new(OversizedOutput)))
        .reasoner(scripted_reasoner([
            SimTurn::tool_call("exec", json!({"source": "(define (main input) (port-call \"oversized\" input))", "input": {}})),
            SimTurn::text("limit observed"),
        ]))
        .build()
        .await
        .unwrap();

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox.send(Message::user("run oversized")).await.unwrap();
    svit.block().await.unwrap();

    let tool_result = svit
        .messages()
        .unwrap()
        .iter()
        .find(|message| message.role == MessageRole::ToolResult)
        .map(message_text)
        .unwrap();
    assert!(tool_result.contains("maximum text bytes exceeded"));
    assert!(!tool_result.contains(&"x".repeat(1024)));
}

#[tokio::test]
async fn standard_library_processes_structured_data() {
    let records = json!({
        "items": [
            {"name": "alpha", "active": false},
            {"name": "beta", "active": true}
        ]
    });
    let mut svit = Svit::builder("svit://local/native-data-tools")
        .unwrap()
        .memory("records", value!({"items": records["items"].clone()}))
        .ports(Ports::new())
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "source": "(define (main input) (search \"/memory/records\" \"^beta$\"))",
                    "input": null
                }),
            ),
            SimTurn::tool_call(
                "exec",
                json!({
                    "source": "(define (main input) (jq \".items[] | select(.active) | .name\" input))",
                    "input": records
                }),
            ),
            SimTurn::text("found active record"),
        ]))
        .build()
        .await
        .unwrap();

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox
        .send(Message::user("find the active record"))
        .await
        .unwrap();
    svit.block().await.unwrap();

    let tool_results = svit
        .messages()
        .unwrap()
        .iter()
        .filter(|message| message.role == MessageRole::ToolResult)
        .map(message_text)
        .collect::<Vec<_>>();
    assert!(tool_results[0].contains("/memory/records/items/1/name"));
    assert!(tool_results[1].contains("beta"));
}

#[tokio::test]
async fn jq_standard_library_processes_json_returned_by_the_http_port() {
    let mut svit = Svit::builder("svit://local/http-jq-pipeline")
        .unwrap()
        .ports(Ports::new().http(
            HttpAllowlist::new().allow("https://8.8.8.8/tree"),
            TreeFixtureHttp,
        ))
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "source": r#"
                        (define (main input)
                          (let ((response
                                 (port-call "http"
                                   (value-map "method" "GET" "url" "https://8.8.8.8/tree"))))
                            (jq "[.tree[] | select(.type == \"blob\")] | length" response)))
                    "#,
                    "input": null,
                }),
            ),
            SimTurn::text("counted blobs"),
        ]))
        .build()
        .await
        .unwrap();

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox.send(Message::user("count blobs")).await.unwrap();
    svit.block().await.unwrap();

    let tool_result = svit
        .messages()
        .unwrap()
        .iter()
        .find(|message| message.role == MessageRole::ToolResult)
        .map(message_text)
        .unwrap();
    assert!(tool_result.contains("[2]"), "{tool_result}");
}

#[tokio::test]
async fn model_catalog_scenario_uses_a_saved_script_to_reduce_http_into_memory() {
    let script = r#"
        (define (main input)
          (let ((response
                 (port-call "http"
                   (value-map "method" "GET" "url" "https://models.dev/models.json"))))
            (let ((summary
                   (jq "to_entries | map(.value) | {model_count: length, latest_gpt_models: ([.[] | select(.id | startswith(\"openai/gpt\"))] | sort_by(.release_date) | reverse | .[:2] | map({id, name, release_date}))}" response)))
              (do
                (write "/memory/model_catalog" (value-get summary "/0"))
                (value-get summary "/0")))))
    "#;
    let mut svit = Svit::builder("svit://local/model-catalog-scenario")
        .unwrap()
        .ports(Ports::new().http(
            HttpAllowlist::new().allow("https://models.dev/models.json"),
            ModelCatalogFixtureHttp,
        ))
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "write",
                json!({
                    "path": "/lib/summarize-model-catalog",
                    "value": {
                        "source": script,
                        "documentation": "Fetch a model catalog, count its models, and save the newest GPT models."
                    }
                }),
            ),
            SimTurn::tool_call(
                "exec",
                json!({"path": "/lib/summarize-model-catalog", "input": null}),
            ),
            SimTurn::text("Saved the model count and latest GPT models to /memory/model_catalog."),
        ]))
        .build()
        .await
        .unwrap();

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox
        .send(Message::user(
            "Count models.dev models and save the latest GPT models to memory.",
        ))
        .await
        .unwrap();
    svit.block().await.unwrap();

    assert_eq!(
        svit.read("/memory/model_catalog").unwrap(),
        Some(value!({
            "model_count": 4,
            "latest_gpt_models": [
                {"id": "openai/gpt-5.6", "name": "GPT-5.6", "release_date": "2026-08-01"},
                {"id": "openai/gpt-5.4", "name": "GPT-5.4", "release_date": "2026-06-01"}
            ]
        }))
    );
    assert!(svit.read("/lib/summarize-model-catalog").unwrap().is_some());
}

#[tokio::test]
async fn builtin_http_is_denied_without_an_explicit_url_grant() {
    // THREAT[TM-CAP-004]: Merely supplying a URL does not grant HTTP authority.
    let mut svit = Svit::builder("svit://local/native-network-denied")
        .unwrap()
        .ports(Ports::new().http(HttpAllowlist::new(), FixtureHttp))
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "source": "(define (main input) (port-call \"http\" input))",
                    "input": {"method": "GET", "url": "https://example.com"}
                }),
            ),
            SimTurn::text("network denied"),
        ]))
        .build()
        .await
        .unwrap();

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox.send(Message::user("try the network")).await.unwrap();
    svit.block().await.unwrap();

    let tool_result = message_text(
        svit.messages()
            .unwrap()
            .iter()
            .find(|message| message.role == MessageRole::ToolResult)
            .unwrap(),
    );
    assert!(tool_result.contains("HTTP URL is not allowed"));
}

#[tokio::test]
async fn inline_exec_can_call_http_and_commit_memory() {
    // THREAT[TM-CAP-004]: An allowed call passes both the host-selected URL
    // policy and host-owned transport before its response reaches the model.
    let tools = Ports::new().http(
        HttpAllowlist::new().allow("https://8.8.8.8/data"),
        FixtureHttp,
    );
    let mut svit = Svit::builder("svit://local/native-network-allowed")
        .unwrap()
        .ports(tools)
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "source": "(define (main input) ((lambda (response) (do (write \"/memory/inline-http\" response) response)) (port-call \"http\" input)))",
                    "input": {"method": "GET", "url": "https://8.8.8.8/data"}
                }),
            ),
            SimTurn::text("network allowed"),
        ]))
        .build()
        .await
        .unwrap();

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox.send(Message::user("read the fixture")).await.unwrap();
    svit.block().await.unwrap();

    let tool_result = message_text(
        svit.messages()
            .unwrap()
            .iter()
            .find(|message| message.role == MessageRole::ToolResult)
            .unwrap(),
    );
    assert!(tool_result.contains("fixture"), "{tool_result}");
    assert!(
        svit.read("/memory/inline-http").unwrap().unwrap().to_json()["body"]
            .as_str()
            .unwrap()
            .contains("fixture")
    );
    assert_eq!(svit.read("/lib/inline").unwrap(), None);
}

#[tokio::test]
#[cfg(feature = "persistence-turso")]
// THREAT[TM-CAP-004] THREAT[TM-EFF-005] THREAT[TM-PERS-002]
async fn persisted_model_authored_script_invokes_host_http_builtin() {
    let store = TursoProcessStore::memory().await.unwrap();
    let process = Process::new("svit://local/scripted-http").unwrap();
    let durable = store.create(process).await.unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let ports = Ports::new().http(
        HttpAllowlist::new().allow("https://8.8.8.8/data"),
        CountingFixtureHttp(requests.clone()),
    );
    let mut svit = Svit::persisted(durable)
        .unwrap()
        .ports(ports)
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "write",
                json!({
                    "path": "/lib/fetch-everruns",
                    "value": {
                        "source": "(define (main input) ((lambda (response) (do (write \"/memory/http-response\" response) response)) (port-call \"http\" (value-map \"method\" \"GET\" \"url\" \"https://8.8.8.8/data\"))))",
                        "documentation": "Fetch the Everruns homepage."
                    }
                }),
            ),
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/lib/fetch-everruns",
                    "input": null
                }),
            ),
            SimTurn::text("fetched through the saved script"),
        ]))
        .build()
        .await
        .unwrap();

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox
        .send(Message::user(
            "create and invoke a reusable Everruns fetch script",
        ))
        .await
        .unwrap();
    svit.block().await.unwrap();

    let tool_results = svit
        .messages()
        .unwrap()
        .iter()
        .filter(|message| message.role == MessageRole::ToolResult)
        .map(message_text)
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 2);
    assert!(
        tool_results[1].contains("\"status\":200"),
        "{}",
        tool_results[1]
    );
    assert!(
        tool_results[1].contains("source") && tool_results[1].contains("fixture"),
        "{}",
        tool_results[1]
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(
        svit.read("/memory/http-response")
            .unwrap()
            .unwrap()
            .to_json()
            .get("body")
            .and_then(serde_json::Value::as_str)
            .unwrap()
            .contains("fixture")
    );
    assert_eq!(svit.read("/inbox").unwrap(), Some(value!([])));
}

#[tokio::test]
// THREAT[TM-EFF-001] THREAT[TM-EFF-005]
async fn failed_script_does_not_repeat_builtin_or_commit_guest_state() {
    let requests = Arc::new(AtomicUsize::new(0));
    let ports = Ports::new().http(
        HttpAllowlist::new().allow("https://8.8.8.8/data"),
        CountingFixtureHttp(requests.clone()),
    );
    let mut svit = Svit::builder("svit://local/failed-scripted-http")
        .unwrap()
        .library(
            "fetch-then-fail",
            Script::new(
                "(define (main input) (do (write \"/memory/uncommitted\" true) (port-call \"http\" input) (panic \"failed after HTTP\")))",
            ),
        )
        .ports(ports)
        .reasoner(scripted_reasoner([]))
        .build()
        .await
        .unwrap();
    let version = svit.version().unwrap();

    let error = svit
        .exec(
            "/lib/fetch-then-fail",
            value!({"method": "GET", "url": "https://8.8.8.8/data"}),
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("failed after HTTP"));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(svit.version().unwrap(), version);
    assert!(svit.read("/memory/uncommitted").unwrap().is_none());
}

#[tokio::test]
async fn builtin_llm_uses_only_the_host_selected_nested_model() {
    // THREAT[TM-EFF-005]: Nested model execution requires a host-selected
    // driver and remains outside the process transaction.
    let mut svit = Svit::builder("svit://local/native-llm")
        .unwrap()
        .ports(Ports::new().llm(scripted_reasoner([SimTurn::text("nested answer")])))
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "source": "(define (main input) (port-call \"llm\" input))",
                    "input": {"prompt": "nested question"}
                }),
            ),
            SimTurn::text("outer answer"),
        ]))
        .build()
        .await
        .unwrap();

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox.send(Message::user("delegate once")).await.unwrap();
    svit.block().await.unwrap();

    let tool_result = message_text(
        svit.messages()
            .unwrap()
            .iter()
            .find(|message| message.role == MessageRole::ToolResult)
            .unwrap(),
    );
    assert!(tool_result.contains("nested answer"));
}

#[tokio::test]
async fn builtin_spawn_runs_and_retains_an_isolated_child_svit() {
    // THREAT[TM-FORK-002]: spawn forks committed state, runs the child with
    // separately supplied model authority, and retains an isolated snapshot.
    let child_id = svit::ProcessId::new("svit://local/native-child").unwrap();
    let mut parent = Svit::builder("svit://local/native-parent")
        .unwrap()
        .memory("owner", value!("parent"))
        .ports(Ports::new().spawn(scripted_reasoner([SimTurn::text("child answer")])))
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "source": "(define (main input) (port-call \"spawn\" input))",
                    "input": {"id": "svit://local/native-child", "task": "analyze this"}
                }),
            ),
            SimTurn::tool_call(
                "exec",
                json!({
                    "source": "(define (main input) (port-call \"spawn\" input))",
                    "input": {"id": "svit://local/native-child", "task": "duplicate"}
                }),
            ),
            SimTurn::text("parent answer"),
        ]))
        .build()
        .await
        .unwrap();

    let inbox = parent.inbox();
    parent.start().unwrap();
    inbox.send(Message::user("create a child")).await.unwrap();
    parent.block().await.unwrap();

    assert_eq!(parent.child_ids(), vec![child_id.clone()]);
    assert_eq!(
        parent.read("/memory/owner").unwrap(),
        Some(value!("parent"))
    );
    let snapshot = parent.child_snapshot(&child_id).unwrap().unwrap();
    let child = Process::restore(&snapshot).unwrap();
    assert_eq!(
        child.read("/system/lineage/parent").unwrap(),
        Some(value!("svit://local/native-parent"))
    );
    let thread = child.read("/thread").unwrap().unwrap().to_json();
    assert_eq!(thread["process_id"], "svit://local/native-child");
    assert!(thread.get("messages").is_none());
    assert!(thread.get("events").is_none());
    let duplicate_rejected = parent
        .messages()
        .unwrap()
        .iter()
        .filter(|message| message.role == MessageRole::ToolResult)
        .any(|message| message_text(message).contains("child address already exists"));
    assert!(duplicate_rejected);
}

#[tokio::test]
async fn bounded_data_operations_reject_unbounded_or_oversized_work() {
    // THREAT[TM-DOS-008]: Runtime-library jq rejects unbounded constructs and
    // search rejects oversized expressions before evaluation.
    let mut svit = Svit::builder("svit://local/native-limits")
        .unwrap()
        .memory("text", value!("bounded"))
        .ports(Ports::new())
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "source": "(define (main input) (jq \"def f: f; f\" input))",
                    "input": null
                }),
            ),
            SimTurn::tool_call(
                "exec",
                json!({
                    "source": "(define (main input) (search \"/memory/text\" input))",
                    "input": "x".repeat(5 * 1024)
                }),
            ),
            SimTurn::text("limit observed"),
        ]))
        .build()
        .await
        .unwrap();

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox.send(Message::user("loop forever")).await.unwrap();
    svit.block().await.unwrap();

    let tool_results = svit
        .messages()
        .unwrap()
        .iter()
        .filter(|message| message.role == MessageRole::ToolResult)
        .map(message_text)
        .collect::<Vec<_>>();
    assert!(tool_results[0].contains("unbounded construct"));
    assert!(tool_results[1].contains("pattern limit exceeded"));
}
