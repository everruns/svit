use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::json;
use svit::{
    Builtin, BuiltinContext, BuiltinExtension, BuiltinManual, BuiltinResult, Builtins, ContentPart,
    HttpAllowlist, HttpRequest, HttpResponse, HttpTransport, HttpTransportError, LLMSIM_MODEL_ID,
    Limits, LlmSimConfig, Message, MessageRole, ObserveError, Process, Reasoner, Script,
    SimToolCall, SimTurn, Svit, SvitError, SvitEvent, Value, llm_sim_provider, value,
};

struct FixtureHttp;

struct ReadValue;

#[async_trait]
impl Builtin for ReadValue {
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
        .limits(["Read-only process context."])
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

struct FixtureBuiltins;

impl BuiltinExtension for FixtureBuiltins {
    fn builtins(&self) -> Vec<(String, Box<dyn Builtin>)> {
        vec![("read-value".into(), Box::new(ReadValue))]
    }
}

struct OversizedOutput;

#[async_trait]
impl Builtin for OversizedOutput {
    fn manual(&self) -> BuiltinManual {
        BuiltinManual::new("Return an oversized fixture.", json!({"type": "object"}))
    }

    async fn execute(&self, _context: BuiltinContext, _input: serde_json::Value) -> BuiltinResult {
        BuiltinResult::text("x".repeat(300 * 1024))
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
        inbox.send(Message::user("too late")),
        Err(svit::SvitError::InboxClosed)
    ));
}

#[tokio::test]
async fn svit_standard_builtin_setup_derives_model_tools() {
    let svit = Svit::builder("svit://local/builtin-setup")
        .unwrap()
        .reasoner(scripted_reasoner([]))
        .builtins(Builtins::standard())
        .build()
        .await
        .unwrap();

    assert_eq!(
        svit.discover("/bin").unwrap(),
        vec!["http", "jq", "llm", "search", "spawn"]
    );
}

#[tokio::test]
async fn http_allowlist_layers_onto_a_custom_builtin_set() {
    let svit = Svit::builder("svit://local/http-builtins")
        .unwrap()
        .reasoner(scripted_reasoner([]))
        .builtins(Builtins::new().with_http_allowlist(HttpAllowlist::new()))
        .build()
        .await
        .unwrap();

    assert_eq!(svit.discover("/bin").unwrap(), vec!["http", "jq", "search"]);
}

#[tokio::test]
async fn svit_commit_notifications_are_observed_through_the_contract() {
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
    assert_eq!(events.try_recv(), Err(ObserveError::Empty));
    assert!(matches!(outbox.try_recv(), Err(ObserveError::Empty)));

    svit.start().unwrap();
    inbox.send(Message::user("run")).unwrap();
    let queued = events.recv().await.unwrap();
    assert_eq!(queued, SvitEvent::Committed);
    assert_eq!(second_events.recv().await.unwrap(), SvitEvent::Committed);
    outbox.recv().await.unwrap();
    let completed = events.recv().await.unwrap();
    assert_eq!(second_events.recv().await.unwrap(), SvitEvent::Committed);
    svit.block().await.unwrap();

    assert_eq!(completed, SvitEvent::Committed);
    let (root, version) = svit.read_versioned("/").unwrap();
    assert_eq!(root, svit.read("/").unwrap());
    assert_eq!(version, svit.version().unwrap());
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
    inbox.send(Message::user("first question")).unwrap();
    assert_eq!(outbox.recv().await.unwrap().text(), Some("first answer"));

    // The process remains live after a completed turn. A message committed
    // while it is waiting becomes the next turn without restarting the loop.
    inbox.send(Message::user("second question")).unwrap();
    assert_eq!(outbox.recv().await.unwrap().text(), Some("second answer"));

    svit.block().await.unwrap();
    assert_eq!(svit.messages().unwrap().len(), 4);
    assert_eq!(svit.read("/inbox").unwrap(), Some(value!([])));
}

