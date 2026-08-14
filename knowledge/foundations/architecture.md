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
                        +----> opt-in `/bin` built-ins
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
| Reasoning loop | Everruns `Message`/`ContentPart` inbox and outbox with canonical events under host-managed `/thread` |
| Activation | Fresh guest execution, working state, output, logs, and intents |
| Script library | Named source records stored with committed process state |
| Mounts | Virtual external namespaces resolved lazily through host-attached providers, with committed descriptors, node facts, and granted access |
| Lisp adapter | Converts values and exposes only the versioned Svit Lisp surface |
| Snapshot | Versioned deterministic JSON encoding, SHA-256 root hash, restore validation, and fork source |
| Process controller | Serializes multi-client commands, enforces version preconditions, and retains bounded retry receipts |
| Persistence | One canonical `ProcessTransaction` stream per process; adapter-neutral envelope/reducer plus adapter-owned CAS, snapshots, forks, cuts, recovery, and fencing |
| Built-ins | `/bin` process search and JSON filtering with explicit host grants for HTTP, model calls, and local child execution |

The current workspace implements the process and process-owned reasoning loop in
the `svit` crate and provides an interactive three-panel tree host in Lampa.
Lampa maps one lowercase filesystem-safe instance ID to both
`svit://local/lampa/{instance-id}` and
`instances/{instance-id}/svit.db` below its user-data root. Each instance owns
one local Turso store; an existing file must contain the matching root address.
The entry point creates, resumes, or explicitly imports that process and builds
one persisted `Svit`; the TUI thereafter sends only through its durable inbox
and consumes commit notifications, completed-turn outbox messages, and terminal
failure events.
After a commit notification it reads an owned root/version pair through the
`Svit` contract. It never retains a direct reference to `Process`.
The contract exposes a cloneable `Inbox` sink and creates independent `Outbox`
and `Events` observers for transient host consumption; Tokio broadcast channels
remain an implementation detail behind those ports.
Tree expansion and selection remain local UI state, so the TUI does not become
another process-state owner or poll the runtime. Raw durable reasoning events
remain part of the process tree; the timeline does not duplicate message
events already rendered as chat. When several commits arrive before one frame,
Lampa retains the original selected path until the refreshed ancestors resolve;
an intermediate partial tree never replaces operator navigation state. Svit
binds the provider-visible model ID and host-owned provider into a
credential-free `ModelSpec`. Svit is an
advanced Everruns host: it composes the compact single-session host builder and
an explicit `HostComposition` containing only the Svit capability and selected
provider driver. The separate `HostBackends` store bundle installs Svit's
process-backed `EventLog`, while Everruns rebuilds runtime history from that
canonical log. `InProcessRuntime` remains Everruns' current execution mechanism
for advanced embedders; it is kept behind the Svit contract rather than exposed
as Svit's public abstraction. A persisted Svit serializes every mutation
through its adapter-owned `DurableProcessHandle` and updates a cloned committed
read projection only after storage accepts the process transaction. Reasoning
events are appended values within those transactions, never a second
persistence stream. Svit's standard built-in setup derives local
`search` and `jq` plus model-backed `llm` and `spawn` from the instance
configuration. Lampa selects that standard registry without additional HTTP
policy; selecting the complete research registry explicitly grants unrestricted
HTTP destinations. Svit supplies the reusable redirect-denying,
response-bounded transport, and other hosts may attenuate destinations with an
allowlist.
Each append commits the canonical event and its derived guest-readable message
projection together in the Svit process root and, when persisted, in the same
Turso event transaction. One `Reasoner` owns the
provider-visible model ID and host-owned provider, so Svit cannot represent a
partially configured reasoning loop. Built-ins remain separate because their
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
`SvitEvent::Committed` is deliberately notification-only. The atomic
`Svit::read_versioned` operation returns an owned value/version observation and
keeps the mutable process tree behind the Svit abstraction.
`svit::Svit` assembles the Everruns host internally and can expose the complete
generic process surface or attenuate a model to discovery, reads, and a
host-selected script allowlist. Everruns owns the public model, provider, and
loop contracts; Svit owns process state and its canonical event-log adapter.
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
9. Durable loop and replay state is host-managed under `/thread`; untrusted
   scripts and model tools can inspect the configured prompt, derived message
   history, and canonical events but cannot rewrite them.
10. Inbox messages commit before loop notification and are acknowledged only
    after the corresponding turn succeeds.
11. Built-ins remain outside Svit Lisp. They receive typed values and
   a read-only process context, not a shell or implicit ambient host interface.
   Built-ins receive only the HTTP, model, or child-runner authority explicitly
   supplied by the host; custom implementations are trusted native extensions
   and may capture additional explicit host capabilities.
12. `/bin` is generated from attached built-ins and refreshed
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
- Executing external effects inside the transaction was rejected because a
  rollback cannot undo them. The slice records message intents only.
