# Svit Knowledge Update Log

## 2026-08-21

* **Visual identity**: Adopted a radiant sunrise above two sweeping horizons
  as Svit's mark, connecting the Ukrainian name `svit` (world) with a world
  becoming visible. The canonical color and monochrome SVG sources live at the
  repository root; transparent 1024 px PNGs are derived from the same geometry.
* **Public site**: Replaced the early marketing page with a factual project
  index and Nimbus-rendered canonical documents. The site follows Tuika's
  Astro 7, Tailwind 4, pnpm, Nimbus Docs, and Cloudflare static-assets stack;
  its build synchronizes `README.md`, `SECURITY.md`, `CHANGELOG.md`, and public
  `docs/` sources into generated content rather than maintaining a second copy.

## 2026-08-20

* **Everruns dependency refresh**: Advanced the locked Everruns `main`
  revision from `18296be8` to `66c20400`. The resolved facade and test support
  are 0.18.2 and `everruns-host` is 0.20.1; the Svit integration compiled
  without an API adaptation.
* **Tuika dependency refresh**: Updated Lampa from Tuika 0.9.0 to 0.10.0 and
  `tuika-codeformatters` from 0.4.2 to 0.4.3. Lampa compiled without an API
  adaptation; Tuika's alternate-screen mouse capture remains opt-in.
* **Explicit port grants**: Removed the authority-bearing standard port bundle
  and build-time reasoner/HTTP derivation. Hosts now register `http`, `llm`, and
  `spawn` individually. Allowlisted HTTP remains the default explicit form;
  `http_unrestricted` names the broader research-host grant at the call site.
  Lampa deliberately registers unrestricted HTTP and uses its selected
  reasoner for both nested model calls and child turns (TM-CAP-004).

## 2026-08-18

* **Bounded event payloads**: Restored the process limit on canonical event
  payloads. Externalizing thread history moved model and tool output out of
  validated process memory and into the paged EventLog, where no Svit
  validation ran, so an activation could commit output beyond
  `max_text_bytes` and its terminal failure never reached subscribers.
  `ProcessEventLog` now validates the guest-visible payload of message and
  tool-completion events against the committed limits and fails the append
  closed (TM-DOS-003).
* **Content-hash tree**: Every committed node now publishes a structural
  SHA-256 content hash covering its own subtree and nothing above it, so an
  unchanged subtree keeps its hash across commits, forks, and snapshots. The
  root hash is that tree's root rather than a digest of the whole serialized
  root, `Process::node_hash` reads one node's hash, and a `Change` publishes
  the hash each reported path and its ancestors now have, with `None` for a
  removed path. Clients revalidate caches by content instead of discarding
  everything a change could have touched. Snapshot format 8 to 9; mount paths
  publish no hash because their content is external.
* **Host overlays are not commits**: Lampa's bounded thread projection is a
  host overlay, so appending to it refreshes only the overlay rows instead of
  reporting a process commit at the current version.
* **Non-blanking console cache**: A change notification reaches the root,
  because a write below a node changes that node's value and can change its
  child listing. Lampa discarded every touched entry, so any commit, including
  its own host-side thread-history append, left the console with nothing to
  paint until the next resolution. Resolved nodes, listings, and values now
  stay on screen marked stale and are replaced as each one is read again; a
  refreshed listing drops the rows the process no longer reports.
* **Everruns 0.18 kernel boundary**: Moved the pinned Everruns `main` commit to
  the 0.18 neutral-kernel layout. `everruns` no longer re-exports
  `everruns-core`, and the provider-facing surface Svit consumes at the host
  boundary (`typed_id`, `error`/`AgentLoopError`, `DriverRegistry`,
  `ModelSpec`, `Provider`, `ToolResultImage`) now lives in `everruns-provider`.
  Svit depends on `everruns-core` and `everruns-provider` directly instead of
  routing those imports through the facade.
* **Engine-owned sessions**: The one-shot native `llm` port now runs its turn
  through `everruns::Engine::create`, because engines own the session lifecycle
  and `Agent::session` is gone.
* **Capability configuration**: The process and compaction capability
  configuration uses `everruns::CapabilityRef`, the public name of the type
  core previously re-exported as `AgentCapabilityConfig`.

