---
type: Language Contract
title: Experimental Deed Runtime
description: Pinned Deed adapter, synthetic activation grants, and current compatibility boundary.
tags:
  - svit
  - deed
  - sandbox
---

# Experimental Deed Runtime

## Status

Under implementation on the off-main Deed runtime spike. It is executable
integration evidence, not yet the default guest language or a production
isolation boundary.

## Decision

Svit may store a named script with `language: "deed@0.2.12"`. The source is
checked, lowered, and compiled when it enters `/lib`; activation recompiles the
committed source and calls `main(sys: System) -> Int`. Source identity is the
virtual path `/lib/<name>.deed`. The Deed crates are pinned exactly so a
snapshot does not silently acquire different language semantics.

The first adapter uses Deed's standard capability composition rather than
treating an escaping user effect as a resumable host call:

1. Svit flattens scalar leaves from the activation input and transactional
   `/memory` copy into a synthetic environment. Keys retain their absolute
   paths under `/input` and `/memory`.
2. Deed receives that synthetic data through explicitly granted `Io.env`.
   This is not the host process environment.
3. Deed may emit `set-integer`, `set-text`, or `remove` mutation intents through
   its explicitly granted console. This is a captured activation buffer, not
   the host console.
4. Svit parses and applies those intents to the transactional working copy only
   after `main` returns successfully. A Deed trap, contract failure, malformed
   intent, limit failure, or Svit value failure commits none of them.

The current intent encoding is one line per operation:

```text
set-integer<TAB>/memory/path<TAB>42
set-text<TAB>/memory/path<TAB>text
remove<TAB>/memory/path
```

Every other output line fails the activation. Only imports needed for
`Io.env`, `Io.write`, and `sys.console` are accepted. Clock, real environment,
arguments, input console, filesystem, and network imports are rejected before
execution (`TM-ESC-005`).

## Runtime bounds

Svit checks the compiled module's initial memory against `max_guest_memory`,
runs it with a deterministic instruction budget derived from the activation
limit, caps captured intent count and bytes with the persistent-value limits,
rechecks the shared wall deadline, validates output, and then uses the ordinary
Svit commit boundary. The bundled Deed compiled-code runner is a test oracle in
native process memory. Its growth ceiling is not Svit's allocator limit, and
compilation has no independent deterministic fuel. See `L-047`.

## Compatibility boundary

This slice demonstrates Deed scripts reading scalar activation data, updating
`/memory`, surviving snapshot/restore, and rolling back captured writes after a
contract failure. It does not yet implement the full Svit Lisp surface:

- `main` returns only `Int`;
- nested maps and arrays are flattened for reads but cannot be returned;
- mutation intents cover integers, text, and remove only;
- script-library mutation, nested `exec`, logs, messages, mounts, and built-ins
  are unavailable;
- Deed modules and imports beyond the three allowed host imports are rejected;
- the native runner is not a hostile multi-tenant boundary.

Svit Lisp remains supported during the experiment. Replacing it requires a
feature-parity decision and stronger execution containment, not merely changing
the default language tag.
