---
type: Architecture
title: Architecture
description: Trusted-core boundaries and scope of the first executable Svit slice.
tags:
  - svit
  - architecture
  - rust
---

# Architecture

## Status

Initial executable vertical slice implemented. Hardening remains in progress.

## Decision

Svit is a Rust library for durable reasoning over isolated processes. One
`Svit` owns a reason/act loop, a durable conversation thread, and exactly one
process. The process is an actor-like state machine: it handles one transition
at a time and owns one committed state root. A `Reasoner` binds its model and
provider. Everruns implements the current loop behind `svit::Svit`.
Everruns owns the default turn-iteration policy; Svit applies a maximum only
when its host explicitly configures one. Reaching that maximum before a final
answer is a failed Svit turn: the inbox message remains queued and no tool-call
message is published as completed outbox output.
Parallelism comes from independent processes, not shared guest memory.

The first executable slice contains:

```text
Host application ----> Svit
                        |
                        +----> Everruns reason/act loop
                        +----> durable thread / inbox
                        +----> transient outbox / events observers
                        |
                        v
                    Process ----> transaction working copy ----> commit or rollback
                        |                    |
                        |                    v
                        |               restricted Lisp VM
                        |
                        +----> snapshot / restore / fork
                        +----> buffered message intents (not delivery)
                        +----> bounded folder / Turso query mounts
                        +----> opt-in `/ports` ports
```

The trusted core owns validation, resource accounting, transaction boundaries,
canonical serialization, process identity, and state isolation. The embedded
Ketos interpreter is treated as a component inside that boundary, not as proof of
the boundary.

## Initial module responsibilities

| Responsibility | Purpose |
| --- | --- |
| Value model | Bounded serializable guest values with deterministic encoding |
| Process | Address, version, committed root, limits, and lifecycle operations |
| Reasoning loop | Everruns `Message`/`ContentPart` inbox and outbox with canonical events in a process-partitioned paged EventLog; `/thread` holds bounded metadata only |
| Activation | Fresh guest execution, working state, output, logs, and intents |
| Script library | Named source records stored with committed process state |
| Mounts | Virtual external namespaces resolved lazily through host-attached providers, with committed descriptors, node facts, and granted access |
| Lisp adapter | Converts values and exposes only the versioned Svit Lisp surface |
| Snapshot | Versioned deterministic JSON encoding, structural SHA-256 root hash, restore validation, and fork source |
| Process controller | Serializes multi-client commands, enforces version preconditions, and retains bounded retry receipts |
| Persistence | One canonical `ProcessTransaction` stream per process; adapter-neutral envelope/reducer plus adapter-owned CAS, recovery checkpoints, snapshots, forks, cuts, and fencing |
| Ports | `/ports` host integrations with explicit grants for HTTP, model calls, and local child execution |