## 2026-08-17

* **Runnable reasoning contract reconciliation**: Made the canonical knowledge
  consistent with the implemented separation between bounded `/thread`
  metadata and the paged EventLog. Process transactions describe process-root
  changes only; canonical events, message presentation, recent-history reads,
  and Everruns compaction checkpoints use the EventLog. Lampa's tree history
  remains a bounded host overlay, not Svit state, a snapshot, or model context.
* **Execution and trust boundaries**: Recorded that model-facing `exec` can run
  transient inline source through the ordinary activation boundary, including
  ports and durable writes, while named `/lib` scripts are reusable durable
  source. Svit intentionally has no model-specific operation or script
  allowlist, but every guest source, input, value, snapshot, and interpreter
  boundary remains untrusted. The standard research HTTP transport is
  redirect-denying and in-memory; large responses must be reduced before a
  persistent or model-visible value boundary, with streaming and file-backed
  transfer deferred.

## 2026-08-16

* **Bounded thread-history presentation**: Canonical events remain in Svit's
  paged EventLog rather than serialized `/thread` state. Svit now exposes
  bounded recent-event reads and canonical-event observations; Lampa projects
  the latest 200 events and 500 messages as read-only `/thread/events` and
  `/thread/messages` rows without changing process snapshots or model context.

## 2026-08-15

* **Reasoning scenario suite**: Added the first deterministic model-driven
  acceptance scenario: a model writes `/lib/summarize-model-catalog`, runs it
  through `exec`, fetches the generic `models.dev/models.json` fixture through
  `/ports/http`, uses Svit Lisp `jq` to reduce it, and persists only the count
  and newest GPT records at `/memory/model_catalog`. The fixture exceeds the
  persistent value envelope; HTTP and jq carry it only as activation-local
  in-memory data until the script reduces it. Inline `exec` is available for
  one-off scripts, including port calls and durable writes; named `/lib`
  scripts are reserved for reuse.
* **Svit Lisp data library**: `jq` and `search` are now standard-library
  functions, rather than `/ports` capabilities. Jq decodes JSON text and JSON
  HTTP response bodies before evaluation, returns emitted values as an array,
  and has an end-to-end HTTP-to-jq script test covering the filtered-tree
  workflow. Search walks the transactional process tree directly.
* **Trusted Svit execution**: Removed the model-specific read/exec capability
  mode and script allowlists. A Svit reasoning loop always receives the
  complete process surface; attached ports and mounts remain the explicit
  host-authority boundary.
* **Tuika tree host**: Upgraded Lampa to Tuika 0.9 and replaced its local
  expanded-row, selection, viewport, and tree hit-testing mechanics with
  `TreeState` and `TreeList`. The process remains the sole durable state owner;
  Lampa supplies only lazily resolved path rows and labels.

## 2026-08-14

* **Scripted port composition**: Removed `L-005`. Svit-hosted Lisp now
  resolves both `/lib` scripts and the exact `/ports` ports attached by its
  host. An async port suspends guest execution; Svit executes it once and
  replays pure guest segments with recorded results before committing guest
  state once. External effects remain immediate and non-transactional under
  `L-036`. A persisted model-authored HTTP script and a post-HTTP rollback case
  provide end-to-end evidence. Bare `Process::exec` remains `/lib`-only because
  serialized process state contains no host authority, and reversed Lisp
  arguments receive an actionable diagnostic.
* **Bounded durable resume**: Turso now atomically replaces one validated
  recovery checkpoint every 32 process transactions and after replaying an
  older uncheckpointed tail. Resume mutates only its unpublished
  reconstruction, validates complete state and root hashes at bounded
  boundaries, and replays only the newer tail. Recovery blobs now store the
  process snapshot directly, and the resumed reasoning loop reuses the already
  validated thread projection instead of decoding it repeatedly through the
  Everruns event reader. On a copied 839-event Lampa `test1` store, the
  development build restored the 10 MB process in roughly one second and
  constructed reasoning in roughly 0.35 seconds, down from more than 100
  seconds without deleting retained history.
* **Compact store snapshots**: Advanced store snapshots to
  `svit-store-snapshot@2`, embedding the process image as structured JSON
  instead of an integer array while retaining reads of format 1.
