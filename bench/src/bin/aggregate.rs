//! Aggregates, measured for SPEC §107: the benchmark's 100,000 tasks
//! (§48) counted, summed and averaged by group — from every document,
//! from index entries (§107.5), and as application code did it before,
//! a `find` and a loop. SQLite beside them, `GROUP BY` on its columns.
//! Then columns with a condition (§126.10), SQLite's `FILTER (WHERE …)`.
//! `cargo run --release --bin aggregate [documents]`.
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::hint::black_box;
use std::time::{Duration, Instant};
use trunkdb::query::{Aggregate, Condition, Filter};
use trunkdb::{Database, DocId, Document};

/// As in the comparison (`main.rs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Task {
    status: String,
    created: i64,
    tenant: String,
    title: String,
    tries: i64,
    tags: Vec<String>,
}

const STATUSES: [&str; 3] = ["Queued", "Running", "Done"];

fn task(i: usize, n: usize) -> Task {
    Task {
        status: STATUSES[i * 7 % 3].to_string(),
        created: (i * 7919 % n) as i64,
        tenant: format!("t{}", i * 31 % 100),
        title: format!("Task {i}: {}", "lorem ipsum ".repeat(1 + i % 4)),
        tries: (i % 10) as i64,
        tags: (0..i % 3).map(|k| format!("tag{}", (i + k) % 20)).collect(),
    }
}

/// The time per call of `f`, run `n` times after one warm-up.
fn per_call<T>(n: usize, mut f: impl FnMut(usize) -> T) -> Duration {
    black_box(f(0));
    let started = Instant::now();
    for i in 0..n {
        black_box(f(i));
    }
    started.elapsed() / n as u32
}

