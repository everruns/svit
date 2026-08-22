# Svit

[![CI](https://github.com/everruns/svit/actions/workflows/ci.yml/badge.svg)](https://github.com/everruns/svit/actions/workflows/ci.yml)
[![Security policy](https://img.shields.io/badge/security-policy-blue.svg)](SECURITY.md)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Memory and behavior, committed together.**

Svit is a research-stage Rust runtime for agents that need durable state and
reusable code. It keeps structured memory, named Svit Lisp scripts, inbox state,
buffered message intents, and runtime metadata in one serializable process.
Every activation runs against a bounded working copy and either commits one
complete next version or commits nothing.

One `Svit` owns one reason/act loop, one durable conversation thread, and one
serializable `Process`. The host chooses a `Reasoner`, mounts, and ports;
[Everruns](https://github.com/everruns/everruns) implements the current loop
behind the Svit API.

> [!IMPORTANT]
> Svit is runnable and tested, but it has no stable release and is not a proven
> hostile multi-tenant isolation boundary. Production use with untrusted code
> still needs an outer Wasm or OS process boundary.

## Why Svit

- **One process space.** [Memory](docs/memory.md), scripts, queues, metadata,
  and mounted resources use one absolute-path interface rather than unrelated
  agent tools.
- **Atomic activations.** Memory, script, and buffered message changes commit
  together. Syntax, runtime, validation, conversion, and limit failures roll
  back the complete activation.
- **Durable reasoning.** With persistence, process transactions and paged
  [events](docs/events.md) survive restarts without materializing the full
  thread in every snapshot.
- **Portable state.** Processes can be snapshotted, restored, inspected, and
  forked into independently mutable children.
- **Explicit authority.** Guest Lisp has no ambient filesystem, network,
  environment, process, module loader, clock, randomness, or native-extension
  access. Hosts attach external authority through typed mounts and
  [ports](docs/ports.md).
- **Observable change.** Commits report the paths they changed, while committed
  nodes and roots have structural content hashes for precise cache validation.

## Quick start

Create a Rust application and add Svit while its public API is still changing:

```console
cargo new svit-quickstart
cd svit-quickstart
```

```toml
[dependencies]
svit = { git = "https://github.com/everruns/svit", branch = "main" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Replace `src/main.rs` with:

```rust
use svit::{Message, OpenAI, Reasoner, Svit, value};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut svit = Svit::builder("svit://local/quickstart")?
        .memory("facts", value!({}))
        .instructions(
            "Write requested durable facts to the exact process path before replying.",
        )
        .reasoner(Reasoner::new("gpt-5.6-terra", OpenAI::from_env()?))
        .build()
        .await?;

    let inbox = svit.inbox();
    let mut outbox = svit.outbox();

    svit.start()?;
    inbox
        .send(Message::user(
            "Write blue to /memory/facts/release_color, then confirm it.",
        ))
        .await?;

    let reply = outbox.recv().await?;
    drop(inbox);
    svit.block().await?;

    println!("{}", reply.text().unwrap_or_default());
    println!("stored={:?}", svit.read("/memory/facts/release_color")?);
    Ok(())
}
```

Run it with an OpenAI API key:

```console
OPENAI_API_KEY=... cargo run
```

`Inbox::send` commits a message before waking the loop. `Outbox` publishes
completed assistant messages. `Events` publishes committed path changes,
canonical conversation events, derived messages, and sanitized terminal
failures. Hosts inspect state through owned reads; they never receive a mutable
reference to the process tree.

For a complete live example, clone this repository and run the process-owned
support agent:

```console
OPENAI_API_KEY=... cargo run --locked -p svit-support-agent-svit
```

## Process model

```text
Host
├── Reasoner
├── Inbox ─────────────┐
├── Outbox <───────────┤
├── Events <───────────┤
├── Ports              │
└── Mount providers    │
                       v
                    Svit
             reason/act loop + thread
                       │
                       v
                    Process
             state + scripts + limits
             transactions + snapshots
```

The memory tree is the complete guest-visible namespace below `/`, not just the
`/memory` node:

```text
/
├── thread/      bounded durable session metadata
├── memory/      durable application values
├── lib/         named Svit Lisp scripts
├── ports/       manuals for host-attached ports
├── inbox/       durable local input queue
├── mounts/      virtual host resources under explicit grants
├── tasks/       reserved in the current slice
├── children/    reserved in the current slice
└── system/      identity, API, limits, lineage, runtime, and outbox metadata
```

Rust callers, model tools, and Svit Lisp use the same path vocabulary:

```text
discover(path)        list immediate children
read(path)            read one value
stat(path)            inspect kind, access, locality, source, and content facts
write(path, value)    commit a memory, script, or granted mount write
remove(path)          commit a removal
exec(path, input)     run a named /lib script
```

The model-facing `exec` tool can also run transient Svit Lisp source. A fresh,
restricted interpreter runs each activation. Successful activations validate
and commit memory, scripts, and buffered message intents once; failed
activations leave the process version and committed root unchanged.

External effects have a narrower guarantee. Port calls happen immediately and
cannot be rolled back. Granted mount writes are delayed until process
validation succeeds, but the external source cannot join the process
transaction.

## Persistence, snapshots, and forks

The default `persistence-turso` feature provides local Turso persistence. One
canonical `ProcessTransaction` stream records process mutations; a separate
paged `EventLog` retains conversation history. Resume verifies transaction
versions, hashes, mutations, and resulting root hashes without rerunning guest
code.

```rust
use svit::{OpenAI, Process, Reasoner, Svit, TursoProcessStore};

async fn build_persisted() -> Result<Svit, Box<dyn std::error::Error>> {
    let store = TursoProcessStore::open("svit.db").await?;
    let process = Process::builder("svit://local/persisted/demo")?.build()?;
    let durable = store.create(process).await?;

    Ok(Svit::persisted(durable)?
        .reasoner(Reasoner::new("gpt-5.6-terra", OpenAI::from_env()?))
        .build()
        .await?)
}
```

Use `store.resume(address)` to reopen a process. Snapshots preserve canonical
process state and thread metadata. Durable forks share an immutable history
prefix at the fork boundary and then commit independently; process-only forks
start a fresh child session.

| Feature | Purpose |
| --- | --- |
| `persistence-turso` | Local transactions, snapshots, resume, forks, queries, and history cuts |
| `turso-mount` | A bounded host-selected Turso query exposed as a virtual mount |

Use `--no-default-features` for the adapter-neutral runtime and persistence
contracts.

## Ports and mounts

Ports are host-owned async capabilities. `Ports::new()` grants none. Add each
port deliberately and attach the resulting registry when building Svit:

```rust
use svit::{
    HttpAllowlist, OpenAI, Ports, Reasoner, ReqwestHttpTransport, Svit,
};

let reasoner = Reasoner::new("gpt-5.6-terra", OpenAI::from_env()?);
let ports = Ports::new()
    .http(
        HttpAllowlist::new().allow("https://api.github.com/"),
        ReqwestHttpTransport::new()?,
    )
    .llm(reasoner.clone())
    .spawn(reasoner.clone());

let svit = Svit::builder("svit://local/explicit-ports")?
    .reasoner(reasoner)
    .ports(ports)
    .build()
    .await?;
```

Here `http` can reach only the allowlisted origin and its path descendants,
`llm` uses the selected reasoner for nested model calls, and `spawn` uses it for
child Svit turns. A research host that intentionally accepts any HTTP(S)
destination must call `http_unrestricted` by name. Omit any registration the
process should not receive. Port descriptors appear under `/ports`, but
descriptors never carry authority and snapshots never serialize port
implementations.

Mounts project host-selected folders or values below `/mounts` without copying
their contents into the committed root. The root stores only a descriptor;
nodes resolve lazily through a host-owned provider. Providers are never
serialized, so a restored process fails closed until the host reattaches them.

## Lampa process console

Lampa is the interactive reference host for one persisted Svit:

```console
OPENAI_API_KEY=... cargo run --locked -p lampa
```

![Lampa terminal process viewer](docs/lampa.gif)

Lampa shows the conversation beside the complete memory tree, mounts the current
directory read-only at `/mounts/cwd`, and persists each instance in its own
database. It explicitly registers unrestricted `http` plus model-backed `llm`
and `spawn` ports as a research host. Reuse an instance name to resume it:

```console
OPENAI_API_KEY=... cargo run --locked -p lampa -- --instance research-one
```

Use `--mount name=path` or `--mount-rw name=path` to attach folders and
`LAMPA_DATA_DIR` to select the storage root.

## Current scope

Implemented now:

- process-owned reasoning with a durable local inbox, thread, and outbox;
- transactional memory and named Svit Lisp scripts;
- bounded values, execution, diagnostics, snapshots, restore, and forks;
- local Turso persistence with validated transaction replay;
- lazy folder, value, and materialized-query mounts;
- explicit host ports and pure local `jq` and `search` functions;
- VAST version preconditions, conflicts, and bounded retry receipts;
- deterministic examples and adversarial invariant tests.

Not implemented:

- remote message delivery, scheduling, timers, retries, or global routing;
- authenticated process identity, authorization, or secrets;
- distributed ownership, migration, or durable control receipts;
- exactly-once external effects;
- production Wasm/OS isolation or formal hostile-tenant isolation evidence.

The public [vision](docs/vision.md) describes the broader research direction,
not functionality already promised by this implementation.

## Security

Svit validates persistent values and snapshots, enforces configured limits,
uses a fresh restricted interpreter for every activation, caps diagnostics, and
rolls back failed activations. These controls are executable invariants, not a
proof of hostile multi-tenancy. Ketos uses wall-clock deadlines and estimated
interpreter memory rather than deterministic fuel and an allocator byte cap.

Do not report vulnerabilities in public issues. Follow
[`SECURITY.md`](SECURITY.md) for private reporting and the current support
policy.

## Documentation

| Resource | Purpose |
| --- | --- |
| [Vision](docs/vision.md) | Product model and research direction |
| [Examples](examples/README.md) | Runnable end-to-end scenarios |
| [Svit Lisp contract](knowledge/runtimes/lisp-runtime.md) | Versioned guest-language surface |
| [Control protocol](docs/control-protocol.md) | VAST semantics and wire contract |
| [Security policy](SECURITY.md) | Security model, limitations, and reporting |
| [Changelog](CHANGELOG.md) | Unreleased and released changes |
| [Svit skill](skills/svit/SKILL.md) | Usage guidance for Svit-aware models |

Internal engineering decisions and executable claims live in the OKF v0.2
[`knowledge/`](knowledge/) bundle.

## Development

```console
just --list
just build
just test
just examples
just check
just pre-pr
```

Read [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`AGENTS.md`](AGENTS.md) before
changing runtime behavior. Behavioral and security claims require executable
evidence and the corresponding knowledge update.

## License

Svit is available under the [MIT License](LICENSE). See [`NOTICE`](NOTICE) for
third-party attribution.
