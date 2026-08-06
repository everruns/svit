---
type: Protocol
title: Svit Control Protocol 1
description: Versioned multi-client process control, concurrency, retry, and transaction semantics.
tags:
  - svit
  - protocol
  - transaction
  - concurrency
  - vast
---

# Svit Control Protocol 1

## Status

Implemented as a transport-neutral Rust protocol and an in-memory reference
controller. Network transport, authenticated principals, durable receipts, and
distributed process placement remain under implementation. The compatibility
and release contract is defined in [Protocol Maintenance](maintenance.md).

## Scope

Svit Control Protocol 1 lets multiple clients submit activations to one process
without lost updates or duplicate commits. It specifies envelopes, ordering,
optimistic concurrency, retry behavior, and the process transaction boundary.
It does not select HTTP, gRPC, WebSocket, or another transport.

The only mutating command in version 1 is `activate`. A named activation is
already the complete Svit transaction: memory, staged scripts, and outbox
intents commit together, or none commit.

## VAST semantics

Svit calls this model **Versioned Atomic State Transitions (VAST)**. Svit
Control Protocol 1 implements VAST for one process at one valid serialization
point. VAST names the protocol's concurrency and commit semantics; it is not a
second wire protocol, storage format, merge algorithm, external standard, or
distributed-consensus claim.

For a process whose current committed state is version `N` with root `R(N)`, a
new, correctly addressed activation request that passes command validation has
exactly one outcome:

```text
expected_version != N  -> conflict; keep N and R(N); do not execute
expected_version == N  -> committed; publish N+1 and R(N+1)
                       -> rejected; keep N and R(N)
```

The VAST invariants are:

1. **Versioned**: every attempted transition names its expected committed
   version.
2. **Atomic**: success publishes memory, staged scripts, and outbox intents as
   one next root and increments the version exactly once.
3. **State-preserving failure**: conflict, validation failure, cancellation,
   resource exhaustion, syntax failure, and runtime failure leave the committed
   version and root unchanged.
4. **Single next transition**: at most one successful activation can advance a
   given version at the serialization point. Other contenders conflict and must
   observe and recompute; Svit does not merge their transitions.

Deterministic replay, bounded idempotency receipts, snapshot integrity, and
durable storage complement VAST but are separate guarantees. A controller must
still have exclusive, fenced ownership before claiming VAST semantics across
multiple hosts.

Executable evidence lives in the
[control protocol tests](../../crates/svit/tests/control_protocol.rs) for
concurrent commits, conflicts, and retry safety, and the
[process invariant tests](../../crates/svit/tests/process_invariants.rs) for
atomic rollback across activation failure classes.

## Request

Every request contains:

```text
protocol          "svit-control@1" wire major
client_id         client-chosen idempotency namespace; never a principal
request_id        idempotency key scoped to client_id; not transport correlation
process_id        exact logical process address
expected_version  mandatory compare-and-swap precondition
command           activate { script, input }
```

`client_id` and `request_id` contain 1–128 ASCII alphanumeric, `-`, `_`, or `.`
characters. A client must never reuse `(client_id, request_id)` for different
request content.

The protocol string identifies a wire major, independent of crate and schema
artifact versions. Within major 1, decoders ignore unknown fields on known
structures so optional fields can be added compatibly. Unknown operation tags
fail closed. A future transport may add a request correlation identifier for
multiplexing, but it must preserve the semantic idempotency key across retries
and reconnects.

The typed protocol begins after envelope decoding. A transport adapter must
enforce a request byte limit before deserialization. The controller validates
decoded script names and values against process limits before execution or
receipt retention.

## Response

A response copies the protocol, client, request, and process identifiers. Its
`replayed` field says whether the in-memory controller returned a stored
receipt. The outcome is exactly one of:

| Outcome | Meaning | Process state |
| --- | --- | --- |
| `committed` | Activation committed at the serialization point | Version increments exactly once |
| `conflict` | `expected_version` was stale | Unchanged |
| `rejected` | Address, input, script, hook, runtime, or limit validation failed | Unchanged |