* **Lampa transcript selection**: Integrated Tuika's captured mouse selection
  and OSC 52 clipboard path. Plain transcript drags highlight and copy visible
  text, `Ctrl+C` re-copies an active range, and memory-tree clicks keep their
  existing selection behavior.
* **Live reasoning timeline**: Lampa now follows newly committed
  `/thread/messages` entries, rendering intermediate model commentary and tool
  calls while a turn runs. Outbox observation marks completion only, and
  message-ID deduplication reconciles optimistic inbox display with the durable
  projection.
* **Compact tool rows**: Lampa now replaces a pending tool call with one
  Yolop-style completion row containing status, operation, target, and a
  bounded result summary. It derives port names from `/ports` exec paths and
  no longer exposes opaque tool-call IDs.
* **Turn completion**: Removed Svit's hidden eight-iteration override so
  Everruns owns its default loop policy. An explicit cap reached before a final
  answer now fails the Svit turn, retains the inbox item, and never publishes a
  tool call as completed outbox output.
* **Thread history limits**: Separated append-only `/thread/events` and
  `/thread/messages` collection bounds from the per-value entry budget. Every
  record remains independently validated, while the encoded thread and record
  counts have hard envelopes; recorded missing compaction as `L-047`.
* **Canonical domain model**: Made `Svit` the runnable unit that owns one
  reason/act loop, durable conversation thread, and `Process`. The embedding
  application is the host, `Reasoner` binds model and provider, an activation
  is one script transaction, and a turn is one reason/act cycle. Removed the
  separate “Svit agent” entity from current documentation.
* **Current persistence boundary**: Reconciled architecture, process,
  limitations, proposal, and public skill guidance with the implemented
  `Svit::persisted` path. Local Turso persistence covers inbox, reasoning
  events, derived messages, port refresh, acknowledgements, memory,
  snapshots, and forks; durable control receipts, crash qualification, and
  distributed ownership remain open under `L-006`.
* **One operation vocabulary**: Restated the complete process contract as
  `discover`, `read`, `stat`, `write`, `remove`, and `exec` across host calls,
  model tools, and Svit Lisp.

## 2026-08-13

* **One persistence stream**: Named the sole address-keyed envelope
  `ProcessTransaction`; canonical Everruns events remain values appended under
  `/thread/events`, with `/thread/messages` committed as their checked
  projection rather than as a second log.
* **Adapter contract**: Made transaction construction, canonical encoding,
  decoding, integrity validation, and replay available independently of Turso
  through `ProcessTransaction`, `TransactionHead`, and `TransactionQuery`.
  Storage CAS, fencing, ambiguous-write recovery, forks, cuts, and snapshot
  evidence remain adapter obligations (`L-041`).
* **Durability claim boundary**: Documented the S3 conditional-head mapping and
  kept distributed Durable Object guarantees, durable control receipts, and
  formal/model-checked evidence explicitly under implementation.
* **Everruns host composition**: Advanced the pinned Everruns `main` revision
  and made Svit's embedding boundary explicit. Svit now supplies a narrow
  `HostComposition` containing its process capability and selected provider
  driver; `HostBackends` remains the separate process-backed event-store
  bundle. Everruns' still-current `InProcessRuntime` stays private behind the
  `Svit` contract. The updated graph no longer includes the unused fetch, Bash,
  and A2A dependency trees, so `L-039` and their stale audit and license
  exceptions are retired.
* **Compile-checked script embedding**: Added `svit_script!` for direct Lisp
  forms, source string literals, and package-relative files. It catches Lisp
  parser/compiler failures during the Rust build while leaving configured
  limits and execution semantics at the process boundary.
* **Script test harness**: Added `svit_script_test!`, which installs one subject
  in a fresh real process and requires the test body to exercise and assert the
  activation behavior.
* **Build-time compiler boundary**: Compile checking uses a fresh restricted
  Ketos interpreter with null I/O, a null module loader, and bounded resources;
  added `TM-ESC-004` and `TM-DOS-011` with focused evidence.
* **Complete commit observation**: Moved transient commit publication into the
  shared process-state transition boundary. Host operations, reasoning tools,
  inbox transitions, Lisp activations, thread events and metadata, and port
  catalog refresh now publish after the owned read projection is current.
