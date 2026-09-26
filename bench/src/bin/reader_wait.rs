//! How long readers wait while a writer commits (SPEC §57). Two threads
//! look documents up by id as fast as they can, and each lookup is timed,
//! while a third writes: not at all, one document every second, ten a
//! second, one after another, or batches of 1,000. trunkdb's readers wait
//! for a commit (§57.1); redb's read a snapshot and don't, which is what
//! snapshots in trunkdb would buy.
//!
//! `cargo run --release --bin reader_wait -- [seconds per case]` from
//! `bench/` (default 10); `BENCH_ONLY=trunkdb` runs only that store,
//! `BENCH_READERS=4` changes how many threads read (default 2).

use redb::ReadableDatabase;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const DOCUMENTS: usize = 100_000;

type Id = [u8; 16];

/// Opens a fresh store in a directory.
type OpenStore = fn(&Path) -> Arc<dyn Store>;

/// What the readers and the writer need from a store; `Sync`, since
/// they share it across threads.
trait Store: Send + Sync {
    fn name(&self) -> &'static str;
    /// One durable commit.
    fn insert(&self, docs: &[(Id, Vec<u8>)]);
    /// Whether `id` is there, its document read.
    fn get(&self, id: &Id) -> bool;
}

struct Trunk {
    db: trunkdb::Database,
    docs: trunkdb::Collection<trunkdb::Document>,
}

impl Trunk {
    fn open(dir: &Path) -> Self {
        let db = trunkdb::Database::open(dir.join("wait.trunkdb")).unwrap();
        let docs = db.collection("docs");
        Trunk { db, docs }
    }
}

impl Store for Trunk {
    fn name(&self) -> &'static str {
        "trunkdb"
    }

    /// Each body in an object with its `_id`, which an insert uses: a bare
    /// value has no field for an id, so it would get a new one.
    fn insert(&self, docs: &[(Id, Vec<u8>)]) {
        let mut batch = self.db.batch();
        for (id, body) in docs {
            batch.insert(&self.docs, object(id, body)).unwrap();
        }
        batch.commit().unwrap();
    }

    fn get(&self, id: &Id) -> bool {
        self.docs.get(&trunkdb::DocId(*id)).unwrap().is_some()
    }
}

/// `{"_id": id, "body": body}`.
fn object(id: &Id, body: &[u8]) -> trunkdb::Document {
    let mut doc = trunkdb::Document::Object(Default::default());
    if let trunkdb::Document::Object(fields) = &mut doc {
        fields.insert("_id".into(), trunkdb::Document::Id(trunkdb::DocId(*id)));
        fields.insert("body".into(), trunkdb::Document::Binary(body.to_vec()));
    }
    doc
}

const TABLE: redb::TableDefinition<&[u8; 16], &[u8]> = redb::TableDefinition::new("docs");

struct Redb(redb::Database);

impl Store for Redb {
    fn name(&self) -> &'static str {
        "redb"
    }

    fn insert(&self, docs: &[(Id, Vec<u8>)]) {
        let txn = self.0.begin_write().unwrap();
        {
            let mut table = txn.open_table(TABLE).unwrap();
            for (id, body) in docs {
                table.insert(id, body.as_slice()).unwrap();
            }
        }
        txn.commit().unwrap();
    }

    fn get(&self, id: &Id) -> bool {
        let txn = self.0.begin_read().unwrap();
        let table = txn.open_table(TABLE).unwrap();
        table
            .get(id)
            .unwrap()
            .is_some_and(|v| !v.value().is_empty())
    }
}

/// What the writer does while the readers read.
#[derive(Clone, Copy)]
enum Writer {
    None,
    /// One document per commit, `n` commits a second.
    Paced(u32),
    /// One document per commit, one after another.
    Singles,
    /// Commits of this many documents, one after another.
    Batches(usize),
}

impl Writer {
    fn describe(self) -> String {
        match self {
            Writer::None => "no writer".into(),
            Writer::Paced(1) => "1 commit a second".into(),
            Writer::Paced(n) => format!("{n} commits a second"),
            Writer::Singles => "single inserts, back to back".into(),
            Writer::Batches(n) => format!("batches of {n}, back to back"),
        }
    }
}

/// Deterministic ids and bodies, the same for every store.
struct Docs {
    next: u64,
}

impl Docs {
    fn take(&mut self, n: usize) -> Vec<(Id, Vec<u8>)> {
        (0..n)
            .map(|_| {
                self.next += 1;
                let mut id = [0u8; 16];
                // Time-ordered, like the UUIDv7 ids trunkdb makes.
                id[..8].copy_from_slice(&self.next.to_be_bytes());
                let body = format!(
                    "document {} {}",
                    self.next,
                    "x".repeat(self.next as usize % 200)
                );
                (id, body.into_bytes())
            })
            .collect()
    }
}

