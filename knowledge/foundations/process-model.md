---
type: Process Model
title: Process Model
description: Process state, activation, commit, snapshot, restore, and fork semantics.
tags:
  - svit
  - process
  - transaction
---

# Process Model

## Status

Implemented for volatile and local Turso-backed processes. Distributed
ownership remains outside the current slice.

## Process state

The initial process exposes one conventional namespace:

```text
/
├── thread/                 bounded host-managed reasoning metadata; null for a bare Process
│   ├── format              version of the thread metadata contract
│   ├── process_id          owning Svit address
│   ├── session_id          Everruns session identity
│   ├── instructions        optional host instructions, or null
│   └── system_prompt       Svit-owned prompt composed for this address
├── ports/                  host-managed port descriptors
├── memory/                 process-owned durable values
├── lib/                    named scripts
├── tasks/                  reserved; empty in this slice
├── inbox/                  host-managed durable input queue
├── children/               reserved; empty in this slice
├── mounts/                 virtual external resources, resolved lazily
└── system/                 read-only runtime metadata
    ├── identity            logical address, explicitly unauthenticated
    ├── capabilities        empty in this slice
    ├── api                 generic process operations
    ├── limits              configured resource limits
    ├── lineage             parent address for forks
    ├── runtime             language and snapshot format
    └── outbox              committed message intents
```

Thread state, memory, scripts, mounts, system metadata, and outbox form
one committed logical root. Building a `Svit` replaces the bare process's null
`/thread` node with optional host instructions, the Svit-owned system prompt,
and its owning process address. The process address is the Svit
identity; Svit does not accept a second runtime name. Instructions are wrapped in
an `<instructions>` block. Restore retains them, while fork recomposes the base
prompt for the child address. Construction also refreshes `/ports` from the
attached port registry.
Canonical events are stored in a paged host `EventLog` partitioned by process
and session. `/thread` is bounded metadata, so snapshots and restore never
decode conversation history. Everruns compaction checkpoints replace older
model context with a compact payload plus a raw suffix while leaving canonical
events queryable. A presentation host may overlay a bounded recent-event or
message window below `/thread`, but that overlay is not a process value,
snapshot content, or guest context.
Canonical events are append-only, so a long-lived Svit stays bounded only when
the host reclaims history it no longer needs. `cut_thread_events` removes every
event at or below an explicit boundary and records that boundary durably, and
it is refused unless a compaction checkpoint already replaced the prefix and no
fork inherits it. Retention is host policy: a turn never reclaims history on its
own. Volatile and durable Svit instances enforce the same rules.
Each `/ports/<name>` record contains a contract version, description, input schema,
output contract, effect class, and limits. `/thread` and `/ports` are guest-readable but writable only through the
trusted host boundary. `/inbox` is appended and acknowledged only
through host APIs; `/tasks` and `/children` remain reserved and empty.
### Mounts

A mount is a virtual namespace, not a copy. `/mounts/<name>` commits only a
descriptor (`kind`, host-disclosed `source`, `locality`, and granted `access`)
while nodes below it resolve through a host-attached `MountProvider` at the
moment they are read, discovered, stated, or written. No source data enters the
committed root, so mount size is independent of process size and snapshot cost.

Every node answers `stat` with the same facts record: `kind`
(`directory`, `leaf`, or `missing`), `access`, `locality`, `mount`, `path`,
`source`, `attached`, and provider `facts` such as byte size, modification
time, or the folder's git branch and commit. A `content` fact names the shape
(`object`, `array`, `text/plain`, `svit-script`, or `scalar`), so a caller can
tell an array from a map without reading either. Committed nodes answer the
same shape with `locality: cache`, so one vocabulary describes the whole tree
and a caller can weigh the cost of a read before making it.

Because `discover`, `stat`, and `read` answer for committed state and mounts
alike, a client browses one namespace. The Lampa console relies on exactly
that: it holds no committed root of its own and resolves every node (memory,
scripts, or mounted folder) through the same three operations.

### Change reporting

Every transition returns a `Change`: the process version it produced, the
canonical paths it touched, and the replayable `Mutation` list. Paths and
mutations come from the same fold, so a live observer and a stored durable
event always describe the same transition. `Change::touches(path)` is the
shared staleness predicate: a path is affected when it is at, below, or above
a changed path, because a write below a node changes that node's value and can
change its child listing.

A `Change` also publishes the content hash each reported path and its
ancestors now have, and `None` for a path the transition removed.
`Change::touches` answers what a client *may* need to re-read; the hash
answers what it *must*. A cached node whose published hash matches what the
client holds is current, whatever the path overlap says, so a client
revalidates by content instead of discarding a subtree on every commit.
`Process::node_hash` reads the same hash outside a transition.