* **Stable batched navigation**: Lampa retains the first selected path across
  all commit notifications drained before a frame. Temporary ancestor rows
  produced during invalidation can no longer replace operator selection or
  collapse the perceived navigation context.
* **Per-instance Lampa stores**: Each lowercase filesystem-safe instance ID now
  maps to `instances/{instance-id}/svit.db` below `LAMPA_DATA_DIR`; existing
  databases fail closed when their root address does not match.
* **Explicit process import**: `ProcessStore::import` records an `imported` base
  at the current version and root hash, then starts a new event tail. Lampa's
  `--import-legacy` uses it once for a selected address without pretending to
  preserve the source event or fork topology (`L-046`).

## 2026-08-12

* **Virtual mounts**: Replaced construction-time snapshot imports with virtual
  mounts. `/mounts/<name>` commits only a descriptor (kind, host-disclosed
  source, locality, and granted access) while nodes below it resolve through a
  host-attached `MountProvider` when they are read, discovered, stated, or
  written. Mount size is now independent of process and snapshot size.
* **Node facts and locality**: Added `stat(path)`, which answers with one facts
  vocabulary for the whole tree: kind, access, locality, mount, path, source,
  attachment, and provider facts such as byte size, modification time, and a
  folder mount's git branch and commit. `locality` states the cost class
  (`cache`, `local`, or `remote`), so a caller can weigh a read before making
  it. Committed state reports `cache`; a materialized Turso query says `cache`
  rather than claiming a live remote view.
* **Granted mount writes**: A descriptor grants `read`, `write`, or
  `read-write`. Activation writes and removals below a granted mount are
  buffered and applied at the commit point after every in-process validation,
  so a failed activation applies none of them. This orders the effect with the
  commit; it is not distributed atomicity (`L-042`, `TM-EFF-006`).
* **Authority is not serialized**: Providers are runtime state. A restored
  process reads its mount descriptors with `attached: false` and fails closed
  below them until the host calls `attach_mount` (`TM-CAP-008`).
* **Path containment**: Mount paths parse into validated segments that reject
  traversal, separators, empty, oversized, and NUL content, and the folder
  provider re-checks links and special files at every segment on every
  resolution (`TM-CAP-007`, `TM-CAP-003`).
* **Ports inside mounts**: `PortContext` gained `stat`, and `search`
  walks a mount subtree node by node under an independent node budget instead
  of materializing it, reporting when a bound truncated the walk
  (`TM-DOS-010`).
* **Lampa mounts the working directory**: The console mounts the current
  directory read-only as `cwd` and accepts `--mount name=path` and
  `--mount-rw name=path`.
* **One console namespace**: Lampa holds no copy of the process root and no
  separate mount browser. It resolves every node (memory, scripts, system
  metadata, or mounted folder) through `discover`, `stat`, and `read`, so
  mounts are represented through the same interface as the rest of the tree.
  Listings are fetched on expand, content on selection and for array-item
  summaries, bounded per directory, and re-resolved on each commit. A `content`
  fact on directory nodes completes the vocabulary the console needs to
  distinguish an array from a map without reading it. Restoring the selected
  row across a commit is now explicit state rather than a side effect of
  holding the whole root.
* **One change stream**: Every transition now returns a `Change` with the new
  version, canonical changed paths, and replayable mutations, folded from one
  source so a live observer and a stored durable event describe the same
  transition. `SvitEvent::Committed(Change)` replaces the payload-free ping,
  and the Turso adapter stopped hand-building a parallel mutation per host
  call. A granted mount write reports its path but carries no mutation: it
  changed an external source, not committed state. Notifications carry paths
  without values, so observers read state back through the API.
* **Precise invalidation**: `Change::touches` is the shared staleness
  predicate (at, below, or above a changed path), so every client invalidates
  identically. Lampa keeps unrelated nodes resolved across a commit, and `r`
  reloads the tree for external mount edits no event can report (`L-045`).
* **Compatibility**: Bumped snapshots to format 7 for the descriptor-only
  `/mounts` schema and the `max_mount_entries` and `max_mount_writes` limits.

