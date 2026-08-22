---
type: Test Strategy
title: Testing Strategy
description: Test organization and executable-example requirements for the first vertical slice.
tags:
  - svit
  - testing
  - examples
---

# Testing Strategy

## Status

Applies to the initial vertical slice as it is implemented.

## Layers

1. Unit tests beside implementation for value, limits, script, snapshot, and
   address behavior.
2. Integration tests under `crates/svit/tests/` for complete process
   transitions and rollback.
3. Rust examples under `crates/svit/examples/` for library API contracts.
4. Consumer scenarios under `examples/` for complete model-driven workflows.
5. Property and fuzz tests for serialization and isolation boundaries (not yet implemented).
6. Documentation tests for public Rust APIs.
7. Headless Tuika rendering and input-state tests for terminal interfaces.
8. Cargo feature-matrix checks for the adapter-neutral core, Turso persistence,
   and Turso query mounts independently.

[`Reasoning Scenarios`](scenarios.md) is the small, named acceptance suite for
model-driven use of the complete Svit surface. Each scenario is deterministic:
it uses a scripted Everruns reasoner and host fixtures, while preserving the
same `discover`, `write`, `exec`, port, and `/memory` interactions a live model
uses. A scenario is not satisfied by a host-specific helper or by preloading
the expected durable result.

Package-relative `.svit-script` files should enter Rust through
`svit_script!(file ...)`, which catches Lisp compiler errors during
`cargo check`. `svit_script_test!` supplies a fresh real process with the
subject installed at `/lib/subject`; its assertion body must execute the
activation and verify the relevant output, committed state, or rollback.
Compile-time checking alone is not an execution test.

## Examples as acceptance tests

The following scenarios execute with assertions and deterministic output under
`crates/svit/examples/`:

- durable counter across activations and snapshot restore;
- a script that saves another named script and discovers it later;
- atomic memory and outbox rollback after a deliberate error;
- parent and two forks diverging without shared mutation;
- denied modules and bounded infinite-loop termination;
- two control clients contending on one version, then resolving the conflict.
- a real folder, a materialized Turso query, and a writable folder mounted
  together, read lazily with node facts, and written back under an explicit
  grant while a read-only mount refuses the same write.
- a compacted prefix of canonical history reclaimed by an explicit retention
  cut, refused before its compaction checkpoint exists, with the retained tail
  readable, its sequences never reused, and the boundary preserved by resume;
- a process-only fork starting a new session, plus a durable fork continuing
  an immutable inherited event prefix without copying it into the process root.
- a started Svit drains its durable inbox, emits completed turns, and leaves a
  failed turn's input queued.
- Svit inherits Everruns' default iteration policy, while an explicit
  iteration cap that stops on a tool call fails visibly, retains the inbox
  message, and publishes no completed outbox message.
- Svit supplies a base prompt without host instructions, wraps optional
  instructions in an `<instructions>` block, persists them across restore, and
  recomposes a forked prompt for the child address.
- the agent reads its projected instructions and composed system prompt from
  bounded `/thread` metadata, while Everruns reconstructs message history and
  canonical events from the paged EventLog during a turn.
- Svit Lisp `(search path pattern)` reads the transactional process tree and
  `(jq filter value)` filters explicit JSON; focused tests cover
  data limits, unrestricted standard HTTP,
  allowlist-denied and host-routed HTTP, nested model selection, and local
  child spawn.
- the owned system prompt documents path-first `/ports` and `/lib` composition.
  The model-catalog scenario writes `/lib/summarize-model-catalog`, invokes it
  through `exec`, obtains a catalog through the generic HTTP port, reduces it
  with Svit Lisp `jq`, and commits only its count and newest GPT records under
  `/memory/model_catalog`; the fixture is intentionally larger than the
  persistent value envelope.
  A persisted model turn writes a reusable `/lib` script, invokes its
  allowlisted `/ports/http` dependency exactly once across guest replay, consumes
  the response, commits once, and drains the inbox. A later guest failure does
  not repeat HTTP and rolls back all guest state. Bare `Process::exec` reports
  that `/ports` execution requires a Svit host, while reversed arguments receive
  an actionable diagnostic.
- `/ports` discovery exposes installed port manuals, while
  resume removes catalog entries for absent host grants.
- a host `PortExtension` contributes a discoverable port that can
  read committed state through the restricted context; the common dispatcher
  rejects oversized extension output.
- Lampa projects `http`, `llm`, and `spawn` under `/ports` after explicitly
  registering unrestricted HTTP and the selected reasoner for both model-backed
  ports; Svit's reusable reqwest transport rejects redirect escape, while large
  in-memory responses must be reduced before crossing a persistent or
  model-visible value boundary.
- compile-fail evidence keeps the standard bundle unavailable, and the explicit
  registry test exposes only the individually registered ports; commit events
  are notifications, and an atomic
  Svit contract read returns an owned value/version pair after inbox and
  completed-turn transitions. Multiple `Events` observers independently see
  the same notifications, and empty `Events` and `Outbox` observations return
  the contract's typed observer error rather than a channel-specific error.
- model-driven writes appear in the same `Events` stream as host and inbox
  transitions, and batched commit notifications preserve Lampa's original
  selected path and expanded branches until the tree resolves again.
- Lampa's render loop depends only on the Svit contract, events, inbox, and
  outbox after construction; it reads after commit notifications rather than
  polling or assembling port state.
- Lampa receives projected canonical messages into its timeline, renders
  intermediate model commentary and tool calls, and deduplicates repeated
  projection reads and optimistically displayed inbox messages by message ID;
  outbox observation changes completion status without duplicating the final
  answer.