Two deliberate asymmetries:

- Mount paths carry no hash. Their content lives outside the committed root,
  so a client re-reads them rather than comparing a committed hash.
- A granted mount write reports its path but carries no mutation. It changed an
  external source, not committed state, so there is nothing to persist or
  replay, but a client caching that node must still read it again.
- A notification carries version and paths without values. Observers read what
  they need back through the process API rather than receiving committed state
  on a broadcast channel.

Mount nodes appear in a change only when this process wrote them. Nothing
reports an external edit to a mounted source, so a client caching mount content
stays stale until it reloads (`L-045`).

`locality` is the cost class, not a guarantee: `cache` is already in host
memory, `local` is host-machine I/O, and `remote` is network-bound and
fallible. A materialized Turso query reports `cache` rather than claiming a
live remote view.

Reading a node returns its content when the provider has one and its facts
otherwise; a folder directory has no content, while a cached value node does.
Reading a mount root always returns facts, which is where mount metadata lives.

`access` is `read`, `write`, or `read-write`. Writes and removals below a
granted mount reach the external source. During an activation they are buffered
and applied at the commit point, after every in-process validation, so a failed
activation leaves the source untouched. Mount sources are external systems, so
this is effect ordering, not distributed atomicity.

Providers are host authority and are never serialized. Restoring a snapshot
restores mount identity without mount authority: the root still reads its
descriptor with `attached: false`, and every other operation fails closed until
the host calls `attach_mount`. Fork shares the providers the host attached,
because attaching them was the host's decision.

The process builder assembles initial memory from separately named items into a
text-keyed map and initial scripts through `library(name, script)` so both
durable namespaces are explicit at the setup boundary. It attaches a `Mount`
through `mount(name, mount)`.
Rust callers, the Svit reasoning loop, and Svit Lisp use the same six generic
process operations:

```text
discover(path)
read(path)
stat(path)
write(path, value)
remove(path)
exec(path, input)
```

`discover` returns deterministic immediate child names across memory, scripts,
mount directories, reserved nodes, and system state at every map or array
depth. `read` returns a value. `stat` returns the facts record for one node.
`write` and `remove` modify `/memory`, one typed `/lib/<name>` entry, or a leaf
below a mount whose descriptor grants writes. The `/mounts` descriptor map
itself and `/ports` are read-only. Svit's model-facing `exec` runs either a
named `/lib/<name>` script or transient source passed directly to the tool.
Inline source is interpreted as one activation and never enters the process
root; named `/lib` scripts are the reusable, inspectable form. Svit Lisp uses
`(exec "/lib/name" input)` and `(port-call "name" input)` respectively. Bare
`Process::exec` accepts only `/lib` because a serializable process does not own
host port authority.
`/ports` entries are descriptive and never grant authority.

When Lisp reaches `port-call`, Svit suspends the guest, executes the exact
host-attached port once, and restarts the script with prior port results
recorded for deterministic replay. Guest execution segments share one
wall-time budget, while time awaiting the external port does not consume
that VM budget. Successful completion commits the final transactional working
copy once. A later script failure rolls back memory, scripts, mounts, messages,
and version, but cannot roll back the already completed external effect.
Nested `/lib` execution continues to share its transaction, deadline, output
limits, and independent nesting-depth limit. Port calls share the same
bounded call count.
Builders, snapshot, restore, and fork are lifecycle operations outside this
process contract. `svit::Svit` preserves these names and semantics rather than
introducing another vocabulary. Svit exposes the complete process surface to
its reasoning loop; host authority enters only through explicitly attached
ports and mounts.

Addresses are validated identifiers. In the initial local-only slice they name
message destinations and fork identities but do not imply global routing,
authentication, reachability, or delivery.

`/system/identity` repeats that logical address for discovery and includes
`authenticated: false`. It is descriptive metadata, never a principal or
capability. A fork replaces this address and records its parent's address under
`/system/lineage/parent` without mutating the parent.

## Ports

A host may attach `Ports` to the process-owned runtime. The default registry is
empty. Hosts can register one `Port` by name or contribute a
bundle through `PortExtension`; later registrations replace earlier
entries of the same name. The frozen registry generates both dispatch and the
catalog, so no separate name switch or descriptor table can drift.

## Standard library

