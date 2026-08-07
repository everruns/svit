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

Implemented for the initial in-memory vertical slice.

## Process state

The initial process exposes one conventional namespace:

```text
/
├── agent/                  host-managed runtime projection; null for a bare Process
│   ├── system_prompt       configured agent instructions
│   ├── messages            conversation projected from events
│   └── events              canonical Agentyk event stream
├── memory/                 agent-owned durable values
├── lib/                    named scripts
├── tasks/                  reserved; empty in this slice
├── inbox/                  host-managed durable input queue
├── children/               reserved; empty in this slice
├── mounts/                 bounded read-only external snapshots
└── system/                 read-only runtime metadata
    ├── identity            logical address, explicitly unauthenticated
    ├── capabilities        empty in this slice
    ├── api                 generic agent operations
    ├── limits              configured resource limits
    ├── lineage             parent address for forks
    ├── runtime             language and snapshot format
    └── outbox              committed message intents
```

Agent-thread state, memory, scripts, mounts, system metadata, and outbox form
one committed logical root. Building a `Svit` replaces the bare process's null
`/agent` node with the configured system prompt, projected messages, and
canonical events. `/agent` is guest-readable but writable only through the
trusted host boundary. `/inbox` is appended and acknowledged only
through host APIs; `/tasks` and `/children` remain reserved and empty.
Each mount contains `kind`, `mode: snapshot-read-only`, and bounded `data`.
Folder mounts contain nested maps with UTF-8 file leaves; Turso mounts contain
maps for the rows returned by one host-selected query. The guest may reflect
over memory, scripts, and imported mount data. Host paths, SQL authority,
connections, enforcement state, and host secrets never appear there.
The process builder assembles initial memory from separately named items into a
text-keyed map and initial scripts through `library(name, script)` so both
durable namespaces are explicit at the setup boundary. It attaches a prepared
`SnapshotMount` through `mount(name, mount)`; import finishes before process
construction and activation.
Rust callers, the Svit agent loop, and Svit Lisp use the same five generic
process operations:

```text
discover(path)
read(path)
write(path, value)
remove(path)
exec(script, input)
```

`discover` returns deterministic immediate child names across memory, scripts,
mount snapshots, reserved nodes, and system state at every map or array depth. `read` returns a
value. `write` and `remove` modify `/memory` or one typed `/lib/<name>` entry.
`/mounts` is read-only. `exec` runs a named script activation. Inside Svit Lisp these operations use
the activation's transactional view; nested `exec` shares its transaction,
deadline, output limits, and independent nesting-depth limit.
Builders, snapshot, restore, and fork are
lifecycle operations outside this agent contract. `svit::Svit` preserves
these names and semantics rather than introducing another vocabulary. A host
may attenuate an agent to a subset of operations and named scripts; that host
policy is not stored in or forgeable through process memory.

Addresses are validated identifiers. In the initial local-only slice they name
message destinations and fork identities but do not imply global routing,
authentication, reachability, or delivery.

`/system/identity` repeats that logical address for discovery and includes
`authenticated: false`. It is descriptive metadata, never a principal or
capability. A fork replaces this address and records its parent's address under
`/system/lineage/parent` without mutating the parent.

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
process state and a SHA-256 root integrity hash. It never contains an executing stack,
host pointers, capabilities, interpreter globals, or uncommitted work.

Restore treats bytes as untrusted, validates format version and all value
invariants, and reconstructs a committed process. Snapshot integrity is not
authorization or authenticity.

## Fork

Fork creates a new process address from a committed state. The child begins
with the parent's committed memory and script library, an empty outbox, and
independent future commits. The current implementation clones the logical root;
structurally shared nodes remain a future optimization. A child mutation cannot
change the parent or a sibling.

Authority inheritance is deferred because the initial slice has no external
capabilities. Future grants must be explicitly attenuated, never inferred from
memory contents.

## Agent loop

`svit::Svit` binds one reason/act loop and one durable conversation thread to
one process. Agentyk implements the current loop behind the Svit API. Each
durable Agentyk event batch is validated and committed under `/agent`; callers
do not construct an external Agentyk agent and attach Svit as a tool.
`/agent/events` is the canonical replay source. `/agent/messages` is rebuilt
from those events at the host-only commit boundary and checked against them on
resume. The model can inspect the projection through ordinary `discover` and
`read` tools without receiving mutation authority.

The host obtains an `Inbox`, starts the process loop, and sends messages to the
durable `/inbox` queue. The loop processes committed messages in order and
acknowledges the exact queue head only after a successful Agentyk turn. A failed
turn leaves its input committed for recovery. Hosts may send while a turn is
running; those messages remain ordered and begin subsequent turns. Both APIs
use Agentyk `Message` values with ordered `ContentPart` values, preserving text,
images, actor metadata, and assistant role instead of reducing the boundary to
strings. Live outbox receivers may await completion before, during, or after
any turn. `block` stops admission, drains the committed queue, and joins the
loop.

A completed process snapshot therefore carries agent history as well as memory
and scripts. Restore resumes the recorded thread. A process fork inherits the
committed thread and then appends independently, so a subagent owns a distinct
child process. Each started Svit owns an independent local Tokio task; no
distributed scheduler, timer, or automatic process discovery is implied.

Agent event commits and Svit Lisp activations are individually atomic process
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
