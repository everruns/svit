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
├── memory/                 agent-owned durable values
├── lib/                    named scripts
├── tasks/                  reserved; empty in this slice
├── inbox/                  reserved; empty in this slice
├── children/               reserved; empty in this slice
├── mounts/                 reserved; empty in this slice
└── system/                 read-only runtime metadata
    ├── identity            logical address, explicitly unauthenticated
    ├── capabilities        empty in this slice
    ├── api                 generic agent operations
    ├── limits              configured resource limits
    ├── lineage             parent address for forks
    ├── runtime             language and snapshot format
    └── outbox              committed message intents
```

`memory`, scripts, system metadata, and outbox form one committed logical root.
The reserved nodes establish the namespace without claiming scheduler,
delivery, child-supervision, or projection behavior. They remain empty and
read-only until those semantics are implemented. The guest may reflect over
memory and scripts. Enforcement state and host secrets never appear there.
The process builder assembles initial memory from separately named items into a
text-keyed map and initial scripts through `library(name, script)` so both
durable namespaces are explicit at the setup boundary.
Rust callers, agent integrations, and Svit Lisp use the same five generic
process operations:

```text
discover(path)
read(path)
write(path, value)
remove(path)
exec(script, input)
```

`discover` returns deterministic immediate child names across memory, scripts,
reserved nodes, and system state at every map or array depth. `read` returns a
value. `write` and `remove` modify `/memory` or one typed `/lib/<name>` entry.
`exec` runs a named script activation. Inside Svit Lisp these operations use
the activation's transactional view; nested `exec` shares its transaction,
deadline, output limits, and independent nesting-depth limit.
Builders, snapshot, restore, and fork are
lifecycle operations outside this agent contract. Adapters preserve these five
names and semantics rather than introducing another vocabulary. A host may
attenuate an agent integration to a subset of operations and named scripts;
that host policy is not stored in or forgeable through process memory.

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

## Determinism

Given the same snapshot, script, input, runtime-language version, and limits,
the transition should produce the same committed state, output, and message
identifiers. Wall-clock duration and internal interrupt counts are not guest
state. The initial replay integration test compares output, logs, messages,
root hashes, and final snapshots from two restores. Broader cross-version
determinism remains unclaimed.
