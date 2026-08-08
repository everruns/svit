# Svit concepts

- **Runtime**: the trusted Rust host that validates and executes processes.
- **Agent**: a Svit-owned reason/act loop and durable thread bound to exactly
  one process; Agentyk is the current loop implementation.
- **Process**: an isolated address, version, host-managed agent-thread state,
  discoverable namespace, memory tree, scripts, metadata, outbox, and limits.
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
- **Native executables**: optional typed programs under `/bin` for committed-process
  search and explicit JSON filtering, with separate host grants for HTTP,
  nested model calls, and one-turn child execution.
- **Executable manual**: `/bin/<name>` describes one installed executable's
  schema, output, effect class, and limits. `/bin` is refreshed on resume and
  never grants authority.

The current implementation slice is local and in memory. It supports bounded,
read-only snapshot mounts imported from a folder or host-selected Turso query.
Scheduling, remote message delivery, durable live projections, guest-script
external capabilities, distributed identity, and production isolation are
deferred. Optional native executables remain outside Svit Lisp and process
transactions.

`/agent` contains host-managed, guest-readable loop and replay state. The
namespace reserves `/tasks` and `/children`; `/inbox` is a host-managed durable
input queue. `/mounts` contains
read-only snapshot records with `kind`, `mode`, and `data`. `/system/identity`
contains a logical address marked `authenticated: false`; it grants no
authority.
