---
type: Threat Model
title: Threat Model
description: Initial assets, trust boundaries, threats, required controls, and stable identifiers.
tags:
  - svit
  - security
  - threats
---

# Threat Model

## Status

Living document. Controls are `REQUIRED` until implementation and regression
tests demonstrate them. This document does not claim the initial runtime is
ready for hostile production multi-tenancy.

## Actors and assets

Threat actors are malicious or buggy guest scripts, model output, agent-event
payloads, inputs, snapshots, messages, and callers attempting cross-process access.

Protected assets are host CPU and memory, host filesystem and network,
environment and credentials, runtime availability, process state, other
tenants, identity material, audit integrity, and snapshot integrity.

## Trust boundaries

```text
UNTRUSTED                         TRUSTED RUST CORE
script / input / snapshot  --->  validation + limits + transaction
                                         |
                                         v
                                  restricted Lisp VM
                                         |
                                         v
                                staged values and intents
                                         |
                                         v
                                  validate then commit
```

The Ketos VM executes untrusted content inside the trusted host process. Memory
safety from Rust and interpreter sandboxing reduce risk but do not constitute a
formal same-process isolation proof. A production deployment should add a Wasm
or OS process boundary until stronger evidence exists.

## Threat ID management

IDs use `TM-<CATEGORY>-<NUMBER>` and are never reused. Deprecated threats keep
their ID. A mitigation includes a nearby code comment:

```rust
// THREAT[TM-ESC-001]: Guest code must not reach ambient host APIs.
// Mitigation: construct the guest library from an explicit allowlist.
```

The same ID appears in at least one focused test before status changes to
`MITIGATED`.

| Prefix | Category |
| --- | --- |
| `TM-DOS` | Resource exhaustion |
| `TM-ESC` | Sandbox escape |
| `TM-INF` | Information disclosure |
| `TM-ISO` | Cross-process and cross-tenant isolation |
| `TM-CAP` | Capability forgery or amplification |
| `TM-AUTH` | Identity, authentication, and authorization |
| `TM-MSG` | Messaging integrity and availability |
| `TM-SNAP` | Snapshot integrity, restore, and rollback |
| `TM-FORK` | Fork isolation and authority inheritance |
| `TM-EFF` | Atomicity and external-effect ordering |
| `TM-INT` | Panics and diagnostic disclosure |
| `TM-AUD` | Audit and replay integrity |
| `TM-PERS` | Durable event integrity and atomicity |
| `TM-SUP` | Interpreter and dependency supply chain |

## Initial threats

