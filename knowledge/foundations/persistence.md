---
type: Architecture
title: Single-Svit Event Persistence
description: Turso-backed address-keyed transaction events with resume, fork, query, snapshot, and cut semantics.
tags:
  - svit
  - persistence
  - event-sourcing
  - turso
  - sqlite
---

# Single-Svit Event Persistence

## Status

Design adopted. The local Turso `DurableProcess` slice is implemented for
create, host write/remove, activation, inbox enqueue/acknowledgement, resume,
fork, query, on-demand snapshot, and cut. Wiring the runnable reasoning `Svit`,
the control protocol's receipts, and built-in catalog refresh through the same
durable owner remains under implementation. Full-Svit completeness is not yet
claimed.

## Required properties

One persistence model must provide all of these properties:

| Property | Contract |
| --- | --- |
| Resumable | Load one base, replay its ordered transaction tail, validate every version and root hash, then continue at the next position |
| Forkable | Create a child base from an exact committed parent position without copying the full parent state |
| Snapshot-capable | Materialize an on-demand state image for replay acceleration, migration, fork detachment, or a history cut |
| Complete | Represent every committed change to the process root and retained control receipts; current implementation covers the `DurableProcess` slice and not yet reasoning or receipts |
| Uniform | Persist one Svit transaction event type; reasoning is not a second event system or special agent event kind |
| Queryable | Stream retained events by position, process version, touched path, source metadata, receipt key, or hash without replaying guest code |
| Cuttable | Replace a retained prefix with a validated base snapshot, keep positions stable, and delete old data only when no fork references it |

## Decision

Persist one Svit as an immutable base plus an append-only tail of uniform
transaction events:

```text
Svit(address) = Base(address, covered_through) + Events(covered_through + 1 ... head)
```

The first durable adapter uses one local Turso database, partitioned by Svit
address. It stores indexed event envelopes, immutable bases, snapshots,
content-addressed payload blobs, and fork references. It does not write a
complete process snapshot on every commit. Materialized receipt state remains
under implementation.

A deterministic reducer reconstructs the current process root and process
version. Receipt reconstruction remains under implementation. Replay never
reruns guest Lisp, model calls, HTTP, built-ins, or other external effects.
Events describe the validated result of a committed transaction, not a command
that might behave differently when repeated.

The logical store contract is:

```text
create(address, base)                                      -> empty tail
load_base(address)                                        -> base
read_events(address, after_position, query)               -> ordered events
append(address, expected_head, event)                      -> new head | conflict
install_base(address, expected_head, new_base)             -> same state, new epoch
```

`create` fails when the address exists; load fails when it does not. There is no
implicit "open or create" operation. `append` and `install_base` are
compare-and-swap operations for one address.

A head is `(position: Option<u64>, hash)`. With an empty tail its position is
null and its hash is the active immutable base hash. Otherwise it names the
last event position and hash. This makes an initial Svit and a newly forked Svit
valid CAS sources before either has emitted an event.

The expanded requirements make a transactional database the better local
adapter. Resume needs ordered range reads; query needs indexes; fork needs
reference tracking; a control commit needs one event and receipt projection to
commit together; and cut needs base installation plus event deletion to be
atomic. Implementing those properties with JSONL would recreate a database
through manifests, sidecar indexes, dependency scans, and multi-file crash
protocols.

