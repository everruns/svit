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

## Examples as acceptance tests

The following scenarios execute with assertions and deterministic output under
`crates/svit/examples/`:

- durable counter across activations and snapshot restore;
- a script that saves another named script and discovers it later;
- atomic memory and outbox rollback after a deliberate error;
- parent and two forks diverging without shared mutation;
- denied modules and bounded infinite-loop termination;
- two control clients contending on one version, then resolving the conflict.
- a real folder and Turso query imported together as read-only snapshot mounts.
- a process-owned agent thread restored and continued in an isolated child process.
- a started Svit drains its durable inbox, emits completed turns, and leaves a
  failed turn's input queued.
- the agent reads its projected system prompt, message history, and canonical
  events through the runtime capability during a turn.
- `/bin/search` reads committed process text and `/bin/jq` filters explicit
  JSON; focused integration tests cover data limits, default-deny and
  host-routed HTTP, nested model selection, and local child spawn.
- `/bin` discovery exposes installed executable manuals, while
  resume removes catalog entries for absent host grants.

`just examples` and CI run examples, not only `cargo check --examples`.
CLI smoke inputs live under `crates/lampa/tests/fixtures/`; they are internal
test data, not public examples. Examples requiring an API key or external
service must be separated from the deterministic core suite and must never
receive secrets on pull-request-controlled code.

The original support-agent consumer scenario uses Agentyk's simulated driver to
prove that model-visible mutation is attenuated, the committed answer is authoritative,
request mismatches roll back, ticket policy is derived from retrieved state, and
retries cannot duplicate a ticket intent. Its optional live-model executable
remains outside the deterministic suite.

Agent-loop integration tests snapshot and restore a conversation, continue a
fork in a child process, assert parent isolation, and enforce script
allowlisting through the Svit-owned builder.
The credentialed `support-agent-v2` consumer exercises one process-owned Svit
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

Python repository validators use `unittest` so they run without extra
dependencies:

```console
python3 -m unittest discover -s scripts/tests -p 'test_*.py'
```
