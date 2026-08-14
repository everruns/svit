# Svit

Svit is a research-stage process runtime. One Svit instance owns a process,
a durable conversation thread, named restricted-Lisp scripts, bounded
transactional execution, snapshots, and forks—without giving guest code an
operating system. Everruns implements the current reason/act loop inside the
Svit API.

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
use svit::{Process, Value, svit_script, value};

fn main() -> svit::Result<()> {
    let mut process = Process::builder("svit://local/demo/counter")?
        .memory("count", value!(0))
        .library("counter", svit_script! {
            (define (main input)
              (let ((count (+ (read "/memory/count") (value-get input "/by"))))
                (do
                  (write "/memory/count" count)
                  (value-map "count" count))))
        })
        .build()?;

    let activation = process.exec("/lib/counter", value!({"by": 3}))?;
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

For scripts stored outside Rust source, `svit_script!` resolves paths from the
consuming package and reports Lisp compiler errors during `cargo check`:

```rust,ignore
let script = svit::svit_script!(file "scripts/counter.svit-script");
```

Inline programs may also use a Rust string literal when the source is generated
or needs escaping. `svit_script_test!` defines a Rust test with a fresh process
and installs the subject at `/lib/subject`. The test body executes real
activations, so it can assert both output and committed state:

```rust
svit::svit_script_test!(identity_script, svit::svit_script! {
    (define (main input) input)
}, |process, script| {
    let activation = process.exec(script, svit::value!({"ok": true}))?;
    assert_eq!(activation.output, svit::value!({"ok": true}));
});
```

Compile-time checking does not run an activation or execute ordinary top-level
forms. The compiler may evaluate Lisp macros and constants, so it runs with
null I/O, no module loader, and the standard Svit limits. Process construction
still enforces configured limits, and activation tests enforce `main(input)`,
runtime behavior, and transaction semantics.

Direct source supports immutable bindings and conditional forms. There are no
mutable `while` or `for` loops; bounded iteration uses tail recursion:

```rust
let script = svit::svit_script! {
    (define (sum-to n acc)
      (if (<= n 0)
        acc
        (sum-to (- n 1) (+ acc n))))

    (define (main input)
      (let ((n (value-get input "/n")))
        (cond
          ((< n 0) (value-map "error" "negative input"))
          (else
            (let ((total (sum-to n 0)))
              (do
                (write "/memory/total" total)
                (value-map "total" total)))))))
};
```

Function parameters and `let` bindings are activation-local. Top-level
definitions are loaded into each fresh activation interpreter and do not carry
state forward. Durable variables belong under `/memory` and change through
`write` or `remove`. `if`, `cond`, `case`, `and`, and `or` provide conditional
control; `do` sequences effects. Every recursive computation remains subject
to the configured execution, call-stack, value-stack, and memory limits.

Persist process transitions as Turso-backed events when resume, queries,
fork references, snapshots, and cuts must survive beyond one `Process` value:

```rust,no_run
use svit::{EventQuery, Process, TursoProcessStore, value};

# async fn example() -> svit::Result<()> {
let store = TursoProcessStore::open("svit.db").await?;
let process = Process::builder("svit://local/demo/persisted")?
    .memory("status", value!("new"))
    .build()?;
let mut durable = store.create(process).await?;
durable.write("/memory/status", value!("ready")).await?;

drop(durable);
let resumed = store.resume("svit://local/demo/persisted").await?;
let events = resumed
    .query(EventQuery::new().path_prefix("/memory"))
    .await?;
assert_eq!(events.len(), 1);
# Ok(())
# }
```

`DurableProcess` publishes its in-memory candidate only after the event, path
projection, and address-head compare-and-swap commit in one Turso transaction.
Resume validates content hashes, the base-bound event chain, typed mutations,
versions, and complete resulting root hashes without rerunning guest code.
`Svit::persisted` attaches a reasoner and host grants to a newly created or
resumed `DurableProcessHandle`; inbox transitions, model-driven process tools,
canonical conversation events, derived messages, and built-in catalog refresh
then use that same durable owner.

Persistence is adapter-neutral at the public boundary. `ProcessStore` creates
and resumes a `DurableProcessHandle`; `PersistedEventRecord` and
`PersistenceSnapshotRecord` expose query and snapshot metadata without Turso
types. `DefaultProcessStore` aliases `TursoProcessStore` when the default
`persistence-turso` feature is enabled.

The default features are:

| Feature | Purpose |
| --- | --- |
| `persistence-turso` | Local Turso event-store implementation and its concrete event, snapshot, and process-handle types |
| `turso-mount` | Host-side materialization of a bounded Turso query as a mount |

Build with `--no-default-features` to compile the process runtime and
persistence traits without Turso. Either Turso feature can be enabled
independently.

For an interactive process, start Lampa with an OpenAI API key:

```console
OPENAI_API_KEY=... cargo run -p lampa
```

Lampa creates or resumes `svit://local/lampa/default` in its own database below
the platform-native user data directory:
`~/Library/Application Support/lampa/instances/default/svit.db` on macOS,
`$XDG_DATA_HOME/lampa/instances/default/svit.db` on Linux, or
`%APPDATA%\lampa\instances\default\svit.db` on Windows. Select another instance
with `--instance <instance-id>` (or `LAMPA_INSTANCE_ID`):

```console
OPENAI_API_KEY=... cargo run -p lampa -- --instance research-one
```

That resumes `svit://local/lampa/research-one` from
`instances/research-one/svit.db`. Instance IDs start with a lowercase letter or
digit and contain at most 64 lowercase letters, digits, `-`, or `_`. Set
`LAMPA_DATA_DIR=/path/to/lampa` to replace the data-directory root while
preserving the per-instance layout. Restarting with the same instance ID and
data directory resumes its committed memory, conversation, and pending inbox.

Import one instance from the former shared database explicitly:

```console
OPENAI_API_KEY=... cargo run -p lampa -- --instance research-one \
  --import-legacy "$HOME/Library/Application Support/lampa/lampa.db"
```

Import is accepted only before the target instance database exists. It
preserves the current process state, version, and root hash, then starts a new
retained-history tail; subsequent starts omit `--import-legacy`.

![Lampa terminal process viewer](docs/lampa.gif)

Three persistent panels show the inbox/outbox conversation and runtime events,
the complete committed process memory tree, and the selected item. Container
previews are bounded, shallow summaries that show scalar child values and
summarize nested containers. Leaf previews render JSON text as highlighted
JSON, detected source text with tree-sitter syntax highlighting, and other text
as Markdown. Leaf rendering is capped at 64 KiB so a large value cannot stall
the TUI. Array rows show a bounded item preview: scalar values appear inline,
objects prefer an identifying field such as `name`, `operation`, `type`, or
`id`, and other containers show their kind and item count.

The middle panel includes `/thread`, `/memory`, scripts, inbox, mounts, and
system state. It holds no copy of the process root: every node is resolved
through `discover`, `stat`, and `read`, so a mounted folder is browsed exactly
like committed memory. A directory is listed when you expand it and a node is
read when you select it, so opening the console never walks the whole tree, and
each committed version re-reads only the paths that transition changed. Press
`r` in the memory panel to reload the tree for external changes no event can
report. Rows whose content lives
outside the process are labelled with their locality, listings are bounded at
200 children per directory, and a truncated listing says so.

Lampa mounts the current directory read-only as `cwd` and accepts
`--mount name=path` and `--mount-rw name=path` for additional roots. Initially only `/` is expanded, leaving its top-level children
closed. `Tab` moves focus forward between chat input, memory navigation, and
preview scrolling; `Shift+Tab` moves backward. In the memory panel, use arrows
or `j`/`k` to move, `Right` to expand, `Left` to collapse or move to the parent,
and `Enter` to toggle nodes.
Click a visible row to select it without recentering the current window. Use
`PageUp`/`PageDown` or the mouse wheel to scroll. Use `--model` or `SVIT_MODEL`
to override the default model. Conversation messages render as Markdown, and
bare HTTP(S) URLs are emitted as clickable terminal hyperlinks. Lampa uses
OpenAI's Responses API for reasoning and function tools and exposes `search`,
`jq`, `http`, `llm`, and `spawn` under `/bin`.
`llm` and `spawn` reuse Lampa's selected model and OpenAI provider. Lampa's
research console grants the standard HTTP built-in without a destination
allowlist:

```console
OPENAI_API_KEY=... cargo run -p lampa
```

Lampa's entry point opens the selected instance's process store and configures
one persisted `Svit` instance. The TUI
then sends through `Inbox`, renders completed messages from `outbox`, and
refreshes memory through `Svit::read_versioned` after
`SvitEvent::Committed`; it does not retain a `Process`, assemble runtimes, or
poll the process tree.

## Generic process contract

Svit exposes five generic operations over absolute paths:

```text
discover(path)        -> immediate child names
read(path)            -> value at an absolute process path
write(path, value)    -> transactional memory or library update
remove(path)          -> transactional memory or library removal
exec(path, input)     -> execute a path with explicit runtime authority
```

The model-facing `exec` resolves `/lib` scripts and installed `/bin` built-ins.
Bare `Process::exec` and nested Svit Lisp `exec` resolve only
`/lib`, because serializable guest state never owns native host authority.
Builders, snapshots, restore, and fork are process lifecycle APIs.

## Process-owned reasoning loop

`svit::Svit` owns both the Everruns loop and its process. Svit constructs the
system prompt from its fixed runtime contract and process address. A host may
add optional `instructions`; Svit appends them verbatim inside an
`<instructions>` block. The instructions, composed prompt, event-derived
message history, and canonical Everruns events are committed under the
host-managed, guest-readable `/thread` node. Restoring a process resumes that
conversation, and forking it creates an isolated child process with
inherited instructions and a prompt recomposed for the child address:

```rust,no_run
use svit::{ContentPart, Message, OpenAI, Reasoner, Svit};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let mut svit = Svit::builder("svit://local/demo/process")?
    .instructions("Work entirely through your Svit process.")
    .reasoner(Reasoner::new("gpt-5.6-terra", OpenAI::from_env()?))
    .build()
    .await?;

let inbox = svit.inbox();
let mut outbox = svit.outbox();

svit.start()?;
inbox.send(Message::user_multimodal(vec![ContentPart::text(
    "Remember that the release color is blue.",
)]))?;
let answer = outbox.recv().await.expect("completed reasoning turn");
assert!(!answer.text().unwrap_or_default().is_empty());

drop(inbox);
svit.block().await?;
let snapshot = svit.snapshot()?;
# let _ = snapshot;
# Ok(())
# }
```

`start` launches the independently running local loop. `Inbox::send` commits an
Everruns `Message`, including its ordered `ContentPart` values, before waking
the loop. `Svit::outbox` returns an `Outbox` observer that emits the durable
assistant `Message` for each completed turn. `Svit::events` returns an `Events`
observer for commit notifications and sanitized terminal failures. Calling
either method again creates another observer. A host observes an owned value/version pair through
`read_versioned`; it never receives a shared mutable process-tree handle. These
notifications do not replace the durable `/thread/events` log. `block` seals the inbox, drains committed
messages, and joins the loop. A subagent is another `Svit` built around a fork returned by
`svit.fork_process(child_address)`.

### Built-ins

Install host-provided built-ins when the process needs search or structured-data processing:

```rust
use svit::Builtins;

# async fn build() -> svit::SvitResult<svit::Svit> {
# use svit::{LLMSIM_MODEL_ID, LlmSimConfig, Reasoner, llm_sim_provider};
let svit = svit::Svit::builder("svit://local/tools")?
    .memory("records", svit::value!([{"name": "beta", "active": true}]))
    .builtins(Builtins::new())
    .reasoner(Reasoner::new(
        LLMSIM_MODEL_ID,
        llm_sim_provider(LlmSimConfig::fixed("done")),
    ))
    .build()
    .await?;
# Ok(svit)
# }
```

The process receives `/bin/search` and `/bin/jq`. `search` runs a bounded
Rust regular expression over text below a committed process path; `jq` runs a
bounded filter over an explicit JSON input. Neither built-in can access the host
filesystem, environment, or processes.

Built-ins are discovered, inspected, and invoked through the generic process
operations:

```text
discover("/bin")               -> ["jq", "search"]
read("/bin/search")            -> description, input_schema, output, effect, limits
exec("/bin/search", input)     -> bounded search result
exec("/lib/analyze", input)    -> transactional Lisp activation
```

`/bin` is a host-managed built-in catalog. Its records are manuals, not
authority: editing a snapshot cannot install a built-in. Resume refreshes the
catalog from the currently attached built-in registry before reasoning begins.

Hosts can implement `Builtin` and register it by name with
`Builtins::builtin`, or bundle related implementations behind
`BuiltinExtension`. Later registrations replace earlier entries with the
same name, matching Bashkit's custom-builtin rule. Each call receives explicit
JSON input and a `BuiltinContext` that exposes committed `read` and
`discover` operations only. An extension is trusted native host code: any
filesystem, network, or other capability it captures is an explicit host grant,
not a Svit sandbox capability.

`Builtins::http` installs `/bin/http` only with a host URL allowlist and
transport. `Builtins::llm` installs a fixed host-selected `/bin/llm`;
`Builtins::spawn` installs `/bin/spawn`, which forks committed state, runs one child
turn, and retains the child for `Svit::child_snapshot`. These effectful built-ins
are host-side built-ins, not Svit Lisp functions, and do not join an
activation transaction.

`Builtins::standard()` selects the complete standard set and explicitly grants
unrestricted HTTP destinations. `with_http_allowlist` attenuates that grant for
hosts that need URL policy. The single `builtins` builder operation installs
the registry; during Svit construction, it derives `llm` and `spawn` from that
instance's `Reasoner` and creates Svit's redirect-denying, response-bounded
`ReqwestHttpTransport`. Later registrations still replace standard entries, so
a specialized host can replace `http` through `Builtins::http` before passing
the registry to Svit.

Run the live process-owned `support-agent-svit` scenario with `OPENAI_API_KEY` set:

```console
just support-agent-svit
```

## What works now

- One discoverable process namespace containing host-managed `/thread`, mutable
  `/memory`, named `/lib` scripts, virtual external resources under `/mounts`,
  reserved `/tasks` and `/children`, a host-managed durable `/inbox`, plus
  read-only `/system` metadata and the outbox.
- Virtual mounts: `/mounts/<name>` commits a descriptor, and folder or
  materialized query nodes below it resolve lazily through a host-attached
  provider. `stat(path)` reports each node's kind, granted access, locality
  (`cache`, `local`, or `remote`), and source facts such as size, modification
  time, and a folder's git branch and commit. Writes below a mount that grants
  them apply at the activation's commit point.
- Named script installation, inspection, replacement, and removal through the
  same generic path operations used for memory.
- Transactional activations: memory, staged scripts, and message intents all
  commit, or none do.
- Structured `log-info!` records and deterministic buffered `send!` intents.
- Versioned JSON snapshots with validation and a SHA-256 root integrity hash.
- Local Turso event persistence for process mutations and runnable reasoning,
  deterministic resume, bounded queries, exact-boundary forks, on-demand
  snapshots, and safe history cuts.
- Forks that copy committed memory and scripts into an independently mutable
  child without copying the parent's outbox. Agent-process forks also inherit
  committed conversation history and isolate future turns.
- A process-owned `svit::Svit` loop implemented by Everruns, with canonical
  conversation events and model-driven process changes committed through an
  optional durable process owner.
- Configurable execution deadline, nested-exec depth, VM stack, namespace,
  syntax, integer, estimated guest-memory, value, text, script, log, message,
  and staged-script limits.
- Typed host activation hooks that may rewrite, deny, or observe activations.
- Opt-in `/bin/search` and `/bin/jq` built-ins plus host-granted `http`, `llm`,
  and isolated one-turn `spawn` built-ins.
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

## Built-in examples

Every Rust example contains assertions and deterministic output:

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

They cover persistence and restore, functional self-reflection, rollback of a
state-plus-message transaction, isolated forks, denied ambient APIs, a bounded
infinite loop, two clients resolving an optimistic concurrency conflict, and
bounded snapshots of a real folder and a Turso query. The process reasoning
example resumes one Everruns-backed thread and continues it in an isolated
subagent process. The built-ins example searches committed process
data, filters explicit JSON, and registers a host extension with read-only
process context. See
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
filesystem or network read-through projections, secrets, durable control
receipts, distributed migration, exactly-once effects, snapshot
signatures, or formal verification. Process addresses are validated logical
identifiers; they are not authenticated principals.

Opt-in HTTP, model calls, and local one-turn child execution are host built-ins,
not guest-script authority or durable effect delivery. Their results are
recorded in the reasoning event stream, but the effects themselves are neither
transactional nor replay-safe.

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
