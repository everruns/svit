# Svit Knowledge Update Log

## 2026-07-31

* **Protocol maintenance**: Adopted major-version negotiation, capability-gated evolution, additive-field compatibility, canonical schema and metadata artifacts, drift guards, conformance vectors, and trusted tenant partitioning requirements after comparing Mira and ACP. Added executable compatibility and exact wire-shape tests; schema generation and remote initialization remain required before wire stabilization.
* **Control protocol**: Added versioned multi-client activation envelopes, linearizable per-process version checks, bounded retry receipts, explicit conflict outcomes, public protocol documentation, and concurrent-client evidence. Transactions stop at the process root and outbox; external systems remain outside the atomic boundary.
* **Documentation**: Separated the public [Svit vision](../docs/vision.md) from the internal [research proposal](research/proposal.md), preserving detailed hypotheses, alternatives, experiments, and open decisions in the knowledge bundle.
* **Delivery**: Defined `main` as a curated semantic history, made pull requests optional coordination artifacts that squash-merge when used, and prohibited unstable pull-request references in release-facing records.
* **Implementation**: Added the first executable Rust slice with transactional Svit Lua activations, one state root, named self-authored scripts, buffered message intents, typed hooks, snapshots, replay, and isolated forks.
* **Examples**: Added deterministic examples for durable memory, self-authored libraries, atomic rollback, forked research, sandbox denial, and execution limits.
* **Evidence**: Added unit, integration, rollback, replay, fork, snapshot-tamper, heap-limit, diagnostic, and sandbox tests. Threat statuses now distinguish mitigated, partial, required, and not-applicable controls.
* **Creation**: Established the OKF v0.2 bundle and maintenance contract.
* **Scope**: Recorded the initial transactional process vertical slice in [Architecture](foundations/architecture.md), [Process Model](foundations/process-model.md), and [Svit Lua Runtime](runtimes/lua-runtime.md).
* **Security**: Added the initial [Threat Model](security/threat-model.md) and [Security Testing](security/security-testing.md) requirements without marking unimplemented controls as mitigated.
* **Operations**: Defined the initial [Testing Strategy](operations/testing.md) and [Limitations](operations/limitations.md).
