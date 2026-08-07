# Changelog

All notable changes to Svit will be documented here.

## Unreleased

### Added

- Initial Rust workspace with the `svit` library and `svit-cli` smoke-test
  binary.
- Transactional Svit Lisp activations, backed by Ketos, over one process state root.
- Named, discoverable scripts with guest-side transactional script creation.
- Structured logs and atomic buffered message intents.
- Versioned snapshots, integrity hashes, restore validation, and isolated
  forks.
- Execution deadline, interpreter memory, stack, namespace, syntax, integer,
  persistent-value, script, log, and message limits.
- Typed activation interceptor hooks.
- Transport-neutral Svit Control Protocol 1 with optimistic version checks,
  explicit conflicts, bounded retry receipts, additive-field compatibility,
  and an in-memory controller.
- Deterministic executable examples for persistence, self-reflection, atomic
  rollback, forks, sandbox limits, and multi-client control.
- OKF v0.2 knowledge bundle, threat model, public Svit skill, and repository
  validation tooling.
- Conventional discoverable process hierarchy with reserved future nodes and
  validated system identity, API, limits, lineage, runtime, capabilities, and
  outbox metadata. Snapshot format 3 records this root schema.
- Schema-aligned `ProcessBuilder::library` construction for initial `/lib`
  entries, replacing the pre-release `script` builder method.
- One absolute-path `discover`, `read`, `write`, `remove`, and `exec` contract
  shared by Rust, agent tools, and Svit Lisp 2, including transactional,
  deadline-sharing, depth-bounded nested script execution.
- Bounded, read-only snapshot mounts for real UTF-8 folders and host-selected
  Turso query results, with a deterministic example covering both sources.

### Security

- Guest environments use a fresh restricted Ketos interpreter with null I/O
  and a module loader that rejects every module.
- Ambient host APIs, modules, and randomness are unavailable to guest scripts.
- Guest diagnostics are sanitized and capped before crossing the public API.
- Folder snapshot imports reject symbolic links and special files; mount data
  is validated against process value limits and grants no live host authority.

No release has been published. The current API and snapshot format may change
without compatibility guarantees.
