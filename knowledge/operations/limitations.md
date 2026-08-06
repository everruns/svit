---
type: Limitations
title: Limitations
description: Explicit negative specification for the research-stage runtime.
tags:
  - svit
  - limitations
  - research
---

# Limitations

## Status

The project is research-stage. This document is the negative specification for
the initial vertical slice. IDs are stable and never reused.

| ID | Limitation | Consequence |
| --- | --- | --- |
| `L-001` | No production multi-tenant isolation claim | Native in-process Ketos requires an additional Wasm or OS boundary for hostile deployment |
| `L-002` | No scheduler, timers, or background activations | Callers invoke processes explicitly |
| `L-003` | Messages are committed intents only | Routing, delivery, retries, ordering across hosts, and dead letters are not implemented |
| `L-004` | Addresses are local validated identifiers, not authenticated identities | Possessing or naming an address proves no authority |
| `L-005` | No external capabilities or projections | Guest code cannot access network, filesystem, models, secrets, clocks, or real-world data |
| `L-006` | No durable database adapter | Snapshots are caller-managed values or bytes; process residency is in memory |
| `L-007` | No live-stack serialization | Coroutines, closures, userdata, VM bytecode, and in-progress activations are not snapshotted |
| `L-008` | Svit Lisp is a restricted versioned subset | Full Scheme, Common Lisp, and unrestricted Ketos compatibility are unsupported |
| `L-009` | Ketos exposes wall-clock execution limits, not deterministic instruction fuel | Budget-boundary success can vary with host load; deterministic replay is only claimed for activations that complete within budget |
| `L-010` | Fork shares only committed logical state | Mailbox policy, capability attenuation, remote lineage, and distributed child supervision are deferred |
| `L-011` | Snapshot hashes provide integrity, not authenticity | Callers must protect storage and add signatures when provenance matters |
| `L-012` | No schema migrations between runtime-language versions | Restore may reject snapshots from unsupported versions |
| `L-013` | No exactly-once external effects | Future adapters need idempotency and commit-log protocols |
| `L-014` | Formal verification is not yet implemented | Determinism, isolation, and atomicity begin as executable invariants, not mathematical proofs |
| `L-015` | Snapshots use a versioned deterministic JSON codec, not deterministic CBOR | The research format is inspectable but larger and not the proposed long-term wire format |
| `L-016` | Persistent byte-string values are not implemented | Binary data must be encoded as text by the caller |
| `L-017` | Runtime hooks are host configuration and are not serialized | Restored processes require the host to attach policy hooks again |
| `L-018` | The control adapter is in-memory and transport-neutral | No network listener, routing, durable receipts, or crash recovery is provided |
| `L-019` | Transactions cover one process root and its outbox only | External systems and other processes cannot join the same atomic commit |
| `L-020` | Control identifiers are not authenticated identities | A transport host must authenticate and authorize clients outside the envelope |
| `L-021` | The controller is not a distributed process lease | Two hosts restored from one snapshot can diverge unless a durable owner and fencing token serialize commits |
| `L-022` | No bounded control-protocol wire decoder or network adapter | Hosts must cap request bytes before deserialization and supply transport security |
| `L-023` | No generated control schema, initialization exchange, or cross-version SDK suite | The current JSON shape is a tested research interface, not a stable remote wire release |
| `L-024` | Ketos memory accounting is an abstract value estimate, not an allocator byte cap | An outer process or Wasm memory limit remains required for hostile workloads |
| `L-025` | Ketos 0.12 unconditionally declares an obsolete REPL dependency stack | The locked graph contains unused unmaintained crates and crate-specific license/advisory exceptions; a maintained fork or interpreter replacement is required before production use |

Remove a limitation only when implementation, tests, public documentation, and
the threat model all agree. Record the change in `knowledge/log.md` rather than
reusing its ID.
