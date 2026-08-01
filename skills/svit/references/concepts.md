# Svit concepts

- **Runtime**: the trusted Rust host that validates and executes processes.
- **Process**: an isolated address, version, memory tree, named script library,
  outbox, and limits.
- **Activation**: one bounded named-script invocation against a transactional
  working copy.
- **Control request**: a client command with a mandatory process-version
  precondition and scoped idempotency key.
- **Commit**: atomic replacement of memory, scripts, and buffered message
  intents after complete validation.
- **Snapshot**: versioned canonical encoding of committed state only.
- **Fork**: a new process identity starting from committed parent state, with
  independent future mutation.
- **Svit Lua**: the restricted, versioned guest language; not full Lua or Luau.

The current implementation slice is local and in memory. Scheduling, message
delivery, projections, external capabilities, distributed identity, and
production isolation are deferred.
