use agentyk::{ContentPart, Message, ModelSpec, Role, SimDriver, SimTurn};
use serde_json::json;
use svit::{Limits, Process, Script, Svit, value};

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
    let prompt = "Inspect your projected runtime state.";
    let mut svit = Svit::builder("svit://local/agent-projection")
        .unwrap()
        .system_prompt(prompt)
        .model(ModelSpec::llmsim())
        .driver(SimDriver::new([
            SimTurn::tool_call("read", json!({"path": "/agent/system_prompt"})),
            SimTurn::tool_call("read", json!({"path": "/agent/messages"})),
            SimTurn::tool_call("read", json!({"path": "/agent/events"})),
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
    assert_eq!(tool_results.len(), 3);
    assert!(tool_results[0].contains(prompt));
    assert!(tool_results[1].contains("inspect projection"));
    assert!(tool_results[2].contains("session_id"));
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
            SimTurn::tool_call("exec", json!({"script": "denied", "input": null})),
            SimTurn::text("denied as expected"),
        ]))
        .allow_scripts(["allowed"])
        .build()
        .await
        .unwrap();

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
