# Svit Lisp runtime

Svit scripts run in a restricted Lisp runtime against one transactional memory-tree working copy. Guest code has no ambient filesystem, network, environment, process, module-loader, clock, or randomness access.

## Discover helpers first

Before writing or modifying a script, evaluate:

```lisp
(runtime-builtins)
```

It returns a list of maps. Each map contains:

- `"name"`: callable helper name;
- `"signature"`: Lisp call shape;
- `"category"`: discovery, memory-tree, structured-data, persistent-value, predicate, scripts, ports, effects, or recoverable;
- `"description"`: short behavioral contract.

Use this runtime result as the authoritative catalog for the running Svit version. Ketos core language forms such as `define`, `lambda`, `if`, `let`, recursion, arithmetic, and ordinary function application are language features and are not listed in this Svit-helper catalog.

## Result composition

Result helpers use the same `"ok"` plus `"value"` or `"error"` maps as the safe runtime operations:

```lisp
(result-ok value)
(result-error message)
(result-ok? result)
(result-value result)
(result-error-message result)
(result-map function result)
(result-and-then function result)
(result-or-else function result)
```

`result-map` transforms a successful value. `result-and-then` chains a function that returns another result. `result-or-else` receives the error value and may recover with another result. A branch that does not apply returns the original result unchanged.

## Structured paths

Use a Lisp list of string map keys and non-negative integer array indices:

```lisp
(value-at response (list "choices" 0 "message" "content"))
(value-at-safe response (list "choices" 0 "message" "content"))
(value-has-path? response (list "choices" 0))
```

`value-at` fails when a component is absent or does not match its container. `value-at-safe` returns a result map. `value-has-path?` returns false for a well-formed path that does not resolve.

## Fail-closed dispatch tables

A dispatch table is ephemeral and can contain only explicitly supplied Lisp functions:

```lisp
(define handlers
  (dispatch-table "search" search "finish" finish))

(dispatch handlers response-type arguments)
(dispatch-safe handlers response-type arguments)
```

Names must be unique. Unknown names fail closed. `dispatch-safe` converts recoverable handler failures into result maps but propagates resource limits, execution failures, and port suspension.

## Structured agent data

Use JSON and map helpers to validate and inspect model or port responses rather than branching on opaque text:

```lisp
(define response
  (json-parse
    "{\"type\":\"tool\",\"arguments\":{\"query\":\"billing\"}}"))

(if (= (map-get response "type") "tool")
    (map-get (map-get response "arguments") "query")
    "done")
```

Useful helpers include:

- `json-parse`, `json-stringify`, `json-parse-safe`;
- `map?`, `map-get`, `map-get-safe`, `map-has?`, `map-set`;
- `list?`, `list-get`;
- `string?`, `number?`, `boolean?`, `null?`;
- `value-map`, `value-array`, `value-get`, `value-null?`.

`map-set` returns a new map; it does not mutate the input. JSON parsing and all derived persistent values remain subject to configured value depth and size limits.

## Recoverable errors

`json-parse-safe`, `map-get-safe`, and `safe-call` return structured result maps:

```lisp
(value-map "ok" true "value" result)
(value-map "ok" false "error" "sanitized diagnostic")
```

Inspect `"ok"` before reading `"value"` or `"error"`. These helpers catch only recoverable guest failures. Resource limits, execution failures, and port suspension remain hard failures and roll back the activation.

## Explicit dispatch

Functions are ordinary validated Lisp values. Select a known function and call it directly:

```lisp
(define search
  (lambda (arguments)
    (map-get arguments "query")))

(define finish
  (lambda (arguments)
    (map-get arguments "answer")))

(define handler
  (if (= response-type "tool") search finish))

(handler arguments)
```

Do not evaluate generated source or dynamically resolve an untrusted function name. Keep dispatch to explicitly defined functions. Existing call-stack, value-stack, and execution limits bound loops and recursion.
