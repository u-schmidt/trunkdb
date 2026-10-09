//! One long analysis beside many small writes: the workload of SPEC
//! §77.1, measured for §83. A snapshot is kept for the whole run
//! (15 seconds unless an argument says otherwise) and counted and
//! listed again and again, while another thread writes one document
//! per commit: mostly inserts, every twentieth an update.
//!
//! It reports how long the commits took, and how much memory the older
//! page versions took for the snapshot's sake.
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use trunkdb::{Database, DocId, query::Filter};

const LOADED: u64 = 200_000;

#[derive(Serialize, Deserialize)]
struct Ping {
    n: u64,
    note: String,
}

fn ping(n: u64, with: &str) -> Ping {
    Ping {
        n,
        note: with.repeat(40),
    }
}

fn show(ns: u64) -> String {
    if ns >= 1_000_000 {
        format!("{:.1} ms", ns as f64 / 1e6)
    } else {
        format!("{:.0} µs", ns as f64 / 1e3)
    }
}

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let dir = tempfile::tempdir().unwrap();
    println!("{LOADED} documents, then {secs} s of single writes\n");
    println!(
        "| analysis | passes | longest pass | commits | commit p50 | p99 | max | most kept | ended |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for analysing in [false, true] {
        let db = Database::open(dir.path().join(format!("{analysing}.trunkdb"))).unwrap();
        let pings = db.collection::<Ping>("pings");
        let mut ids: Vec<DocId> = Vec::new();
        for chunk in 0..LOADED / 1000 {
            let mut batch = db.batch();
            for i in 0..1000 {
                ids.push(batch.insert(&pings, ping(chunk * 1000 + i, "x")).unwrap());
            }
            batch.commit().unwrap();
        }
        let stop = AtomicBool::new(false);
        let passes = AtomicU64::new(0);
        let longest = AtomicU64::new(0);
        let most_kept = AtomicU64::new(0);
        let mut times: Vec<u64> = Vec::new();
        std::thread::scope(|scope| {
            if analysing {
                scope.spawn(|| {
                    // One snapshot for the whole run: every pass has to
                    // find what the first one found.
                    let snapshot = db.snapshot().unwrap();
                    let view = snapshot.collection::<Ping>("pings");
                    while !stop.load(Ordering::Relaxed) {
                        let start = Instant::now();
                        let count = view.count(Filter::new()).unwrap();
                        let sum: u64 = view
                            .find(Filter::new())
                            .unwrap()
                            .iter()
                            .map(|ping| ping.n)
                            .sum();
                        assert_eq!(count as u64, LOADED);
                        assert_eq!(sum, LOADED * (LOADED - 1) / 2);
                        longest.fetch_max(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                        most_kept.fetch_max(db.snapshot_info().kept_bytes, Ordering::Relaxed);
                        passes.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
            let end = Instant::now() + Duration::from_secs(secs);
            let mut n = 10_000_000u64;
            while Instant::now() < end {
                let start = Instant::now();
                if n.is_multiple_of(20) {
                    // Spread over the file: each update is another page.
                    let id = &ids[(n as usize).wrapping_mul(7919) % ids.len()];
                    pings.update(id, ping(n, "z")).unwrap();
                } else {
                    pings.insert(ping(n, "y")).unwrap();
                }
                times.push(start.elapsed().as_nanos() as u64);
                most_kept.fetch_max(db.snapshot_info().kept_bytes, Ordering::Relaxed);
                n += 1;
            }
            stop.store(true, Ordering::Relaxed);
        });
        times.sort_unstable();
        let at = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
        println!(
            "| {} | {} | {} ms | {} | {} | {} | {} | {:.1} MiB | {} |",
            if analysing {
                "one snapshot, all the run"
            } else {
                "none"
            },
            passes.load(Ordering::Relaxed),
            longest.load(Ordering::Relaxed),
            times.len(),
            show(at(0.5)),
            show(at(0.99)),
            show(*times.last().unwrap()),
            most_kept.load(Ordering::Relaxed) as f64 / (1 << 20) as f64,
            db.snapshot_info().ended,
        );
    }
}
