# Changelog

All notable changes to Svit will be documented here.

## Unreleased

### Changed

- Track Everruns `main` and compose the process-owned loop through the
  `everruns-host` backend, canonical event-log, and provider/model contracts.
- Configure each reasoning loop atomically through one `Reasoner` containing
  its provider-visible model ID and Everruns provider. One-shot model calls use
  the Everruns facade internally.
- Advance thread state to `svit-thread@6`; runtime construction records the
  canonical `session.started` event, uses the process address as runtime
  identity, and composes Svit-owned prompts with optional wrapped instructions.
- Keep Lampa as a presentation host: its entry point builds one Svit instance,
  while the TUI consumes commit notifications, inbox, and outbox without
  polling process state or assembling built-ins. Remove the separate direct
  `Process` script-execution command.

### Added

- Initial Rust workspace with the `svit` library and Lampa process console.
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
  outbox metadata. Snapshot format 6 records this root schema and host-managed
  reasoning-loop state.
- Schema-aligned `ProcessBuilder::library` construction for initial `/lib`
  entries, replacing the pre-release `script` builder method.
- One absolute-path `discover`, `read`, `write`, `remove`, and `exec` contract
  shared by Rust, agent tools, and Svit Lisp 2, including transactional,
  deadline-sharing, depth-bounded nested script execution.
- Bounded, read-only snapshot mounts for real UTF-8 folders and host-selected
  Turso query results, with a deterministic example covering both sources.
- A process-owned `svit::Svit` loop implemented with Everruns, with durable
  conversation events carried through snapshot, restore, and isolated forks.
- A live `support-agent-svit` consumer using `gpt-5.6-terra` through a
  process-owned Svit inbox and outbox.
- One Svit built-in setup path, with `Builtins::standard()` deriving `llm`
  and `spawn` from the instance reasoner, accepting explicit HTTP policy,
  and using a reusable redirect-denying, response-bounded reqwest transport.
- Explicit Svit `Inbox`, `Outbox`, and `Events` ports, commit notifications,
  atomic owned value/version reads, and sanitized terminal failures without
  exposing the mutable process tree or channel implementation.
- Bounded Lampa array-row previews for scalar and object items.

### Security

- Guest environments use a fresh restricted Ketos interpreter with null I/O
  and a module loader that rejects every module.
- Ambient host APIs, modules, and randomness are unavailable to guest scripts.
- Guest diagnostics are sanitized and capped before crossing the public API.
- Folder snapshot imports reject symbolic links and special files; mount data
  is validated against process value limits and grants no live host authority.
- Guest scripts and model tools can inspect durable `/thread` history but cannot
  rewrite it; only the trusted process host can replace that bounded state.

No release has been published. The current API and snapshot format may change
without compatibility guarantees.
