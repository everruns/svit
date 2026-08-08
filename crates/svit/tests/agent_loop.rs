use std::collections::BTreeMap;

use agentyk::{ContentPart, Message, ModelSpec, Role, SimDriver, SimTurn};
use async_trait::async_trait;
use serde_json::json;
use svit::{
    Executables, HttpAllowlist, HttpRequest, HttpResponse, HttpTransport, HttpTransportError,
    Limits, Process, Script, Svit, value,
};

struct FixtureHttp;

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
async fn svit_builder_combines_process_definition_and_blocking_inbox_loop() {
    let mut svit = Svit::builder("svit://local/runnable")
        .unwrap()
        .memory("status", value!("idle"))
        .system_prompt("Handle inbox messages inside your Svit process.")
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
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
        .send(Message::user_multimodal(vec![ContentPart::text(
            "Do the configured work.",
        )]))
        .unwrap();
    svit.block().await.unwrap();
    let result = outbox.recv().await.unwrap();

    assert_eq!(result.role, Role::Assistant);
    assert_eq!(result.content, vec![ContentPart::text("work complete")]);
    assert_eq!(result.text(), "work complete");
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
        Err(svit::AgentError::InboxClosed)
    ));
}

#[tokio::test]
async fn started_svit_processes_inbox_messages_in_commit_order() {
    let mut svit = Svit::builder("svit://local/ordered-inbox")
        .unwrap()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
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
    assert_eq!(outbox.recv().await.unwrap().text(), "first answer");

    // The process remains live after a completed turn. A message committed
    // while it is waiting becomes the next turn without restarting the loop.
    inbox.send(Message::user("second question")).unwrap();
    assert_eq!(outbox.recv().await.unwrap().text(), "second answer");

    svit.block().await.unwrap();
    assert_eq!(svit.messages().unwrap().len(), 4);
    assert_eq!(svit.read("/inbox").unwrap(), Some(value!([])));
}

#[tokio::test]
async fn agent_runtime_projects_prompt_messages_and_events() {
    // THREAT[TM-CAP-005]: The executable catalog comes from the exact host
    // runtime attached to this process and remains read-only to guest code.
    let prompt = "Inspect your projected runtime state.";
    let mut svit = Svit::builder("svit://local/agent-projection")
        .unwrap()
        .system_prompt(prompt)
        .executables(Executables::new())
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("read", json!({"path": "/agent/system_prompt"})),
            SimTurn::tool_call("read", json!({"path": "/agent/messages"})),
            SimTurn::tool_call("read", json!({"path": "/agent/events"})),
            SimTurn::tool_call("discover", json!({"path": "/bin"})),
            SimTurn::tool_call("read", json!({"path": "/bin/search"})),
            SimTurn::text("projection inspected"),
        ]))
        .build()
        .await
        .unwrap();

    assert_eq!(
        svit.read("/agent/system_prompt").unwrap(),
        Some(value!(prompt))
    );
    assert_eq!(svit.read("/agent/messages").unwrap(), Some(value!([])));
    assert_eq!(svit.read("/agent/events").unwrap(), Some(value!([])));
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

    let projected_messages = svit.read("/agent/messages").unwrap().unwrap().to_json();
    let session_messages = serde_json::to_value(svit.messages().unwrap()).unwrap();
    assert_eq!(projected_messages, session_messages);
    assert!(!svit.discover("/agent/events").unwrap().is_empty());
    let tool_results = svit
        .messages()
        .unwrap()
        .iter()
        .filter(|message| message.role == Role::Tool)
        .map(Message::text)
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 5);
    assert!(tool_results[0].contains(prompt));
    assert!(tool_results[1].contains("inspect projection"));
    assert!(tool_results[2].contains("session_id"));
    assert!(tool_results[3].contains("search"));
    assert!(tool_results[4].contains("input_schema"));
}

#[tokio::test]
async fn resumed_agent_refreshes_tool_discovery_to_current_host_grants() {
    // THREAT[TM-CAP-005]: Snapshot metadata cannot preserve an absent host
    // grant when a restored process is attached to a new agent runtime.
    let source = Svit::builder("svit://local/tool-projection-source")
        .unwrap()
        .executables(Executables::new().llm(
            ModelSpec::llmsim(),
            SimDriver::new([SimTurn::text("nested")]),
        ))
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([]))
        .build()
        .await
        .unwrap();
    assert!(source.discover("/bin").unwrap().contains(&"llm".into()));
    let snapshot = source.snapshot().unwrap();

    let restored = Process::restore(&snapshot).unwrap();
    let resumed = Svit::resume(restored)
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([]))
        .build()
        .await
        .unwrap();

    let tools = resumed.discover("/bin").unwrap();
    assert!(!tools.contains(&"llm".into()));
    assert!(!tools.contains(&"search".into()));
    assert!(tools.is_empty());
}

