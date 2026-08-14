# Svit

[![CI](https://github.com/everruns/svit/actions/workflows/ci.yml/badge.svg)](https://github.com/everruns/svit/actions/workflows/ci.yml)
[![Security policy](https://img.shields.io/badge/security-policy-blue.svg)](SECURITY.md)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**A durable, scriptable runtime.**

Svit gives an agent one structured space in which to remember, act, and evolve.
One `Svit` instance owns a reason/act loop, a durable conversation thread, one
serializable process, a memory tree, named scripts, inbox and outbox
ports, snapshots, and isolated forks. [Everruns](https://github.com/everruns/everruns)
implements the current reason/act loop behind the Svit API.

> [!IMPORTANT]
> Svit is research-stage software. The current implementation is runnable and
> tested, but it is not a production hostile multi-tenant isolation boundary
> and has no stable release yet.

## Why Svit

- **Durable Svit state.** Conversation, `/memory`, scripts, inbox state, and
  runtime metadata commit into one process state root.
- **Transactional actions.** A bounded script activation commits memory,
  scripts, and buffered message intents together, or commits nothing.
- **One inspectable memory tree.** Agents use the same absolute-path interface
  for committed state, named scripts, built-ins, and mounted resources.
- **Portable execution state.** A committed process can be snapshotted,
  restored, persisted, or forked into an independently mutable child.
- **Explicit authority.** Guest scripts receive no ambient filesystem, network,
  environment, process, module, clock, or randomness access. Host capabilities
  are attached explicitly through mounts and built-ins.

## Quick start

Clone the repository and run the Svit reasoning example:

```console
git clone https://github.com/everruns/svit.git
cd svit
cargo run --locked -p svit --example process_reasoning
```

Expected output:

```text
process_reasoning color=blue version=26
```

The example needs no API key or network access. It starts a `Svit`, submits a
durable inbox message, lets the model invoke a process script, receives the
completed message from the outbox, and verifies the committed value under
`/memory`.

If the [`just`](https://github.com/casey/just) task runner is installed, run all
examples with:

```console
just examples
```

## Use Svit from Rust

Svit is currently consumed from Git while its public API is still changing:

```toml
[dependencies]
svit = { git = "https://github.com/everruns/svit", branch = "main" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

The primary API is `Svit`, not the underlying reason/act-loop implementation:

```rust
use svit::{Message, OpenAI, Reasoner, Svit};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut svit = Svit::builder("svit://local/readme/demo")?
        .instructions("Keep durable facts in your Svit memory tree.")
        .reasoner(Reasoner::new(
            "gpt-5.6-terra",
            OpenAI::from_env()?,
        ))
        .build()
        .await?;

    let inbox = svit.inbox();
    let mut outbox = svit.outbox();

    svit.start()?;
    inbox
        .send(Message::user("Remember that the release color is blue."))
        .await?;

    let reply = outbox.recv().await?;
    println!("{}", reply.text().unwrap_or_default());

    drop(inbox);
    svit.block().await?;
    Ok(())
}
```

`Svit::builder` creates the process and reasoning loop as one Svit-owned
instance. `Inbox::send` commits a message before waking the loop. `Outbox`
publishes completed assistant messages, while `Events` publishes committed
process changes and sanitized terminal failures. Hosts inspect state through
owned reads such as `read` and `read_versioned`; they do not retain a mutable
process-tree reference.

See [`process_reasoning.rs`](crates/svit/examples/process_reasoning.rs) for a
complete executable example.

## Runtime model

```text
Host
 ├── Inbox ───────────────┐
 ├── Outbox <─────────────┤
 └── Events <─────────────┤
                          v
                       Svit
                    ┌─────┴─────┐
             reason/act loop   Process
                              ├── memory tree
                              ├── script library
                              ├── transaction boundary
                              ├── snapshot / restore
                              └── fork
```

The guest-visible memory tree is the complete namespace under `/`, not only
the `/memory` node:

```text
/
├── thread/      durable prompt, messages, and canonical reasoning events
├── memory/      durable application values
├── lib/         named Svit Lisp scripts
├── bin/         manuals for host-attached built-ins
├── inbox/       durable input queue
├── mounts/      virtual host resources under explicit grants
├── tasks/       reserved in the current slice
├── children/    reserved in the current slice
└── system/      identity, API, limits, lineage, runtime, and outbox metadata
```

Rust callers, the reasoning loop, and Svit Lisp share six path operations:

```text
discover(path)        list immediate children
read(path)            read one value
stat(path)            inspect kind, access, locality, and source facts
write(path, value)    commit a memory, script, or granted mount write
remove(path)          commit a removal
exec(path, input)     run a named script or host-attached built-in
```

Model-facing `exec` resolves `/lib` scripts and installed `/bin` built-ins.
Bare `Process::exec` and nested Svit Lisp execution resolve only `/lib`, because
serializable process state never owns native host authority.

## Persistence and forks

The default `persistence-turso` feature provides a local Turso adapter. A host
creates or resumes a durable process, then gives its `DurableProcessHandle` to
`Svit::persisted`. The creation path is:

```rust
use svit::{OpenAI, Process, Reasoner, Svit, TursoProcessStore};

async fn build_persisted() -> Result<Svit, Box<dyn std::error::Error>> {
    let store = TursoProcessStore::open("svit.db").await?;
    let process = Process::builder("svit://local/persisted/demo")?.build()?;
    let durable = store.create(process).await?;

    let svit = Svit::persisted(durable)?
        .reasoner(Reasoner::new("gpt-5.6-terra", OpenAI::from_env()?))
        .build()
        .await?;
    Ok(svit)
}
```

On restart, use `store.resume("svit://local/persisted/demo")` in place of
`store.create(process)` and build the returned handle through the same
`Svit::persisted` path.

Every mutation belongs to one canonical `ProcessTransaction` stream per
process. Conversation events are values under `/thread/events`, and
`/thread/messages` is their checked projection rather than a second log.
Resume validates transaction hashes, versions, mutations, and resulting root
hashes without rerunning guest code.

`Svit::snapshot` captures process state and the durable conversation together.
`Svit::fork_process` creates an isolated child process with inherited committed
state; future parent and child mutations do not cross process boundaries.

| Feature | Purpose |
| --- | --- |
| `persistence-turso` | Local process-transaction store, snapshots, resume, forks, queries, and history cuts |
| `turso-mount` | Host-side materialization of a bounded Turso query as a mount |

Use `--no-default-features` for the adapter-neutral runtime and persistence
contracts.

## Lampa process console

Lampa is the interactive reference host for one persisted Svit instance:

```console
OPENAI_API_KEY=... cargo run --locked -p lampa
```

![Lampa terminal process viewer](docs/lampa.gif)

Lampa shows the conversation, complete memory tree, and selected value. It
mounts the current directory read-only as `/mounts/cwd`, renders messages and
text previews as Markdown, and exposes the standard `/bin` built-ins.

Each instance has its own database. Reuse an instance ID to resume its committed
conversation and memory:

```console
OPENAI_API_KEY=... cargo run --locked -p lampa -- --instance research-one
```

Use `--mount name=path` or `--mount-rw name=path` to attach folders. Set
`LAMPA_DATA_DIR` to choose the storage root; otherwise Lampa uses the native
application-data directory.

## The lower-level `Process` API

`Process` is the serializable state machine owned by `Svit`. Use it directly
when you need bounded script activations without a model loop:

```rust
use svit::{Process, Value, svit_script, value};

fn main() -> svit::Result<()> {
    let mut process = Process::builder("svit://local/readme/counter")?
        .memory("count", value!(0))
        .library("increment", svit_script! {
            (define (main input)
              (let ((next (+ (read "/memory/count")
                             (value-get input "/by"))))
                (do
                  (write "/memory/count" next)
                  next)))
        })
        .build()?;

    let activation = process.exec("/lib/increment", value!({"by": 3}))?;
    assert_eq!(activation.output, value!(3));
    assert_eq!(process.read("/memory/count")?, Some(Value::Integer(3)));
    Ok(())
}
```

A fresh restricted Lisp VM runs each activation against a transactional working
copy. Syntax, runtime, conversion, validation, or resource-limit failure leaves
the committed process unchanged. `svit_script!` compile-checks embedded scripts;
`svit_script_test!` executes them in a fresh real process for state assertions.

Svit Lisp is a versioned restricted Ketos surface, not unrestricted Ketos,
Scheme, or Common Lisp. Persistent values are null, booleans, signed integers,
finite floats, text, arrays, and text-keyed maps. See the
[runtime contract](knowledge/runtimes/lisp-runtime.md) and
[examples](examples/README.md).

## Built-ins and mounts

`Builtins::new()` installs bounded local `search` and `jq` implementations.
Hosts may explicitly add HTTP, model, child-process, or custom built-ins. The
`/bin` entries are manuals for the attached implementations, not serialized
authority. Restoring a process does not recreate a missing host grant.

`Builtins::standard()` additionally selects `http`, `llm`, and `spawn`. It is a
research-host preset and explicitly grants unrestricted HTTP destinations;
hosts that need destination policy should apply `with_http_allowlist` or
install a narrower transport.

Mounts project host-selected resources into `/mounts` without copying them into
the committed root. A committed descriptor records identity, locality, and
granted access; the host-owned provider resolves nodes lazily. Providers are
never serialized and must be reattached after restore.

## Current scope

Implemented now:

- process-owned Everruns reasoning with durable inbox, thread, and outbox;
- transactional memory and named Svit Lisp scripts;
- bounded values, execution, diagnostics, snapshots, restore, and forks;
- local Turso persistence with one canonical process-transaction stream;
- virtual folder and materialized-query mounts;
- explicit built-ins for local data work and host-granted external effects;
- VAST control semantics with process-version preconditions and conflicts;
- runnable acceptance examples and adversarial invariant tests.

Not implemented:

- message delivery, scheduling, timers, retries, or global routing;
- authenticated process identity, authorization, or secrets;
- distributed ownership, migration, durable control receipts, or exactly-once
  external effects;
- production Wasm/OS isolation or formal hostile-tenant isolation evidence.

The public [vision](docs/vision.md) describes the broader research direction;
it is not a promise that those capabilities already exist.

## Security

Svit validates persistent values and snapshots, applies configured resource
limits, starts a fresh restricted interpreter for every activation, and rolls
back failed activations. Those controls do not prove safe hostile
multi-tenancy: the interpreter is native code, wall-time checks are not
instruction-level fuel, and estimated memory is not an allocator cap.
Run hostile workloads behind a Wasm or OS boundary with outer CPU, memory, and
time limits.

Do not report vulnerabilities in public issues. Follow
[`SECURITY.md`](SECURITY.md) for private reporting and the current support
policy.

## Documentation

| Resource | Purpose |
| --- | --- |
| [Vision](docs/vision.md) | Product model and research direction |
| [Examples](examples/README.md) | End-to-end scenarios |
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
changing runtime behavior. Contributions should keep changes small, update the
relevant knowledge, and add executable evidence for behavioral and security
claims.

## License

Svit is available under the [MIT License](LICENSE). See [`NOTICE`](NOTICE) for
third-party attribution.