## 2026-08-11

* **Single-Svit event persistence design**: Adopted one uniform address-keyed
  transaction event type in a local Turso database. An immutable base plus event
  tail supports deterministic resume, exact-position forks, complete memory
  changes, SQL queries, atomic receipts, on-demand snapshots, and safe history
  cuts. Reasoning events are ordinary tree mutations rather than a second agent
  event domain. Turso transactions own event append, head CAS, projections,
  fork references, and cuts; the portable event contract remains adapter-neutral.
* **Local Turso process store**: Implemented address-keyed base-plus-event-tail
  persistence for host mutations, activations, inbox transitions, deterministic
  resume, hash-validated queries, exact-boundary forks, on-demand snapshots, and
  safe cuts. Head CAS keeps failed stale writes out of both Turso and the local
  process. Control receipts remain explicit follow-up work rather than being
  claimed complete.
* **Durable Svit owner**: Added `Svit::persisted` over the adapter-neutral
  `DurableProcessHandle`. Inbox transitions, model-visible process tools,
  append-only canonical reasoning events, derived messages, and port
  catalog refresh commit through one serialized owner before Svit refreshes its
  read projection.
* **Persistent Lampa instances**: Lampa now creates or resumes
  `svit://local/lampa/{instance-id}` in one shared database below the
  platform-native user data directory. `--instance` selects the address and
  `LAMPA_DB` overrides the database path. Restarting with the same store and
  instance ID recovers committed memory, thread history, and pending inbox;
  different instance addresses remain isolated.
* **Persistence adapter boundary**: Added adapter-neutral `ProcessStore`,
  `DurableProcessHandle`, event-record, and snapshot-record traits. The default
  `TursoProcessStore` implementation is now behind the enabled-by-default
  `persistence-turso` feature; `--no-default-features` compiles the runtime and
  contracts without Turso, while `turso-mount` can be selected independently.
* **Everruns values at the boundary**: Removed Svit's public `AgentModel`
  wrapper. A `Reasoner` now pairs the provider-visible model ID with the
  Everruns `Provider`, including `OpenAI` configuration, and constructs the
  credential-free `ModelSpec` at the host boundary.
* **Facade for ordinary calls**: Native one-shot `llm` execution now uses the
  `everruns::Agent` facade. Only the process-owned loop uses `everruns-host`,
  because it must install Svit's canonical process-backed `EventLog`.
* **Compact host assembly**: The process-owned loop now seeds its harness,
  agent, and resumable session through Everruns' `single_session` builder.
* **Svit-owned prompt**: Removed the independent agent name and public
  `system_prompt` configuration. The process address now identifies the
  Everruns harness and agent. Svit composes its own base prompt, including
  durable memory-tree guidance, appends optional application `instructions`
  inside an `<instructions>` block, persists instructions separately, and
  recomposes fork prompts for the child address. Agent state advanced to
  `svit-thread@6`.
* **Reasoner boundary**: Replaced independent model/provider setters with one
  `Reasoner` value across Svit and model-backed ports. The public error is
  now `SvitError`, avoiding agent terminology in the configuration API;
  ports remain separate explicit host capabilities.
* **Thread projection**: Renamed the durable public `/agent` projection to
  `/thread`, renamed the internal adapter and runnable example around reasoning,
  advanced thread state to `svit-thread@6`, and advanced snapshots to format 6.
* **Lampa value previews**: Shallow container previews now show bounded scalar
  child values while keeping nested containers summarized by kind and item
  count.
* **Lampa mouse selection**: Plain left clicks on visible memory rows now map
  through the current list viewport and update the selected value preview.
* **Port extensions**: Split each port into its own module
  and replaced the closed configuration switch with host registration through
  `Port` and `PortExtension`, following Bashkit's later-wins rule.
  The common boundary bounds JSON input/output and exposes committed reads only.
* **One port setup path**: Removed the parallel native setup and
  custom-transport builder operations. Hosts always attach one `Ports`
  registry through `ports`; `Ports::standard()` marks the complete set
  for reasoner resolution during Svit construction. The standard research set
  grants unrestricted HTTP destinations, while `with_http_allowlist`
  attenuates that grant and later registrations win.