The current workspace implements the process and process-owned reasoning loop in
the `svit` crate and provides an interactive three-panel tree host in Lampa.
Lampa maps one lowercase filesystem-safe instance ID to both
`svit://local/lampa/{instance-id}` and
`instances/{instance-id}/svit.db` below its user-data root. Each instance owns
one local Turso store; an existing file must contain the matching root address.
The entry point creates, resumes, or explicitly imports that process and builds
one persisted `Svit`; the TUI thereafter sends only through its durable inbox
and consumes commit notifications, completed-turn outbox signals, and terminal
failure events. Its conversation timeline receives projected messages from the
post-commit observer and reads only a bounded durable tail on startup, so
intermediate model commentary, tool calls, and tool results appear promptly.
After a commit notification it reads an owned root/version pair through the
`Svit` contract. It never retains a direct reference to `Process`.
The contract exposes a cloneable `Inbox` sink and creates independent `Outbox`
and `Events` observers for transient host consumption; Tokio broadcast channels
remain an implementation detail behind those ports.
Tree expansion, stable-path selection, ancestor fallback, viewport retention,
and tree hit testing remain local UI state through Tuika's `TreeState` and
`TreeList`, so the TUI does not become another process-state owner or poll the
runtime. Raw durable reasoning events
remain outside the process tree; the timeline projects their derived messages
and deduplicates optimistic inbox display by message ID. The transient outbox
only marks a turn complete; it is not a second message source. Tool presentation
correlates each result with its preceding call, replaces a pending row when the
result arrives, and shows status, operation, target, and a bounded outcome on
one line instead of exposing the internal call ID. Transcript selection is also
local UI state: Tuika resolves and paints plain mouse drags over the visible
transcript, and Lampa writes the selected text through OSC 52 after release.
Memory-tree presses remain tree operations and clear any transcript selection.
When several commits arrive before one frame,
Lampa retains the original selected path until the refreshed ancestors resolve;
an intermediate partial tree never replaces operator navigation state. Svit
binds the provider-visible model ID and host-owned provider into a
credential-free `ModelSpec`. Svit is an
advanced Everruns host: it composes the compact single-session host builder and
an explicit `HostComposition` containing only the Svit capability and selected
provider driver. The separate `HostBackends` store bundle installs Svit's
process-backed `EventLog` and durable compaction-checkpoint store. Everruns
rebuilds a checkpoint plus its necessary event suffix. `InProcessRuntime` remains Everruns' current execution mechanism
for advanced embedders; it is kept behind the Svit contract rather than exposed
as Svit's public abstraction. A persisted Svit serializes every mutation
through its adapter-owned `DurableProcessHandle` and updates a cloned committed
read projection only after storage accepts the process transaction. Canonical
reasoning events append through the separate EventLog, never as process-tree
values or `ProcessTransaction` mutations. Svit performs no implicit port
derivation. Hosts individually register `http`, `llm`, `spawn`, or custom ports
and pass one frozen registry to the Svit builder. Lampa explicitly registers
unrestricted HTTP plus model-backed `llm` and `spawn` ports. Svit supplies the
reusable redirect-denying, in-memory HTTP response transport, and other hosts
may attenuate destinations with an allowlist. A script must reduce a large
response before it crosses the persistent or model-visible value boundary;
file-backed transfer remains open.
Each append commits the canonical event before its derived message is observed;
it never grows the Svit process root. One `Reasoner` owns the
provider-visible model ID and host-owned provider, so Svit cannot represent a
partially configured reasoning loop. Ports remain separate because their
external authority is not part of reasoning identity.
The process address is the Svit identity; there is no independent runtime name.
Svit owns the base system prompt, including the generic requirement to use
the memory tree for durable facts and working state, and derives the prompt from
that address. Optional application `instructions` are appended inside an
`<instructions>` block and persisted separately so restore preserves them and
fork can recompose the prompt for the child address.
The value preview is a read-only presentation adapter. Tree rows retain only a
path locator rather than cloning their persistent subtrees. Container previews
render bounded shallow child summaries with scalar values inline and nested
containers summarized by kind and item count. Array tree rows inline a bounded
scalar preview; object items prefer a conventional identity field, then their
first scalar field, then a kind/count summary. Leaf previews are capped before
classification as embedded JSON, source code, or Markdown. Formatted lines are
cached by selection and width, and each frame gives the scroll view only its
visible window. These presentation bounds prevent render work from scaling with
the entire process tree and do not change committed process state.
`SvitEvent::Committed` is deliberately notification-only. `SvitEvent::CanonicalEvent`
and `SvitEvent::Message` are likewise host observations over the paged event
history, not process-tree nodes. The atomic `Svit::read_versioned` operation
returns an owned value/version observation and keeps the mutable process tree
behind the Svit abstraction.
`svit::Svit` assembles the Everruns host internally and exposes one complete
generic process surface. Everruns owns the public model, provider, and loop
contracts; Svit owns process state and its canonical event-log adapter.
Module names may evolve; the ownership boundary is the decision.

## Trusted-boundary rules

1. Guest code receives no ambient host authority.
2. The host converts and validates all values at the guest boundary.
3. An activation changes committed state only through one successful commit.
4. Message sends are buffered data committed with state; this slice does not
   deliver them.
5. Each activation uses a fresh guest VM so globals cannot leak between
   activations or tenants.
6. Snapshot bytes are untrusted on restore and pass the same invariant checks
   as newly created state.
7. Every mutating control request carries an expected process version and is
   checked at the same serialization point as commit.
8. External data enters only through a host-created mount; the committed root
   stores mount identity, resolution runs through a host-attached provider
   under the descriptor's granted access, and activations receive persistent
   values, never filesystem or database handles.
9. `/thread` is bounded host-managed session metadata. Canonical events and
   compaction checkpoints are host infrastructure, so untrusted scripts and
   model tools cannot rewrite or materialize history in process memory.
10. Inbox messages commit before loop notification and are acknowledged only
    after the corresponding turn succeeds.
11. Svit Lisp may select a host-attached port by its `/ports` path. Svit
   suspends and replays the guest around that async call; the port itself
   remains trusted host code receiving typed values and a read-only committed
   process context, not a shell or implicit ambient host interface. Ports
   receive only the HTTP, model, or child-runner authority explicitly supplied
   by the host; custom implementations are trusted native extensions and may
   capture additional explicit host capabilities.
12. `/ports` is generated from attached ports and refreshed
   on resume. It describes runtime availability but never grants authority.

## Deferred architecture

Schedulers, durable effect delivery, read-through or writable projections,
durable control receipts, distributed routing, auth services, migrations
between hosts, and production Wasm/OS isolation are outside this slice. See
[Limitations](../operations/limitations.md).

## Alternatives considered

- A single long-lived Lisp environment was rejected because it couples durable
  state to interpreter internals and makes rollback, serialization, and tenant
  isolation harder to state.
- Serializing Lisp stacks, closures, quotations, or foreign values was rejected. Only
  committed Svit values and script source cross activation boundaries.
- Treating external effects as transactional was rejected because rollback
  cannot undo them. A Svit-hosted script may invoke a port immediately;
  Svit records its result while replaying pure guest segments and commits guest
  state only after the complete script succeeds. Durable effect receipts and
  exactly-once recovery remain deferred.
