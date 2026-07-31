---
type: Language Contract
title: Svit Lua Runtime
description: Versioned guest-language surface and Lua sandbox boundary for the initial slice.
tags:
  - svit
  - lua
  - sandbox
---

# Svit Lua Runtime

## Status

Implemented for the initial language surface; compatibility and hardening work
remain open.

## Decision

Svit Lua is a small versioned guest language, not a promise of full Lua or
Luau compatibility. The current implementation may use Luau through `mlua`,
but interpreter choice is hidden behind the language boundary.

A named script defines `main(input)`. Each activation creates a fresh VM,
loads a safe standard-library subset explicitly, installs the current memory
working copy and buffered host functions, executes `main`, and converts all
results back into bounded Svit values before commit.

```lua
function main(input)
    memory.count = (memory.count or 0) + (input.by or 1)
    log.info("counter updated", { count = memory.count })
    return { count = memory.count }
end
```

## Initial guest surface

The target surface is intentionally small:

| Name | Meaning |
| --- | --- |
| `memory` | Transactional guest-owned memory |
| `input` | Immutable activation input |
| `scripts.list()` | Discover committed named scripts |
| `scripts.read(name)` | Inspect committed script source and metadata |
| `scripts.save(name, source, metadata)` | Stage a named script for commit |
| `log.info(message, fields)` | Append a bounded activation log record |
| `send(address, message)` | Buffer a message intent for atomic commit |

Exact Rust and Lua signatures are stabilized by executable examples and tests,
not by this early document alone.

## Value conversion

Persistent values are null, booleans, signed integers, finite floats, text,
lists, and text-keyed maps. Script records are permitted only under `/lib`.
Byte values are not implemented. During conversion the runtime rejects:

- cycles and shared table identity that cannot be represented canonically;
- non-text map keys;
- sparse or mixed list/map tables;
- NaN and infinities;
- unsupported functions, threads, userdata, and host objects;
- values exceeding configured depth, collection, text, or encoded-size limits.

## Allowed library

The safe library is selected explicitly and versioned. The implemented surface
contains basic table, string, deterministic math (without `random` or
`randomseed`), UTF-8, and bit operations plus a small base-function allowlist.
The runtime does not expose
`os`, `io`, `debug`, package/module loading, FFI, native modules, environment
access, filesystem access, network access, host process control, wall clock,
or ambient randomness.

Interpreter sandbox helpers are defense in depth, not the language contract.
Tests must assert that every denied entry point is absent.

## Limits

The VM is bounded by input size, memory, interrupt ticks, call depth where the
interpreter supports it, output, logs, staged scripts, and host calls. Luau
interrupt callbacks are described as versioned execution ticks; Svit does not
claim exact cross-version instruction metering.

Limit failures are ordinary activation failures and therefore roll back state,
scripts, and outbox. The implementation bounds interrupt ticks, VM heap growth,
value depth/entries/text, script source, logs, message count, and staged script
count. Aggregate encoded-output accounting and an outer wall-time boundary are
still required.

## Reflection

Guest code can inspect its memory and named script library. It cannot reflect
over host pointers, enforcement internals, credentials, other processes, or
ungranted capabilities. Runtime API discovery will grow only with executable
examples and explicit threat analysis.