* **Lampa port catalog**: Attached the standard ports to Lampa.
  Svit's standard registry derives `llm` and `spawn` from its reasoner;
  Lampa supplies no HTTP policy. Svit owns the reusable redirect-denying,
  response-bounded reqwest transport.
* **Svit host contract**: Committed transitions publish notifications rather
  than process state. Hosts obtain owned value/version observations through an
  atomic Svit operation and cannot retain the mutable process tree. `Inbox` is
  the input sink; `Outbox` and `Events` are explicit independent observers over
  private stream implementations. Terminal failures use the event observer with
  sanitized diagnostics. Lampa consumes this contract instead of polling or
  maintaining runtime configuration.
* **Lampa host boundary**: Removed the standalone direct-`Process` script
  runner. Lampa now has one mode: configure a Svit instance and present its
  lifecycle, events, inbox, and outbox.
* **Lampa list labels**: Array rows now show bounded scalar text, identifying
  object fields when available, and container summaries otherwise.
* **Lampa tree viewport**: The memory tree now persists its visible window so
  mouse selection does not recenter lower rows. Initial expansion stops at the
  root, leaving top-level process sections closed.
* **Lampa Markdown transcript**: Conversation text now uses Tuika's Markdown
  formatting at the panel width, and Lampa's terminal backend emits bare web
  URLs as OSC 8 hyperlinks.

## 2026-08-10

* **Everruns main**: Moved the workspace dependency from Everruns 0.17.25 to
  the upstream `main` branch; `Cargo.lock` fixes the reviewed source commit.
* **Host abstractions**: Replaced the retired runtime compatibility surface,
  writable message store, event bus, and driver registry with
  `everruns-host` `HostBackends`, the coherent `EventLog`/`EventReader` SPI,
  and separate `ModelSpec`/`Provider` configuration.
* **Canonical replay**: Advanced agent state to `svit-agent@4`, adopted the
  initial `session.started` event, and reject resumed event streams whose
  session, IDs, or contiguous sequence violate the event-log contract.

## 2026-08-09

* **Everruns loop engine**: Replaced Agentyk with Everruns 0.17.25 behind the
  process-owned `svit::Svit` API. Svit supplies the Everruns runtime with a
  process-backed event bus, message store, and typed process capability.
* **Durable projection**: Advanced agent state to `svit-agent@3`. Canonical
  Everruns events and their exact derived message projection commit under
  `/thread` and are revalidated on resume.
* **Provider surface**: Added `AgentModel` for the deterministic Everruns
  simulator, Everruns' OpenAI Responses driver, and host-provided Everruns
  driver registries. Removed the `svit-agentyk` adapter and Lampa's custom
  Responses driver.
* **Dependency boundary**: Recorded `L-039` for Everruns' unconditional,
  unused fetch, Bash, and A2A dependency graph. Audit and license exceptions
  are exact and temporary; Svit registers none of those capabilities.
* **Consumer migration**: Ported Lampa, both support-agent examples, native
  model/spawn executables, integration tests, and runnable examples to the
  process-owned Everruns path.

## 2026-08-07

* **Ports**: Added `/ports/search` and `/ports/jq` over
  committed process text and explicit JSON, with no shell or ambient host
  interface.
* **Port discovery**: Added host-managed `/ports` manuals derived from
  installed native implementations, including schemas, output contracts,
  effect classes, and limits. Resume refreshes the catalog from current host grants.
* **Explicit effects**: Added default-deny, host-allowlisted HTTP plus optional
  host-routed transport and a fixed host-selected `llm` tool. Both remain
  outside Svit activation transactions and replay guarantees.
* **Child execution**: Named process creation `spawn` rather than overloading
  transactional script `exec`; it forks committed state, runs one child turn,
  rejects duplicate local addresses, and exposes child snapshots to the host.
* **Tool security**: Added `TM-DOS-008`, `TM-ESC-003`, `TM-EFF-005`,
  `TM-FORK-002`, and `TM-CAP-004` with focused reasoning-loop evidence for limits,
  host isolation, effect grants, fork lineage, and network policy.
* **Tool limitations**: Recorded the bounded jq subset, non-transactional
  HTTP/model effects, and the non-durable local child registry as `L-035`
  through `L-037`.
