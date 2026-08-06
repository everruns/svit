# Executable examples

The Rust examples are deterministic acceptance tests for the initial process
runtime. Run them from the repository root:

```console
cargo run -p svit --example durable_counter
cargo run -p svit --example self_authoring_library
cargo run -p svit --example atomic_outbox
cargo run -p svit --example fork_research
cargo run -p svit --example sandbox_limits
cargo run -p svit --example multi_client_control
```

`multi_client_control` demonstrates two clients using optimistic version
preconditions: one commits, a stale request conflicts without mutation, and a
retry against the observed version commits exactly once.

## Agentyk support-agent demo

`support-agent/` runs `gpt-5.6-terra` through Agentyk. The model only sees four
generic Svit tools:

- `discover`: list children under any Svit process path;
- `exec`: execute a named script transactionally;
- `get`: get a value from the process tree;
- `set`: transactionally set a value below `/memory`.

`search_support_docs` and `commit_support_result` are Svit scripts, not agent
tools. Search reads documents from Svit memory. Commit writes the result to
Svit memory and appends a ticket message to the Svit outbox. There is no turn
counter or custom agent framework.

Run the live path with `OPENAI_API_KEY` injected by Doppler:

```console
doppler run --project PROJECT --config CONFIG -- \
  cargo run -p svit-support-agent
```

The demo does not contact Jira, Linear, or another production system. It only
shows the committed ticket intent in the Svit outbox.
