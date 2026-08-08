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
| `L-002` | No scheduler, timers, or automatic process activation | Callers explicitly start each local process loop |
| `L-003` | Outgoing Lisp messages are committed intents only | Routing, delivery, retries, ordering across hosts, and dead letters are not implemented; local inbox submission is explicit host API |
| `L-004` | Addresses are local validated identifiers, not authenticated identities | Possessing or naming an address proves no authority |
| `L-005` | Svit Lisp has no live or writable external capabilities | Guest code can read recorded snapshot mounts and `/bin` manuals but cannot execute `/bin` or access network, live filesystem/database handles, models, secrets, or clocks; host-side executable dispatch does not widen the Lisp surface |
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
| `L-026` | `/tasks` and `/children` are reserved empty nodes; `/inbox` accepts only explicit local host submissions | Discovery does not imply scheduling, remote delivery, or child supervision behavior; the optional local `spawn` registry is not stored under `/children` |
| `L-027` | Mounts are construction-time read-only snapshots | External changes are not observed until the host imports a new mount into a new process; writes and effect intents are not implemented |
| `L-028` | Folder and Turso snapshot values support the current persistent value model only | Folder files and SQL text must be UTF-8; symbolic links, special files, and Turso blobs are rejected; callers must prevent concurrent folder-tree replacement during import |
| `L-029` | The Turso Rust database engine dependency is pre-release | SQL compatibility and operational behavior may change before its stable release; snapshot mount callers must test upgrades |
| `L-030` | Turso mount construction has no internal query deadline | Callers must wrap expensive or remote host-selected queries in an outer timeout before process construction |
| `L-031` | One Svit process owns exactly one Agentyk conversation thread | Multiple participants or independent threads require separate processes in the current implementation |
| `L-032` | Agent events and script activations commit separately around external model and tool calls | Recovery of an incomplete turn is at-least-once for external actions; a whole model turn is not one atomic Svit activation |
| `L-033` | The Agentyk loop records host-generated event identifiers and timestamps plus provider output | Process snapshots preserve the recorded thread, but deterministic Svit Lisp replay does not imply deterministic model-turn replay |
| `L-034` | The live agent-turn outbox is a bounded in-memory notification stream | Slow listeners may lag; durable conversation recovery uses `/agent`, not the live receiver |
| `L-035` | Native `jq` rejects recursive, input-generator, and range constructs | The supported bounded jq surface is deliberately smaller than the jq CLI; compose tool results through typed model calls |
| `L-036` | Opt-in HTTP and nested model calls are immediate external effects outside process transactions | Failure or replay cannot roll them back; hosts must enforce authorization, cost, idempotency, and reconciliation policies |
| `L-037` | `spawn` runs one local child turn and retains the child only in the current parent runtime | Parent snapshots do not include the child registry; there is no child scheduling, cancellation propagation, durable supervision, or distributed ownership |
| `L-038` | `/bin` stores executable manuals, not executable authority | A bare restored `Process` may contain historical manuals; building or resuming `Svit` refreshes them from the attached `Executables`, and only that host runtime can dispatch `/bin` paths |

Remove a limitation only when implementation, tests, public documentation, and
the threat model all agree. Record the change in `knowledge/log.md` rather than
reusing its ID.