fn show(d: Duration) -> String {
    if d.is_zero() {
        return "—".to_string();
    }
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
        .map(|arg| arg.parse().expect("the number of documents"))
        .unwrap_or(100_000);
    let dir = tempfile::tempdir().unwrap();
    let tasks: Vec<([u8; 16], Task)> = (0..n)
        .map(|i| (*uuid::Uuid::now_v7().as_bytes(), task(i, n)))
        .collect();

    // `plain` has no index: every aggregate reads every document. `tasks`
    // has the comparison's indexes and `(tenant, tries)`.
    let db = Database::open(dir.path().join("aggregate.trunkdb")).unwrap();
    let plain = db.collection::<Document>("plain");
    let indexed = db.collection::<Document>("tasks");
    indexed.ensure_index("tenant").unwrap();
    indexed.ensure_index("created").unwrap();
    indexed.ensure_index(["status", "created"]).unwrap();
    indexed.ensure_index(["tenant", "tries"]).unwrap();
    for chunk in tasks.chunks(1000) {
        let mut batch = db.batch();
        for (id, task) in chunk {
            let mut doc = Document::from_value(task).unwrap();
            if let Document::Object(fields) = &mut doc {
                fields.insert("_id".into(), Document::Id(DocId(*id)));
            }
            batch.insert(&plain, doc.clone()).unwrap();
            batch.insert(&indexed, doc).unwrap();
        }
        batch.commit().unwrap();
    }
    let typed = db.collection::<Task>("plain");

    let sqlite = Connection::open(dir.path().join("aggregate.sqlite")).unwrap();
    sqlite
        .execute_batch(
            "PRAGMA journal_mode = WAL;
            CREATE TABLE tasks (
                id BLOB PRIMARY KEY,
                status TEXT NOT NULL,
                created INTEGER NOT NULL,
                tenant TEXT NOT NULL,
                tries INTEGER NOT NULL,
                doc TEXT NOT NULL
            );
            CREATE INDEX tasks_tenant ON tasks (tenant);
            CREATE INDEX tasks_created ON tasks (created);
            CREATE INDEX tasks_status_created ON tasks (status, created);
            CREATE INDEX tasks_tenant_tries ON tasks (tenant, tries);",
        )
        .unwrap();
    {
        let tx = sqlite.unchecked_transaction().unwrap();
        let mut insert = tx
            .prepare(
                "INSERT INTO tasks (id, status, created, tenant, tries, doc) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .unwrap();
        for (id, t) in &tasks {
            let doc = serde_json::to_string(t).unwrap();
            insert
                .execute(params![
                    &id[..],
                    t.status,
                    t.created,
                    t.tenant,
                    t.tries,
                    doc
                ])
                .unwrap();
        }
        drop(insert);
        tx.commit().unwrap();
    }
    let rows = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> usize {
        let mut statement = sqlite.prepare_cached(sql).unwrap();
        let columns = statement.column_count();
        statement
            .query_map(p, |row| {
                (0..columns)
                    .map(|i| row.get::<_, rusqlite::types::Value>(i))
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap()
            .map(Result::unwrap)
            .count()
    };
    let explain = |c: &trunkdb::Collection<Document>, f: Filter, a: &Aggregate| {
        let run = c.explain_aggregate(f, a).unwrap();
        format!(
            "{:?}, {} read, {} from entries",
            run.plan, run.stats.documents_examined, run.stats.from_entries
        )
    };

    println!("{n} tasks\n");
    println!("| query | trunkdb | how | SQLite |");
    println!("|---|---:|---|---:|");
    let row = |what: &str, ours: Duration, how: String, theirs: Duration| {
        println!("| {what} | {} | {how} | {} |", show(ours), show(theirs));
    };

    let per_status = Aggregate::new().group_by("status").count("n");
    let status_sql = "SELECT status, count(*) FROM tasks GROUP BY status";
    row(
        "per status, a find and a loop",
        per_call(5, |_| {
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for task in typed.find(Filter::new()).unwrap() {
                *counts.entry(task.status).or_default() += 1;
            }
            counts.len()
        }),
        "typed, every document".into(),
        Duration::ZERO,
    );
    row(
        "per status, no index",
        per_call(5, |_| {
            plain.aggregate(Filter::new(), &per_status).unwrap().len()
        }),
        explain(&plain, Filter::new(), &per_status),
        Duration::ZERO,
    );
    row(
        "per status, on (status, created)",
        per_call(200, |_| {
            indexed.aggregate(Filter::new(), &per_status).unwrap().len()
        }),
        explain(&indexed, Filter::new(), &per_status),
        per_call(200, |_| rows(status_sql, &[])),
    );

    let per_tenant = Aggregate::new()
        .group_by("tenant")
        .count("n")
        .avg("tries", "average");
    row(
        "per tenant, count and average tries, no index",
        per_call(5, |_| {
            plain.aggregate(Filter::new(), &per_tenant).unwrap().len()
        }),
        explain(&plain, Filter::new(), &per_tenant),
        Duration::ZERO,
    );
    row(
        "the same, on (tenant, tries)",
        per_call(20, |_| {
            indexed.aggregate(Filter::new(), &per_tenant).unwrap().len()
        }),
        explain(&indexed, Filter::new(), &per_tenant),
        per_call(20, |_| {
            rows(
                "SELECT tenant, count(*), avg(tries) FROM tasks GROUP BY tenant",
                &[],
            )
        }),
    );

    let tenant = |i: usize| format!("t{}", i * 37 % 100);
    let sum = Aggregate::new().sum("tries", "total");
    row(
        "one tenant's sum of tries (1,000)",
        per_call(100, |i| {
            indexed
                .aggregate(Filter::new().eq("tenant", tenant(i)), &sum)
                .unwrap()
                .len()
        }),
        explain(&indexed, Filter::new().eq("tenant", "t1"), &sum),
        per_call(100, |i| {
            rows(
                "SELECT sum(tries) FROM tasks WHERE tenant = ?",
                &[&tenant(i)],
            )
        }),
    );
    let average = Aggregate::new().avg("tries", "average");
    row(
        "one tenant's average tries (1,000)",
        per_call(100, |i| {
            indexed
                .aggregate(Filter::new().eq("tenant", tenant(i)), &average)
                .unwrap()
                .len()
        }),
        explain(&indexed, Filter::new().eq("tenant", "t1"), &average),
        per_call(100, |i| {
            rows(
                "SELECT avg(tries) FROM tasks WHERE tenant = ?",
                &[&tenant(i)],
            )
        }),
    );

    let newest = Aggregate::new().group_by("status").max("created", "newest");
    row(
        "newest per status, on (status, created)",
        per_call(20, |_| {
            indexed.aggregate(Filter::new(), &newest).unwrap().len()
        }),
        explain(&indexed, Filter::new(), &newest),
        per_call(20, |_| {
            rows(
                "SELECT status, max(created) FROM tasks GROUP BY status",
                &[],
            )
        }),
    );

    // SPEC §126.10: columns with a condition.
    println!();
    let long = Aggregate::new()
        .group_by("tenant")
        .count("n")
        .avg("tries", "mean_long")
        .filter(Condition::gte("tries", 4));
    let long_sql =
        "SELECT tenant, count(*), avg(tries) FILTER (WHERE tries >= 4) FROM tasks GROUP BY tenant";
    row(
        "per tenant, all and mean tries of tries >= 4, no index",
        per_call(5, |_| plain.aggregate(Filter::new(), &long).unwrap().len()),
        explain(&plain, Filter::new(), &long),
        Duration::ZERO,
    );
    row(
        "the same, on (tenant, tries)",
        per_call(20, |_| {
            indexed.aggregate(Filter::new(), &long).unwrap().len()
        }),
        explain(&indexed, Filter::new(), &long),
        per_call(20, |_| rows(long_sql, &[])),
    );

    let since = (n * 4 / 5) as i64;
    let recent = Aggregate::new()
        .group_by("status")
        .count("n")
        .count("recent")
        .filter(Condition::gte("created", since));
    row(
        "per status, all and those since a time, on (status, created)",
        per_call(20, |_| {
            indexed.aggregate(Filter::new(), &recent).unwrap().len()
        }),
        explain(&indexed, Filter::new(), &recent),
        per_call(20, |_| {
            rows(
                "SELECT status, count(*), count(*) FILTER (WHERE created >= ?) FROM tasks GROUP BY status",
                &[&since],
            )
        }),
    );

    // Three counts: in one aggregate, one pass over every document; as
    // three `count`s, each through its index.
    let queued = || Condition::eq("status", "Queued");
    let tried = || Condition::gte("tries", 5);
    let late = || Condition::gte("created", since);
    let three = Aggregate::new()
        .count("queued")
        .filter(queued())
        .count("tried")
        .filter(tried())
        .count("late")
        .filter(late());
    row(
        "three counts in one aggregate",
        per_call(5, |_| {
            indexed.aggregate(Filter::new(), &three).unwrap().len()
        }),
        explain(&indexed, Filter::new(), &three),
        per_call(20, |_| {
            rows(
                "SELECT count(*) FILTER (WHERE status = 'Queued'), \
                 count(*) FILTER (WHERE tries >= 5), \
                 count(*) FILTER (WHERE created >= ?) FROM tasks",
                &[&since],
            )
        }),
    );
    row(
        "the same as three `count`s",
        per_call(20, |_| {
            [queued(), tried(), late()]
                .map(|c| indexed.count(Filter::new().and(c)).unwrap())
                .len()
        }),
        "three index reads".into(),
        Duration::ZERO,
    );
}