* **Dependency review**: Added direct jaq, regex, and URL dependencies for the
  native implementations; no shell runtime is included.
* **Compatibility**: Advanced process snapshots to format 5 for the durable
  `/ports` port catalog; agent state remains `svit-agent@2`.

* **Lampa**: Added persistent inbox/outbox chat, complete committed process
  memory, and JSON item-preview panels with headless UI evidence.
* **Agent ownership**: Added `svit::Svit` as the process-owning reason/act
  API, with Agentyk as its internal loop engine rather than an external agent
  that consumes Svit as a capability.
* **Process lifetime**: Added `Svit::start`, cloneable `Inbox` handles,
  completed-turn outbox listeners, and blocking drain/join. There is no
  separate entrypoint message.
* **Durable inbox**: Host sends commit to `/inbox` before waking the loop;
  successful turns acknowledge the exact observed head and failures retain it.
* **Message envelope**: Inbox and live outbox use Agentyk `Message` values with
  ordered `ContentPart` values rather than plain input and `TurnResult` output.
* **Runtime projection**: `/thread` now exposes the configured system prompt,
  event-derived message history, and canonical Agentyk events through the
  ordinary read-only runtime surface. Agent state format advanced to
  `svit-agent@2`.
* **Durable thread**: Added host-managed, guest-readable `/thread` state so
  snapshots, restores, and forks carry the committed conversation event log.
* **Subagents**: Defined a subagent as a Svit agent built around a forked child
  process; child turns inherit committed history and isolate future mutation.
* **Consumer example**: Added credentialed `support-agent-svit`, using
  `gpt-5.6-terra` through one process-owned Svit inbox and outbox. Deterministic
  lifecycle, snapshot, and fork evidence remains in the test suite.
* **Audit boundary**: Added `TM-AUD-001` and executable evidence preventing
  guest scripts and model tools from rewriting durable replay state.
* **Compatibility**: Bumped snapshots to format 4 for the `/thread` root node.
* **Limitations**: Recorded the one-thread-per-process constraint and the
  non-atomic boundary between agent event commits and external model/tool calls.

## 2026-08-06

* **Agent authority**: Added an attenuated Agentyk capability mode that exposes discovery, reads, and only host-allowlisted scripts; generic mutation remains available only through the explicit full-access constructor.
* **Support commit contract**: Bound support retrieval and commit to a host-issued request ID, derived source and ticket data from process state, rejected duplicate or policy-invalid commits atomically, and made the validated committed answer authoritative over model final text.
* **Security evidence**: Added `TM-CAP-002` with focused adapter and deterministic simulated-agent tests for tool attenuation, script denial, request-binding rollback, idempotency, provenance, deterministic ticket policy, and committed response rendering.
* **Snapshot mounts**: Added bounded, read-only construction-time imports for
  real UTF-8 folders and host-selected Turso query rows under `/mounts`.
* **Authority boundary**: Mounts persist values, kind, and mode but never host
  paths, database connections, query capability, or other live authority.
  Folder imports reject symbolic links and special files.
* **Evidence**: Added focused link-rejection and read-only rollback tests plus
  an executable example that reads both mount kinds and verifies deterministic
  results.
* **Consumer example**: Moved support documents from embedded process memory to
  a real folder snapshot and added Turso-backed account context; the support
  search script consumes both mounts before the model commits its response.

## 2026-08-05