#[tokio::test]
async fn reasoning_loop_projects_prompt_messages_and_events() {
    // THREAT[TM-CAP-005]: The built-in catalog comes from the exact host
    // runtime attached to this process and remains read-only to guest code.
    let instructions = "Inspect your projected runtime state.";
    let mut svit = Svit::builder("svit://local/thread-projection")
        .unwrap()
        .instructions(instructions)
        .builtins(Builtins::new())
        .reasoner(scripted_reasoner([
            SimTurn::tool_call("read", json!({"path": "/thread/system_prompt"})),
            SimTurn::tool_call("read", json!({"path": "/thread/messages"})),
            SimTurn::tool_call("read", json!({"path": "/thread/events"})),
            SimTurn::tool_call("discover", json!({"path": "/bin"})),
            SimTurn::tool_call("read", json!({"path": "/bin/search"})),
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
    assert_eq!(svit.read("/thread/messages").unwrap(), Some(value!([])));
    let initial_events = svit.read("/thread/events").unwrap().unwrap().to_json();
    assert_eq!(initial_events.as_array().unwrap().len(), 1);
    assert_eq!(initial_events[0]["type"], "session.started");
    assert_eq!(svit.discover("/bin").unwrap(), vec!["jq", "search"]);
    let search_manual = svit.read("/bin/search").unwrap().unwrap().to_json();
    assert_eq!(search_manual["name"], "search");
    assert_eq!(search_manual["effect"], "read");
    assert_eq!(
        search_manual["input_schema"]["required"],
        json!(["path", "pattern"])
    );
    assert!(search_manual["limits"].is_array());

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox.send(Message::user("inspect projection")).unwrap();
    svit.block().await.unwrap();

    let projected_messages = svit.read("/thread/messages").unwrap().unwrap().to_json();
    let session_messages = serde_json::to_value(svit.messages().unwrap()).unwrap();
    assert_eq!(projected_messages, session_messages);
    assert!(!svit.discover("/thread/events").unwrap().is_empty());
    let tool_results = svit
        .messages()
        .unwrap()
        .iter()
        .filter(|message| message.role == MessageRole::ToolResult)
        .map(message_text)
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 5);
    assert!(tool_results[0].contains(instructions));
    assert!(tool_results[1].contains("inspect projection"));
    assert!(tool_results[2].contains("session_id"));
    assert!(tool_results[3].contains("search"));
    assert!(tool_results[4].contains("input_schema"));
}

#[tokio::test]
async fn resumed_svit_refreshes_builtin_discovery_to_current_host_grants() {
    // THREAT[TM-CAP-005]: Snapshot metadata cannot preserve an absent host
    // grant when a restored process is attached to a new agent runtime.
    let source = Svit::builder("svit://local/tool-projection-source")
        .unwrap()
        .builtins(Builtins::new().llm(scripted_reasoner([SimTurn::text("nested")])))
        .reasoner(scripted_reasoner([]))
        .build()
        .await
        .unwrap();
    assert!(source.discover("/bin").unwrap().contains(&"llm".into()));
    let snapshot = source.snapshot().unwrap();

    let restored = Process::restore(&snapshot).unwrap();
    let resumed = Svit::resume(restored)
        .reasoner(scripted_reasoner([]))
        .build()
        .await
        .unwrap();

    let tools = resumed.discover("/bin").unwrap();
    assert!(!tools.contains(&"llm".into()));
    assert!(!tools.contains(&"search".into()));
    assert!(tools.is_empty());
}

#[tokio::test]
async fn reasoning_resume_rejects_a_divergent_message_projection() {
    // THREAT[TM-AUD-001]: Events are canonical; a forged derived history must
    // not become the conversation seen by a resumed agent.
    let mut svit = Svit::builder("svit://local/projection-source")
        .unwrap()
        .reasoner(scripted_reasoner([SimTurn::text("recorded answer")]))
        .build()
        .await
        .unwrap();
    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox.send(Message::user("record this")).unwrap();
    svit.block().await.unwrap();

    let mut process = svit
        .fork_process("svit://local/projection-tampered")
        .unwrap();
    let mut state = process.read("/thread").unwrap().unwrap().to_json();
    state["messages"][0]["id"] = state["messages"][1]["id"].clone();
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
            .contains("messages do not match the Everruns event projection"),
        "{error}"
    );
}

#[tokio::test]
async fn reasoning_resume_rejects_an_invalid_canonical_event_sequence() {
    // THREAT[TM-AUD-001]: The EventLog SPI requires a unique, increasing
    // sequence. A snapshot cannot forge the replay order accepted on resume.
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
    state["events"][0]["sequence"] = json!(2);
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
        error.to_string().contains("event sequence is invalid"),
        "{error}"
    );
}

#[tokio::test]
async fn svit_resumes_its_thread_from_the_process_snapshot() {
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
    inbox.send(Message::user("first question")).unwrap();
    agent.block().await.unwrap();
    let snapshot = agent.snapshot().unwrap();
    drop(agent);

    let restored = Process::restore(&snapshot).unwrap();
    let mut resumed = Svit::resume(restored)
        .reasoner(scripted_reasoner([SimTurn::text("second answer")]))
        .build()
        .await
        .unwrap();

    assert_eq!(resumed.messages().unwrap().len(), 2);
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
    inbox.send(Message::user("second question")).unwrap();
    resumed.block().await.unwrap();
    assert_eq!(resumed.messages().unwrap().len(), 4);
    assert_eq!(
        resumed.messages().unwrap()[0].text(),
        Some("first question")
    );
    assert_eq!(resumed.messages().unwrap()[1].text(), Some("first answer"));
}

