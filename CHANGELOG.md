# Changelog

All notable changes to Svit will be documented here.

## Unreleased

### Changed

- Replace construction-time snapshot mounts with virtual mounts. `/mounts/<name>`
  commits only a descriptor — kind, host-disclosed source, locality, and granted
  access — while nodes below it resolve through a host-attached `MountProvider`
  when they are read, discovered, stated, or written. Mount data no longer
  enters the committed root, so mount size is independent of process and
  snapshot size.
- Replace `SnapshotMount` with `Mount`, `MountProvider`, `MountDescriptor`,
  `MountNode`, `MountPath`, `MountAccess`, and `Locality`. `Mount::folder`,
  `Mount::writable_folder`, `Mount::value`, `Mount::writable_value`, and
  `Mount::turso_query` cover the built-in providers.
- `Process::read` and `DurableProcessHandle::read` return owned values because
  mount nodes are resolved rather than borrowed from committed state.
- Bump snapshots to format 7 for the descriptor-only `/mounts` schema and the
  new `max_mount_entries` and `max_mount_writes` limits.
- Browse one namespace in Lampa. The console no longer holds a copy of the
  process root or a separate mount browser: every node is resolved through
  `discover`, `stat`, and `read`, so a mounted folder is browsed exactly like
  committed memory. Listings are fetched on expand, content on selection and
  for array-item summaries, bounded at 200 children per directory, and
  re-resolved on each committed version.
- Store each Lampa instance in
  `instances/<instance-id>/svit.db`, selected below `LAMPA_DATA_DIR`. Instance
  IDs are lowercase filesystem-safe address segments, and an existing database
  must contain its matching root address.
- Add explicit legacy shared-database import. `ProcessStore::import` preserves
  the current process version and root while beginning a new retained-history
  tail at the imported boundary.

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
- Let Lampa select `Builtins::standard()` without HTTP policy configuration;
  the standard registry grants unrestricted destinations for research hosts,
  while `with_http_allowlist` remains available for attenuation.
- Keep Lampa's memory viewport stable when selecting lower visible rows and
  start with only the tree root expanded.
- Navigate Lampa panels backward with `Shift+Tab` while retaining forward
  traversal on plain `Tab`.
- Render Lampa conversation messages as Markdown and emit bare HTTP(S) URLs as
  OSC 8 terminal hyperlinks.
- Route runnable Svit inbox, reasoning, process-tool, and built-in catalog
  transitions through an adapter-owned durable process handle.

### Added

- `stat(path)` in Rust, Svit Lisp, the reasoning-loop tool set, and
  `BuiltinContext`. Every node — committed or mounted — answers with one facts
  record: kind, granted access, locality (`cache`, `local`, or `remote`),
  mount, path, source, attachment, and provider facts such as byte size,
  modification time, and a folder mount's git branch and commit.
- Writable mounts. A descriptor grants `read`, `write`, or `read-write`.
  Activation writes and removals below a granted mount are buffered and applied
  at the commit point after every in-process validation, so a failed activation
  applies none of them. External sources still cannot join the transaction.
- `Process::attach_mount` and `Svit::attach_mount` so a host can restore mount
  authority after a snapshot restore, which never carries providers.
- Mount-aware `search`: the built-in walks a mount subtree node by node under
  an independent node budget and reports when a bound truncated the walk.
- Lampa mounts the current directory read-only as `cwd` and accepts
  `--mount name=path` and `--mount-rw name=path`.
- A `content` fact on directory nodes (`object` or `array`) completes the
  `stat` vocabulary, so a client can tell an array from a map without reading
  it.


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
  and `spawn` from the instance reasoner, granting unrestricted research HTTP
  unless attenuated, and using a reusable redirect-denying, response-bounded
  reqwest transport.
- Explicit Svit `Inbox`, `Outbox`, and `Events` ports, commit notifications,
  atomic owned value/version reads, and sanitized terminal failures without
  exposing the mutable process tree or channel implementation.
- Bounded Lampa array-row previews for scalar and object items.

- Report what every transition changed. `Process::write`, `remove`,
  `enqueue_inbox`, `acknowledge_inbox`, `replace_thread_state`, and
  `attach_mount` now return a `Change` carrying the new version, the canonical
  changed paths, and the replayable mutations; `Activation` carries the same
  paths. Paths and mutations come from one fold, so a live observer and a
  stored durable event describe the same transition, and the Turso adapter no
  longer hand-builds a parallel mutation for each host call.
- `SvitEvent::Committed(Change)` replaces the payload-free notification, so a
  subscriber learns which paths went stale instead of invalidating everything.
  A notification carries version and paths but no values.
- Lampa invalidates only what a commit named. An unrelated commit no longer
  costs a re-walk of every open directory, and `r` in the memory panel reloads
  the tree for external changes no event can report.

### Fixed

- Keep the selected Lampa row across a committed version. The console now
  remembers the path and restores it once the tree resolves it again, instead
  of falling back to the root.

### Security

- `TM-CAP-007`: mount paths parse into validated segments that reject
  traversal, separators, empty, oversized, and NUL content before a provider
  observes the request.
- `TM-CAP-008`: mount providers are runtime state and are never serialized; a
  restored mount reads its descriptor with `attached: false` and fails closed
  until the host reattaches it.
- `TM-EFF-006`: mount writes are ordered with the process commit; a failed
  activation applies none of them.
- `TM-DOS-010`: mount listings, leaf reads, subtree searches, and console
  listings are bounded independently of committed-value limits.


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