#[tokio::test]
async fn agent_resume_rejects_a_divergent_message_projection() {
    // THREAT[TM-AUD-001]: Events are canonical; a forged derived history must
    // not become the conversation seen by a resumed agent.
    let mut svit = Svit::builder("svit://local/projection-source")
        .unwrap()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("recorded answer")]))
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
    let mut state = process.read("/agent").unwrap().unwrap().to_json();
    state["messages"] = json!([]);
    process
        .replace_agent_state(svit::Value::from_json(state).unwrap())
        .unwrap();

    let result = Svit::resume(process)
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([]))
        .build()
        .await;
    let error = match result {
        Ok(_) => panic!("divergent projection resumed"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("messages do not match the event projection")
    );
}

#[tokio::test]
async fn svit_agent_resumes_its_thread_from_the_process_snapshot() {
    let prompt = "Keep support answers concise.";
    let mut agent = Svit::builder("svit://local/durable-agent")
        .unwrap()
        .system_prompt(prompt)
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("first answer")]))
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
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("second answer")]))
        .build()
        .await
        .unwrap();

    assert_eq!(resumed.messages().unwrap().len(), 2);
    assert_eq!(
        resumed.read("/agent/system_prompt").unwrap(),
        Some(value!(prompt))
    );
    let inbox = resumed.inbox();
    resumed.start().unwrap();
    inbox.send(Message::user("second question")).unwrap();
    resumed.block().await.unwrap();
    assert_eq!(resumed.messages().unwrap().len(), 4);
    assert_eq!(resumed.messages().unwrap()[0].text(), "first question");
    assert_eq!(resumed.messages().unwrap()[1].text(), "first answer");
}

#[tokio::test]
async fn subagent_owns_an_isolated_forked_process() {
    // THREAT[TM-FORK-001]: A subagent inherits committed thread state but
    // appends future events only to its own process.
    let mut parent = Svit::builder("svit://local/parent-agent")
        .unwrap()
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("parent answer")]))
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
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("child answer")]))
        .build()
        .await
        .unwrap();
    let inbox = child.inbox();
    child.start().unwrap();
    inbox.send(Message::user("child question")).unwrap();
    child.block().await.unwrap();

    assert_eq!(parent.root_hash().unwrap(), parent_hash);
    assert_eq!(parent.messages().unwrap().len(), 2);
    assert_eq!(child.messages().unwrap().len(), 4);
}

#[tokio::test]
async fn process_owned_agent_enforces_its_script_allowlist() {
    // THREAT[TM-CAP-002]: Svit assembles the Agentyk tools and checks script
    // authority before executing model-supplied arguments.
    let mut agent = Svit::builder("svit://local/restricted-agent")
        .unwrap()
        .memory("changed", value!(false))
        .library(
            "denied",
            Script::new(r#"(define (main input) (do (write "/memory/changed" true) input))"#),
        )
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
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

    let process = agent.process();
    assert_eq!(
        process.lock().unwrap().read("/memory/changed").unwrap(),
        Some(&value!(false))
    );
}

#[tokio::test]
async fn agent_event_growth_fails_closed_at_the_process_limit() {
    // THREAT[TM-DOS-003]: Model output enters the durable event stream only
    // through bounded Svit value validation.
    let limits = Limits {
        max_text_bytes: 4 * 1024,
        ..Limits::default()
    };
    let mut agent = Svit::builder("svit://local/bounded-agent")
        .unwrap()
        .limits(limits)
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([SimTurn::text("x".repeat(8 * 1024))]))
        .build()
        .await
        .unwrap();

    let inbox = agent.inbox();
    agent.start().unwrap();
    inbox
        .send(Message::user("produce too much output"))
        .unwrap();
    let error = agent.block().await.unwrap_err();

    assert!(error.to_string().contains("maximum text bytes exceeded"));
    let process = agent.process();
    let process = process.lock().unwrap();
    let committed = process.read("/agent").unwrap().unwrap().to_json();
    assert!(!committed.to_string().contains(&"x".repeat(8 * 1024)));
    assert!(!process.discover("/inbox").unwrap().is_empty());
}

