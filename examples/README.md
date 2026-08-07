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

`support-agent/` runs `gpt-5.6-terra` through Agentyk. The support model sees an
attenuated Svit tool surface:

- `discover`: list children under any Svit process path;
- `read`: read a value by absolute process path;
- `exec`: transactionally execute `search_support_docs` or
  `commit_support_result`.

`search_support_docs` and `commit_support_result` are Svit scripts, not agent
tools. The host places the question and request ID in process memory. Search
records its source IDs and ticket policy there. Commit derives those fields
from committed state, writes the result exactly once, and atomically appends an
authorized ticket intent. Generic writes, removes, and other scripts are not
available to the model.

The process exposes these values under `/memory/docs` and `/memory/request`,
the scripts under `/lib`, and the queued ticket under `/system/outbox`.
`/system/identity`, `/system/api`, `/system/limits`, `/system/lineage`, and
`/system/runtime` are discoverable runtime metadata. Reserved `/tasks`,
`/inbox`, `/children`, and `/mounts` nodes remain empty in this example.

Run the live path with `OPENAI_API_KEY` injected by Doppler:

```console
doppler run --project PROJECT --config CONFIG -- \
  cargo run -p svit-support-agent
```

The demo does not contact Jira, Linear, or another production system. It only
shows the committed ticket intent in the Svit outbox. The displayed answer is
loaded from the validated committed result; independent final text from the
model is ignored.