| ID | Threat | Required control | Status |
| --- | --- | --- | --- |
| `TM-DOS-001` | Infinite or expensive guest computation monopolizes a worker | Per-activation execution deadline plus independent outer containment | PARTIAL — Ketos deadline test passes; deterministic fuel and an outer supervisor remain required |
| `TM-DOS-002` | Guest allocations exhaust host memory | VM memory estimate and bounded conversion before allocation/commit | PARTIAL — guest-memory and conversion tests pass; the estimate is not an allocator byte cap |
| `TM-DOS-003` | State, output, logs, scripts, or outbox grow without bound | Independent byte/count/depth limits with fail-closed accounting | PARTIAL — configured limits exist; aggregate output cases remain |
| `TM-DOS-004` | Client request ids grow the control receipt cache without bound | Independent hard receipt-count maximum and eviction | MITIGATED for the in-memory controller |
| `TM-DOS-005` | Oversized encoded control requests exhaust memory before value validation | Transport byte cap before deserialization plus decoded value limits | REQUIRED — no transport adapter exists |
| `TM-DOS-006` | Nested script execution resets the wall-time budget or exhausts the native stack | Shared activation deadline plus an independent hard nested-exec depth | MITIGATED for Svit Lisp nested `exec` |
| `TM-DOS-007` | Importing a hostile folder or SQL result exhausts host resources before process validation | Incremental entry/text bounds and an outer host deadline for SQL execution | PARTIAL — materialized data is bounded; SQL execution has no mount-owned deadline |
| `TM-DOS-008` | A model-authored built-in request monopolizes CPU, memory, or output | Common JSON input/output caps; linear-time bounded regex search; jq filter/result caps with recursive and generator constructs rejected | PARTIAL — built-in and extension output rejection evidence passes; extension CPU work and jaq still require native-process outer containment |
| `TM-DOS-010` | A hostile or huge mount source exhausts host resources through listing, leaf reads, or a subtree walk | Per-listing entry cap, per-leaf text cap enforced before materialization, an independent node budget for mount search, and a bounded console listing | PARTIAL — listing, leaf, and search bounds have executable evidence; aggregate per-activation mount I/O and wall-time budgets remain |
| `TM-DOS-011` | Build-time compilation of a hostile script hangs or exhausts the Rust build process through macro or constant evaluation | Compile in a fresh restricted Ketos interpreter with the standard Svit execution, stack, namespace, memory, integer, and syntax limits | PARTIAL — focused macro-expansion deadline evidence passes; native-process outer containment remains required |
| `TM-DOS-009` | A hostile event, replay tail, fork chain, or query exhausts storage-process resources | Event byte cap, replay/fork/query count limits, and bounded query text | PARTIAL — focused event-query limit evidence passes; aggregate database size, snapshot bytes, and wall-time budgets remain |
| `TM-ESC-001` | Guest reaches filesystem, network, environment, modules, FFI, or host processes | Fresh VM, null I/O, null module loader, typed host functions, and denial tests | MITIGATED for the Svit Lisp API |
| `TM-ESC-002` | Malformed guest value exploits interpreter/Rust conversion | Immutable typed containers, total conversion over supported types, checked sizes, and fuzzing | PARTIAL — unsupported-function and boundary tests pass; fuzzing remains |
| `TM-ESC-003` | A model-authored built-in request reaches the host filesystem, executable search path, or inherited environment | Built-ins accept only committed process values or explicit JSON and expose no shell, filesystem, process, or environment interface | PARTIAL — focused tests pass; the surrounding agent runtime remains native in-process code |
| `TM-ESC-004` | Build-time script compilation evaluates a guest macro or constant with ambient host I/O or module authority | Fresh compiler interpreter with null I/O and a null module loader | MITIGATED for `svit_script!` compilation |
| `TM-INF-001` | Diagnostics reveal host paths, backtraces, pointers, or dependency internals | Domain errors, source-level sanitization, and diagnostic byte cap | PARTIAL — focused test passes; broader fuzzing remains |
| `TM-ISO-001` | State or globals leak between processes or activations | Fresh VM and process-owned committed root; cross-process invariant tests | MITIGATED for the in-memory process API |
| `TM-EFF-001` | Failed activation or host write/remove leaves process state partially committed | One validation and commit point per transition; rollback tests for every failure class | PARTIAL — activation and host mutation rollback cases pass; interpreter-panic containment remains required |
| `TM-EFF-002` | Concurrent clients overwrite state derived from the same process version | Mandatory version CAS at the process serialization point | MITIGATED for the in-memory controller |
| `TM-EFF-003` | Retrying after a lost response commits an activation twice | Scoped request id, bounded terminal receipts, and version CAS after eviction | MITIGATED for the in-memory controller; durable result replay is not implemented |
| `TM-EFF-004` | Two hosts concurrently commit the same logical process version | Durable ownership lease and fencing token checked by storage | REQUIRED — the reference controller is single-host only |
| `TM-EFF-006` | A mount write applies while the activation that requested it rolls back | Mount writes and removals are buffered during the activation and applied at the commit point after every in-process validation; a failure applies none of them | PARTIAL — ordering and rollback evidence passes; a crash between the applied mount write and the root swap is L-042, and external sources cannot join the transaction |
| `TM-EFF-005` | Retried or failed HTTP and nested model tools duplicate non-transactional external effects | Explicit host grants, documented transaction boundary, bounded calls, and host idempotency/reconciliation policy | PARTIAL — explicit model selection and HTTP grant tests pass; durable effect receipts are not implemented |
| `TM-MSG-001` | Guest forges sender identity or nondeterministic message IDs | Host-derived sender and deterministic IDs; delivery remains out of scope | MITIGATED for buffered intents |
| `TM-MSG-002` | Failed or concurrent inbox acknowledgement drops or reorders input | Host-only enqueue plus exact-head acknowledgement after a successful turn, with complete-root validation before commit | MITIGATED for the in-memory process API |
| `TM-SNAP-001` | Malformed snapshot bypasses state invariants | Versioned decoder, complete revalidation, size cap, and fuzzing | PARTIAL — format, hash, limit, trailing-data, and size controls exist; fuzzing remains |
| `TM-SNAP-002` | Snapshot hash is mistaken for authenticity | Document hash as integrity only; future authenticity requires host signatures | OPEN |
| `TM-FORK-001` | Child writes mutate parent or sibling | Isolated committed roots, empty child outbox, and fork tests | MITIGATED for the in-memory process API |
| `TM-FORK-002` | `spawn` amplifies parent authority or aliases parent state | Fork committed state through `Process::fork`, supply child model authority separately, reserve child addresses, and retain isolated child snapshots | PARTIAL — focused isolation and lineage evidence passes; durable supervision and capability attenuation are not implemented |
| `TM-FORK-003` | A history cut removes an event boundary still needed to restore a fork | Transactional fork-reference index; refuse parent cuts until each child is detached by its own snapshot cut | MITIGATED for the local Turso process store |
| `TM-CAP-001` | A string or reflected value forges authority | Mounts expose persistent values and host-attached typed providers only; a guest-visible name selects a mount but never constructs one, and access is checked against the committed descriptor | MITIGATED for the virtual mount API |
| `TM-CAP-002` | An untrusted model uses generic mutation or an unintended script beyond its domain workflow | Host-selected tool attenuation and script allowlisting before process activation | MITIGATED for the Everruns read/exec capability mode |
| `TM-CAP-003` | A folder mount escapes its configured root through a link or special file | Host-only root selection, per-segment link and special-file rejection on every resolution, bounded UTF-8 reads, and no guest-visible handles | PARTIAL — encountered links are rejected on read, listing, and write; concurrent source-tree replacement is caller-controlled |
| `TM-CAP-004` | A model-supplied URL widens the network authority selected by the host | Host-selected unrestricted or allowlisted HTTP policy, URL validation, redirect refusal in the standard transport, response caps, and policy integration tests | PARTIAL — unrestricted and attenuated policy plus standard reqwest redirect and streamed-response evidence pass; unrestricted research hosts accept SSRF exposure by design, while custom transport DNS and redirect enforcement remains a host responsibility |
| `TM-CAP-005` | A forged or stale `/bin` catalog is mistaken for current host authority | Host-only projection from attached built-ins, refresh on every build/resume, descriptive-only semantics, and exact-catalog tests | MITIGATED for the in-memory agent runtime |
| `TM-CAP-007` | A guest path escapes its mount root through traversal, separators, or depth | Mount paths are parsed into validated segments that reject `.`, `..`, empty, oversized, separator, and NUL content before any provider observes the request | MITIGATED for the mount path parser |
| `TM-CAP-008` | Restoring a snapshot silently restores live external authority | Providers are runtime state and are never serialized; a restored mount reads its descriptor with `attached: false` and fails closed until the host reattaches it | MITIGATED for the in-memory process API |
| `TM-CAP-006` | A host built-in extension receives process mutation or ambient capability merely by being dispatched | Explicit host-only registration and a `BuiltinContext` limited to committed reads and discovery; extra authority must be deliberately captured by trusted host code | MITIGATED for the Svit-supplied context; extension implementations remain trusted native code |
| `TM-AUTH-001` | Client-controlled identifiers are mistaken for authenticated identity or a tenant boundary | API and docs distinguish identifiers from identity; a future transport authenticates and authorizes before tenant-scoped receipt lookup | REQUIRED |
| `TM-INT-001` | Panic crosses the activation boundary or poisons committed state | Panic containment outside guest transaction and unchanged-state tests | REQUIRED |
| `TM-AUD-001` | Guest code or model tools rewrite durable thread history or diverge its message projection from canonical events | Host-managed `/thread` mutation boundary, process-backed `EventLog`, event-derived messages, and session/ID/sequence/projection validation on resume | MITIGATED for the in-memory process API |
| `TM-PERS-001` | Corrupted, reordered, deleted, or spliced persisted events reconstruct attacker-selected state | Content-addressed blobs, base-bound hash chain, stable positions, typed reducer, complete root validation, and resulting-root checks | PARTIAL — corruption and resume/reducer evidence passes; authenticated storage and systematic fault injection remain |
| `TM-PERS-002` | A failed or concurrent durable append publishes a partial event, projection, head, or in-memory state | One immediate Turso transaction plus address-head compare-and-swap; publish the prepared process only after commit | MITIGATED for the local Turso process store |
| `TM-SUP-001` | Vulnerable interpreter or dependency compromises the boundary | Lockfile, pinned toolchain, audit/deny/vet gates, and defense-in-depth isolation plan | REQUIRED |

## Security claims for the initial slice

The slice may claim a specific property only when its implementation and test
are present. It must not claim formal proof, production-grade multi-tenancy,
authenticated messaging, exactly-once delivery, secret isolation under native
memory compromise, or safe migration between mutually untrusted hosts.

## Caller responsibilities

Until a production supervisor exists, callers must enforce outer wall-clock
timeouts, decide whether native in-process execution is acceptable, protect
snapshot storage, and avoid treating process addresses as authenticated
principals. Callers must not execute external actions directly from buffered
message intents without their own authorization and idempotency policy.