Svit Lisp owns local deterministic data functions rather than projecting them as
host capabilities. `(jq filter value)` evaluates a bounded jq filter and returns
an array containing every value emitted by that filter. `(search path pattern)`
uses bounded regular-expression matching over the activation's process tree,
walking mounts lazily. `jq` accepts structured JSON, JSON text, and the JSON
response envelope returned by the HTTP port; for the envelope form it evaluates
the body when that body is JSON. `value-map`, `value-array`, `value-get`, and
Ketos arithmetic follow the same rule: they are part of the interpreter surface,
are available in every activation, and do not appear under `/ports`.

When Svit is built, it derives `/ports` from the exact port implementations
attached by the host. A snapshot records the last catalog for
inspection, but resume refreshes it from current host configuration before a
turn; catalog values never authorize execution. Generic operations such as
`discover`, `read`, and `exec` are API operations and do not appear under `/ports`.
Every implementation receives bounded explicit JSON and a `PortContext`
exposing committed reads and discovery without process
mutation. Extension implementations are trusted native host code and may use
only additional capabilities deliberately captured during registration.

There is no standard port bundle or build-time port derivation. A host adds
`http`, `llm`, and `spawn` individually and passes the frozen registry to Svit.
`http` requires a host-selected allowlist and transport;
`http_unrestricted` makes an unrestricted HTTP-destination grant explicit at
the call site. `llm` and `spawn` each require their own host-selected
`Reasoner`. The reusable reqwest transport refuses redirects but does not
impose a response-size cap; a script must reduce a large response before it
crosses the persistent or model-visible value boundary. Later registrations
win, including a specialized host's explicit `http` adapter. Presentation
hosts do not reconstruct this registry after the Svit instance is built.

Hosts choosing a smaller port set have no HTTP authority unless they add it
explicitly with an allowlist and transport or call `http_unrestricted`.
`/ports/llm` uses one
host-selected model and driver. A port remains a host dispatch rather than
a nested activation, even when Lisp invokes it, and its external effects cannot
be rolled back with process state.

A running Svit publishes host-only operational events. `Committed` is a
notification that process state changed; it does not expose the process root.
`CanonicalEvent` reports one newly appended EventLog record, and `Message`
reports one message derived from that record. The host may obtain an owned
value and its version atomically through the Svit contract. The shared
process-state transition boundary publishes `Committed` exactly once after
refreshing its read projection, while EventLog append observers publish the
canonical event before its derived message. A failure event contains only a
sanitized diagnostic. These transient notifications do not replace the durable
process event log or outbox. Each `events` or `outbox` call creates an
independent observer behind the Svit contract; consumers do not receive the
underlying channel implementation.

`Svit::persisted` accepts any `DurableProcessHandle`. One serialized owner
routes host writes, removals, Lisp activations, inbox transitions, reasoning
events, and port catalog refresh through that handle. The owner publishes a
new cloned read projection only after the adapter accepts the transition, so a
CAS conflict cannot leak an uncommitted candidate into Svit reads or tools.

The child port is named `spawn`, distinguishing process creation from
executing a `/lib` script. It takes a child address and task,
forks the last committed parent state, uses a separately supplied child model driver, runs one child turn, and
retains the completed child in the parent runtime's local registry. `child_ids` and
`child_snapshot` expose that registry to the Rust host. It is not stored under
`/children`, included in the parent snapshot, scheduled, or supervised.

## Activation transition

A named script activation accepts a bounded input value and runs against a
working copy of the committed root:

```text
activate(process, script, input, limits)
  -> output + logs + committed message intents
```

On success, the runtime validates every staged value and script, atomically
replaces the root, appends buffered message intents, and increments the process
version exactly once. On any failure, version, memory, scripts, and outbox are
unchanged.

A host `write` or `remove` constructs and validates a replacement root before
its single commit assignment. A rejected path or value leaves the committed
root and version unchanged.

The initial implementation may clone values during activation. Persistent
structural sharing is an optimization, not part of the public contract.

## Multi-client control

Svit Control Protocol 1 serializes all client requests for one process. A
mutating request includes a mandatory `expected_version`. The controller checks
that precondition under the same lock that contains activation commit. If two
clients race the same version, at most one successful activation can commit the
next version; stale requests return a conflict without executing guest code.

An exact `(client_id, request_id)` retry returns its bounded cached receipt.
After receipt eviction, the stale version precondition still prevents a second
commit. These guarantees are implemented by the in-memory reference controller.
Durable replay across a host crash requires a storage adapter that atomically
persists the process commit and receipt.