#[tokio::test]
async fn child_process_owns_an_isolated_forked_process() {
    // THREAT[TM-FORK-001]: A subagent inherits committed thread state but
    // appends future events only to its own process.
    let mut parent = Svit::builder("svit://local/parent-agent")
        .unwrap()
        .instructions("Keep inherited context.")
        .reasoner(scripted_reasoner([SimTurn::text("parent answer")]))
        .build()
        .await
        .unwrap();
    let inbox = parent.inbox();
    parent.start().unwrap();
    inbox.send(Message::user("parent question")).unwrap();
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
    inbox.send(Message::user("child question")).unwrap();
    child.block().await.unwrap();

    assert_eq!(parent.root_hash().unwrap(), parent_hash);
    assert_eq!(parent.messages().unwrap().len(), 2);
    assert_eq!(child.messages().unwrap().len(), 4);
}

#[tokio::test]
async fn process_reasoning_enforces_its_script_allowlist() {
    // THREAT[TM-CAP-002]: Svit assembles the Everruns tools and checks script
    // authority before executing model-supplied arguments.
    let mut agent = Svit::builder("svit://local/restricted-agent")
        .unwrap()
        .memory("changed", value!(false))
        .library(
            "denied",
            Script::new(r#"(define (main input) (do (write "/memory/changed" true) input))"#),
        )
        .reasoner(scripted_reasoner([
            SimTurn::tool_call("exec", json!({"path": "/lib/denied", "input": null})),
            SimTurn::text("denied as expected"),
        ]))
        .allow_scripts(["allowed"])
        .build()
        .await
        .unwrap();

    assert!(agent.discover("/bin").unwrap().is_empty());

    let inbox = agent.inbox();
    agent.start().unwrap();
    inbox.send(Message::user("try the denied script")).unwrap();
    agent.block().await.unwrap();

    assert_eq!(agent.read("/memory/changed").unwrap(), Some(value!(false)));
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
    // read-only process context as a standard built-in.
    let mut svit = Svit::builder("svit://local/builtin-extension")
        .unwrap()
        .memory("color", value!("blue"))
        .builtins(Builtins::new().extension(FixtureBuiltins))
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/bin/read-value",
                    "input": {"path": "/memory/color"}
                }),
            ),
            SimTurn::text("read extension value"),
        ]))
        .build()
        .await
        .unwrap();

    assert!(
        svit.discover("/bin")
            .unwrap()
            .contains(&"read-value".into())
    );
    let manual = svit.read("/bin/read-value").unwrap().unwrap().to_json();
    assert_eq!(manual["effect"], "read");
    assert_eq!(manual["output"], "The committed JSON value at path.");

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox.send(Message::user("read the color")).unwrap();
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
async fn later_builtin_registration_replaces_the_default_entry() {
    let svit = Svit::builder("svit://local/builtin-override")
        .unwrap()
        .builtins(
            Builtins::standard()
                .with_http_allowlist(HttpAllowlist::new())
                .builtin("search", Box::new(ReadValue)),
        )
        .reasoner(scripted_reasoner([]))
        .build()
        .await
        .unwrap();

    let manual = svit.read("/bin/search").unwrap().unwrap().to_json();
    assert_eq!(manual["description"], "Read one committed process value.");
    assert_eq!(manual["effect"], "read");
}

#[tokio::test]
async fn host_builtin_output_is_globally_bounded() {
    // THREAT[TM-DOS-008]: Host extensions cannot bypass the aggregate output
    // limit enforced by the built-in dispatcher.
    let mut svit = Svit::builder("svit://local/builtin-output-limit")
        .unwrap()
        .builtins(Builtins::new().builtin("oversized", Box::new(OversizedOutput)))
        .reasoner(scripted_reasoner([
            SimTurn::tool_call("exec", json!({"path": "/bin/oversized", "input": {}})),
            SimTurn::text("limit observed"),
        ]))
        .build()
        .await
        .unwrap();

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox.send(Message::user("run oversized")).unwrap();
    svit.block().await.unwrap();

    let tool_result = svit
        .messages()
        .unwrap()
        .iter()
        .find(|message| message.role == MessageRole::ToolResult)
        .map(message_text)
        .unwrap();
    assert!(tool_result.contains("built-in output limit exceeded"));
    assert!(!tool_result.contains(&"x".repeat(1024)));
}

