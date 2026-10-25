---
type: Research Note
title: Shared Event Log Evaluation
description: Benchmark evidence for ordering Everruns events and Svit process transactions in one log.
tags:
  - svit
  - persistence
  - event-sourcing
  - benchmark
---

# Shared Event Log Evaluation

## Question

Svit persists two streams per process: `ProcessTransaction` envelopes with a
process position, and canonical Everruns events in the paged `EventLog` with a
per-session sequence (see [persistence](../foundations/persistence.md)). The
proposal is one sequence for both. The concern was replay cost and a new
dependence on snapshots.

## Method

`crates/svit/benches/event_log.rs` runs a synthetic turn of ten records: inbox
enqueue, seven agent events (150 B to 8 KB, tool results varied), one memory
write, inbox acknowledgement. It measures three layouts:

| Layout | Meaning |
| --- | --- |
| split | Current design, real Turso adapter |
| unified | One physical log, one global sequence, a `kind` column and `(kind, seq)` index, process checkpoint every 32 process records |
| embedded | Agent events committed as process-root mutations, real adapter with raised limits |

Split and unified prototypes write the exact bytes the real split run
produced into a minimal raw Turso schema, so both layouts store identical
records. Replay uses the public `ProcessTransaction::replay`, which clones the
root per record, so no-snapshot replay times are an upper bound. Numbers are
from one 4-core cloud container with file-backed Turso 0.8.1 and are
indicative, not a production claim.

```text
cargo bench -p svit --bench event_log -- <turns> <embedded_turns>
```

## Results at 10,000 turns

100,000 records: 30,000 process transactions (23.9 MiB) and 70,000 agent
events (93.6 MiB). Process snapshot 154 KiB.

| Layout | Append | Resume (checkpoint + tail) | Resume (no snapshot) | Recent 200 agent events | Full agent read |
| --- | --- | --- | --- | --- | --- |
| split, prototype | 39 to 55 s | 7.0 ms | 9.9 s | 1.2 ms | 258 ms |
| unified, prototype | 50 to 55 s | 6.6 ms | 10.8 s | 1.1 ms | 246 ms |
| unified, one commit per turn | 10.7 s | 6.5 ms | 9.5 s | 1.1 ms | 243 ms |
| split, real adapter | 491 s | 34 ms | not measured | 11 ms | 4.4 s |

Append ranges are two runs; split and unified per-record appends are within
run-to-run noise. A unified scan without the kind index read all 117 MiB and
took 9.7 s, the same as filtered replay, because reduction dominates.

Embedded layout, real adapter, 300 s budget:

| Turns | Events in root | Turn p50 | Snapshot |
| --- | --- | --- | --- |
| 250 | 1,750 | 97 ms | 2.0 MiB |
| 500 | 3,500 | 251 ms | 3.8 MiB |
| 956 | 6,692 | 532 ms | 7.2 MiB |

With default limits the embedded layout fails at about 30 turns, when
`/memory` exceeds `max_text_bytes`.

## Findings

1. **A shared physical log costs nothing measurable** when records stay typed
   and indexed by kind. Resume, agent reads, and storage match the split
   layout.
2. **Snapshots are not a new requirement.** Process replay without a
   checkpoint is about 10 s for 30,000 transactions in every layout; the
   existing 32-record recovery checkpoint already bounds it. Agent context is
   bounded by Everruns compaction checkpoints, which a shared log keeps.
3. **Embedding events in the process root is the expensive variant.** Every
   commit validates and hashes the growing root and every checkpoint copies the
   conversation, so turn cost grows linearly and total cost quadratically. This
   repeats the August observation that motivated the paged `EventLog`.
4. **Commit count dominates write cost.** Each durable commit costs 4 to 5 ms;
   one commit per turn instead of per record is 4.6 times faster. Only a shared
   log can make a reasoning event and the process change it causes one atomic
   commit.
5. **Everruns already permits it.** `EventReader` documents that per-session
   sequences may have gaps when a reader projects a physical log into a filtered
   logical sequence, and `append_conditional` compares against the highest
   assigned sequence including filtered entries. Svit's reader currently treats
   any gap as corruption.
6. **The benchmark exposed a quadratic read.** Turso planned the exact-sequence
   thread event lookup through the `(address, event_id)` index. Fixed
   2026-10-12; the full read fell from 868 s to 4.4 s. Paging still issues one
   query per event, about 17 times slower than a range read.

## Recommendation

Adopted 2026-10-19: range reads and the shared journal are implemented in the
Turso adapter (see [persistence](../foundations/persistence.md)). Since
2026-10-25 a turn's terminal event and its inbox acknowledgement commit
together; other atomic step commits remain under implementation. Unify ordering at the storage layer, not
in the process root:

1. Replace per-event thread lookups with one range query per page that
   tolerates sequence gaps.
2. Introduce one address-keyed log with a global position, a record kind, and
   the existing typed payloads; Everruns reads its filtered projection and
   process resume reads process records after its checkpoint.
3. Commit each reasoning step and the process transactions it causes in one
   storage transaction.
4. Then use the single position for fork points (proposal open decision 8),
   one retention floor for cuts, and one hash chain for audit.
