# Svit concepts

- **Runtime**: the trusted Rust host that validates and executes processes.
- **Process**: an isolated address, version, discoverable namespace, memory
  tree, named script library, system metadata, outbox, and limits.
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
- **Svit Lisp**: the restricted, versioned Ketos guest language with the same
  generic process operations as host callers and agent tools; not full Scheme,
  Common Lisp, or unrestricted Ketos.

The current implementation slice is local and in memory. It supports bounded,
read-only snapshot mounts imported from a folder or host-selected Turso query.
Scheduling, message delivery, live projections, external capabilities,
distributed identity, and production isolation are deferred.

The namespace reserves `/tasks`, `/inbox`, and `/children`. `/mounts` contains
read-only snapshot records with `kind`, `mode`, and `data`. `/system/identity`
contains a logical address marked `authenticated: false`; it grants no
authority.