This is **Versioned Atomic State Transitions (VAST)** semantics: one request may
advance the current version by exactly one atomic root replacement; a stale or
rejected request leaves the committed root unchanged. VAST is the controlled
transition model, while `svit-control@1` remains the concrete wire major. It
does not merge concurrent activations or establish distributed process
ownership.

See [Svit Control Protocol 1](../protocols/control-protocol.md).

## Scripts

A named script is source plus bounded metadata. Source, not VM bytecode or a
closure, is canonical state. Saving or replacing a script participates in the
same transaction as memory and outbox updates. Staged scripts must compile
before commit. Scripts supplied to the process builder are validated with the
complete initial root and begin at version zero; adding a script to an existing
process is a committed state transition that increments its version.

## Snapshots and restore

A snapshot contains a versioned deterministic representation of all committed
process state and a SHA-256 root integrity hash. That hash is the root of the
content-hash tree every committed node publishes: a node hashes a type tag and
then its own leaf bytes or its children's digests, so a hash covers one
subtree and nothing above it. An unchanged subtree keeps its hash across
commits, forks, and snapshots, and a mount node hashes its committed
descriptor rather than the external source it resolves through. A snapshot
never contains an executing stack, host pointers, capabilities, interpreter
globals, or uncommitted work.

Restore treats bytes as untrusted, validates format version and all value
invariants, and reconstructs a committed process. Snapshot integrity is not
authorization or authenticity.

The adopted durable-storage design reconstructs this same committed state from
an immutable base plus one address-keyed tail of uniform `ProcessTransaction`
records. Transaction position is separate from process version because future
receipt-only metadata need not change process state. Both are separate from the
Everruns sequence stored in the paired paged `EventLog`: transaction position
orders durable envelopes, process version orders committed roots, and Everruns
sequence orders canonical reasoning events. On-demand
snapshots support detached forks, migration, and safe history cuts; they are
not written on every commit. The local Turso adapter separately atomically
replaces one internal recovery checkpoint every 32 transactions and after the
first resume of an older tail. It validates that checkpoint and reduces only
the newer tail during ordinary resume without deleting retained transactions.
The local `DurableProcess` adapter implements the process and runnable
reasoning transition slices; durable control receipts remain under
implementation. See
[Single-Svit Process Transaction Persistence](persistence.md).

## Fork

Fork creates a new process address from a committed state. The child begins
with the parent's committed memory and script library, an empty outbox, and
independent future commits. The current implementation clones the logical root;
structurally shared nodes remain a future optimization. A child mutation cannot
change the parent or a sibling.

Authority inheritance is deferred because the initial slice has no external
capabilities. Future grants must be explicitly attenuated, never inferred from
memory contents.

## Reasoning loop

`svit::Svit` binds one reason/act loop and one durable conversation thread to
one process. Everruns implements the current loop behind the Svit API. Each
durable Everruns event is validated and committed through Svit's paired paged
`EventLog`; callers do not construct an external Everruns runtime and attach
Svit as a tool. `/thread` is session metadata only. `/ports` is refreshed from the currently attached port registry, so
a restored process does not retain execution authority omitted by the new host.
The model can inspect manuals through ordinary `discover` and `read` operations.

The host obtains an `Inbox`, starts the process loop, and sends messages to the
durable `/inbox` queue. The loop processes committed messages in order and
acknowledges the exact queue head only after a successful Everruns turn. A failed
turn leaves its input committed for recovery. Hosts may send while a turn is
running; those messages remain ordered and begin subsequent turns. Both APIs
use Everruns `Message` values with ordered `ContentPart` values, preserving text,
images, actor metadata, and assistant role instead of reducing the boundary to
strings. Live outbox receivers may await completion before, during, or after
any turn. `block` stops admission, drains the committed queue, and joins the
loop.

A process snapshot carries memory, scripts, and thread metadata, not event
history. A process-only fork starts a fresh session. A durable
`DurableProcess::fork` shares the exact immutable event prefix and checkpoints
whose source sequence lies in that prefix, then appends independently. Each started Svit owns an independent local Tokio task; no
distributed scheduler, timer, or automatic process discovery is implied.

Reasoning-event commits and Svit Lisp activations are individually atomic process
transitions. A complete model turn is not one Svit activation: model calls and
other external actions occur between event commits and cannot join a process
transaction.

## Determinism

Given the same snapshot, script, input, runtime-language version, and limits,
the transition should produce the same committed state, output, and message
identifiers. Wall-clock duration and internal interrupt counts are not guest
state. The initial replay integration test compares output, logs, messages,
root hashes, and final snapshots from two restores. Broader cross-version
determinism remains unclaimed.
