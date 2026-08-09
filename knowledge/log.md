# Svit Knowledge Update Log

## 2026-08-09

* **Everruns loop engine**: Replaced Agentyk with Everruns 0.17.25 behind the
  process-owned `svit::Svit` API. Svit supplies the Everruns runtime with a
  process-backed event bus, message store, and typed process capability.
* **Durable projection**: Advanced agent state to `svit-agent@3`. Canonical
  Everruns events and their exact derived message projection commit under
  `/agent` and are revalidated on resume.
* **Provider surface**: Added `AgentModel` for the deterministic Everruns
  simulator, Everruns' OpenAI Responses driver, and host-provided Everruns
  driver registries. Removed the `svit-agentyk` adapter and Lampa's custom
  Responses driver.
* **Dependency boundary**: Recorded `L-039` for Everruns' unconditional,
  unused fetch, Bash, and A2A dependency graph. Audit and license exceptions
  are exact and temporary; Svit registers none of those capabilities.
* **Consumer migration**: Ported Lampa, both support-agent examples, native
  model/spawn executables, integration tests, and runnable examples to the
  process-owned Everruns path.

## 2026-08-07

* **Native executables**: Added `/bin/search` and `/bin/jq` over
  committed process text and explicit JSON, with no shell or ambient host
  interface.
* **Executable discovery**: Added host-managed `/bin` manuals derived from
  installed native implementations, including schemas, output contracts,
  effect classes, and limits. Resume refreshes the catalog from current host grants.
* **Explicit effects**: Added default-deny, host-allowlisted HTTP plus optional
  host-routed transport and a fixed host-selected `llm` tool. Both remain
  outside Svit activation transactions and replay guarantees.
* **Child execution**: Named process creation `spawn` rather than overloading
  transactional script `exec`; it forks committed state, runs one child turn,
  rejects duplicate local addresses, and exposes child snapshots to the host.
* **Tool security**: Added `TM-DOS-008`, `TM-ESC-003`, `TM-EFF-005`,
  `TM-FORK-002`, and `TM-CAP-004` with focused agent-loop evidence for limits,
  host isolation, effect grants, fork lineage, and network policy.
* **Tool limitations**: Recorded the bounded jq subset, non-transactional
  HTTP/model effects, and the non-durable local child registry as `L-035`
  through `L-037`.
* **Dependency review**: Added direct jaq, regex, and URL dependencies for the
  native implementations; no shell runtime is included.
* **Compatibility**: Advanced process snapshots to format 5 for the durable
  `/bin` executable catalog; agent state remains `svit-agent@2`.

* **Lampa**: Added persistent inbox/outbox chat, complete committed process
  memory, and JSON item-preview panels with headless UI evidence.
* **Agent ownership**: Added `svit::Svit` as the process-owning reason/act
  API, with Agentyk as its internal loop engine rather than an external agent
  that consumes Svit as a capability.
* **Process lifetime**: Added `Svit::start`, cloneable `Inbox` handles,
  completed-turn outbox listeners, and blocking drain/join. There is no
  separate entrypoint message.
* **Durable inbox**: Host sends commit to `/inbox` before waking the loop;
  successful turns acknowledge the exact observed head and failures retain it.
* **Message envelope**: Inbox and live outbox use Agentyk `Message` values with
  ordered `ContentPart` values rather than plain input and `TurnResult` output.
* **Runtime projection**: `/agent` now exposes the configured system prompt,
  event-derived message history, and canonical Agentyk events through the
  ordinary read-only runtime surface. Agent state format advanced to
  `svit-agent@2`.
* **Durable thread**: Added host-managed, guest-readable `/agent` state so
  snapshots, restores, and forks carry the committed conversation event log.
* **Subagents**: Defined a subagent as a Svit agent built around a forked child
  process; child turns inherit committed history and isolate future mutation.
* **Consumer example**: Added credentialed `support-agent-v2`, using
  `gpt-5.6-terra` through one process-owned Svit inbox and outbox. Deterministic
  lifecycle, snapshot, and fork evidence remains in the test suite.
* **Audit boundary**: Added `TM-AUD-001` and executable evidence preventing
  guest scripts and model tools from rewriting durable replay state.
* **Compatibility**: Bumped snapshots to format 4 for the `/agent` root node.
* **Limitations**: Recorded the one-thread-per-process constraint and the
  non-atomic boundary between agent event commits and external model/tool calls.

## 2026-08-06

* **Agent authority**: Added an attenuated Agentyk capability mode that exposes discovery, reads, and only host-allowlisted scripts; generic mutation remains available only through the explicit full-access constructor.
* **Support commit contract**: Bound support retrieval and commit to a host-issued request ID, derived source and ticket data from process state, rejected duplicate or policy-invalid commits atomically, and made the validated committed answer authoritative over model final text.
* **Security evidence**: Added `TM-CAP-002` with focused adapter and deterministic simulated-agent tests for tool attenuation, script denial, request-binding rollback, idempotency, provenance, deterministic ticket policy, and committed response rendering.
* **Snapshot mounts**: Added bounded, read-only construction-time imports for
  real UTF-8 folders and host-selected Turso query rows under `/mounts`.
* **Authority boundary**: Mounts persist values, kind, and mode but never host
  paths, database connections, query capability, or other live authority.
  Folder imports reject symbolic links and special files.
* **Evidence**: Added focused link-rejection and read-only rollback tests plus
  an executable example that reads both mount kinds and verifies deterministic
  results.
* **Consumer example**: Moved support documents from embedded process memory to
  a real folder snapshot and added Turso-backed account context; the support
  search script consumes both mounts before the model commits its response.

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
