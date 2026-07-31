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

The initial process owns:

```text
Process = {
  address,
  version,
  memory,
  scripts,
  outbox,
  limits
}
```

`memory`, `scripts`, and `outbox` form one committed logical root even if the
Rust implementation uses separate typed fields. The guest may reflect over
memory and scripts. Enforcement state and host secrets never appear there.

Addresses are validated identifiers. In the initial local-only slice they name
message destinations and fork identities but do not imply global routing,
authentication, reachability, or delivery.

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

The initial implementation may clone values during activation. Persistent
structural sharing is an optimization, not part of the public contract.

## Scripts

A named script is source plus bounded metadata. Source, not VM bytecode or a
closure, is canonical state. Saving or replacing a script participates in the
same transaction as memory and outbox updates. Staged scripts must compile
before commit.

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
