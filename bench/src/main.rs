//! trunkdb against SQLite, redb and sled on one document workload (SPEC
//! §48). Every store gets the same documents under the same 16-byte
//! ids, the same three indexes, and a durable commit per batch; every
//! query must return the same answer everywhere before its time counts.
//!
//! `cargo run --release -- [documents]` from `bench/` (default 100000);
//! `BENCH_ONLY=trunkdb,redb` runs only those.

mod stores;

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use stores::{Redb, Sled, Sqlite, Store, Trunk};

/// The document every store holds: a task, as in the SPEC's examples.
/// Indexed: `tenant`, `created`, and `(status, created)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub status: String,
    pub created: i64,
    pub tenant: String,
    pub title: String,
    pub tries: i64,
    pub tags: Vec<String>,
}

pub type Id = [u8; 16];

/// Opens a fresh store in a directory.
type OpenStore = fn(&std::path::Path) -> Box<dyn Store>;

pub const STATUSES: [&str; 3] = ["Queued", "Running", "Done"];
const BATCH: usize = 1000;

fn task(i: usize, n: usize) -> Task {
    Task {
        status: STATUSES[i * 7 % 3].to_string(),
        // Every value once, but not in insertion order.
        created: (i * 7919 % n) as i64,
        tenant: format!("t{}", i * 31 % 100),
        title: format!("Task {i}: {}", "lorem ipsum ".repeat(1 + i % 4)),
        tries: (i % 10) as i64,
        tags: (0..i % 3).map(|k| format!("tag{}", (i + k) % 20)).collect(),
    }
}

/// A small deterministic generator, so every store sees the same random
/// picks.
struct XorShift(u64);

impl XorShift {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// One measured step: what, how to show it, and each store's value.
struct Row {
    what: String,
    unit: Unit,
    values: Vec<Option<f64>>,
}

#[derive(Clone, Copy)]
enum Unit {
    /// Operations per second, from a count and a duration.
    PerSecond,
    /// Microseconds per operation.
    Micros,
    /// Milliseconds per operation.
    Millis,
    Megabytes,
    Seconds,
}

impl Unit {
    fn show(self, value: f64) -> String {
        match self {
            Unit::PerSecond if value >= 10_000.0 => format!("{:.0}k/s", value / 1000.0),
            Unit::PerSecond => format!("{value:.0}/s"),
            Unit::Micros => format!("{value:.1} µs"),
            Unit::Millis => format!("{value:.2} ms"),
            Unit::Megabytes => format!("{value:.1} MB"),
            Unit::Seconds => format!("{value:.2} s"),
        }
    }
}

fn time<R>(f: impl FnOnce() -> R) -> (Duration, R) {
    let start = Instant::now();
    let result = f();
    (start.elapsed(), result)
}

/// What a store answered, to compare across stores.
#[derive(Debug, PartialEq, Default)]
struct Answers {
    got: usize,
    by_tenant: usize,
    in_range: usize,
    oldest: Vec<i64>,
    scanned: usize,
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .map(|arg| arg.parse().expect("the number of documents"))
        .unwrap_or(100_000);
    assert!(
        n >= 20_000 && n.is_multiple_of(BATCH),
        "a multiple of {BATCH}, at least 20000"
    );
    let ids: Vec<Id> = (0..n + 1000)
        .map(|_| *uuid::Uuid::now_v7().as_bytes())
        .collect();
    let tasks: Vec<(Id, Task)> = (0..n).map(|i| (ids[i], task(i, n))).collect();
    let singles: Vec<(Id, Task)> = (0..1000)
        .map(|i| (ids[n + i], task(n + i, n + 1000)))
        .collect();

