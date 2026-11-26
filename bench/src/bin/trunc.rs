//! Groups by a date-time cut to a unit (SPEC §211, §212): a million
//! readings, one a second, counted per minute, hour and day through an
//! index on the date-time. Beside trunkdb's aggregate, two bounds from
//! counts through the same index: one count per bucket, each a range
//! counted from entries a leaf at a time (§101, §104), and one count of
//! the whole range. SQLite beside them, `GROUP BY at / 60` on an index.
//! `cargo run --release --bin trunc [readings]`.
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::hint::black_box;
use std::time::{Duration, Instant};
use trunkdb::query::{Aggregate, Filter, Trunc};
use trunkdb::{Database, DateTime, Document};

/// A reading of a device, one a second.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Reading {
    at: DateTime,
    device: String,
    lat: f64,
    lon: f64,
    speed: f64,
}

/// 2026-01-01T00:00:00Z.
const START: i64 = 1_767_225_600;
const MINUTE: i64 = 60;
const HOUR: i64 = 3600;
const DAY: i64 = 86_400;

fn at(secs: i64) -> DateTime {
    DateTime::from_unix(secs, 0).unwrap()
}

fn reading(i: usize) -> Reading {
    Reading {
        at: at(START + i as i64),
        device: format!("d{}", i % 10),
        lat: 52.0 + (i % 1000) as f64 / 10_000.0,
        lon: 13.0 + (i % 777) as f64 / 10_000.0,
        speed: (i % 120) as f64 / 3.0,
    }
}

/// The time per call of `f`, run `n` times after one warm-up.
fn per_call<T>(n: usize, mut f: impl FnMut() -> T) -> Duration {
    black_box(f());
    let started = Instant::now();
    for _ in 0..n {
        black_box(f());
    }
    started.elapsed() / n as u32
}

fn show(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    match us {
        us if us >= 10_000.0 => format!("{:.1} ms", us / 1000.0),
        us if us >= 1000.0 => format!("{:.2} ms", us / 1000.0),
        us => format!("{us:.1} µs"),
    }
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .map(|arg| arg.parse().expect("the number of readings"))
        .unwrap_or(1_000_000);
    let end = START + n as i64;
    let dir = tempfile::tempdir().unwrap();

    let db = Database::open(dir.path().join("trunc.trunkdb")).unwrap();
    let readings = db.collection::<Document>("readings");
    readings.ensure_index("at").unwrap();
    let loading = Instant::now();
    for chunk in (0..n).collect::<Vec<_>>().chunks(1000) {
        let mut batch = db.batch();
        for &i in chunk {
            batch
                .insert(&readings, Document::from_value(&reading(i)).unwrap())
                .unwrap();
        }
        batch.commit().unwrap();
    }
    db.checkpoint().unwrap();
    eprintln!("trunkdb loaded in {:.1?}", loading.elapsed());

    let sqlite = Connection::open(dir.path().join("trunc.sqlite")).unwrap();
    sqlite
        .execute_batch(
            "PRAGMA journal_mode = WAL;
            CREATE TABLE readings (
                at INTEGER NOT NULL,
                device TEXT NOT NULL,
                lat REAL NOT NULL,
                lon REAL NOT NULL,
                speed REAL NOT NULL
            );
            CREATE INDEX readings_at ON readings (at);",
        )
        .unwrap();
    {
        let tx = sqlite.unchecked_transaction().unwrap();
        let mut insert = tx
            .prepare("INSERT INTO readings VALUES (?, ?, ?, ?, ?)")
            .unwrap();
        for i in 0..n {
            let r = reading(i);
            insert
                .execute(params![START + i as i64, r.device, r.lat, r.lon, r.speed])
                .unwrap();
        }
        drop(insert);
        tx.commit().unwrap();
    }
    let sql_groups = |unit: i64, from: i64| -> usize {
        let mut statement = sqlite
            .prepare_cached("SELECT at / ?1, count(*) FROM readings WHERE at >= ?2 GROUP BY 1")
            .unwrap();
        statement
            .query_map(params![unit, from], |row| row.get::<_, i64>(1))
            .unwrap()
            .map(Result::unwrap)
            .count()
    };

    let since = |from: i64| Filter::new().gte("at", at(from));
    let explain = |filter: Filter, aggregate: &Aggregate| {
        let run = readings.explain_aggregate(filter, aggregate).unwrap();
        format!(
            "{} documents read, {} built from entries",
            run.stats.documents_examined, run.stats.from_entries
        )
    };
    // Today's way to count buckets from entries: a count of each range.
    let bucket_counts = |unit: i64, from: i64| -> usize {
        let first = from - from.rem_euclid(unit);
        (first..end)
            .step_by(unit as usize)
            .map(|b| {
                readings
                    .count(
                        Filter::new()
                            .gte("at", at(b.max(from)))
                            .lt("at", at(b + unit)),
                    )
                    .unwrap()
            })
            .filter(|&c| c > 0)
            .count()
    };
    let range_count = |from: i64| readings.count(since(from)).unwrap();

    println!("{n} readings, one a second, an index on `at`");
    // How many entries a leaf holds: whether a minute's fill one.
    let stats = db.stats().unwrap();
    let index = &stats.collections[0].indexes[0];
    println!(
        "the index: {} pages, depth {}, {:.0} entries a page\n",
        index.pages,
        index.depth,
        index.entries as f64 / index.pages as f64
    );
    println!(
        "| query | rows | trunkdb | how | a count per bucket | one count of the range | SQLite |"
    );
    println!("|---|---:|---:|---|---:|---:|---:|");
    let minute = Trunc::every(Duration::from_secs(60));
    let hour = Trunc::every(Duration::from_secs(3600));
    let day = Trunc::every(Duration::from_secs(86_400));
    // The last: whether the filter bounds `at`, or there is none. Without
    // one, an index is walked only if it holds every document (§107.5),
    // which step 1 never asks: a cut needs documents (§210.2).
    let cases: [(&str, Trunc, i64, i64, usize, bool); 6] = [
        (
            "per minute, the last hour",
            minute,
            MINUTE,
            end - HOUR,
            50,
            true,
        ),
        (
            "per minute, the last day",
            minute,
            MINUTE,
            end - DAY,
            10,
            true,
        ),
        ("per hour, all, `at >= start`", hour, HOUR, START, 3, true),
        ("per hour, all, no filter", hour, HOUR, START, 3, false),
        ("per day, all, `at >= start`", day, DAY, START, 3, true),
        ("per day, all, no filter", day, DAY, START, 3, false),
    ];
    for (what, unit, secs, from, runs, bounded) in cases {
        let aggregate = Aggregate::new()
            .group_by_trunc("at", unit)
            .count()
            .named("n");
        let filter = || if bounded { since(from) } else { Filter::new() };
        let rows = readings.aggregate(filter(), &aggregate).unwrap().len();
        assert_eq!(rows, bucket_counts(secs, from), "{what}");
        assert_eq!(rows, sql_groups(secs, from), "{what}");
        println!(
            "| {what} | {rows} | {} | {} | {} | {} | {} |",
            show(per_call(runs, || readings
                .aggregate(filter(), &aggregate)
                .unwrap()
                .len())),
            explain(filter(), &aggregate),
            show(per_call(runs * 4, || bucket_counts(secs, from))),
            show(per_call(runs * 20, || range_count(from))),
            show(per_call(runs, || sql_groups(secs, from))),
        );
    }
}
