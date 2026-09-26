//! What the fuzz targets share (SPEC §55): a small database with every
//! kind of page and index in it, a way to open bytes as a database, and
//! the calls made on whatever opens.
//!
//! A target's only rule is that trunkdb doesn't panic or hang: any
//! `Err` is a correct answer to damaged input.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::OnceLock;
use trunkdb::query::{Condition, Filter};
use trunkdb::{Database, Document, IndexOptions};

#[derive(Serialize, Deserialize)]
struct Task {
    status: String,
    created: i64,
    tenant: String,
    title: String,
    tags: Vec<String>,
    nick: Option<String>,
    lines: Vec<Line>,
}

#[derive(Serialize, Deserialize)]
struct Line {
    sku: String,
    qty: i64,
}

/// The pages of a database built once per run, without their checksums
/// (`trunkdb::fuzzing::unseal`): two collections, several data pages, a
/// document in overflow pages, a plain, a unique, a compound, a sparse
/// and two multikey indexes, deep enough for branch pages, and free
/// pages from a dropped collection.
pub fn base() -> &'static [u8] {
    static BASE: OnceLock<Vec<u8>> = OnceLock::new();
    BASE.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.trunkdb");
        let db = Database::open(&path).unwrap();
        let tasks = db.collection::<Task>("tasks");
        tasks.ensure_index("tenant").unwrap();
        tasks
            .ensure_index_with("created", trunkdb::IndexOptions::new().unique())
            .unwrap();
        tasks.ensure_index(["status", "created"]).unwrap();
        let sparse = IndexOptions::new().sparse();
        tasks.ensure_index_with("nick", sparse).unwrap();
        tasks.ensure_index("tags[*]").unwrap();
        tasks.ensure_index("lines[*].sku").unwrap();
        let mut batch = db.batch();
        for i in 0..400i64 {
            let task = Task {
                status: ["Queued", "Running", "Done"][(i % 3) as usize].to_string(),
                created: i * 7 % 400,
                tenant: format!("t{}", i % 5),
                title: "x".repeat(if i == 7 {
                    30_000
                } else {
                    (i * 13 % 90) as usize
                }),
                tags: (0..i % 3).map(|k| format!("tag{}", (i + k) % 6)).collect(),
                nick: (i % 4 == 0).then(|| format!("n{i}")),
                lines: (0..i % 2 + 1)
                    .map(|k| Line {
                        sku: format!("A{}", (i + k) % 9),
                        qty: i % 11,
                    })
                    .collect(),
            };
            batch.insert(&tasks, task).unwrap();
        }
        batch.commit().unwrap();
        tasks.delete_many(Filter::new().eq("tenant", "t3")).unwrap();

        let notes = db.collection::<Document>("notes");
        notes.ensure_index("n").unwrap();
        for n in 0..20i64 {
            let doc = Document::Object(
                [
                    ("n".to_string(), Document::Int(n)),
                    ("f".to_string(), Document::Float(n as f64 / 3.0)),
                ]
                .into_iter()
                .collect(),
            );
            notes.insert(doc).unwrap();
        }
        let scratch = db.collection::<Document>("scratch");
        for n in 0..30i64 {
            scratch.insert(Document::Int(n)).unwrap();
        }
        db.drop_collection("scratch").unwrap();
        db.checkpoint().unwrap();
        drop((tasks, notes, db));
        trunkdb::fuzzing::unseal(&std::fs::read(&path).unwrap())
    })
}

/// Which calls `exercise` makes, from a byte the input chooses: every
/// run doing everything would make each one slow, so each does a part,
/// and the fuzzer finds its way to all of them.
pub const CHECK: u8 = 1;
pub const FIND: u8 = 2;
pub const EXPORT: u8 = 4;
/// Writes and a compaction, which change pages the input may have
/// damaged.
pub const WRITE: u8 = 8;
pub const ALL: u8 = CHECK | FIND | EXPORT | WRITE;

/// Writes `file` (and `wal`, if any) into a fresh directory, opens it,
/// and runs `exercise` on it if it opens.
pub fn open_and_exercise(file: &[u8], wal: Option<&[u8]>, calls: u8) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fuzz.trunkdb");
    std::fs::write(&path, file).unwrap();
    if let Some(wal) = wal {
        std::fs::write(wal_path(&path), wal).unwrap();
    }
    if let Ok(db) = Database::open(&path) {
        exercise(&db, calls);
    }
}

fn wal_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".wal");
    name.into()
}

/// The `calls` chosen: every kind of read, through each kind of plan,
/// and writes.
pub fn exercise(db: &Database, calls: u8) {
    let _ = db.file_info();
    if calls & CHECK != 0 {
        let _ = db.check();
    }
    let names = db.collections().unwrap_or_default();
    for name in names.iter().take(4).filter(|_| calls & FIND != 0) {
        let docs = db.collection::<Document>(name);
        let _ = docs.indexes();
        let _ = docs.count(Filter::new());
        let _ = docs.find(Filter::new());
        let _ = docs.find(Filter::new().eq("tenant", "t1"));
        let _ = docs.find(Filter::new().gte("created", 100).lt("created", 140));
        let _ = docs.find(
            Filter::new()
                .eq("status", "Queued")
                .sort_desc("created")
                .limit(5),
        );
        let _ = docs.find(Filter::new().sort_asc("created").limit(5));
        let _ = docs.find(Filter::new().eq("tags[*]", "tag2"));
        let _ = docs.find(Filter::new().eq("nick", "n8"));
        let _ = docs.find(Filter::new().exists("nick"));
        let _ = docs.find(Filter::new().elem_match(
            "lines",
            Condition::eq("sku", "A3") & Condition::gte("qty", 2),
        ));
        let _ =
            docs.find(Filter::new().any_of([Condition::eq("tenant", "t2"), Condition::eq("n", 4)]));
        if let Ok(cursor) = docs.cursor(Filter::new()) {
            for _ in cursor.take(100) {}
        }
    }
    if calls & EXPORT != 0 {
        let _ = db.export(std::io::sink());
    }
    if calls & WRITE != 0 {
        let tasks = db.collection::<Document>("tasks");
        let doc = Document::Object(
            [
                ("tenant".to_string(), Document::String("t1".into())),
                ("created".to_string(), Document::Int(1_000)),
                ("status".to_string(), Document::String("Queued".into())),
            ]
            .into_iter()
            .collect(),
        );
        let _ = tasks.insert(doc);
        let _ = tasks.delete_many(Filter::new().eq("tenant", "t2").limit(3));
        let _ = db.compact();
        let _ = db.check();
    }
}

/// Applies `edits` to `pages` (without checksums): each 4 bytes are
/// `[page][offset, u16][value]`, the page and offset wrapped into range.
/// Returns which pages changed, in order, each once.
pub fn apply_edits(pages: &mut [u8], edits: &[u8]) -> Vec<usize> {
    let count = pages.len() / trunkdb::fuzzing::USABLE_PAGE_SIZE;
    let mut changed = Vec::new();
    for edit in edits.as_chunks::<4>().0 {
        let page = edit[0] as usize % count;
        let offset =
            u16::from_le_bytes([edit[1], edit[2]]) as usize % trunkdb::fuzzing::USABLE_PAGE_SIZE;
        pages[page * trunkdb::fuzzing::USABLE_PAGE_SIZE + offset] = edit[3];
        if !changed.contains(&page) {
            changed.push(page);
        }
    }
    changed
}
