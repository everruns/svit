# Svit concepts

- **Runtime**: the trusted Rust host that validates and executes processes.
- **Process**: an isolated address, version, memory tree, named script library,
  outbox, and limits.
- **Activation**: one bounded named-script invocation against a transactional
  working copy.
- **Control request**: a client command with a mandatory process-version
  precondition and scoped idempotency key.
- **VAST**: Versioned Atomic State Transitions; one controlled activation may
  atomically advance the observed process version, while rejection or conflict
  preserves committed state. Concurrent activations are not merged.
- **Commit**: atomic replacement of memory, scripts, and buffered message
  intents after complete validation.
- **Snapshot**: versioned canonical encoding of committed state only.
- **Fork**: a new process identity starting from committed parent state, with
  independent future mutation.
- **Svit Lisp**: the restricted, versioned Ketos guest language; not full Scheme, Common Lisp, or unrestricted Ketos.

The current implementation slice is local and in memory. Scheduling, message
delivery, projections, external capabilities, distributed identity, and
production isolation are deferred.
