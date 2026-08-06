---
type: Language Contract
title: Svit Lisp Runtime
description: Versioned guest-language surface and Ketos sandbox boundary for the initial slice.
tags:
  - svit
  - lisp
  - sandbox
---

# Svit Lisp Runtime

## Status

Implemented for the initial language surface. Deterministic instruction fuel,
dependency hardening, and broader adversarial testing remain open.

## Decision

Svit Lisp 2 is a small versioned Lisp surface implemented with the pure-Rust
Ketos bytecode interpreter. It is not a compatibility promise for Scheme,
Common Lisp, or unrestricted Ketos.

Standalone Svit Lisp source uses the `.svit-script` extension. Virtual source
paths inside the process script library use `/lib/<name>.svit-script`. The
extension and diagnostic identity belong to Svit; Ketos remains an interpreter
implementation detail. The `.svit` extension is reserved for a future Svit
manifest format; manifests are not implemented.

A named script defines `(main input)`. Each activation creates a fresh Ketos
interpreter, installs null I/O and a module loader that rejects every module,
exposes explicit Svit functions, executes against a transactional memory copy,
and converts all results to bounded Svit values before commit.

```lisp
(define (main input)
  (let ((count (+ (read "/memory/count") (value-get input "/by"))))
    (do
      (write "/memory/count" count)
      (log-info! "counter updated" (value-map "count" count))
      (value-map "count" count))))
```

## Guest surface

| Name | Meaning |
| --- | --- |
| `input` | Immutable activation input passed to `main` and bound globally |
| `(value-get value path)` | Read a nested Svit value by slash path |
| `(value-map key value ...)` | Construct a text-keyed persistent map |
| `(value-array value ...)` | Construct a persistent array, including an empty array |
| `(value-null? value)` | Test whether a value is the persistent null value |
| `(discover path)` | List immediate children at an absolute process path |
| `(read path)` | Read from the transactional process hierarchy |
| `(write path value)` | Stage a `/memory` or typed `/lib/<name>` update |
| `(remove path)` | Stage a `/memory` or `/lib/<name>` removal |
| `(exec script input)` | Execute a named script inside the same activation transaction |
| `(log-info! message fields?)` | Append a bounded activation log record |
| `(send! address message)` | Buffer a message intent for atomic commit |
| `*svit-version*` | The string `Svit Lisp 2` |

Ketos core arithmetic, comparison, immutable list, string, lexical binding,
function, and conditional forms remain available. Output from `print` and
related functions is discarded by null I/O. All module imports fail closed;
the Ketos `random`, `math`, and `code` modules are unavailable.

## Value boundary

Persistent values remain null, booleans, signed 64-bit integers, finite
floats, text, arrays, and text-keyed maps. Maps, arrays, and null enter Ketos as
immutable typed foreign values so empty arrays remain distinct from Lisp unit.
`value-get` projects a nested primitive or typed value. Non-empty native Lisp
lists may be returned as arrays; use `value-array` when the empty-array
distinction matters.

The boundary rejects ratios, oversized integers, names, characters, paths,
bytes, structs, functions, lambdas, quotations, and foreign values not created
by Svit. Script records never enter the guest data model.

## Process operation semantics

All process operations use absolute paths. `discover` and `read` traverse the
transactional hierarchy. `write` and `remove` mutate `/memory` values or one
typed `/lib/<name>` entry; `/system` and reserved nodes are read-only. A
library write is a map with required text `source` and optional text
`documentation`.

Nested `exec` uses the same working memory, staged library changes, logs,
message intents, and activation deadline. It has an independent maximum depth
because each call creates a fresh interpreter. A nested failure restores its
call checkpoint and, unless handled by a future language construct, rejects
the outer activation. Any syntax, runtime, conversion, limit, or validation
failure before commit discards the complete activation working copy.

## Isolation and authority

The runtime installs `GlobalIo::null()` and `NullModuleLoader`, and creates a
fresh interpreter for every activation. Guest code receives no filesystem,
network, environment, host process, module, clock, or randomness capability.
Rust functions capture only the process working copy and bounded activation
buffers.

Ketos is native code inside the host process. Pure Rust reduces the FFI and
C/C++ interpreter surface but is not proof of hostile same-process tenant
isolation. Production use still requires an outer Wasm or OS boundary.

## Limits

Ketos enforces wall-clock execution time, call-stack size, value-stack size,
namespace entries, abstract guest-memory units, integer bits, and syntax depth.
Svit separately bounds nested exec depth, persistent values, source bytes,
logs, messages, and staged scripts. Every limit failure rolls back the activation.

Ketos 0.12 does not expose deterministic instruction fuel. Therefore the
execution deadline can vary with host load and cannot support cross-run
deterministic failure at the budget boundary. This remains an explicit
limitation and research requirement.

## Snapshot compatibility

Snapshot format 3 stores Lisp scripts, the Ketos-oriented limit schema, and the
conventional process namespace with validated system metadata. Restore rejects
formats 1 and 2 rather than translating old roots or runtime semantics.