#[tokio::test]
async fn builtin_search_and_jq_process_structured_data() {
    let records = json!({
        "items": [
            {"name": "alpha", "active": false},
            {"name": "beta", "active": true}
        ]
    });
    let mut svit = Svit::builder("svit://local/native-data-tools")
        .unwrap()
        .memory("records", value!({"items": records["items"].clone()}))
        .builtins(Builtins::new())
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/bin/search",
                    "input": {
                        "path": "/memory/records",
                        "pattern": "^beta$"
                    }
                }),
            ),
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/bin/jq",
                    "input": {
                        "filter": ".items[] | select(.active) | .name",
                        "input": records
                    }
                }),
            ),
            SimTurn::text("found active record"),
        ]))
        .build()
        .await
        .unwrap();

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox.send(Message::user("find the active record")).unwrap();
    svit.block().await.unwrap();

    let tool_results = svit
        .messages()
        .unwrap()
        .iter()
        .filter(|message| message.role == MessageRole::ToolResult)
        .map(message_text)
        .collect::<Vec<_>>();
    assert!(tool_results[0].contains("/memory/records/items/1/name"));
    assert!(tool_results[1].contains("\"beta\""));
}

#[tokio::test]
async fn builtin_http_is_denied_without_an_explicit_url_grant() {
    // THREAT[TM-CAP-004]: Merely supplying a URL does not grant HTTP authority.
    let mut svit = Svit::builder("svit://local/native-network-denied")
        .unwrap()
        .builtins(Builtins::new().http(HttpAllowlist::new(), FixtureHttp))
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/bin/http",
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
    inbox.send(Message::user("try the network")).unwrap();
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
async fn builtin_http_uses_the_host_allowlist_and_transport() {
    // THREAT[TM-CAP-004]: An allowed call passes both the host-selected URL
    // policy and host-owned transport before its response reaches the model.
    let tools = Builtins::new().http(
        HttpAllowlist::new().allow("https://8.8.8.8/data"),
        FixtureHttp,
    );
    let mut svit = Svit::builder("svit://local/native-network-allowed")
        .unwrap()
        .builtins(tools)
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/bin/http",
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
    inbox.send(Message::user("read the fixture")).unwrap();
    svit.block().await.unwrap();

    let tool_result = message_text(
        svit.messages()
            .unwrap()
            .iter()
            .find(|message| message.role == MessageRole::ToolResult)
            .unwrap(),
    );
    assert!(tool_result.contains("fixture"), "{tool_result}");
}

#[tokio::test]
async fn builtin_llm_uses_only_the_host_selected_nested_model() {
    // THREAT[TM-EFF-005]: Nested model execution requires a host-selected
    // driver and remains outside the process transaction.
    let mut svit = Svit::builder("svit://local/native-llm")
        .unwrap()
        .builtins(Builtins::new().llm(scripted_reasoner([SimTurn::text("nested answer")])))
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/bin/llm",
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
    inbox.send(Message::user("delegate once")).unwrap();
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
        .builtins(Builtins::new().spawn(scripted_reasoner([SimTurn::text("child answer")])))
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/bin/spawn",
                    "input": {"id": "svit://local/native-child", "task": "analyze this"}
                }),
            ),
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/bin/spawn",
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
    inbox.send(Message::user("create a child")).unwrap();
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
    let messages = child.read("/thread/messages").unwrap().unwrap().to_json();
    assert!(messages.to_string().contains("analyze this"));
    assert!(messages.to_string().contains("child answer"));
    let duplicate_rejected = parent
        .messages()
        .unwrap()
        .iter()
        .filter(|message| message.role == MessageRole::ToolResult)
        .any(|message| message_text(message).contains("child address already exists"));
    assert!(duplicate_rejected);
}

#[tokio::test]
async fn builtin_data_tools_reject_unbounded_or_oversized_work() {
    // THREAT[TM-DOS-008]: Native tools reject unbounded jq constructs and
    // oversized search expressions before evaluation.
    let mut svit = Svit::builder("svit://local/native-limits")
        .unwrap()
        .memory("text", value!("bounded"))
        .builtins(Builtins::new())
        .reasoner(scripted_reasoner([
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/bin/jq",
                    "input": {"filter": "def f: f; f", "input": null}
                }),
            ),
            SimTurn::tool_call(
                "exec",
                json!({
                    "path": "/bin/search",
                    "input": {
                        "path": "/memory/text",
                        "pattern": "x".repeat(5 * 1024)
                    }
                }),
            ),
            SimTurn::text("limit observed"),
        ]))
        .build()
        .await
        .unwrap();

    let inbox = svit.inbox();
    svit.start().unwrap();
    inbox.send(Message::user("loop forever")).unwrap();
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