#[tokio::test]
async fn native_search_and_jq_process_structured_data() {
    let records = json!({
        "items": [
            {"name": "alpha", "active": false},
            {"name": "beta", "active": true}
        ]
    });
    let mut svit = Svit::builder("svit://local/native-data-tools")
        .unwrap()
        .memory("records", value!({"items": records["items"].clone()}))
        .executables(Executables::new())
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
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
        .filter(|message| message.role == Role::Tool)
        .map(|message| message.text())
        .collect::<Vec<_>>();
    assert!(tool_results[0].contains("/memory/records/items/1/name"));
    assert!(tool_results[1].contains("\"beta\""));
}

#[tokio::test]
async fn native_http_is_denied_without_an_explicit_url_grant() {
    // THREAT[TM-CAP-004]: Merely supplying a URL does not grant HTTP authority.
    let mut svit = Svit::builder("svit://local/native-network-denied")
        .unwrap()
        .executables(Executables::new().http(HttpAllowlist::new(), FixtureHttp))
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
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

    let tool_result = svit
        .messages()
        .unwrap()
        .iter()
        .find(|message| message.role == Role::Tool)
        .unwrap()
        .text();
    assert!(tool_result.contains("HTTP URL is not allowed"));
}

#[tokio::test]
async fn native_http_uses_the_host_allowlist_and_transport() {
    // THREAT[TM-CAP-004]: An allowed call passes both the host-selected URL
    // policy and host-owned transport before its response reaches the model.
    let tools = Executables::new().http(
        HttpAllowlist::new().allow("https://8.8.8.8/data"),
        FixtureHttp,
    );
    let mut svit = Svit::builder("svit://local/native-network-allowed")
        .unwrap()
        .executables(tools)
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
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

    let tool_result = svit
        .messages()
        .unwrap()
        .iter()
        .find(|message| message.role == Role::Tool)
        .unwrap()
        .text();
    assert!(tool_result.contains("fixture"), "{tool_result}");
}

#[tokio::test]
async fn native_llm_uses_only_the_host_selected_nested_model() {
    // THREAT[TM-EFF-005]: Nested model execution requires a host-selected
    // driver and remains outside the process transaction.
    let nested = SimDriver::new([SimTurn::text("nested answer")]);
    let mut svit = Svit::builder("svit://local/native-llm")
        .unwrap()
        .executables(Executables::new().llm(ModelSpec::llmsim(), nested))
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
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

    let tool_result = svit
        .messages()
        .unwrap()
        .iter()
        .find(|message| message.role == Role::Tool)
        .unwrap()
        .text();
    assert!(tool_result.contains("nested answer"));
}

#[tokio::test]
async fn native_spawn_runs_and_retains_an_isolated_child_svit() {
    // THREAT[TM-FORK-002]: spawn forks committed state, runs the child with
    // separately supplied model authority, and retains an isolated snapshot.
    let child_id = svit::ProcessId::new("svit://local/native-child").unwrap();
    let child_driver = SimDriver::new([SimTurn::text("child answer")]);
    let mut parent = Svit::builder("svit://local/native-parent")
        .unwrap()
        .memory("owner", value!("parent"))
        .executables(Executables::new().spawn(ModelSpec::llmsim(), child_driver))
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
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
        Some(&value!("svit://local/native-parent"))
    );
    let messages = child.read("/agent/messages").unwrap().unwrap().to_json();
    assert!(messages.to_string().contains("analyze this"));
    assert!(messages.to_string().contains("child answer"));
    let duplicate_rejected = parent
        .messages()
        .unwrap()
        .iter()
        .filter(|message| message.role == Role::Tool)
        .any(|message| message.text().contains("child address already exists"));
    assert!(duplicate_rejected);
}

#[tokio::test]
async fn native_data_tools_reject_unbounded_or_oversized_work() {
    // THREAT[TM-DOS-008]: Native tools reject unbounded jq constructs and
    // oversized search expressions before evaluation.
    let mut svit = Svit::builder("svit://local/native-limits")
        .unwrap()
        .memory("text", value!("bounded"))
        .executables(Executables::new())
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
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
        .filter(|message| message.role == Role::Tool)
        .map(|message| message.text())
        .collect::<Vec<_>>();
    assert!(tool_results[0].contains("unbounded construct"));
    assert!(tool_results[1].contains("pattern limit exceeded"));
}
