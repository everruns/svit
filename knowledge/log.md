# Svit Knowledge Update Log

## 2026-08-06

* **Agent authority**: Added an attenuated Agentyk capability mode that exposes discovery, reads, and only host-allowlisted scripts; generic mutation remains available only through the explicit full-access constructor.
* **Support commit contract**: Bound support retrieval and commit to a host-issued request ID, derived source and ticket data from process state, rejected duplicate or policy-invalid commits atomically, and made the validated committed answer authoritative over model final text.
* **Security evidence**: Added `TM-CAP-002` with focused adapter and deterministic simulated-agent tests for tool attenuation, script denial, request-binding rollback, idempotency, provenance, deterministic ticket policy, and committed response rendering.

## 2026-08-05

* **Process namespace**: Adopted the conventional `/memory`, `/lib`, `/tasks`, `/inbox`, `/children`, `/mounts`, and `/system` hierarchy. Deferred top-level nodes are validated as empty and read-only rather than claiming unimplemented behavior.
* **Builder vocabulary**: Replaced the initial `script(name, script)` builder method with `library(name, script)` so process assembly maps directly to `/memory` and `/lib`.
* **Generic operations**: Unified Rust, agent-tool, and Svit Lisp access as `discover`, `read`, `write`, `remove`, and `exec` over absolute process paths. Library entries now use the same typed write and remove boundary as memory instead of separate post-build script APIs.
* **Nested execution**: Svit Lisp 2 adds transactional named-script `exec`; nested calls share working state and the outer deadline, roll back on failure, and are independently depth-bounded by `max_exec_depth`.
* **System discovery**: Added validated runtime metadata for logical identity, generic API operations, limits, fork lineage, language and snapshot format, the empty capability set, and buffered outbox. Logical identity is explicitly marked unauthenticated.
* **Compatibility**: Bumped snapshots to format 3 because the canonical root schema now includes the conventional hierarchy and system metadata; formats 1 and 2 restore fail closed.
* **Language identity**: Adopted `.svit-script` for standalone Svit Lisp source and virtual script-library diagnostics, reserved `.svit` for an unimplemented future manifest format, and kept Ketos as an interpreter implementation detail.
* **Runtime replacement**: Replaced Luau through `mlua` with the pure-Rust Ketos interpreter and defined the versioned [Svit Lisp Runtime](runtimes/lisp-runtime.md).
* **Guest contract**: Separated lexical variables from durable process state through explicit generic operations; added immutable typed maps and arrays plus bounded log and message functions.
* **Security boundary**: Installed null I/O and a module loader that rejects every Ketos module, created a fresh interpreter for every activation, and retained one post-validation commit point.
* **Limits**: Adopted Ketos wall-time, stack, namespace, syntax, integer, and abstract-memory restrictions. Recorded the absence of deterministic instruction fuel and allocator byte caps as limitations rather than preserving Luau-specific names.
* **Dependency review**: Recorded Ketos 0.12's unconditionally declared, obsolete REPL dependency stack as `L-025`; audit exceptions are exact and limited to crates that Svit does not expose.
* **Compatibility**: Bumped snapshots to format 2 because stored scripts and serialized limit semantics changed; format 1 restores fail closed.
* **Evidence**: Migrated unit, integration, protocol, documentation, CLI, and executable-example coverage to Lisp, including rollback, replay, fork isolation, module denial, fresh globals, and every activation buffer limit.

## 2026-07-31

* **VAST semantics**: Named and specified Versioned Atomic State Transitions as the control protocol's per-process concurrency and commit model without changing the `svit-control@1` wire identifier or extending the distributed ownership claim.
* **Protocol maintenance**: Adopted major-version negotiation, capability-gated evolution, additive-field compatibility, canonical schema and metadata artifacts, drift guards, conformance vectors, and trusted tenant partitioning requirements after comparing Mira and ACP. Added executable compatibility and exact wire-shape tests; schema generation and remote initialization remain required before wire stabilization.
* **Control protocol**: Added versioned multi-client activation envelopes, linearizable per-process version checks, bounded retry receipts, explicit conflict outcomes, public protocol documentation, and concurrent-client evidence. Transactions stop at the process root and outbox; external systems remain outside the atomic boundary.
* **Documentation**: Separated the public [Svit vision](../docs/vision.md) from the internal [research proposal](research/proposal.md), preserving detailed hypotheses, alternatives, experiments, and open decisions in the knowledge bundle.
* **Delivery**: Defined `main` as a curated semantic history, made pull requests optional coordination artifacts that squash-merge when used, and prohibited unstable pull-request references in release-facing records.
* **Implementation**: Added the first executable Rust slice with transactional Svit Lua activations, one state root, named self-authored scripts, buffered message intents, typed hooks, snapshots, replay, and isolated forks.
* **Examples**: Added deterministic examples for durable memory, self-authored libraries, atomic rollback, forked research, sandbox denial, and execution limits.
* **Evidence**: Added unit, integration, rollback, replay, fork, snapshot-tamper, heap-limit, diagnostic, and sandbox tests. Threat statuses now distinguish mitigated, partial, required, and not-applicable controls.
* **Creation**: Established the OKF v0.2 bundle and maintenance contract.
* **Scope**: Recorded the initial transactional process vertical slice in [Architecture](foundations/architecture.md), [Process Model](foundations/process-model.md), and the original Lua runtime contract, now superseded by the [Svit Lisp Runtime](runtimes/lisp-runtime.md).
* **Security**: Added the initial [Threat Model](security/threat-model.md) and [Security Testing](security/security-testing.md) requirements without marking unimplemented controls as mitigated.
* **Operations**: Defined the initial [Testing Strategy](operations/testing.md) and [Limitations](operations/limitations.md).
