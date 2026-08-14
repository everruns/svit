# Built-in examples

The Rust examples exercise the main Svit and Process APIs. Run them from the
repository root:

```console
cargo run -p svit --example durable_counter
cargo run -p svit --example self_authoring_library
cargo run -p svit --example atomic_outbox
cargo run -p svit --example fork_research
cargo run -p svit --example sandbox_limits
cargo run -p svit --example multi_client_control
cargo run -p svit --example mounted_resources
cargo run -p svit --example process_reasoning
cargo run -p svit --example builtins
```

`multi_client_control` demonstrates two clients using optimistic version
preconditions: one commits, a stale request conflicts without mutation, and a
retry against the observed version commits exactly once.

`mounted_resources` mounts a real UTF-8 folder, the result of a host-chosen
Turso query, and a writable scratch folder. Scripts read and stat them through
the ordinary `/mounts` hierarchy, one node at a time; the snapshot holds mount
descriptors rather than mount data. A granted write reaches the real folder at
the activation's commit point, while the read-only mount refuses the same
write. No host path, database connection, or SQL authority enters the
activation.

`process_reasoning` runs the simulated Everruns driver through `svit::Svit`.
It obtains inbox and outbox handles, starts the loop, submits one durable
Everruns `Message` with content parts, receives the assistant message while the
loop is live, and then blocks until the committed queue drains.

`builtins` runs the default `search` and `jq` implementations and a custom
`BuiltinExtension`. The extension reads committed process state through
`BuiltinContext` without receiving process mutation authority.

## Process-configured support workflow

`support-agent-process/` constructs memory, mounts, and scripts through the
lower-level `Process` builder, then runs that process through `Svit::resume`
with `gpt-5.6-terra`. The model sees an attenuated Svit capability surface:

- `discover`: list children under any Svit process path;
- `read`: read a value by absolute process path;
- `exec`: transactionally execute `search_support_docs` or
  `commit_support_result`.

`search_support_docs` and `commit_support_result` are Svit scripts, not agent
tools. The host places the question and request ID in process memory. Search
reads documents from a real folder snapshot and account context from a Turso
query snapshot, then records source IDs and ticket policy in process memory.
Commit derives those fields from committed state, writes the result exactly
once, and atomically appends an authorized ticket intent. Generic writes,
removes, and other scripts are not available to the model.

The process exposes mounted data under `/mounts/support_docs` and
`/mounts/account_context`, request and committed result state under
`/memory/request`, scripts under `/lib`, and the queued ticket under
`/system/outbox`.
`/system/identity`, `/system/api`, `/system/limits`, `/system/lineage`, and
`/system/runtime` are discoverable runtime metadata. Reserved `/tasks` and
`/children` remain empty.

Run the live path with `OPENAI_API_KEY` injected by Doppler:

```console
doppler run --project PROJECT --config CONFIG -- \
  cargo run -p svit-support-agent-process
```

The demo does not contact Jira, Linear, or another production system. It only
shows the committed ticket intent in the Svit outbox. The displayed answer is
loaded from the validated committed result; independent final text from the
model is ignored.

## Direct Svit support workflow

`support-agent-svit/` builds the complete instance directly through
`Svit::builder`. It uses OpenAI `gpt-5.6-terra` and demonstrates:

- one runnable `Svit`, with Everruns behind the Svit API;
- explicit inbox submission, process start, outbox listening, and blocking drain;
- a model-authored support answer committed to process memory before reply.

Run it with `OPENAI_API_KEY` injected by Doppler:

```console
doppler run --project PROJECT --config CONFIG -- just support-agent-svit
```

This credentialed demo is excluded from `just examples`; loop, inbox, snapshot,
and fork behavior remains covered by the Svit test suite.
