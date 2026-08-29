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

The Svit model tool `exec` accepts either a `/lib/<name>` path or one transient
`source` program. Inline source is interpreted through the same activation
boundary, can call attached ports, and can write durable state, but is never
stored in `/lib` or any other process node. A model uses a named script when it
expects reuse or wants the source to remain inspectable; it uses inline source
for a one-off operation. This is a tool-level form, distinct from Lisp's
`(exec path input)`, which resolves named `/lib` scripts only.

Rust hosts may write Lisp forms directly inside `svit_script!`, supply a source
string literal, or embed a package-relative file. The macro invokes the Ketos
compiler during the Rust build and then constructs the ordinary `Script`
record. It does not execute top-level forms or replace process-boundary
validation: configured source and syntax limits are enforced when the script
enters `/lib`, while `main(input)` and runtime behavior are checked by
activation. Ketos compilation may evaluate Lisp macros and constants; the
build-time compiler therefore uses null I/O, a null module loader, and the
standard Svit limit profile.

Bindings are immutable. Function parameters and `let` introduce lexical local
bindings; `define` adds activation-local functions or values to the fresh
interpreter. Durable variables live under `/memory`, not in Lisp globals.
`if`, `cond`, `case`, `and`, and `or` provide conditional control, and `do`
sequences expressions. Svit Lisp has no mutable `while` or `for` loop; bounded
iteration uses tail-recursive functions and remains subject to every activation
resource limit.

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
| `(jq filter value)` | Run a bounded jq filter and return its emitted values as a persistent array |
| `(search path pattern)` | Search text below one process or mount path and return bounded match records |
| `(discover path)` | List immediate children at an absolute process path |
| `(read path)` | Read from the transactional process hierarchy, resolving mount nodes lazily |
| `(stat path)` | Describe one node: kind, granted access, locality, and source facts |
| `(write path value)` | Stage a `/memory`, typed `/lib/<name>`, or granted mount-leaf update |
| `(remove path)` | Stage a `/memory`, `/lib/<name>`, or granted mount-node removal |
| `(exec path input)` | Execute a `/lib` script, with the path first |
| `(port-call name input)` | Invoke one host-attached named port |
| `(log-info! message fields?)` | Append a bounded activation log record |
| `(send! address message)` | Buffer a message intent for atomic commit |
| `*svit-version*` | The string `Svit Lisp 2` |

Ketos core arithmetic, comparison, immutable list, string, lexical binding,
function, and conditional forms remain available. Output from `print` and
related functions is discarded by null I/O. All module imports fail closed;
the Ketos `random`, `math`, and `code` modules are unavailable.

`jq` and `search` are Svit standard-library data transformations, not host
ports. `jq` accepts one explicit Svit value, decodes JSON text and JSON HTTP
response bodies when present, and returns the filter's emitted values as an
array. `search` uses a Rust regular expression over the activation's process
view and lazily walks mounts. Jq filters and search patterns are limited to 4
KiB; jq output is limited to the persistent value envelope; jq recursive,
generator, and range constructs are rejected; and both operations bound their
result counts. Jq may reduce a larger activation-local port response, but that
response remains in process memory until the activation ends (`L-048`). They
have no host authority.

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

`runtime-builtins` returns the canonical catalog of Svit-provided guest helpers as
validated maps containing name, signature, category, and description. Ketos core
language forms are outside this Svit-helper catalog. The canonical model system
prompt directs agents to inspect this catalog before writing or modifying Lisp.

Generic structured-data built-ins expose validated JSON-compatible values:
`json-parse`, `json-stringify`, `map-get`, `map-has?`, `map-set`, `list-get`,
and the `map?`, `list?`, `string?`, `number?`, `boolean?`, and `null?`
predicates. JSON parsing and derived maps pass through the same value limits as
activation input and commit values.

