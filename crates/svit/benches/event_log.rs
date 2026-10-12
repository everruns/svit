//! Shared event log benchmark.
//!
//! Compares three ways to order agent (Everruns) events and Svit process
//! transactions:
//!
//! - `split`: the current design, a process transaction stream plus a separate
//!   paged EventLog, each with its own sequence;
//! - `unified`: one physical log with one global sequence, where each record is
//!   either a process transaction or an agent event, plus a kind index;
//! - `embedded`: agent events committed as process-root mutations, so one
//!   process transaction stream carries everything.
//!
//! The split numbers come from the real Turso adapter. The unified and split
//! prototypes replay the exact bytes that run produced against a minimal raw
//! Turso schema so the layouts are compared on identical records. The embedded
//! numbers come from the real adapter with raised limits.
//!
//! ```text
//! cargo bench -p svit --bench event_log -- [turns] [embedded_turns]
//! ```

use everruns_contracts::typed_id::SessionId;
use everruns_core::RuntimeMessage;
use everruns_core::events::{EventContext, EventRequest, InputMessageData};
use everruns_core::host::{EventReadLimit, EventReadRequest};
use std::path::Path;
use std::time::{Duration, Instant};
use svit::{
    DurableProcessHandle, Limits, Process, ProcessTransaction, TransactionQuery, TursoProcessStore,
    Value,
};

/// Payload sizes of the agent events one turn appends: input message, reason
/// started, model output with a tool call, tool started, tool completed (the
/// variable entry), final model output, turn completed.
const AGENT_EVENT_BYTES: [usize; 7] = [200, 150, 900, 200, 0, 1200, 150];
const TOOL_RESULT_MIN: usize = 1000;
const TOOL_RESULT_MAX: usize = 8000;
const NOTE_KEYS: usize = 256;
const CHECKPOINT_INTERVAL: usize = 32;
const CONTEXT_WINDOW: usize = 200;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Process,
    Agent,
}

struct Record {
    kind: Kind,
    bytes: Vec<u8>,
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn between(&mut self, low: usize, high: usize) -> usize {
        low + (self.next() as usize) % (high - low)
    }
}

fn filler(bytes: usize, seed: usize) -> String {
    const WORDS: &str = "svit process memory activation transaction snapshot fork outbox inbox \
                         model tool result reasoning event sequence checkpoint replay ";
    let mut text = format!("{seed}:");
    while text.len() < bytes {
        text.push_str(WORDS);
    }
    text.truncate(bytes);
    text
}

fn base_process(address: &str, limits: Limits) -> Process {
    let mut notes = serde_json::Map::new();
    for key in 0..NOTE_KEYS {
        notes.insert(format!("n{key}"), serde_json::json!({"turn": -1}));
    }
    Process::builder(address)
        .unwrap()
        .limits(limits)
        .memory(
            "notes",
            Value::from_json(serde_json::Value::Object(notes)).unwrap(),
        )
        .memory("thread", Value::from_json(serde_json::json!({})).unwrap())
        .build()
        .unwrap()
}

fn agent_request(session: SessionId, bytes: usize, seed: usize) -> EventRequest {
    EventRequest::new(
        session,
        EventContext::empty(),
        InputMessageData::new(RuntimeMessage::user(filler(bytes, seed))),
    )
}

fn agent_sizes(rng: &mut Lcg) -> [usize; 7] {
    let mut sizes = AGENT_EVENT_BYTES;
    sizes[4] = rng.between(TOOL_RESULT_MIN, TOOL_RESULT_MAX);
    sizes
}

struct Timings(Vec<Duration>);

impl Timings {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn total(&self) -> Duration {
        self.0.iter().sum()
    }