Use the already pinned Rust `turso` crate, not the legacy `libsql` embedded
replica API. Turso documents the crate as its local/embedded Rust database with
async transactions and MVCC concurrent writes; optional push/pull sync is a
separate feature. The first adapter is local-only and must not enable remote
sync or claim multi-host ownership until fencing and merge behavior are
designed. See the official [Rust reference](https://docs.turso.tech/sdk/rust/reference)
and [Rust quickstart](https://docs.turso.tech/sdk/rust/quickstart).

The SQL schema is adapter detail. The portable contract remains immutable base,
uniform transaction envelopes, ordered reads, address-scoped CAS, snapshots,
fork references, and cuts. A future S3 adapter implements that contract without
placing a live database file in object storage.

The public Rust boundary is adapter-neutral: `ProcessStore` creates and resumes
an associated `DurableProcessHandle`; event and snapshot results implement
`PersistedEventRecord` and `PersistenceSnapshotRecord`. The concrete local
types are exported only with the enabled-by-default `persistence-turso` Cargo
feature. `DefaultProcessStore` is then an alias for `TursoProcessStore`.
Disabling default features leaves the traits, event query, mutations, and core
process runtime available without the Turso dependency. The independent
`turso-mount` feature controls Turso query snapshot imports.

## Base

A base establishes the state before its event tail. It contains:

```text
base_format                 "svit-base@1"
address                     exact ProcessId string
covered_through_position    null for a new/forked stream, otherwise last cut event
anchor_event_hash           null or hash at covered_through_position
process_version             version of the reconstructed base root
root_hash                   hash of the reconstructed base root
origin                      created | fork | snapshot
base_hash                   SHA-256 of the canonical base without this field
```

Origins have exact meanings:

- `created` contains the initial limits, named memory values, scripts, and
  mount descriptors needed to construct the conventional version-zero root;
  mount providers are host runtime state and are never persisted;
- `fork` references an exact parent address, position, event hash, and root
  hash, then applies child identity, lineage, and empty Lisp-outbox rules;
- `snapshot` references a validated store snapshot produced for replay
  acceleration, fork detachment, migration, or a cut.

Base payloads use the same content-addressed envelope storage as events. A base
hash is integrity metadata, not writer authentication. Receipt state in bases
remains under implementation.

## One uniform transaction event

There are no separate memory, activation, inbox, thread, or agent event kinds.
Every tail record is a `svit-transaction@1` envelope:

```text
event_format             "svit-transaction@1"
address                  exact ProcessId string
position                 stable zero-based position in this address stream
previous_hash            active base hash for the first tail event, otherwise prior event hash
process_version_before   committed process version before the transaction
process_version_after    resulting process version
mutations                ordered process-tree mutations
touched_paths            canonical paths derived from mutations
source                   optional non-authoritative query metadata
resulting_root_hash      SHA-256 of the complete resulting process root
event_hash               SHA-256 of the canonical envelope without this field
```

Every successful process transition advances `process_version` exactly once,
including a successful read-only activation whose mutation list is empty.
Under the future receipt extension, a receipt-only transaction advances event
position while preserving process version and root hash, and a retained exact
retry appends nothing.

`source` identifies the trusted transition boundary, such as `activation`,
`host.write`, `host.remove`, or an inbox operation. Future reasoning and control
integration will add their own descriptive sources. It does not
select reducer behavior, grant authority, or create an agent-specific storage
domain.

The previous hash binds the tail to its exact immutable base and then detects
deletion, reordering, and splicing within the retained tail. The resulting root
hash checks the reducer after each process transition. Unknown event formats or
mutation operations fail closed.

## Mutations

The reducer accepts a small typed operation set over absolute process paths:

```text
set(path, value)
remove(path)
append(path, values)
remove_front(path, expected_value_hash)
```

`set` replaces or creates the value at a path using the live path contract.
`remove` uses the same map or array semantics as the live process API. `append`
avoids replacing growing arrays such as inbox, outbox, and thread history.
`remove_front` makes queue acknowledgement conditional on the exact observed
head. Empty-container replacement is expressed as `set`.

The operations are sufficient to represent every current process change:

- guest memory writes and removals;
- script save and removal under `/lib`;
- committed Lisp outbox intents;
- host write and removal;
- inbox enqueue and exact-head acknowledgement;
- reasoning initialization and each canonical Everruns event;
- the derived message projection for that Everruns event;
- descriptive built-in catalog refresh;
- future changes to reserved nodes only after their path schemas are defined.

An activation records its ordered memory mutations, library mutations, and
outbox appends in one transaction envelope. The transition boundary must emit
this write set directly while guest operations occur; it must not infer a diff
from two serialized roots.

The live commit and replay path use the same reducer:

1. Resolve and validate typed mutation values.
2. Apply the ordered mutations to a private copy of the preceding root.
3. Validate the complete root, scripts, thread projection, and limits. Receipt
   validation remains under implementation.
4. Require the version transition, derived touched paths, and resulting root
   hash to match the envelope.
5. Publish the candidate only after the event is durable.

Executable equivalence tests must prove for every mutator that reducing its
emitted event produces the exact candidate root. This is the completeness
criterion: a state change that cannot produce such an event cannot commit.

## Reasoning events are ordinary mutations

Status: **Under implementation.** The event shape is adopted, but the current
`ProcessEventLog` still commits to an in-memory `Process` and does not yet use
`DurableProcess`.

Everruns must not own a second durable event log. A successful
process-backed `EventLog::append` must emit an ordinary Svit transaction whose
mutations append the one canonical Everruns value to `/thread/events` and its
new derived messages to `/thread/messages`. Initialization sets the thread
metadata once.

Prior conversation state is never rewritten into later records. Query metadata
may say `source: reasoning`, but the reducer sees the same `append` operations
used for any other process array. Svit therefore persists one event stream, not
an agent event stream beside a memory event stream.

Agent event commits and Lisp activations remain separate process transactions
around external model and built-in calls. Uniform persistence does not make a
whole model turn or an external effect transactional.

## Control receipts

Status: **Under implementation.** The current Turso schema and reducer do not
persist controller receipts.

A control-triggered process commit must include its receipt insertion and any
receipt evictions in the same transaction event as its process mutations. A
conflict or rejection produces a receipt-only transaction with identical
before/after process versions and root hashes.

`receipt_delta` contains the complete bounded request and terminal response
being inserted plus the exact receipt keys evicted. Replay therefore does not
depend on current host retention configuration. Receipt lookup remains subject
to the control protocol's future authentication and tenant partitioning rules.

Failed guest activations and rejected direct host mutations append nothing
because they changed neither process state nor durable receipt state. Attempt
logging is a separate operational audit concern and is not part of the Svit
state source.

## Content-addressed envelopes

The implemented adapter stores each complete event, base, and snapshot
encoding as one content-addressed BLOB. It recomputes the SHA-256 before decode
and then runs the normal domain validation. The transaction inserts the BLOB
before the referencing row and publishes the new head in that same transaction.

Splitting individual large values into independently referenced blobs is
**Under implementation**. Until then, the event byte cap rejects a mutation set
whose complete canonical envelope is too large. Verified garbage collection
is also deferred; unreachable content-addressed BLOBs may remain after a cut.

## Resume

Resume is deterministic and streaming:

1. Load the address row and capture its exact target head, then load its
   selected immutable base.
2. Validate the base hash and reconstruct its created, forked, or snapshotted
   process root.
3. Stream retained events after `covered_through_position` in position order
   without loading the log into memory.
4. Validate address, position, content hash, hash chain, bounds, version
   transition, touched paths, typed mutations, and resulting root hash for
   every event.
5. Attach current host hooks and other runtime authority only after replay;
   durable reasoning and catalog refresh remain under implementation.
6. Continue at the next stable position and process version.

Guest code and external effects are never executed during these steps. Replay
has explicit event-count and lineage-depth budgets. A replay deadline remains
under implementation.

## Fork

Fork creates a child base referencing an exact committed parent view:

```text
parent_address
parent_position       null when the parent tail is empty
parent_head_hash      parent base hash when empty, otherwise last event hash
parent_root_hash
```

Child resume reconstructs that parent view, applies child identity, lineage,
and empty Lisp-outbox rules, then replays only the child's own transaction
tail. Later parent and child commits are independent. The child never copies a
full parent snapshot merely to fork.

This creates a storage dependency on the referenced parent history. The store
must reject lineage cycles, bound recursive replay, and prevent physical
deletion of a referenced prefix. An on-demand child snapshot can detach the
fork by atomically replacing its fork base with a snapshot base; only then may
the parent prefix become collectible.

## Snapshots

A store snapshot is created only on demand or by explicit host policy. It
contains enough information to resume without the covered prefix:

```text
snapshot_format
address
covered_through_position
anchor_event_hash
process_version
process_root
root_hash
snapshot_hash
```

It never contains hooks, credentials, model providers, built-in authority,
live observers, executing stacks, or uncommitted work.

Snapshots serve four concrete purposes:

1. bound replay work for a long event tail;
2. detach a fork from parent history;
3. package one Svit for migration or backup;
4. establish the new base for a history cut.

A replay-acceleration snapshot is disposable while its covered events remain.
Once a successful cut deletes those events, its snapshot base becomes
authoritative. Snapshot size is therefore paid occasionally and deliberately,
not on every commit. The current snapshot contains one complete validated
`Process` snapshot; structural tree chunking and receipt inclusion remain
under implementation.

## Query

The adapter exposes streaming queries over retained event envelopes. Initial
predicates are:

```text
position range
process-version range
touched path or path prefix
source metadata or application tag
receipt client/request key
event hash
include or omit mutation payloads
```

The implemented API supports position range, process-version range, exact
event hash, exact source, canonical path prefix, and a bounded result limit.
Receipt predicates and metadata-only payload omission are under implementation.

`touched_paths` is derived canonically from `mutations` and checked during
replay, so it cannot drift from the actual change. Queries can inspect envelope
metadata without resolving large blobs. Including payloads resolves and
validates them under explicit query byte limits.

The Turso adapter uses ordinary indexes over event position, process version,
source, event hash, and the normalized `event_paths` projection. Envelope and
mutation bytes remain the authoritative history; `event_paths` is derived from
each validated envelope. Queries decode and validate returned event BLOBs
rather than exposing arbitrary SQL over guest values.

A query sees only retained history. After a cut, events covered by the new base
are unavailable unless archived separately; the base exposes only its boundary
position, hashes, version, and current state.

## Cut

The first adapter cuts only at the current committed head while the Svit owner
is quiescent. One Turso transaction:

1. Acquire an immediate writer transaction and verify the exact head
   position/hash, process version, and root.
2. Insert and hash a store snapshot for that boundary.
3. Insert the immutable snapshot base and require that no retained fork
   reference needs the event prefix being cut.
4. Update the address row from the expected old head to the snapshot base,
   retaining the covered position and setting the head hash to the base hash.
5. Delete covered event and event-path rows, then commit all changes together.
6. Resume at the old head position plus one with `previous_hash` set to the new
   base hash. Unreachable blobs are reclaimed only by later verified GC.

Cut does not change process version, root hash, or event positions. The new
base records the prior anchor event, and the next event binds to that base hash.
Cut changes retention, so it is a storage lifecycle operation rather than a
process transaction event.

If any child references the covered history, the transaction refuses the cut.
The host must first detach those children with their own snapshot bases. The
database may retain free pages after logical deletion; file-space reclamation
is an engine-maintenance concern and is not part of cut semantics.

## Turso schema

One host-selected database file stores multiple Svits; `address` is the
partition key and logical identity. An address is always bound as a SQL value
and is never interpolated into a file path or statement.

The initial normalized schema is:

| Table | Authoritative or projection | Purpose |
| --- | --- | --- |
| `svits` | Head projection guarded by CAS | Active base, covered/head positions and hashes, process version, root hash, lifecycle status |
| `bases` | Authoritative | Immutable created, fork, or snapshot base envelope keyed by base hash |
| `events` | Authoritative | One canonical `svit-transaction@1` row per `(address, position)` |
| `blobs` | Authoritative payload storage | Content-addressed canonical bytes keyed by SHA-256 |
| `snapshots` | Authoritative when selected as a base | On-demand process images at exact event boundaries |
| `fork_refs` | Authoritative lifecycle metadata | Child-to-parent boundary references that prevent unsafe cuts |
| `event_paths` | Rebuildable query projection | Canonical touched paths for each event position |

Canonical event, base, mutation, and snapshot encodings are stored as
bounded BLOBs. Fields required for validation, CAS, and query are duplicated in
typed columns and checked against the canonical encoding. Database rows never
store hooks, credentials, provider objects, or built-in authority.

Required constraints include unique `(address, position)`, unique event and
base hashes, child-address uniqueness, non-null hash lengths, and foreign-key
or equivalent application checks for projections. Schema migrations are
explicit and versioned; an unknown schema version fails closed.

## Commit boundary

The Turso `DurableProcess` implementation owns one mutable `Process` plus its observed
event head. Its asynchronous write, remove, exec, inbox, fork, snapshot, query,
and cut operations pass through the Turso store. A caller must still serialize
operations on that owner; concurrent owners are rejected by head CAS.
Persistence is not
an activation hook because hooks do not cover all mutation paths and currently
run before a later store transaction could become durable.

For one process transaction, the owner first builds and validates the candidate
through the common reducer. It then begins a database transaction, verifies the
address row's expected head, inserts the content BLOB, inserts the canonical
event and path rows, and updates the head with
an address-and-old-head predicate. Only a successful database commit permits
the owner to publish memory, run committed hooks, wake the inbox, or return
success.

A transaction failure leaves all database tables unchanged. Ambiguous commit
recovery and durable-owner poisoning remain under implementation. Turso's
async API is used directly; guest code never receives a connection or SQL
capability.

## S3 evolution

The logical contract exposes immutable bases, blobs, ordered event reads, head
CAS, and base installation rather than Turso connections. A future
S3 adapter can map bases, blobs, and events to immutable objects and use one
small head object as the serialization point:

```text
svits/<address-hash>/bases/<base-hash>.json
svits/<address-hash>/events/<position>-<event-hash>.json
svits/<address-hash>/blobs/<sha256>
svits/<address-hash>/head.json
```

It writes immutable objects first, then conditionally updates `head.json`
against its observed entity tag. S3 documents
[`If-Match` and `If-None-Match` conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)
and [strong read-after-write consistency](https://docs.aws.amazon.com/AmazonS3/latest/userguide/Welcome.html#ConsistencyModel),
but the adapter still needs conflict, timeout, orphan-object, deletion, and
authorization evidence. Head CAS serializes commits; it is not a renewable
execution lease and does not fence external effects.

## Implementation evidence

Focused executable evidence currently proves:

- activation mutations replay to the exact version and root without executing
  guest code;
- host writes, canonical script writes, read-only activations, inbox append,
  and exact-head acknowledgement share the uniform event type;
- stale writers cannot publish a database event or their prepared local root;
- bounded position/version/path/source/hash queries validate event BLOBs;
- forked roots remain isolated, a parent cut refuses a live child reference,
  and cutting the child detaches the reference;
- resume after cut preserves parent and child state while retained queries are
  empty; and
- corrupt content-addressed event bytes fail closed on resume.

Required evidence still missing includes durable reasoning and receipt
integration, every mutator and rollback class, fork-cycle fault injection,
projection rebuilding, snapshot corruption, transaction cancellation, database
lock contention, crash recovery, schema migration, oversized base/snapshot
rows, database-file replacement, and systematic unknown-operation/hash-chain
faults. Threats remain `PARTIAL` or `REQUIRED` where those tests do not exist.

## Deferred work

This decision does not add discovery, automated snapshot cadence, archival
queries across cut history, garbage collection, implemented schema migrations,
signatures, encryption key management, Turso Sync, distributed leases, durable
external-effect delivery, or an S3 adapter. History grows until an explicit
snapshot and safe cut; configured quotas fail commits before storage or replay
work becomes unbounded. The pinned Turso engine remains pre-release and cannot
support a production-stability claim without upgrade and crash testing.