- Lampa collapses a completed tool call and result into one bounded row with a
  success or failure marker, operation, target, and result summary. Port
  `exec` calls use the port name, errors remain visible, and internal tool
  call IDs do not appear in the transcript.
- a persisted Svit commits model-driven memory, append-only canonical reasoning
  events, derived messages, port catalog refresh, inbox enqueue, and
  exact-head acknowledgement through one Turso owner; reopening by address
  reconstructs the same memory and conversation without rerunning the model or
  appending a transaction when the host grants and prompt are unchanged.
- Turso publishes a recovery checkpoint atomically every 32 transactions,
  retains only the newest checkpoint per process, resumes through it plus the
  newer tail, and fails closed when either an uncovered event or the checkpoint
  bytes are corrupted. Its blob is a direct process snapshot rather than a
  nested public snapshot envelope.
- reasoning startup reads its compact Everruns checkpoint and required paged
  EventLog suffix without decoding history from `/thread`; canonical appends
  are observable through `SvitEvent::CanonicalEvent`.
- a process transaction cut reclaims the envelopes it orphans and strands
  nothing: no stored blob survives without a base, transaction, thread event,
  checkpoint, or snapshot referencing it, and the cut boundary still resumes to
  the same version and root hash;
- a thread-history retention cut reclaims only a prefix a compaction checkpoint
  already replaced, refuses a boundary outside the committed range, refuses a
  prefix a fork inherits, is idempotent when repeated, survives resume, and
  leaves reads and sequence allocation consistent across the reclaimed
  boundary; volatile and durable owners are covered by the same assertions.
- cumulative thread history does not consume one guest value's entry budget;
  every event and message remains independently validated, while the host
  collections fail closed at their separate record and encoded-byte envelope.
- Lampa maps one validated instance ID to
  `svit://local/lampa/{instance-id}` and a distinct
  `instances/{instance-id}/svit.db`; reopening resumes that address, a database
  containing a different root fails closed, and explicit legacy import
  preserves current state/version/hash while rejecting an existing target.
- Lampa renders a bounded, read-only `/thread/events` and `/thread/messages`
  history overlay from the EventLog; those rows remain absent from the
  committed process tree and snapshots.
- adapter-neutral process import starts an empty retained-history tail at the
  imported version; its first subsequent transaction begins at position zero
  and advances from that version.
- Lampa array rows show bounded scalar previews, prefer conventional object
  identity fields, fall back to the first scalar field, and summarize other
  containers by kind and item count.
- Lampa resolves every node through the same `discover`/`stat`/`read`
  interface, keeps unrelated nodes resolved across a commit that did not name
  them, and re-reads an externally changed mount only on an explicit reload.
- Lampa opens only the memory-tree root. Tuika's stable-path tree state
  preserves the selected node, ancestor fallback, expanded branches, and the
  visible window across refreshes; clicks use the exact resolved window.
- Lampa panel focus cycles forward with plain `Tab` and backward with
  `Shift+Tab`.
- Lampa consumes a plain transcript drag, paints its selected cells, and
  extracts their text for deferred clipboard copy without changing memory-tree
  selection; `Ctrl+C` re-copies an active range instead of quitting.
- Lampa assistant messages consume Markdown emphasis delimiters and style bare
  HTTP(S) URLs as links before the hyperlink backend emits terminal targets.

`just examples` and CI run examples, not only `cargo check --examples`.
Examples requiring an API key or external service must be separated from the
deterministic core suite and must never receive secrets on
pull-request-controlled code.

The support-agent-process consumer scenario uses Everruns' simulated driver to
prove that the committed answer is authoritative, request mismatches roll back,
ticket policy is derived from retrieved state, and retries cannot duplicate a
ticket intent. Its optional live-model executable remains outside the
deterministic suite.

Agent-loop integration tests snapshot and restore a conversation, continue a
fork in a child process, and assert parent isolation.
The credentialed `support-agent-svit` consumer exercises one process-owned Svit
with `gpt-5.6-terra`. It remains outside the deterministic suite; lifecycle,
snapshot, and fork behavior stays covered by integration tests.

## Transaction matrix

Every staged resource is checked against every terminal result:

| Result | Memory | Scripts | Outbox | Version |
| --- | --- | --- | --- | --- |
| Success | Commit | Commit | Commit | Increment once |
| Syntax error | Unchanged | Unchanged | Unchanged | Unchanged |
| Runtime error | Unchanged | Unchanged | Unchanged | Unchanged |
| Invalid value | Unchanged | Unchanged | Unchanged | Unchanged |
| Invalid staged script | Unchanged | Unchanged | Unchanged | Unchanged |
| Any limit exceeded | Unchanged | Unchanged | Unchanged | Unchanged |
| Stale control version | Unchanged | Unchanged | Unchanged | Unchanged |

Multi-client tests run simultaneous requests against one controller and assert
that only one commits the contested version. Retry tests cover exact receipt
replay, request-id content mismatch, rejected requests, and receipt eviction.
Wire tests pin exact request shapes, prove that known structures ignore additive
fields within a major, and prove that unknown operations fail closed.
Execution-deadline tests assert rollback but do not claim deterministic failure
timing across hosts.

## Determinism checks

Restore the same snapshot twice, run the same named script with the same input
and limits, and compare output, committed encoding, root hash, logs, and message
identifiers. If an interpreter detail prevents a stable value, keep it outside
guest-observable state and document it.

## Repository tooling

`just check-features` runs Clippy with no default features, with only
`persistence-turso`, and with only `turso-mount`. This proves the default Turso
adapter is removable and neither Turso integration accidentally requires the
other.

Python repository validators use `unittest` so they run without extra
dependencies:

```console
python3 -m unittest discover -s scripts/tests -p 'test_*.py'
```