struct Outcome {
    commits: usize,
    reads: Vec<u64>,
}

fn run(
    store: &Arc<dyn Store>,
    ids: &Arc<Vec<Id>>,
    docs: &mut Docs,
    writer: Writer,
    time: Duration,
    readers: usize,
) -> Outcome {
    let stop = Arc::new(AtomicBool::new(false));
    let commits = Arc::new(AtomicUsize::new(0));
    let readers: Vec<_> = (0..readers)
        .map(|r| {
            let (store, ids, stop) = (store.clone(), ids.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ r as u64;
                let mut times = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    let id = &ids[(rng % ids.len() as u64) as usize];
                    let start = Instant::now();
                    assert!(store.get(id));
                    times.push(start.elapsed().as_nanos() as u64);
                }
                times
            })
        })
        .collect();

    // The writer's documents, made up front so making them isn't timed.
    let chunk = match writer {
        Writer::Batches(n) => n,
        _ => 1,
    };
    let planned = match writer {
        Writer::None => 0,
        Writer::Paced(n) => (time.as_secs_f64() * n as f64).ceil() as usize + 1,
        _ => 20_000,
    };
    let pending: Vec<Vec<(Id, Vec<u8>)>> = (0..planned).map(|_| docs.take(chunk)).collect();
    let start = Instant::now();
    for (i, batch) in pending.iter().enumerate() {
        if start.elapsed() >= time {
            break;
        }
        if let Writer::Paced(n) = writer {
            let at = Duration::from_secs_f64(i as f64 / n as f64);
            if let Some(wait) = at.checked_sub(start.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        store.insert(batch);
        commits.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(rest) = time.checked_sub(start.elapsed()) {
        std::thread::sleep(rest);
    }
    stop.store(true, Ordering::Relaxed);
    let mut reads: Vec<u64> = readers
        .into_iter()
        .flat_map(|r| r.join().unwrap())
        .collect();
    reads.sort_unstable();
    Outcome {
        commits: commits.load(Ordering::Relaxed),
        reads,
    }
}

fn show(nanos: u64) -> String {
    match nanos {
        n if n >= 1_000_000 => format!("{:.1} ms", n as f64 / 1e6),
        n => format!("{:.1} µs", n as f64 / 1e3),
    }
}

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .map(|arg| arg.parse().expect("seconds per case"))
        .unwrap_or(10);
    let time = Duration::from_secs(seconds);
    let only = std::env::var("BENCH_ONLY").ok();
    let readers: usize =
        std::env::var("BENCH_READERS").map_or(2, |n| n.parse().expect("a number of readers"));
    let cases = [
        Writer::None,
        Writer::Paced(1),
        Writer::Paced(10),
        Writer::Singles,
        Writer::Batches(1000),
    ];
    let openers: Vec<OpenStore> = vec![|dir| Arc::new(Trunk::open(dir)), |dir| {
        let db = redb::Database::create(dir.join("wait.redb")).unwrap();
        Arc::new(Redb(db))
    }];

    println!(
        "{DOCUMENTS} documents, {readers} readers looking them up by id, {seconds} s per case; {} {}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("| store | writer | commits | reads | p50 | p99 | p99.9 | max |");
    println!("|---|---|---:|---:|---:|---:|---:|---:|");
    for open in &openers {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        let wanted = only.as_ref().is_none_or(|only| {
            only.split(',')
                .any(|n| n.eq_ignore_ascii_case(store.name()))
        });
        if !wanted {
            continue;
        }
        eprintln!("{}…", store.name());
        let mut docs = Docs { next: 0 };
        let mut ids = Vec::new();
        for _ in 0..DOCUMENTS / 1000 {
            let batch = docs.take(1000);
            ids.extend(batch.iter().map(|(id, _)| *id));
            store.insert(&batch);
        }
        let ids = Arc::new(ids);
        for writer in cases {
            let outcome = run(&store, &ids, &mut docs, writer, time, readers);
            let at = |q: f64| outcome.reads[((outcome.reads.len() - 1) as f64 * q) as usize];
            println!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |",
                store.name(),
                writer.describe(),
                outcome.commits,
                outcome.reads.len(),
                show(at(0.5)),
                show(at(0.99)),
                show(at(0.999)),
                show(*outcome.reads.last().unwrap()),
            );
        }
    }
}