A conflict includes the expected version, actual version, and current root
hash. A rejection includes the unchanged version, root hash, stable error code,
and capped diagnostic.

## Concurrency and isolation level

VAST provides **linearizable, optimistic transactions per process** when every
request for that process passes through one valid serialization point:

1. The controller orders requests by entry to its process lock.
2. It compares `expected_version` while holding that lock.
3. A matching request executes one activation against that committed version.
4. The activation either commits one next version or leaves the process
   unchanged.
5. The terminal receipt is recorded before the lock is released.

Two successful transactions cannot both commit from the same process version.
If two clients submit version `N`, at most one can commit `N + 1`; the other
observes a conflict unless the first activation rejects without committing.

This is not a multi-command interactive transaction. Clients do not hold locks
between requests. A client reads a version and root hash, prepares an activation,
then submits it with that version as its precondition.

There must be exactly one active serialization point for a logical process.
Two controllers restored from the same snapshot can each commit their own
version `N + 1`; the reference controller is not a distributed lease. A
multi-host implementation requires ownership leases and fencing before it may
claim the same guarantee across hosts.

## Retry contract

Within receipt retention, an exact retry of `(client_id, request_id)` returns
the original terminal outcome with `replayed = true`. Reusing the key with
different content is rejected.

The reference controller retains a bounded number of receipts. After eviction,
the original version precondition still prevents an already committed request
from committing again: retrying it produces a conflict. The client may lose the
original result but cannot duplicate the state transition.

A durable host must atomically persist the new process root, version, outbox,
and request receipt. If that atomic storage transaction is not implemented, the
host may claim compare-and-swap safety but not durable result replay across a
crash.

Receipt lookup is part of the authorized operation. A remote host must
authenticate and authorize the caller before checking a receipt, then partition
receipts by its trusted tenant boundary. The client-chosen `client_id` is not a
safe tenant partition.

## Client algorithm

```text
observe process -> version N, root hash H
build request with a new request_id and expected_version N
submit request

committed -> accept version N+1 and result
conflict  -> observe current state, recompute intent, use a new request_id
rejected  -> fix the request; retry the identical request_id only to recover
             a response believed to be lost
```

Clients must not blindly change `expected_version` after conflict. The script
input may have been derived from stale state and must be reconsidered.

## Identity and authorization

Control identifiers are correlation data. They grant no authority and are
fully client-controlled. A transport adapter must authenticate a principal,
authorize the command for the addressed process, and pass that identity through
trusted host context rather than accepting it from the envelope.

Svit Control Protocol 1 has no authenticated transport adapter. Until one is
implemented, the in-memory controller is suitable only inside a caller's
existing trust boundary.

## Evolution and conformance

The current major has one base operation, `activate`, and therefore no optional
capabilities. Before a remote transport is stable, it must add an explicit
initialization exchange for major selection, capabilities, implementation
information, and transport authentication requirements. Omitted capabilities
mean unsupported; capabilities describe behavior and never grant authority.

Canonical Rust wire types, exact wire tests, and additive-field compatibility
tests exist. Generated JSON Schema, method/capability/error metadata, stable and
unstable artifact separation, SDK generation, and oldest/newest cross-version
tests are required before remote stabilization. The maintenance playbook owns
the complete gate.

## External systems and mounts

The atomic boundary ends at the committed process root and outbox. It cannot
include an external filesystem, HTTP service, model provider, database, or
another Svit process in the same transaction.

- A read-only projection result must become a bounded, recorded activation
  input so replay does not silently reread different external state.
- A writable projection creates an effect intent. The intent commits with
  process state and is dispatched afterward with an idempotency key.
- A failure before commit produces no effect intent.
- A failure after external dispatch requires retry, reconciliation, or a
  compensating action; Svit cannot roll the external system back.

Cross-process atomic commits, two-phase commit, and exactly-once external
effects are not protocol guarantees.

## Reference implementation boundary

`ProcessController` provides the serialization point, compare-and-swap check,
bounded receipts, observation, and snapshot access for one in-memory `Process`.
It does not provide discovery, routing, leases, persistence, authentication,
authorization, fairness, cancellation, a bounded wire decoder, or a network
listener.