`json-parse-safe`, `map-get-safe`, and `safe-call` return a map containing
`"ok"` plus `"value"` on success or a sanitized `"error"` on a recoverable
guest failure. Execution limits, resource failures, and port suspension remain
hard failures and cannot be converted into data. Validated functions dispatch
through ordinary Lisp application; the existing Ketos execution and call-stack
budgets bound loops and recursion.

Result combinators (`result-ok`, `result-error`, predicates, accessors, map,
and-then, and or-else) compose the same result-map contract. `value-at`,
`value-at-safe`, and `value-has-path?` traverse typed string-key/integer-index
paths without evaluating expressions. `dispatch-table` stores only explicitly
supplied ephemeral function values; `dispatch` and `dispatch-safe` reject
unknown names, and the safe form preserves hard failures.

## Process operation semantics

All process operations use absolute paths. `discover` and `read` traverse the
transactional hierarchy. `stat` returns the facts record for one node: `kind`,
`access`, `locality`, `mount`, `path`, `source`, `attached`, and provider
`facts`.

Paths below `/mounts/<name>` resolve through the host-attached provider at call
time rather than from committed state. One `read` resolves one node under the
activation's limits: a leaf returns its content, a directory returns its facts,
and `discover` lists its children. A mount with no attached provider fails
closed.

`write` and `remove` mutate `/memory` values, one typed `/lib/<name>` entry, or
a leaf below a mount whose descriptor grants writes; `/system` and reserved
nodes are read-only. A library write is a map with required text `source` and
optional text `documentation`. Mount writes are buffered for the activation and
applied at the commit point, so a failed activation applies none of them.

A committed activation reports the canonical paths it changed, including any
mount path it wrote, so a client invalidates exactly what went stale.

Nested `exec` uses the same working memory, staged library changes, logs,
message intents, and activation deadline. It has an independent maximum depth
because each call creates a fresh interpreter. A nested failure restores its
call checkpoint and, unless handled by a future language construct, rejects
the outer activation. A `port-call` suspends guest execution while Svit invokes
the exact host-attached port. Svit then replays pure guest segments with
the recorded result, without repeating the completed port, and commits the
final working copy once. Guest segments share one execution-time budget;
waiting for the async port is outside that VM budget. Any syntax, runtime,
conversion, limit, or validation failure before commit discards the complete
activation working copy, but cannot undo an external port effect.

## Isolation and authority

The runtime installs `GlobalIo::null()` and `NullModuleLoader`, and creates a
fresh interpreter for every activation. Guest code reaches mount sources only
through host-attached providers under the descriptor's granted access and may
select only the `/ports` implementations attached by its Svit host. It never
receives a host path, handle, connection, or credential, and has no ambient
filesystem, network, environment, host process, module, clock, or randomness
capability. Rust functions capture only the process working copy, bounded
activation buffers, and the typed port suspension boundary.

Ketos is native code inside the host process. Pure Rust reduces the FFI and
C/C++ interpreter surface but is not proof of hostile same-process tenant
isolation. Production use still requires an outer Wasm or OS boundary.

## Limits

Ketos enforces wall-clock execution time, call-stack size, value-stack size,
namespace entries, abstract guest-memory units, integer bits, and syntax depth.
Svit separately bounds nested exec depth, persistent values, source bytes,
logs, messages, and staged scripts. Those bound one activation and one value;
`max_tree_nodes` and `max_tree_text_bytes` additionally bound the whole
committed root, so state accumulated across many valid activations still fails
closed. Every limit failure rolls back the activation.

Ketos 0.12 does not expose deterministic instruction fuel. Therefore the
execution deadline can vary with host load and cannot support cross-run
deterministic failure at the budget boundary. This remains an explicit
limitation and research requirement.

## Snapshot compatibility

Snapshot format 3 stores Lisp scripts, the Ketos-oriented limit schema, and the
conventional process namespace with validated system metadata. Restore rejects
formats 1 and 2 rather than translating old roots or runtime semantics.
