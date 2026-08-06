# Svit Knowledge Update Log

## 2026-08-05

* **Runtime replacement**: Replaced Luau through `mlua` with the pure-Rust Ketos interpreter and defined the versioned [Svit Lisp Runtime](runtimes/lisp-runtime.md).
* **Guest contract**: Separated lexical variables from durable memory through explicit `memory-get`, `memory-set!`, and `memory-remove!` operations; added immutable typed maps and arrays plus bounded script, log, and message functions.
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
