---
type: Architecture
title: Architecture
description: Trusted-core boundaries and scope of the first executable Svit slice.
tags:
  - svit
  - architecture
  - rust
---

# Architecture

## Status

Initial executable vertical slice implemented. Hardening remains in progress.

## Decision

Svit is a Rust library for running isolated agent processes. A process is an
actor-like state machine: it handles one activation at a time and owns one
committed state root. Parallelism comes from independent processes, not shared
guest memory.

The first executable slice contains:

```text
Rust caller
    |
    v
Process ----> transaction working copy ----> commit or rollback
    |                    |
    |                    v
    |               restricted Lisp VM
    |
    +----> snapshot / restore / fork
    +----> buffered message intents (not delivery)
```

The trusted core owns validation, resource accounting, transaction boundaries,
canonical serialization, process identity, and state isolation. The embedded
Ketos interpreter is treated as a component inside that boundary, not as proof of
the boundary.

## Initial module responsibilities

| Responsibility | Purpose |
| --- | --- |
| Value model | Bounded serializable guest values with deterministic encoding |
| Process | Address, version, committed root, limits, and lifecycle operations |
| Activation | Fresh guest execution, working state, output, logs, and intents |
| Script library | Named source records stored with committed process state |
| Lisp adapter | Converts values and exposes only the versioned Svit Lisp surface |
| Snapshot | Versioned deterministic JSON encoding, SHA-256 root hash, restore validation, and fork source |
| Process controller | Serializes multi-client commands, enforces version preconditions, and retains bounded retry receipts |

The current workspace implements these responsibilities in the `svit` crate,
provides a thin `svit-cli` crate, and keeps the Agentyk integration in the
separate `svit-agentyk` adapter crate. The core does not depend on an agent
framework. Module names may evolve; the boundaries are the decision.

## Trusted-boundary rules

1. Guest code receives no ambient host authority.
2. The host converts and validates all values at the guest boundary.
3. An activation changes committed state only through one successful commit.
4. Message sends are buffered data committed with state; this slice does not
   deliver them.
5. Each activation uses a fresh guest VM so globals cannot leak between
   activations or tenants.
6. Snapshot bytes are untrusted on restore and pass the same invariant checks
   as newly created state.
7. Every mutating control request carries an expected process version and is
   checked at the same serialization point as commit.

## Deferred architecture

Schedulers, external effect adapters, projections, durable databases,
distributed routing, auth services, migrations between hosts, and production
Wasm/OS isolation are outside this slice. See
[Limitations](../operations/limitations.md).

## Alternatives considered

- A single long-lived Lisp environment was rejected because it couples durable
  state to interpreter internals and makes rollback, serialization, and tenant
  isolation harder to state.
- Serializing Lisp stacks, closures, quotations, or foreign values was rejected. Only
  committed Svit values and script source cross activation boundaries.
- Executing external effects inside the transaction was rejected because a
  rollback cannot undo them. The slice records message intents only.
