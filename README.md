# Svit

Svit is a research-stage agent process runtime. One Svit agent owns a process,
a durable conversation thread, named restricted-Lisp scripts, bounded
transactional execution, snapshots, and forks—without giving guest code an
operating system. Agentyk implements the current reason/act loop inside the
Svit agent API.

The current implementation is a deliberately small vertical slice. It is
runnable and tested, but it is not yet a production multi-tenant platform or a
formally verified security boundary.

## Quick start

Run the durable counter example:

```console
$ cargo run -p svit --example durable_counter
durable_counter count=9 version=4
```

The same lifecycle through the Rust API:

```rust
use svit::{Process, Script, Value, value};

fn main() -> svit::Result<()> {
    let mut process = Process::builder("svit://local/demo/counter")?
        .memory("count", value!(0))
        .library("counter", Script::new(r#"
            (define (main input)
              (let ((count (+ (read "/memory/count") (value-get input "/by"))))
                (do
                  (write "/memory/count" count)
                  (value-map "count" count))))
        "#))
        .build()?;

    let activation = process.exec("counter", value!({"by": 3}))?;
    assert_eq!(activation.output, value!({"count": 3}));
    assert_eq!(
        process.read("/memory/count")?,
        Some(&Value::Integer(3)),
    );

    let snapshot = process.snapshot()?;
    let restored = Process::restore(&snapshot)?;
    let child = restored.fork("svit://local/demo/child")?;
    assert_eq!(child.read("/memory/count")?, Some(&Value::Integer(3)));
    Ok(())
}
```

Run one standalone script through the CLI:

```console
cargo run -p svit-cli -- exec path/to/script.svit-script '{"input": "value"}'
```

The CLI creates a fresh process, installs the script as `main`, executes one
activation, and prints committed memory, output, and version as JSON. It is a
smoke-test tool, not a persistent process supervisor.

## Generic process contract

Rust callers, agent integrations, and Svit Lisp use the same five operations:

```text
discover(path)        -> immediate child names
read(path)            -> value at an absolute process path
write(path, value)    -> transactional memory or library update
remove(path)          -> transactional memory or library removal
exec(script, input)   -> transactional named-script execution
```

Builders, snapshots, restore, and fork are process lifecycle APIs, not a second
agent-operation vocabulary.

## Process-owned agent loop

`svit::Svit` owns both the Agentyk loop and its process. The configured system
prompt, event-derived message history, and canonical Agentyk events are
committed under the host-managed, guest-readable `/agent` node. Restoring a
process resumes that conversation, and forking it creates an isolated child
agent process with inherited history:

```rust,no_run
use agentyk::{ModelSpec, OpenAiDriver};
use svit::{ContentPart, Message, Svit};

# async fn example(api_key: String) -> svit::SvitResult<()> {
let mut svit = Svit::builder("svit://local/demo/agent")?
    .system_prompt("Work entirely through your Svit process.")
    .model(ModelSpec::openai("gpt-5.6-terra").api_key(api_key))
    .driver(OpenAiDriver::new())
    .build()
    .await?;

let inbox = svit.inbox();
let mut outbox = svit.outbox();

svit.start()?;
inbox.send(Message::user_multimodal(vec![ContentPart::text(
    "Remember that the release color is blue.",
)]))?;
let answer = outbox.recv().await.expect("completed agent turn");
assert!(!answer.text().is_empty());

drop(inbox);
svit.block().await?;
let snapshot = svit.snapshot()?;
# let _ = snapshot;
# Ok(())
# }
```

`start` launches the independently running local loop. `Inbox::send` commits an
Agentyk `Message`, including its ordered `ContentPart` values, before waking
the loop. The live outbox emits the durable assistant `Message` for each
completed turn. `block` seals the inbox, drains committed messages, and joins
the loop. A subagent is another `Svit` built around a fork returned by
`svit.fork_process(child_address)`.

Run the live process-owned support-agent scenario with `OPENAI_API_KEY` set:

```console
just support-agent-v2
```

## What works now

- One discoverable process namespace containing host-managed `/agent`, mutable `/memory`, named
  `/lib` scripts, read-only folder and Turso query snapshots under `/mounts`,
  reserved `/tasks` and `/children`, a host-managed durable `/inbox`, plus read-only `/system`
  metadata and the outbox.
- Named script installation, inspection, replacement, and removal through the
  same generic path operations used for memory.
- Transactional activations: memory, staged scripts, and message intents all
  commit, or none do.
- Structured `log-info!` records and deterministic buffered `send!` intents.
- Versioned JSON snapshots with validation and a SHA-256 root integrity hash.
- Forks that copy committed memory and scripts into an independently mutable
  child without copying the parent's outbox. Agent-process forks also inherit
  committed conversation history and isolate future turns.
- A process-owned `svit::Svit` loop implemented by Agentyk, with bounded
  durable events carried by snapshots, restores, and forks.
