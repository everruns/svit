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

Svit is a Rust library for running isolated agents. A Svit agent owns one
process and its durable conversation thread. A process is an actor-like state
machine: it handles one transition at a time and owns one committed state root.
Agentyk implements the current host-side reason/act loop behind `svit::Svit`.
Parallelism comes from independent processes, not shared guest memory.

The first executable slice contains:

```text
Rust caller ----> Svit Agent
                      |
                      +----> Agentyk reason/act loop
                      |
                      v
                  Process ----> transaction working copy ----> commit or rollback
                      |                    |
                      |                    v
                      |               restricted Lisp VM
                      |
                      +----> snapshot / restore / fork
                      +----> durable inbox / live turn outbox
                      +----> buffered message intents (not delivery)
                      +----> bounded folder / Turso snapshot import
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
| Agent loop | Agentyk `Message`/`ContentPart` inbox and outbox with durable events under host-managed `/agent` |
| Activation | Fresh guest execution, working state, output, logs, and intents |
| Script library | Named source records stored with committed process state |
| Snapshot mounts | Bounded read-only folder trees and host-selected Turso query rows |
| Lisp adapter | Converts values and exposes only the versioned Svit Lisp surface |
| Snapshot | Versioned deterministic JSON encoding, SHA-256 root hash, restore validation, and fork source |
| Process controller | Serializes multi-client commands, enforces version preconditions, and retains bounded retry receipts |

The current workspace implements the process and process-owned agent loop in
the `svit` crate and provides a thin `svit-cli` crate. `svit::Svit` assembles
the Agentyk engine internally and can expose the complete generic process
surface or attenuate a model to discovery, reads, and a host-selected script
allowlist. Agentyk is an implementation dependency, not the owner of the agent
or process. Module names may evolve; the ownership boundary is the decision.

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
8. External data enters only through a host-created bounded snapshot mount;
   activations receive persistent values, never filesystem or database handles.
9. Durable loop and replay state is host-managed under `/agent`; untrusted
   scripts and model tools can inspect the configured prompt, derived message
   history, and canonical events but cannot rewrite them.
10. Inbox messages commit before loop notification and are acknowledged only
    after the corresponding turn succeeds.

## Deferred architecture

Schedulers, external effect adapters, read-through or writable projections,
durable process databases, distributed routing, auth services, migrations
between hosts, and production Wasm/OS isolation are outside this slice. See
[Limitations](../operations/limitations.md).

## Alternatives considered

- A single long-lived Lisp environment was rejected because it couples durable
  state to interpreter internals and makes rollback, serialization, and tenant
  isolation harder to state.
- Serializing Lisp stacks, closures, quotations, or foreign values was rejected. Only
  committed Svit values and script source cross activation boundaries.
- Executing external effects inside the transaction was rejected because a
  rollback cannot undo them. The slice records message intents only.
