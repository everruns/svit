---
type: Test Strategy
title: Security Testing
description: Adversarial tests required before security properties can be claimed.
tags:
  - svit
  - security
  - testing
---

# Security Testing

## Status

Required for the initial vertical slice.

## Test layers

1. Unit tests exercise limit arithmetic, value conversion, address parsing,
   canonical encoding, and error sanitization.
2. Integration tests execute adversarial scripts through the same public API
   used by examples.
3. Property tests generate nested values and assert conversion, snapshot round
   trips, and fork isolation invariants.
4. Fuzz targets feed arbitrary bytes to snapshot decoding and arbitrary guest
   Lisp values to conversion boundaries where practical.
5. Outer-process tests cover hangs or memory failures that cannot be contained
   reliably inside the test process.

## Required invariant tests

- `TM-EFF-001`: syntax, runtime, conversion, invalid staged script, memory
  limit, execution limit, panic, and rejected host write/remove failures preserve the
  full committed root.
- `TM-ESC-001`: module loading fails closed, I/O is discarded, and no host
  filesystem, network, environment, process, clock, or randomness function is installed.
- `TM-ISO-001`: globals and memory never cross activation or process identity.
- `TM-FORK-001`: parent and sibling hashes remain unchanged after child writes.
- `TM-SNAP-001`: truncation, unknown versions, excessive nesting, oversized
  lengths, invalid floats, and trailing data are rejected.
- `TM-DOS-001` through `TM-DOS-003`: every budget is tested exactly at, below,
  and above its boundary.
- `TM-DOS-004`: receipt retention rejects zero and values above its hard host
  maximum.
- `TM-DOS-005`: decoded oversized values are rejected before receipt retention;
  the transport byte cap remains required.
- `TM-DOS-006`: nested `exec` shares the outer activation deadline, stops at
  the configured nesting depth, and rolls back the complete activation.
- `TM-DOS-007`: folder entry/text and Turso row/text limits stop materializing
  data at their configured bounds; query execution still needs an outer deadline.
- `TM-DOS-008`: the common built-in dispatcher rejects oversized extension
  output; `search` rejects oversized patterns and `jq` rejects
  recursive or generator constructs before evaluation.
- `TM-DOS-011`: build-time recursive macro expansion fails under the compiler
  execution or call-stack limit.
- `TM-ESC-003`: data built-ins accept only a committed process path or
  explicit JSON; no shell, filesystem, executable, or environment input exists.
- `TM-ESC-004`: build-time script compilation rejects module loading while
  evaluating guest macros and constants under null I/O.
- `TM-CAP-003`: folder imports reject symbolic links and special files, and
  mounted data remains read-only through host and guest path operations.
- `TM-CAP-004`: HTTP is denied without a matching host allowlist entry, and an
  allowed fixture request passes through both URL policy and host transport;
  Svit's standard reqwest transport refuses redirects and bounds the streamed
  body.
- `TM-CAP-005`: `/bin` exposes the exact installed built-in manuals during a
  turn, and resume removes entries whose host grants are no longer configured.
- `TM-CAP-006`: a host extension executes through bounded explicit JSON and a
  context exposing committed reads without process mutation methods.
- `TM-EFF-002`: concurrent clients cannot both commit from one process version.
- `TM-EFF-003`: exact retry replays a receipt, and retry after eviction cannot
  duplicate a committed activation.
- `TM-EFF-004`: the negative test demonstrates that independent controllers do
  not provide distributed ownership; this remains required until leases exist.
- `TM-EFF-005`: the `llm` command can call only the host-selected driver; tests
  do not claim transactional or replay-safe external effects.
- `TM-FORK-002`: `spawn` records lineage, preserves parent memory, retains an
  independently restorable child, and rejects duplicate child addresses.
- `TM-AUTH-001`: remote transport tests must prove authorization precedes
  receipt lookup and that two tenant scopes cannot observe each other's
  receipts; no remote transport exists yet.
- `TM-AUTH-001`: namespace tests must show that discoverable process identity
  is marked unauthenticated and cannot be modified through the memory mutation
  API.
- `TM-CAP-002`: an attenuated agent capability omits generic mutation tools,
  denies unlisted scripts before activation, and still executes an allowed script.
- `TM-INF-001`: all guest-visible failures are capped and exclude host paths,
  Rust backtraces, pointers, and raw interpreter debug output.
- `TM-AUD-001`: host commits to `/thread` succeed atomically, guest writes fail
  without mutation, invalid host replacement values preserve the root and
  version, message projection must match canonical events on resume, and the
  event log rejects foreign sessions, duplicate IDs, or invalid sequences.
- `TM-MSG-002`: inbox enqueue and exact-head acknowledgement commit once;
  rejected values, mismatched acknowledgement, and guest writes preserve state.

## Status rule

A threat remains `REQUIRED` or `OPEN` until the enforcing code and a focused
test referencing its ID both exist. A passing example is useful evidence but
does not replace adversarial boundary tests.

## Unsafe and dependency review

Run `cargo audit`, `cargo deny`, and supply-chain review in CI. Inventory all
`unsafe` reachable from the core, including dependencies. Any project-owned
`unsafe` requires a local safety argument and a test at the violated invariant,
or removal.
