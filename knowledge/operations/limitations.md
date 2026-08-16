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
| `L-006` | Durable persistence remains local and incomplete | The address-keyed Turso adapter persists host writes/removals, activations, inbox and acknowledgement changes, paged reasoning events and compaction checkpoints, port refresh, forks, snapshots, queries, and cuts; durable control receipts, crash qualification, and distributed ownership are not implemented or proven |
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
| `L-027` | Mounts observe an external source, so a mount view is never a transaction | A listing, `stat`, and `read` can each see a different state, a node can disappear between them, and folder git facts report branch and commit only, never working-tree cleanliness |
| `L-028` | Mount values support the current persistent value model only | Folder files and SQL text must be UTF-8; symbolic links, special files, and Turso blobs are rejected |
| `L-029` | The Turso Rust database engine dependency is pre-release | SQL compatibility and operational behavior may change before its stable release; snapshot-mount callers and the persistence adapter must test upgrades and crash behavior |
| `L-030` | Turso mount construction has no internal query deadline | Callers must wrap expensive or remote host-selected queries in an outer timeout before process construction |
| `L-031` | One Svit process owns exactly one Everruns conversation thread | Multiple participants or independent threads require separate processes in the current implementation |
| `L-032` | Reasoning events and script activations commit separately around external model and tool calls | Recovery of an incomplete turn is at-least-once for external actions; a whole model turn is not one atomic Svit activation |
| `L-033` | The Everruns loop records host-generated event identifiers and timestamps plus provider output | Process snapshots preserve the recorded thread, but deterministic Svit Lisp replay does not imply deterministic model-turn replay |
| `L-034` | The live reasoning-turn outbox is a bounded in-memory notification stream | Slow listeners may lag; durable conversation recovery uses `/thread`, not the live receiver |
| `L-035` | The Svit Lisp `jq` standard-library function rejects recursive, input-generator, and range constructs | The supported bounded jq surface is deliberately smaller than the jq CLI; compose its emitted values with Svit Lisp |
| `L-036` | Opt-in HTTP and nested model calls are immediate external effects outside process transactions, including when Svit Lisp invokes them | Guest replay does not repeat a completed call within one activation, but later script failure, persistence conflict, process retry, or recovery cannot roll it back; hosts must enforce authorization, cost, idempotency, and reconciliation policies |
| `L-037` | `spawn` runs one local child turn and retains the child only in the current parent runtime | Parent snapshots do not include the child registry; there is no child scheduling, cancellation propagation, durable supervision, or distributed ownership |
| `L-038` | `/ports` stores port manuals, not port authority | A bare restored `Process` may contain historical manuals; building or resuming `Svit` refreshes them from the attached `Ports`, and only that host runtime can dispatch `/ports` paths |
| `L-040` | Host-defined `Port` implementations are trusted native code, not sandboxed guest extensions | Svit bounds their JSON input and all persistent/model-visible script output, and supplies only a read-only process context; an implementation can use any host capability it deliberately captures, including activation-local memory, so hosts must review extensions as part of the trusted computing base |
| `L-041` | The adapter-neutral API shares the canonical `ProcessTransaction` encoding, validation, and reducer, but not a storage engine | A non-Turso adapter must still enforce head CAS, immutable writes, fork references, snapshot/query bounds, cuts, ambiguous-write recovery, and executor fencing; current executable storage evidence covers `TursoProcessStore` only |
| `L-042` | Mount writes are ordered with the process commit, not atomic with it | A buffered mount write applies immediately before the root swap, so a crash between the two can leave the external source ahead of committed process state; external sources cannot join a process transaction |
| `L-043` | Mount providers resolve synchronously | A source needing async I/O must be materialized by the host before mounting, so a Turso query mount reports `cache` locality instead of a live remote view |
| `L-044` | Mount authority is not serialized | A restored process reads mount descriptors but resolves nothing below them until the host calls `attach_mount` again; forks share the parent's attached providers by default |
| `L-045` | Change reporting covers committed transitions only | A mounted source edited outside the process emits no event, so a client caching mount content stays stale until it reloads; mount change notification would need an opt-in provider watcher |
| `L-046` | Process import preserves current state but not source history topology | Import starts a new transaction tail at the existing version; prior transaction envelopes, snapshots, and fork references remain only in the source store |
| `L-047` | Context compaction requires a provider-supported strategy | Turso durably stores Everruns checkpoints, but a provider without native compaction falls back to Everruns' configured policies; retention and archival of canonical event history remain host policy |
| `L-048` | Port responses are activation-local in-memory values, not streams or temporary files | A script can reduce a response larger than the durable value envelope before committing a small result, but the complete response remains in process memory during the activation; generic temporary-workspace and streaming-transfer contracts are deferred |

Remove a limitation only when implementation, tests, public documentation, and
the threat model all agree. Record the change in `knowledge/log.md` rather than
reusing its ID.