    fn percentile(&self, percentile: f64) -> Duration {
        let mut sorted = self.0.clone();
        sorted.sort();
        if sorted.is_empty() {
            return Duration::ZERO;
        }
        let index = ((sorted.len() - 1) as f64 * percentile).round() as usize;
        sorted[index]
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn us(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

fn file_bytes(path: &Path) -> u64 {
    let mut total = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let wal = path.with_extension("db-wal");
    total += std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0);
    total
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Runs the real split adapter and returns the records in their true
/// interleaved order plus the process snapshots at each checkpoint boundary.
async fn real_split(dir: &Path, turns: usize) -> (Vec<Record>, Vec<(usize, Vec<u8>)>, Process) {
    let address = "svit://bench/split";
    let path = dir.join("split.db");
    let store = TursoProcessStore::open(&path).await.unwrap();
    let base = base_process(address, Limits::default());
    let base_copy = base.clone();
    let mut durable = store.create(base).await.unwrap();
    let log = durable.event_log();
    let session = SessionId::new();
    let mut rng = Lcg(7);

    let mut process_ops = Timings::new();
    let mut agent_ops = Timings::new();
    let mut turn_ops = Timings::new();
    // Order of records: per turn P, A x7, P, P.
    let mut order = Vec::with_capacity(turns * 10);
    let mut process_count = 0usize;
    let started = Instant::now();

    for turn in 0..turns {
        let turn_started = Instant::now();
        let message = Value::from_json(serde_json::json!({"text": filler(200, turn)})).unwrap();
        let op = Instant::now();
        durable.enqueue_inbox(message.clone()).await.unwrap();
        process_ops.0.push(op.elapsed());
        order.push(Kind::Process);
        process_count += 1;

        for (index, bytes) in agent_sizes(&mut rng).into_iter().enumerate() {
            let op = Instant::now();
            log.append(agent_request(session, bytes, turn * 10 + index))
                .await
                .unwrap();
            agent_ops.0.push(op.elapsed());
            order.push(Kind::Agent);
        }

        let note =
            Value::from_json(serde_json::json!({"turn": turn, "text": filler(500, turn)})).unwrap();
        let op = Instant::now();
        durable
            .write(&format!("/memory/notes/n{}", turn % NOTE_KEYS), note)
            .await
            .unwrap();
        process_ops.0.push(op.elapsed());
        order.push(Kind::Process);
        process_count += 1;

        let op = Instant::now();
        durable.acknowledge_inbox(&message).await.unwrap();
        process_ops.0.push(op.elapsed());
        order.push(Kind::Process);
        process_count += 1;
        turn_ops.0.push(turn_started.elapsed());
    }
    let wall = started.elapsed();

    let records_total = turns * 10;
    println!("\n## split (real Turso adapter, current design)");
    println!(
        "turns={turns} records={records_total} process_tx={} agent_events={}",
        process_ops.0.len(),
        agent_ops.0.len()
    );
    println!(
        "append wall={:.2}s  throughput={:.0} records/s  turn p50={:.2}ms p99={:.2}ms",
        wall.as_secs_f64(),
        records_total as f64 / wall.as_secs_f64(),
        ms(turn_ops.percentile(0.5)),
        ms(turn_ops.percentile(0.99))
    );
    println!(
        "process tx p50={:.0}us p99={:.0}us total={:.2}s | agent event p50={:.0}us p99={:.0}us total={:.2}s",
        us(process_ops.percentile(0.5)),
        us(process_ops.percentile(0.99)),
        process_ops.total().as_secs_f64(),
        us(agent_ops.percentile(0.5)),
        us(agent_ops.percentile(0.99)),
        agent_ops.total().as_secs_f64(),
    );
    let snapshot_bytes = durable.process_projection().snapshot().unwrap().len();
    println!(
        "process snapshot={:.1} KiB  db file={:.1} MiB",
        snapshot_bytes as f64 / 1024.0,
        mib(file_bytes(&path))
    );

    // Real resume: base + recovery checkpoint + newer tail.
    let final_hash = durable.root_hash();
    drop(durable);
    let op = Instant::now();
    let resumed = store.resume(address).await.unwrap();
    let resume = op.elapsed();
    assert_eq!(resumed.root_hash(), final_hash);
    let op = Instant::now();
    let recent = resumed
        .recent_thread_events(session, CONTEXT_WINDOW)
        .await
        .unwrap();
    let recent_time = op.elapsed();
    assert_eq!(recent.len(), CONTEXT_WINDOW);

    // Everruns-style full history read through the paged EventReader.
    let reader = resumed.event_log();
    let op = Instant::now();
    let mut request = EventReadRequest::new(session, EventReadLimit::new(1024).unwrap());
    let mut agent_bytes = Vec::with_capacity(turns * 7);
    loop {
        let page = reader.read_page(request).await.unwrap();
        for event in &page.events {
            agent_bytes.push(serde_json::to_vec(event).unwrap());
        }
        let Some(cursor) = page.next_cursor else {
            break;
        };
        request = EventReadRequest::from_cursor(cursor, EventReadLimit::new(1024).unwrap());
    }
    let full_read = op.elapsed();
    println!(
        "resume process={:.1}ms  recent {CONTEXT_WINDOW} events={:.1}ms  full agent history read={:.0}ms ({} events)",
        ms(resume),
        ms(recent_time),
        ms(full_read),
        agent_bytes.len()
    );

    // Pull the exact process transaction bytes for the prototypes.
    let mut process_bytes = Vec::with_capacity(process_count);
    let mut after = None;
    loop {
        let mut query = TransactionQuery::new().limit(1000);
        if let Some(position) = after {
            query = query.after_position(position);
        }
        let page = resumed.transactions(query).await.unwrap();
        if page.is_empty() {
            break;
        }
        after = page.last().map(ProcessTransaction::position);
        for transaction in page {
            process_bytes.push(transaction.to_bytes().unwrap());
        }
    }
    assert_eq!(process_bytes.len(), process_count);
    assert_eq!(agent_bytes.len(), turns * 7);

    // Rebuild exact checkpoint images by replaying the captured stream once.
    let mut process = base_copy.clone();
    let mut images = Vec::new();
    for (index, bytes) in process_bytes.iter().enumerate() {
        ProcessTransaction::from_bytes(bytes)
            .unwrap()
            .replay(&mut process)
            .unwrap();
        if (index + 1).is_multiple_of(CHECKPOINT_INTERVAL) {
            images.push((index + 1, process.snapshot().unwrap()));
        }
    }
    assert_eq!(process.root_hash(), final_hash);

    let mut process_iter = process_bytes.into_iter();
    let mut agent_iter = agent_bytes.into_iter();
    let records = order
        .into_iter()
        .map(|kind| Record {
            kind,
            bytes: match kind {
                Kind::Process => process_iter.next().unwrap(),
                Kind::Agent => agent_iter.next().unwrap(),
            },
        })
        .collect();
    (records, images, base_copy)
}

async fn connect(path: &Path) -> turso::Connection {
    let database = turso::Builder::new_local(path.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let connection = database.connect().unwrap();
    // Keep the database alive for the connection's lifetime.
    std::mem::forget(database);
    connection
}

const SPLIT_SCHEMA: &str = "
CREATE TABLE proc (pos INTEGER PRIMARY KEY, bytes BLOB NOT NULL);
CREATE TABLE agent (seq INTEGER PRIMARY KEY, bytes BLOB NOT NULL);
CREATE TABLE ckpt (id INTEGER PRIMARY KEY CHECK (id = 1), covered INTEGER NOT NULL, bytes BLOB NOT NULL);
";

const UNIFIED_SCHEMA: &str = "
CREATE TABLE log (seq INTEGER PRIMARY KEY, kind INTEGER NOT NULL, bytes BLOB NOT NULL);
CREATE INDEX log_by_kind ON log(kind, seq);
CREATE TABLE ckpt (id INTEGER PRIMARY KEY CHECK (id = 1), covered INTEGER NOT NULL, bytes BLOB NOT NULL);
";

#[derive(Clone, Copy, PartialEq)]
enum Layout {
    Split,
    Unified,
    UnifiedTurnAtomic,
}

impl Layout {
    fn name(self) -> &'static str {
        match self {
            Self::Split => "split",
            Self::Unified => "unified",
            Self::UnifiedTurnAtomic => "unified, one SQL tx per turn",
        }
    }
}

async fn write_layout(
    path: &Path,
    layout: Layout,
    records: &[Record],
    images: &[(usize, Vec<u8>)],
) -> Duration {
    let connection = connect(path).await;
    let schema = if layout == Layout::Split {
        SPLIT_SCHEMA
    } else {
        UNIFIED_SCHEMA
    };
    connection.execute_batch(schema).await.unwrap();
    let mut images = images.iter().peekable();
    let mut process_count = 0usize;
    let mut agent_count = 0usize;
    let started = Instant::now();
    let mut open = false;
    for (seq, record) in records.iter().enumerate() {
        let batch_start = layout != Layout::UnifiedTurnAtomic || seq % 10 == 0;
        let batch_end = layout != Layout::UnifiedTurnAtomic || seq % 10 == 9;
        if batch_start {
            connection.execute("BEGIN IMMEDIATE", ()).await.unwrap();
            open = true;
        }
        match (layout, record.kind) {
            (Layout::Split, Kind::Process) => {
                connection
                    .execute(
                        "INSERT INTO proc (pos, bytes) VALUES (?1, ?2)",
                        turso::params![process_count as i64, record.bytes.clone()],
                    )
                    .await
                    .unwrap();
            }
            (Layout::Split, Kind::Agent) => {
                connection
                    .execute(
                        "INSERT INTO agent (seq, bytes) VALUES (?1, ?2)",
                        turso::params![agent_count as i64 + 1, record.bytes.clone()],
                    )
                    .await
                    .unwrap();
            }
            (_, kind) => {
                let kind = i64::from(kind == Kind::Process);
                connection
                    .execute(
                        "INSERT INTO log (seq, kind, bytes) VALUES (?1, ?2, ?3)",
                        turso::params![seq as i64, kind, record.bytes.clone()],
                    )
                    .await
                    .unwrap();
            }
        }
        match record.kind {
            Kind::Process => process_count += 1,
            Kind::Agent => agent_count += 1,
        }
        if record.kind == Kind::Process
            && let Some((boundary, image)) = images.peek()
            && *boundary == process_count
        {
            // The checkpoint covers a process position in split and a global
            // sequence in unified layouts.
            let covered = if layout == Layout::Split {
                process_count as i64
            } else {
                seq as i64 + 1
            };
            connection
                .execute(
                    "INSERT OR REPLACE INTO ckpt (id, covered, bytes) VALUES (1, ?1, ?2)",
                    turso::params![covered, image.clone()],
                )
                .await
                .unwrap();
            images.next();
        }
        if batch_end && open {
            connection.execute("COMMIT", ()).await.unwrap();
            open = false;
        }
    }
    if open {
        connection.execute("COMMIT", ()).await.unwrap();
    }
    started.elapsed()
}

fn blob(row: &turso::Row, index: usize) -> Vec<u8> {
    match row.get_value(index).unwrap() {
        turso::Value::Blob(bytes) => bytes,
        _ => panic!("expected blob"),
    }
}

fn integer(row: &turso::Row, index: usize) -> i64 {
    match row.get_value(index).unwrap() {
        turso::Value::Integer(value) => value,
        _ => panic!("expected integer"),
    }
}

/// Process resume from the checkpoint plus its newer process tail.
async fn resume_with_checkpoint(path: &Path, layout: Layout) -> (Duration, String, usize) {
    let connection = connect(path).await;
    let started = Instant::now();
    let mut rows = connection
        .query("SELECT covered, bytes FROM ckpt WHERE id = 1", ())
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let covered = integer(&row, 0);
    let mut process = Process::restore(&blob(&row, 1)).unwrap();
    drop(rows);
    let tail_sql = if layout == Layout::Split {
        "SELECT bytes FROM proc WHERE pos >= ?1 ORDER BY pos"
    } else {
        "SELECT bytes FROM log WHERE kind = 1 AND seq >= ?1 ORDER BY seq"
    };
    let mut rows = connection.query(tail_sql, [covered]).await.unwrap();
    let mut replayed = 0;
    while let Some(row) = rows.next().await.unwrap() {
        ProcessTransaction::from_bytes(&blob(&row, 0))
            .unwrap()
            .replay(&mut process)
            .unwrap();
        replayed += 1;
    }
    (started.elapsed(), process.root_hash(), replayed)
}

/// Process resume from the base with no snapshot: replays every process
/// transaction. `scan_all` additionally reads every agent record because the
/// unified log has no kind index to skip them.
async fn resume_without_snapshot(
    path: &Path,
    layout: Layout,
    base: &Process,
    scan_all: bool,
) -> (Duration, String, usize, u64) {
    let connection = connect(path).await;
    let started = Instant::now();
    let mut process = base.clone();
    let sql = match (layout, scan_all) {
        (Layout::Split, _) => "SELECT 1, bytes FROM proc ORDER BY pos",
        (_, false) => "SELECT kind, bytes FROM log WHERE kind = 1 ORDER BY seq",
        (_, true) => "SELECT kind, bytes FROM log ORDER BY seq",
    };
    let mut rows = connection.query(sql, ()).await.unwrap();
    let mut replayed = 0;
    let mut bytes_read = 0u64;
    while let Some(row) = rows.next().await.unwrap() {
        let bytes = blob(&row, 1);
        bytes_read += bytes.len() as u64;
        if integer(&row, 0) == 1 {
            ProcessTransaction::from_bytes(&bytes)
                .unwrap()
                .replay(&mut process)
                .unwrap();
            replayed += 1;
        }
    }
    (started.elapsed(), process.root_hash(), replayed, bytes_read)
}

/// Reads the newest agent window and the whole agent history.
async fn agent_reads(path: &Path, layout: Layout) -> (Duration, Duration, usize) {
    let connection = connect(path).await;
    let (window_sql, full_sql) = if layout == Layout::Split {
        (
            "SELECT bytes FROM agent ORDER BY seq DESC LIMIT ?1",
            "SELECT bytes FROM agent ORDER BY seq",
        )
    } else {
        (
            "SELECT bytes FROM log WHERE kind = 0 ORDER BY seq DESC LIMIT ?1",
            "SELECT bytes FROM log WHERE kind = 0 ORDER BY seq",
        )
    };
    let started = Instant::now();
    let mut rows = connection
        .query(window_sql, [CONTEXT_WINDOW as i64])
        .await
        .unwrap();
    let mut window = 0;
    while let Some(row) = rows.next().await.unwrap() {
        let _: serde_json::Value = serde_json::from_slice(&blob(&row, 0)).unwrap();
        window += 1;
    }
    assert_eq!(window, CONTEXT_WINDOW);
    let window_time = started.elapsed();
    drop(rows);
    let started = Instant::now();
    let mut rows = connection.query(full_sql, ()).await.unwrap();
    let mut count = 0;
    while let Some(row) = rows.next().await.unwrap() {
        let _: serde_json::Value = serde_json::from_slice(&blob(&row, 0)).unwrap();
        count += 1;
    }
    (window_time, started.elapsed(), count)
}

async fn prototypes(
    dir: &Path,
    records: &[Record],
    images: &[(usize, Vec<u8>)],
    base: &Process,
    final_hash: &str,
) {
    println!("\n## layout prototypes (raw Turso, identical record bytes)");
    let process_records = records.iter().filter(|r| r.kind == Kind::Process).count();
    let total_bytes: usize = records.iter().map(|r| r.bytes.len()).sum();
    let process_bytes: usize = records
        .iter()
        .filter(|r| r.kind == Kind::Process)
        .map(|r| r.bytes.len())
        .sum();
    println!(
        "records={} process={} agent={}  bytes: process={:.1} MiB agent={:.1} MiB  checkpoint image={:.1} KiB",
        records.len(),
        process_records,
        records.len() - process_records,
        mib(process_bytes as u64),
        mib((total_bytes - process_bytes) as u64),
        images.last().map(|(_, i)| i.len()).unwrap_or(0) as f64 / 1024.0,
    );
    println!(
        "| layout | append | records/s | db MiB | resume (ckpt+tail) | resume (no snapshot) | agent window {CONTEXT_WINDOW} | full agent read |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    for layout in [Layout::Split, Layout::Unified, Layout::UnifiedTurnAtomic] {
        let path = dir.join(format!(
            "proto-{}.db",
            layout.name().replace([' ', ','], "-")
        ));
        let append = write_layout(&path, layout, records, images).await;
        let (resume, hash, tail) = resume_with_checkpoint(&path, layout).await;
        assert_eq!(hash, final_hash);
        assert!(tail < CHECKPOINT_INTERVAL);
        let (cold, hash, replayed, _) = resume_without_snapshot(&path, layout, base, false).await;
        assert_eq!(hash, final_hash);
        assert_eq!(replayed, process_records);
        let (window, full, count) = agent_reads(&path, layout).await;
        assert_eq!(count, records.len() - process_records);
        println!(
            "| {} | {:.2}s | {:.0} | {:.1} | {:.1}ms ({tail} tx) | {:.2}s | {:.1}ms | {:.0}ms |",
            layout.name(),
            append.as_secs_f64(),
            records.len() as f64 / append.as_secs_f64(),
            mib(file_bytes(&path)),
            ms(resume),
            cold.as_secs_f64(),
            ms(window),
            ms(full),
        );
    }
    // Unified log without a kind index: replay must read everything.
    let path = dir.join("proto-unified.db");
    let (scan, hash, replayed, bytes) =
        resume_without_snapshot(&path, Layout::Unified, base, true).await;
    assert_eq!(hash, final_hash);
    println!(
        "unified full scan, no snapshot, no kind filter: {:.2}s, replayed {replayed} process tx, read {:.1} MiB",
        scan.as_secs_f64(),
        mib(bytes)
    );
}

/// Agent events stored as process-root mutations, the pre-August design where
/// conversation history lived under the memory tree.
async fn embedded(dir: &Path, turns: usize) {
    let address = "svit://bench/embedded";
    let path = dir.join("embedded.db");
    let store = TursoProcessStore::open(&path).await.unwrap();
    let limits = Limits {
        max_text_bytes: 64 * 1024 * 1024,
        max_value_entries: 1_000_000,
        max_tree_nodes: 10_000_000,
        max_tree_text_bytes: 512 * 1024 * 1024,
        ..Limits::default()
    };
    let mut durable = store.create(base_process(address, limits)).await.unwrap();
    let mut rng = Lcg(7);
    let mut event = 0usize;
    let mut window = Timings::new();
    let mut total = Duration::ZERO;
    let deadline = Duration::from_secs(300);
    println!("\n## embedded (agent events as process-root mutations, real adapter)");
    println!("| turns | events in root | turn p50 | turn p99 | snapshot MiB | db MiB |");
    println!("|---|---|---|---|---|---|");
    let mut completed = 0;
    for turn in 0..turns {
        let started = Instant::now();
        let message = Value::from_json(serde_json::json!({"text": filler(200, turn)})).unwrap();
        durable.enqueue_inbox(message.clone()).await.unwrap();
        for bytes in agent_sizes(&mut rng) {
            let value =
                Value::from_json(serde_json::json!({"text": filler(bytes, event)})).unwrap();
            durable
                .write(&format!("/memory/thread/e{event}"), value)
                .await
                .unwrap();
            event += 1;
        }
        let note =
            Value::from_json(serde_json::json!({"turn": turn, "text": filler(500, turn)})).unwrap();
        durable
            .write(&format!("/memory/notes/n{}", turn % NOTE_KEYS), note)
            .await
            .unwrap();
        durable.acknowledge_inbox(&message).await.unwrap();
        let elapsed = started.elapsed();
        window.0.push(elapsed);
        total += elapsed;
        completed = turn + 1;
        if completed.is_multiple_of(250) || completed == turns || total > deadline {
            let snapshot = durable.process_projection().snapshot().unwrap().len();
            println!(
                "| {completed} | {event} | {:.1}ms | {:.1}ms | {:.1} | {:.1} |",
                ms(window.percentile(0.5)),
                ms(window.percentile(0.99)),
                mib(snapshot as u64),
                mib(file_bytes(&path)),
            );
            window = Timings::new();
        }
        if total > deadline {
            println!("stopped at the {}s budget", deadline.as_secs());
            break;
        }
    }
    let final_hash = durable.root_hash();
    drop(durable);
    let started = Instant::now();
    let resumed = store.resume(address).await.unwrap();
    println!(
        "{completed} turns appended in {:.1}s; resume={:.0}ms",
        total.as_secs_f64(),
        ms(started.elapsed())
    );
    assert_eq!(resumed.root_hash(), final_hash);
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1).filter(|arg| arg != "--bench");
    let turns: usize = args.next().map(|a| a.parse().unwrap()).unwrap_or(10_000);
    let embedded_turns: usize = args.next().map(|a| a.parse().unwrap()).unwrap_or(2_000);
    let dir = tempfile::tempdir().unwrap();
    println!("# shared event log benchmark: {turns} turns (10 records/turn)");

    if turns > 0 {
        split_and_prototypes(dir.path(), turns).await;
    }
    if embedded_turns > 0 {
        embedded(dir.path(), embedded_turns).await;
    }
}

async fn split_and_prototypes(dir: &Path, turns: usize) {
    let (records, images, base) = real_split(dir, turns).await;
    let mut final_process = base.clone();
    for record in records.iter().filter(|r| r.kind == Kind::Process) {
        ProcessTransaction::from_bytes(&record.bytes)
            .unwrap()
            .replay(&mut final_process)
            .unwrap();
    }
    prototypes(dir, &records, &images, &base, &final_process.root_hash()).await;
}