* **Process namespace**: Adopted the conventional `/memory`, `/lib`, `/tasks`, `/inbox`, `/children`, `/mounts`, and `/system` hierarchy. Deferred top-level nodes are validated as empty and read-only rather than claiming unimplemented behavior.
* **Builder vocabulary**: Replaced the initial `script(name, script)` builder method with `library(name, script)` so process assembly maps directly to `/memory` and `/lib`.
* **Generic operations**: Unified Rust, agent-tool, and Svit Lisp access as `discover`, `read`, `write`, `remove`, and `exec` over absolute process paths. Library entries now use the same typed write and remove boundary as memory instead of separate post-build script APIs.
* **Nested execution**: Svit Lisp 2 adds transactional named-script `exec`; nested calls share working state and the outer deadline, roll back on failure, and are independently depth-bounded by `max_exec_depth`.
* **System discovery**: Added validated runtime metadata for logical identity, generic API operations, limits, fork lineage, language and snapshot format, the empty capability set, and buffered outbox. Logical identity is explicitly marked unauthenticated.
* **Compatibility**: Bumped snapshots to format 3 because the canonical root schema now includes the conventional hierarchy and system metadata; formats 1 and 2 restore fail closed.
* **Language identity**: Adopted `.svit-script` for standalone Svit Lisp source and virtual script-library diagnostics, reserved `.svit` for an unimplemented future manifest format, and kept Ketos as an interpreter implementation detail.
* **Runtime replacement**: Replaced Luau through `mlua` with the pure-Rust Ketos interpreter and defined the versioned [Svit Lisp Runtime](runtimes/lisp-runtime.md).
* **Guest contract**: Separated lexical variables from durable process state through explicit generic operations; added immutable typed maps and arrays plus bounded log and message functions.
* **Security boundary**: Installed null I/O and a module loader that rejects every Ketos module, created a fresh interpreter for every activation, and retained one post-validation commit point.
* **Limits**: Adopted Ketos wall-time, stack, namespace, syntax, integer, and abstract-memory restrictions. Recorded the absence of deterministic instruction fuel and allocator byte caps as limitations rather than preserving Luau-specific names.
* **Dependency review**: Recorded Ketos 0.12's unconditionally declared, obsolete REPL dependency stack as `L-025`; audit exceptions are exact and limited to crates that Svit does not expose.
* **Compatibility**: Bumped snapshots to format 2 because stored scripts and serialized limit semantics changed; format 1 restores fail closed.
* **Evidence**: Migrated unit, integration, protocol, documentation, CLI, and executable-example coverage to Lisp, including rollback, replay, fork isolation, module denial, fresh globals, and every activation buffer limit.

## 2026-07-31

* **VAST semantics**: Named and specified Versioned Atomic State Transitions as the control protocol's per-process concurrency and commit model without changing the `svit-control@1` wire identifier or extending the distributed ownership claim.
* **Protocol maintenance**: Adopted major-version negotiation, capability-gated evolution, additive-field compatibility, canonical schema and metadata artifacts, drift guards, conformance vectors, and trusted tenant partitioning requirements after comparing Mira and ACP. Added executable compatibility and exact wire-shape tests; schema generation and remote initialization remain required before wire stabilization.
* **Control protocol**: Added versioned multi-client activation envelopes, linearizable per-process version checks, bounded retry receipts, explicit conflict outcomes, public protocol documentation, and concurrent-client evidence. Transactions stop at the process root and outbox; external systems remain outside the atomic boundary.
* **Documentation**: Separated the public [Svit vision](../docs/vision.md) from the internal [research proposal](research/proposal.md), preserving detailed hypotheses, alternatives, experiments, and open decisions in the knowledge bundle.
* **Delivery**: Defined `main` as a curated semantic history, made pull requests optional coordination artifacts that squash-merge when used, and prohibited unstable pull-request references in release-facing records.
* **Implementation**: Added the first executable Rust slice with transactional Svit Lua activations, one state root, named self-authored scripts, buffered message intents, typed hooks, snapshots, replay, and isolated forks.
* **Examples**: Added deterministic examples for durable memory, self-authored libraries, atomic rollback, forked research, sandbox denial, and execution limits.
* **Evidence**: Added unit, integration, rollback, replay, fork, snapshot-tamper, heap-limit, diagnostic, and sandbox tests. Threat statuses now distinguish mitigated, partial, required, and not-applicable controls.
* **Creation**: Established the OKF v0.2 bundle and maintenance contract.
* **Scope**: Recorded the initial transactional process vertical slice in [Architecture](foundations/architecture.md), [Process Model](foundations/process-model.md), and the original Lua runtime contract, now superseded by the [Svit Lisp Runtime](runtimes/lisp-runtime.md).
* **Security**: Added the initial [Threat Model](security/threat-model.md) and [Security Testing](security/security-testing.md) requirements without marking unimplemented controls as mitigated.
* **Operations**: Defined the initial [Testing Strategy](operations/testing.md) and [Limitations](operations/limitations.md).
