//! Projection, measured for SPEC §106: the benchmark's 100,000 tasks
//! (§48) read untyped, whole and with a few fields selected, and with a
//! selection an index's entries answer (§106.5) — SQLite beside them,
//! `SELECT doc` against a column list. `cargo run --release --bin
//! select [documents]`.
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::hint::black_box;
use std::time::{Duration, Instant};
use trunkdb::{Database, DocId, Document, query::Filter};

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

    // trunkdb: the comparison's indexes in `tasks`; in `paired`, the same
    // documents with `(tenant, status)` instead, whose entries answer a
    // selection of those two.
    let db = Database::open(dir.path().join("select.trunkdb")).unwrap();
    let docs = db.collection::<Document>("tasks");
    docs.ensure_index("tenant").unwrap();
    docs.ensure_index("created").unwrap();
    docs.ensure_index(["status", "created"]).unwrap();
    let paired = db.collection::<Document>("paired");
    paired.ensure_index(["tenant", "status"]).unwrap();
    for chunk in tasks.chunks(1000) {
        let mut batch = db.batch();
        for (id, task) in chunk {
            let mut doc = Document::from_value(task).unwrap();
            if let Document::Object(fields) = &mut doc {
                fields.insert("_id".into(), Document::Id(DocId(*id)));
            }
            batch.insert(&docs, doc.clone()).unwrap();
            batch.insert(&paired, doc).unwrap();
        }
        batch.commit().unwrap();
    }

    let sqlite = Connection::open(dir.path().join("select.sqlite")).unwrap();
    sqlite
        .execute_batch(
            "PRAGMA journal_mode = WAL;
            CREATE TABLE tasks (
                id BLOB PRIMARY KEY,
                status TEXT NOT NULL,
                created INTEGER NOT NULL,
                tenant TEXT NOT NULL,
                doc TEXT NOT NULL
            );
            CREATE INDEX tasks_tenant ON tasks (tenant);
            CREATE INDEX tasks_created ON tasks (created);
            CREATE INDEX tasks_status_created ON tasks (status, created);",
        )
        .unwrap();
    {
        let tx = sqlite.unchecked_transaction().unwrap();
        let mut insert = tx
            .prepare("INSERT INTO tasks (id, status, created, tenant, doc) VALUES (?, ?, ?, ?, ?)")
            .unwrap();
        for (id, t) in &tasks {
            let doc = serde_json::to_string(t).unwrap();
            insert
                .execute(params![&id[..], t.status, t.created, t.tenant, doc])
                .unwrap();
        }
        drop(insert);
        tx.commit().unwrap();
    }
    // Untyped, as trunkdb's `Document`: a JSON value per row.
    let whole_rows = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> usize {
        let mut statement = sqlite.prepare_cached(sql).unwrap();
        let rows = statement
            .query_map(p, |row| row.get::<_, String>(0))
            .unwrap()
            .map(|doc| serde_json::from_str::<serde_json::Value>(&doc.unwrap()).unwrap());
        rows.count()
    };
    let column_rows = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> usize {
        let mut statement = sqlite.prepare_cached(sql).unwrap();
        let rows = statement
            .query_map(p, |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, rusqlite::types::Value>(1)?,
                    row.get::<_, rusqlite::types::Value>(2)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap);
        rows.count()
    };

    let tenant = |i: usize| format!("t{}", i * 37 % 100);
    let explain = |c: &trunkdb::Collection<Document>, f: Filter| {
        let run = c.explain_run(f).unwrap();
        format!(
            "{} read, {} from entries",
            run.stats.documents_examined, run.stats.from_entries
        )
    };
    println!("{n} tasks, untyped\n");
    println!("| query | trunkdb | documents | SQLite |");
    println!("|---|---:|---|---:|");
    let row = |what: &str, ours: Duration, how: String, theirs: Duration| {
        let theirs = match theirs.is_zero() {
            true => "—".to_string(),
            false => show(theirs),
        };
        println!("| {what} | {} | {how} | {theirs} |", show(ours));
    };

    // tenant == x: 1,000 documents.
    let by_tenant = |i: usize| Filter::new().eq("tenant", tenant(i));
    let two = ["tenant", "status"];
    row(
        "tenant == x, whole",
        per_call(100, |i| docs.find(by_tenant(i)).unwrap().len()),
        explain(&docs, by_tenant(1)),
        per_call(100, |i| {
            whole_rows("SELECT doc FROM tasks WHERE tenant = ?", &[&tenant(i)])
        }),
    );
    row(
        "tenant == x, select tenant, status",
        per_call(100, |i| docs.find(by_tenant(i).select(two)).unwrap().len()),
        explain(&docs, by_tenant(1).select(two)),
        per_call(100, |i| {
            column_rows(
                "SELECT id, tenant, status FROM tasks WHERE tenant = ?",
                &[&tenant(i)],
            )
        }),
    );
    row(
        "the same, on (tenant, status)",
        per_call(100, |i| {
            paired.find(by_tenant(i).select(two)).unwrap().len()
        }),
        explain(&paired, by_tenant(1).select(two)),
        Duration::ZERO,
    );

    // Page 50 of 20, the queued by creation.
    let page = || {
        Filter::new()
            .eq("status", "Queued")
            .sort_asc("created")
            .skip(1000)
            .limit(20)
    };
    let page_sql = |columns: &str| {
        format!(
            "SELECT {columns} FROM tasks WHERE status = 'Queued' ORDER BY created LIMIT 20 OFFSET 1000"
        )
    };
    row(
        "page 50 of 20, whole",
        per_call(2000, |_| docs.find(page()).unwrap().len()),
        explain(&docs, page()),
        per_call(2000, |_| whole_rows(&page_sql("doc"), &[])),
    );
    row(
        "page 50 of 20, select status, created",
        per_call(2000, |_| {
            docs.find(page().select(["status", "created"]))
                .unwrap()
                .len()
        }),
        explain(&docs, page().select(["status", "created"])),
        per_call(2000, |_| column_rows(&page_sql("id, status, created"), &[])),
    );
    row(
        "page 50 of 20, select status",
        per_call(2000, |_| {
            docs.find(page().select(["status"])).unwrap().len()
        }),
        explain(&docs, page().select(["status"])),
        Duration::ZERO,
    );

    // Every id.
    row(
        "every document, whole",
        per_call(5, |_| docs.find(Filter::new()).unwrap().len()),
        explain(&docs, Filter::new()),
        per_call(5, |_| whole_rows("SELECT doc FROM tasks", &[])),
    );
    let ids = || Filter::new().select([] as [&str; 0]);
    row(
        "every id, select nothing",
        per_call(5, |_| docs.find(ids()).unwrap().len()),
        explain(&docs, ids()),
        per_call(5, |_| column_rows("SELECT id, id, id FROM tasks", &[])),
    );

    // A scan: tries > 7, 20,000 of 100,000.
    let scan = || Filter::new().gt("tries", 7);
    let scan_sql = |columns: &str| {
        format!("SELECT {columns} FROM tasks WHERE json_extract(doc, '$.tries') > 7")
    };
    row(
        "tries > 7 (a scan), whole",
        per_call(5, |_| docs.find(scan()).unwrap().len()),
        explain(&docs, scan()),
        per_call(5, |_| whole_rows(&scan_sql("doc"), &[])),
    );
    row(
        "tries > 7 (a scan), select tenant, status",
        per_call(5, |_| docs.find(scan().select(two)).unwrap().len()),
        explain(&docs, scan().select(two)),
        per_call(5, |_| column_rows(&scan_sql("id, tenant, status"), &[])),
    );
}
