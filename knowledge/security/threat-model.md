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

Threat actors are malicious or buggy guest scripts, inputs, snapshots,
messages, and callers attempting cross-process access.

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
| `TM-ESC-001` | Guest reaches filesystem, network, environment, modules, FFI, or host processes | Fresh VM, null I/O, null module loader, typed host functions, and denial tests | MITIGATED for the Svit Lisp API |
| `TM-ESC-002` | Malformed guest value exploits interpreter/Rust conversion | Immutable typed containers, total conversion over supported types, checked sizes, and fuzzing | PARTIAL — unsupported-function and boundary tests pass; fuzzing remains |
| `TM-INF-001` | Diagnostics reveal host paths, backtraces, pointers, or dependency internals | Domain errors, source-level sanitization, and diagnostic byte cap | PARTIAL — focused test passes; broader fuzzing remains |
| `TM-ISO-001` | State or globals leak between processes or activations | Fresh VM and process-owned committed root; cross-process invariant tests | MITIGATED for the in-memory process API |
| `TM-EFF-001` | Failed activation or host write/remove leaves process state partially committed | One validation and commit point per transition; rollback tests for every failure class | PARTIAL — activation and host mutation rollback cases pass; interpreter-panic containment remains required |
| `TM-EFF-002` | Concurrent clients overwrite state derived from the same process version | Mandatory version CAS at the process serialization point | MITIGATED for the in-memory controller |
| `TM-EFF-003` | Retrying after a lost response commits an activation twice | Scoped request id, bounded terminal receipts, and version CAS after eviction | MITIGATED for the in-memory controller; durable result replay is not implemented |
| `TM-EFF-004` | Two hosts concurrently commit the same logical process version | Durable ownership lease and fencing token checked by storage | REQUIRED — the reference controller is single-host only |
| `TM-MSG-001` | Guest forges sender identity or nondeterministic message IDs | Host-derived sender and deterministic IDs; delivery remains out of scope | MITIGATED for buffered intents |
| `TM-SNAP-001` | Malformed snapshot bypasses state invariants | Versioned decoder, complete revalidation, size cap, and fuzzing | PARTIAL — format, hash, limit, trailing-data, and size controls exist; fuzzing remains |
| `TM-SNAP-002` | Snapshot hash is mistaken for authenticity | Document hash as integrity only; future authenticity requires host signatures | OPEN |
| `TM-FORK-001` | Child writes mutate parent or sibling | Isolated committed roots, empty child outbox, and fork tests | MITIGATED for the in-memory process API |
| `TM-CAP-001` | A string or reflected value forges authority | No external capabilities in the slice; future references use unforgeable host handles | NOT APPLICABLE to the current slice |
| `TM-CAP-002` | An untrusted model uses generic mutation or an unintended script beyond its domain workflow | Host-selected tool attenuation and script allowlisting before process activation | MITIGATED for the Agentyk read/exec capability mode |
| `TM-AUTH-001` | Client-controlled identifiers are mistaken for authenticated identity or a tenant boundary | API and docs distinguish identifiers from identity; a future transport authenticates and authorizes before tenant-scoped receipt lookup | REQUIRED |
| `TM-INT-001` | Panic crosses the activation boundary or poisons committed state | Panic containment outside guest transaction and unchanged-state tests | REQUIRED |
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
