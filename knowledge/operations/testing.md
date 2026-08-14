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
4. Consumer scenarios under `examples/` for agent-facing workflows.
5. Property and fuzz tests for serialization and isolation boundaries (not yet implemented).
6. Documentation tests for public Rust APIs.
7. Headless Tuika rendering and input-state tests for terminal interfaces.
8. Cargo feature-matrix checks for the adapter-neutral core, Turso persistence,
   and Turso query mounts independently.

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
- a process-owned agent thread restored and continued in an isolated child process.
- a started Svit drains its durable inbox, emits completed turns, and leaves a
  failed turn's input queued.
- Svit supplies a base prompt without host instructions, wraps optional
  instructions in an `<instructions>` block, persists them across restore, and
  recomposes a forked prompt for the child address.
- the agent reads its projected instructions, composed system prompt, message
  history, and canonical events through the runtime capability during a turn.
- `/bin/search` reads committed process text and `/bin/jq` filters explicit
  JSON; focused tests cover data limits, unrestricted standard HTTP,
  allowlist-denied and host-routed HTTP, nested model selection, and local
  child spawn.
- `/bin` discovery exposes installed built-in manuals, while
  resume removes catalog entries for absent host grants.
- a host `BuiltinExtension` contributes a discoverable built-in that can
  read committed state through the restricted context; the common dispatcher
  rejects oversized extension output.
- Lampa projects `http`, `jq`, `llm`, `search`, and `spawn` under `/bin` by
  selecting the standard registry without additional HTTP policy; Svit's
  reusable reqwest transport rejects redirect escape and oversized streamed
  responses.
- the Svit standard built-in setup derives the full catalog from one instance's
  `Reasoner`; commit events are notifications, and an atomic
  Svit contract read returns an owned value/version pair after inbox and
  completed-turn transitions. Multiple `Events` observers independently see
  the same notifications, and empty `Events` and `Outbox` observations return
  the contract's typed observer error rather than a channel-specific error.
- Lampa's render loop depends only on the Svit contract, events, inbox, and
  outbox after construction; it reads after commit notifications rather than
  polling or assembling built-in state.
- a persisted Svit commits model-driven memory, append-only canonical reasoning
  events, derived messages, built-in catalog refresh, inbox enqueue, and
  exact-head acknowledgement through one Turso owner; reopening by address
  reconstructs the same memory and conversation without rerunning the model or
  appending a transaction when the host grants and prompt are unchanged.
- Lampa maps one validated instance ID to
  `svit://local/lampa/{instance-id}` and a distinct
  `instances/{instance-id}/svit.db`; reopening resumes that address, a database
  containing a different root fails closed, and explicit legacy import
  preserves current state/version/hash while rejecting an existing target.
- adapter-neutral process import starts an empty retained-history tail at the
  imported version; its first subsequent transaction begins at position zero
  and advances from that version.
- Lampa array rows show bounded scalar previews, prefer conventional object
  identity fields, fall back to the first scalar field, and summarize other
  containers by kind and item count.
- Lampa resolves every node through the same `discover`/`stat`/`read`
  interface, keeps unrelated nodes resolved across a commit that did not name
  them, and re-reads an externally changed mount only on an explicit reload.
- Lampa opens only the memory-tree root and preserves the visible window when
  a mouse click selects its bottom row; keyboard navigation scrolls only when
  the selection leaves that window.
- Lampa panel focus cycles forward with plain `Tab` and backward with
  `Shift+Tab`.
- Lampa assistant messages consume Markdown emphasis delimiters and style bare
  HTTP(S) URLs as links before the hyperlink backend emits terminal targets.

`just examples` and CI run examples, not only `cargo check --examples`.
Examples requiring an API key or external service must be separated from the
deterministic core suite and must never receive secrets on
pull-request-controlled code.

The support-agent-process consumer scenario uses Everruns' simulated driver to
prove that model-visible mutation is attenuated, the committed answer is authoritative,
request mismatches roll back, ticket policy is derived from retrieved state, and
retries cannot duplicate a ticket intent. Its optional live-model executable
remains outside the deterministic suite.

Agent-loop integration tests snapshot and restore a conversation, continue a
fork in a child process, assert parent isolation, and enforce script
allowlisting through the Svit-owned builder.
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