- Configurable execution deadline, nested-exec depth, VM stack, namespace,
  syntax, integer, estimated guest-memory, value, text, script, log, message,
  and staged-script limits.
- Typed host activation hooks that may rewrite, deny, or observe activations.
- Reflection over committed memory and the named script library.
- Discoverable system metadata for the logical address, runtime, API, limits,
  lineage, and the explicitly empty capability set. The address is marked
  unauthenticated and grants no authority.
- Svit Control Protocol 1 with Versioned Atomic State Transitions (VAST)
  semantics: mandatory process-version preconditions, atomic next-version
  commits, conflict responses, and bounded idempotency receipts.
- Additive-field compatibility and exact wire-shape tests, with generated schema
  and remote version negotiation required before protocol stabilization.

See [Controlling a Svit process](docs/control-protocol.md) for VAST semantics,
the wire protocol, and its exact transaction boundary.

## Svit Lisp 2

A named script defines `main(input)`. The initial guest surface is:

```text
input
(value-get value path)
(value-map key value ...)
(value-array value ...)
(value-null? value)
(read path)
(write path value)
(remove path)
(discover path)
(exec script input)
(log-info! message fields?)
(send! process-address body)
```

Svit Lisp is a versioned restricted Ketos surface, not full Scheme, Common
Lisp, or unrestricted Ketos. Guest code has core arithmetic, comparisons,
lexical functions and immutable list/string operations. Null I/O and a module
loader that rejects every module leave it without filesystem, network,
environment, host process, wall clock, or ambient randomness access.

Persistent values are null, booleans, signed integers, finite floats, text,
arrays, and text-keyed maps. Maps and arrays enter Lisp as immutable typed
values. Ratios, oversized integers, paths, bytes, functions, lambdas, quotes,
unrecognized foreign values, NaN, and infinity cannot cross a commit boundary.

All process paths are absolute. `write` and `remove` accept `/memory` paths and
individual `/lib/<name>` entries. A library write supplies a map containing
`source` and optional `documentation`. `/system` and reserved nodes are
read-only. Nested `exec` shares the outer activation transaction, deadline,
and bounded execution depth.

## Executable examples

Every Rust example contains assertions and deterministic output:

```console
cargo run -p svit --example durable_counter
cargo run -p svit --example self_authoring_library
cargo run -p svit --example atomic_outbox
cargo run -p svit --example fork_research
cargo run -p svit --example sandbox_limits
cargo run -p svit --example multi_client_control
cargo run -p svit --example mounted_resources
cargo run -p svit --example process_owned_agent
```

They cover persistence and restore, functional self-reflection, rollback of a
state-plus-message transaction, isolated forks, denied ambient APIs, a bounded
infinite loop, two clients resolving an optimistic concurrency conflict, and
bounded snapshots of a real folder and a Turso query. The process-owned agent
example resumes one Agentyk-backed thread and continues it in an isolated
subagent process. See
[examples/README.md](examples/README.md), or run all of them with `just examples`.

## Current security status

The runtime starts a fresh restricted Ketos VM for every activation, installs
null I/O and rejects all modules. Ketos wall time, stacks, namespace, syntax,
integer size, and estimated memory are limited; persistent values and snapshots
are bounded and validated; failures leave committed state unchanged;
guest-visible interpreter diagnostics are capped.

Those controls do not establish a formal hostile-tenant isolation proof. The
interpreter is native code in the host process, the deadline is not
deterministic instruction fuel, estimated memory is not an allocator byte cap,
and a SHA-256 snapshot hash provides integrity rather than provenance or
authorization. Deploy untrusted tenants behind a Wasm or OS process boundary
and enforce outer CPU, memory, and time limits until stronger evidence exists.

See the canonical [threat model](knowledge/security/threat-model.md),
[security-testing requirements](knowledge/security/security-testing.md), and
[limitations](knowledge/operations/limitations.md). Report vulnerabilities as
described in [SECURITY.md](SECURITY.md).

## Not implemented

Svit does not yet provide scheduling, timers, background activations, message
delivery, retries, global routing, authenticated identity, authorization,
live or writable external capabilities, filesystem or network read-through
projections, secrets, durable process storage, distributed migration, exactly-once effects, snapshot
signatures, or formal verification. Process addresses are validated logical
identifiers; they are not authenticated principals.

The control protocol currently has an in-memory reference adapter, not a
network listener, authenticated transport, durable receipt store, or
distributed ownership lease. Its atomic commit covers one process root and
outbox, not an external system or another process.

The broader direction is described in the public [Svit vision](docs/vision.md).
Research goals are not current API promises.

## Development

The repository uses a pinned Rust toolchain and `just` as its command index:

```console
just build
just test
just examples
just check
just pre-pr
```

Durable engineering decisions live in the OKF v0.2 `knowledge/` bundle. Read
[AGENTS.md](AGENTS.md) and [CONTRIBUTING.md](CONTRIBUTING.md) before changing
runtime behavior.

## License

MIT. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