    let makers: Vec<OpenStore> = vec![
        |dir| Box::new(Trunk::open(dir)),
        |dir| Box::new(Sqlite::open(dir)),
        |dir| Box::new(Redb::open(dir)),
        |dir| Box::new(Sled::open(dir)),
    ];
    let mut names = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut answers: Vec<Answers> = Vec::new();
    let only = std::env::var("BENCH_ONLY").ok();
    let mut s = 0;
    for make in &makers {
        let dir = tempfile::tempdir().unwrap();
        let mut store = make(dir.path());
        let wanted = only.as_ref().is_none_or(|only| {
            only.split(',')
                .any(|name| name.eq_ignore_ascii_case(store.name()))
        });
        if !wanted {
            continue;
        }
        names.push(store.name());
        eprintln!("{}…", store.name());
        let mut record = |what: &str, unit: Unit, value: Option<f64>| {
            if s == 0 {
                rows.push(Row {
                    what: what.to_string(),
                    unit,
                    values: Vec::new(),
                });
            }
            let row = rows.iter_mut().find(|r| r.what == what).unwrap();
            row.values.push(value);
        };
        let per_second = |count: usize, took: Duration| Some(count as f64 / took.as_secs_f64());
        let micros = |count: usize, took: Duration| Some(took.as_secs_f64() * 1e6 / count as f64);
        let millis = |count: usize, took: Duration| Some(took.as_secs_f64() * 1e3 / count as f64);
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        let mut answer = Answers::default();

        let (took, ()) = time(|| {
            for chunk in tasks.chunks(BATCH) {
                store.insert(chunk);
            }
        });
        record(
            &format!("insert {n}, {BATCH} per commit"),
            Unit::PerSecond,
            per_second(n, took),
        );
        let (took, ()) = time(|| {
            for single in &singles {
                store.insert(std::slice::from_ref(single));
            }
        });
        record(
            "insert 1000, one per commit",
            Unit::Micros,
            micros(1000, took),
        );

        let picks: Vec<usize> = (0..10_000).map(|_| rng.below(n)).collect();
        let (took, got) = time(|| {
            picks
                .iter()
                .filter(|&&i| store.get(&tasks[i].0).as_ref() == Some(&tasks[i].1))
                .count()
        });
        answer.got = got;
        record("get by id", Unit::Micros, micros(picks.len(), took));

        let tenants: Vec<String> = (0..100).map(|_| format!("t{}", rng.below(100))).collect();
        let (took, found) = time(|| tenants.iter().map(|t| store.by_tenant(t)).sum());
        answer.by_tenant = found;
        record(
            &format!("find tenant == x ({} docs)", n / 100),
            Unit::Millis,
            millis(tenants.len(), took),
        );

        let starts: Vec<i64> = (0..100).map(|_| rng.below(n - 1000) as i64).collect();
        let (took, found) = time(|| starts.iter().map(|&lo| store.in_range(lo, lo + 1000)).sum());
        answer.in_range = found;
        record(
            "find created in a range (1000 docs)",
            Unit::Millis,
            millis(starts.len(), took),
        );

        let statuses: Vec<&str> = (0..2000).map(|_| STATUSES[rng.below(3)]).collect();
        let (took, oldest) = time(|| {
            statuses
                .iter()
                .map(|s| store.oldest(s, 20))
                .last()
                .unwrap_or_default()
        });
        answer.oldest = oldest;
        record(
            "status == x, oldest 20",
            Unit::Micros,
            micros(statuses.len(), took),
        );

        let (took, scanned) = time(|| (0..5).map(|_| store.count_tries_above(7)).sum());
        answer.scanned = scanned;
        record("scan: tries > 7, unindexed", Unit::Millis, millis(5, took));

        let changed: Vec<(Id, Task)> = (0..10_000)
            .map(|_| {
                let i = rng.below(n);
                let mut task = tasks[i].1.clone();
                task.status = STATUSES[rng.below(3)].to_string();
                task.created += n as i64;
                task.title.push_str(" (edited)");
                (tasks[i].0, task)
            })
            .collect();
        // One document once per batch, as a store's update expects.
        let mut seen = std::collections::HashSet::new();
        let changed: Vec<(Id, Task)> = changed
            .into_iter()
            .filter(|(id, _)| seen.insert(*id))
            .collect();
        let (took, ()) = time(|| {
            for chunk in changed.chunks(BATCH) {
                store.update(chunk);
            }
        });
        record(
            &format!("update {}, {BATCH} per commit", changed.len()),
            Unit::PerSecond,
            per_second(changed.len(), took),
        );

        let deleted: Vec<Id> = tasks.iter().step_by(10).map(|(id, _)| *id).collect();
        let (took, ()) = time(|| {
            for chunk in deleted.chunks(BATCH) {
                store.delete(chunk);
            }
        });
        record(
            &format!("delete {}, {BATCH} per commit", deleted.len()),
            Unit::PerSecond,
            per_second(deleted.len(), took),
        );

        let mb = |bytes: u64| Some(bytes as f64 / 1e6);
        record("file size", Unit::Megabytes, mb(store.size()));
        let (took, compacted) = time(|| store.compact());
        record(
            "compact",
            Unit::Seconds,
            compacted.then_some(took.as_secs_f64()),
        );
        record(
            "file size after compact",
            Unit::Megabytes,
            compacted.then(|| store.size() as f64 / 1e6),
        );
        answers.push(answer);
        drop(store);
        s += 1;
    }

    for (name, answer) in names.iter().zip(&answers).skip(1) {
        assert_eq!(
            answer, &answers[0],
            "{name} answered differently from {}",
            names[0]
        );
    }

    println!(
        "{n} documents; {} {}, {} cores\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism().map_or(0, |p| p.get())
    );
    println!("| | {} |", names.join(" | "));
    println!("|---|{}", "---:|".repeat(names.len()));
    for row in rows {
        let cells: Vec<String> = row
            .values
            .iter()
            .map(|v| v.map_or("—".to_string(), |v| row.unit.show(v)))
            .collect();
        println!("| {} | {} |", row.what, cells.join(" | "));
    }
}
