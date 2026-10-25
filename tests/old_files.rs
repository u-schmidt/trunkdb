//! Files written by earlier releases, read by this one (SPEC §120).
//!
//! `tests/fixtures/<release>/` holds a database that release wrote,
//! `trunkdb.db`, and what it holds, written by the same run:
//! `documents.jsonl`, one `{"collection", "document"}` per line, and
//! `indexes.jsonl`, one `{"collection", "fields", "unique", "sparse"}`
//! per line. Every fixture is opened and read here, then written to.
//!
//! A release that changes how files are written adds its own with
//! `cargo test --test old_files -- --ignored write_this_releases_fixture`,
//! which writes `tests/fixtures/<the version in Cargo.toml>/` and refuses
//! to write over one that's there (SPEC §120.4, §120.9).

use std::fs;
use std::path::{Path, PathBuf};

use trunkdb::query::{Filter, QueryPlan};
use trunkdb::{Database, Document, IndexOptions, OpenOptions};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn doc(json: &str) -> Document {
    json.parse().unwrap_or_else(|e| panic!("{json}: {e}"))
}

fn field<'a>(doc: &'a Document, name: &str) -> Option<&'a Document> {
    match doc {
        Document::Object(map) => map.get(name),
        _ => None,
    }
}

/// A small generator, so a fixture's data is the same each time it's
/// written.
struct Numbers(u64);

impl Numbers {
    fn next(&mut self, below: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % below
    }
}

/// What this release writes: every kind of value, documents on overflow
/// pages, indexes of each kind, one with a branch above its leaves,
/// deletes and updates that leave free space, and a dropped collection's
/// pages on the free list. Kept small (SPEC §120.9): 21 pages.
fn write_database(path: &Path) {
    let db = Database::open(path).unwrap();

    let kinds = db.collection::<Document>("kinds");
    let first = kinds.insert(doc(r#"{"kind": "null", "v": null}"#)).unwrap();
    for json in [
        r#"{"kind": "bool", "v": true}"#,
        r#"{"kind": "bool", "v": false}"#,
        r#"{"kind": "int", "v": -9223372036854775808}"#,
        r#"{"kind": "int", "v": 9223372036854775807}"#,
        r#"{"kind": "int", "v": 0}"#,
        r#"{"kind": "float", "v": 5.0}"#,
        r#"{"kind": "float", "v": -0.0}"#,
        r#"{"kind": "float", "v": 2.5e-300}"#,
        r#"{"kind": "float", "v": {"$float": "NaN"}}"#,
        r#"{"kind": "float", "v": {"$float": "Infinity"}}"#,
        r#"{"kind": "float", "v": {"$float": "-Infinity"}}"#,
        r#"{"kind": "string", "v": ""}"#,
        r#"{"kind": "string", "v": "Größe ✓ \"quoted\"\nnext line\t\u0000"}"#,
        r#"{"kind": "binary", "v": {"$binary": ""}}"#,
        r#"{"kind": "binary", "v": {"$binary": "AAEC/f7/"}}"#,
        r#"{"kind": "date", "v": {"$date": "2024-05-01T12:00:00.123456789Z"}}"#,
        r#"{"kind": "date", "v": {"$date": "1969-12-31T23:59:59Z"}}"#,
        r#"{"kind": "array", "v": [1, 2.0, "three", null, [true, []], {}]}"#,
        r#"{"kind": "object", "v": {"a": {"b": {"c": [{"d": 1}]}}, "": "empty name", "a.b": "dot"}}"#,
    ] {
        kinds.insert(doc(json)).unwrap();
    }
    kinds
        .insert(doc(&format!(
            r#"{{"kind": "id", "v": {{"$id": "{first}"}}}}"#
        )))
        .unwrap();
    let long: String = (0..9_000)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    kinds
        .insert(doc(&format!(r#"{{"kind": "overflow", "v": "{long}"}}"#)))
        .unwrap();
    let numbers: Vec<String> = (0..1_000).map(|i| (i * 7).to_string()).collect();
    kinds
        .insert(doc(&format!(
            r#"{{"kind": "overflow", "v": [{}]}}"#,
            numbers.join(", ")
        )))
        .unwrap();

    let tasks = db.collection::<Document>("tasks");
    tasks.ensure_index("status").unwrap();
    tasks.ensure_index(["team", "created"]).unwrap();
    tasks
        .ensure_index_with("n", IndexOptions::new().unique())
        .unwrap();
    tasks
        .ensure_index_with("owner", IndexOptions::new().sparse())
        .unwrap();
    let statuses = ["open", "blocked", "done", "dropped"];
    // Long names, so the compound index has more than a leaf's worth of
    // entries and a branch above them, with 60 documents.
    let teams =
        ["red", "green", "blue", "gray", "black"].map(|team| format!("{team}-{}", "t".repeat(150)));
    let mut random = Numbers(0x2545_f491_4f6c_dd1d);
    let mut ids = Vec::new();
    for n in 0..60u64 {
        let hours = match n % 2 {
            0 => (random.next(40)).to_string(),
            _ => format!("{}.5", random.next(40)),
        };
        let owner = match n % 5 {
            0 => String::new(),
            1 => r#", "owner": null"#.to_string(),
            _ => format!(r#", "owner": "person-{}""#, random.next(30)),
        };
        let json = format!(
            r#"{{"n": {n}, "title": "task {n}", "status": "{}", "team": "{}", "hours": {hours}, "created": {{"$date": "2026-{:02}-{:02}T{:02}:00:00Z"}}, "tags": ["t{}", "t{}"]{owner}}}"#,
            statuses[random.next(4) as usize],
            teams[random.next(5) as usize],
            1 + random.next(12),
            1 + random.next(28),
            random.next(24),
            random.next(10),
            random.next(10),
        );
        ids.push(tasks.insert(doc(&json)).unwrap());
    }
    for (n, id) in ids.iter().enumerate() {
        if n % 7 == 0 {
            tasks.delete(id).unwrap();
        } else if n % 11 == 0 {
            let mut task = tasks.get(id).unwrap().unwrap();
            if let Document::Object(map) = &mut task {
                map.insert("title".into(), Document::from("x".repeat(500)));
                map.insert("status".into(), Document::from("done"));
            }
            tasks.update(id, task).unwrap();
        }
    }

    let scratch = db.collection::<Document>("scratch");
    for n in 0..40 {
        scratch
            .insert(doc(&format!(
                r#"{{"n": {n}, "pad": "{}"}}"#,
                "p".repeat(200)
            )))
            .unwrap();
    }
    db.drop_collection("scratch").unwrap();
    db.checkpoint().unwrap();
}

/// Each collection's documents in id order, and its indexes, as text
/// lines: what `documents.jsonl` and `indexes.jsonl` hold.
fn contents(db: &Database) -> (Vec<String>, Vec<String>) {
    let mut documents = Vec::new();
    let mut indexes = Vec::new();
    for name in db.collections().unwrap() {
        let collection = db.collection::<Document>(&name);
        for document in collection.find(Filter::new()).unwrap() {
            documents.push(format!(
                r#"{{"collection": {}, "document": {document}}}"#,
                Document::from(name.as_str())
            ));
        }
        for index in collection.indexes().unwrap() {
            let fields: Vec<Document> = index
                .fields()
                .iter()
                .map(|f| Document::from(f.as_str()))
                .collect();
            indexes.push(format!(
                r#"{{"collection": {}, "fields": {}, "unique": {}, "sparse": {}}}"#,
                Document::from(name.as_str()),
                Document::Array(fields),
                index.is_unique(),
                index.is_sparse(),
            ));
        }
    }
    (documents, indexes)
}

#[test]
#[ignore = "writes this release's fixture; run once per release"]
fn write_this_releases_fixture() {
    let dir = fixtures().join(env!("CARGO_PKG_VERSION"));
    assert!(!dir.exists(), "{} is there already", dir.display());
    let work = tempfile::tempdir().unwrap();
    let path = work.path().join("trunkdb.db");
    write_database(&path);

    // The lines are read back from the closed file, so they say what's in
    // it, not what was meant to be.
    let db = Database::open_with(&path, OpenOptions::default().read_only(true)).unwrap();
    let report = db.check().unwrap();
    assert!(report.is_ok(), "{:?}", report.problems);
    let (documents, indexes) = contents(&db);
    drop(db);

    fs::create_dir_all(&dir).unwrap();
    fs::copy(&path, dir.join("trunkdb.db")).unwrap();
    fs::write(dir.join("documents.jsonl"), documents.join("\n") + "\n").unwrap();
    fs::write(dir.join("indexes.jsonl"), indexes.join("\n") + "\n").unwrap();
}

/// The lines of a fixture's file, each read as a `Document` and printed
/// again, so they compare as this build prints them.
fn read_lines(path: &Path) -> Vec<Document> {
    fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .map(doc)
        .collect()
}

fn printed(docs: impl IntoIterator<Item = Document>) -> Vec<String> {
    docs.into_iter().map(|d| d.to_string()).collect()
}

/// The fixtures, oldest release first.
fn releases() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(fixtures())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("trunkdb.db").exists())
        .collect();
    dirs.sort();
    dirs
}

/// The releases a fixture's header names (SPEC §125): its own, as
/// "created by" and "written by", from 0.24.0 on, when the header began
/// to name them; unknown for one before.
fn named_releases(release: &str) -> Option<String> {
    let parts: Vec<u16> = release
        .split('.')
        .map(|part| part.parse().unwrap_or_else(|_| panic!("{release}")))
        .collect();
    (parts[..] >= [0, 24, 0][..]).then(|| release.to_string())
}

/// What a fixture says each collection holds, by collection, in order.
fn expected(dir: &Path) -> Vec<(String, Vec<Document>, Vec<Document>)> {
    let mut collections: Vec<(String, Vec<Document>, Vec<Document>)> = Vec::new();
    fn slot(
        collections: &mut Vec<(String, Vec<Document>, Vec<Document>)>,
        name: &Document,
    ) -> usize {
        let Document::String(name) = name else {
            panic!("{name}")
        };
        match collections.iter().position(|(n, _, _)| n == name) {
            Some(at) => at,
            None => {
                collections.push((name.clone(), Vec::new(), Vec::new()));
                collections.len() - 1
            }
        }
    }
    for line in read_lines(&dir.join("documents.jsonl")) {
        let at = slot(&mut collections, field(&line, "collection").unwrap());
        collections[at]
            .1
            .push(field(&line, "document").unwrap().clone());
    }
    for line in read_lines(&dir.join("indexes.jsonl")) {
        let at = slot(&mut collections, field(&line, "collection").unwrap());
        collections[at].2.push(line);
    }
    collections
}

fn first_field(index: &Document) -> &str {
    let Some(Document::Array(fields)) = field(index, "fields") else {
        panic!("{index}")
    };
    let Document::String(first) = &fields[0] else {
        panic!("{index}")
    };
    first
}

/// The same documents, and, where the filter sorts, the sort key in the
/// same order: documents with equal keys may come in any order.
fn same_answer(found: Vec<Document>, expected: Vec<Document>, key: &str, what: &str) {
    let keys = |docs: &[Document]| -> Vec<String> {
        docs.iter()
            .map(|d| field(d, key).map(|v| v.to_string()).unwrap_or_default())
            .collect()
    };
    assert_eq!(keys(&found), keys(&expected), "{what}");
    let mut found = printed(found);
    let mut expected = printed(expected);
    found.sort();
    expected.sort();
    assert_eq!(found, expected, "{what}");
}

/// Filters on each index's first field: a sort each way, a range, and
/// null; and equality on values documents hold, which must be read
/// through an index (`true`) rather than a scan.
fn filters_through(index: &Document, documents: &[Document]) -> Vec<(Filter, bool)> {
    let first = first_field(index);
    let mut filters = vec![
        (Filter::new().sort_asc(first), false),
        (Filter::new().sort_desc(first), false),
        (Filter::new().is_null(first), false),
    ];
    for document in documents.iter().step_by(37) {
        if let Some(value) = field(document, first) {
            // A sparse index holds no nulls, so null is found by a scan.
            let indexed = *value != Document::Null;
            filters.push((Filter::new().eq(first, value.clone()), indexed));
            filters.push((
                Filter::new().gte(first, value.clone()).sort_asc(first),
                false,
            ));
        }
    }
    filters
}

#[test]
fn every_release_since_0_23_reads_as_it_was_written() {
    let releases = releases();
    assert!(
        !releases.is_empty(),
        "no fixtures in {}",
        fixtures().display()
    );
    for dir in releases {
        let release = dir.file_name().unwrap().to_string_lossy().into_owned();
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("trunkdb.db");
        fs::copy(dir.join("trunkdb.db"), &path).unwrap();
        let expected = expected(&dir);

        let db = Database::open_with(&path, OpenOptions::default().read_only(true)).unwrap();
        let report = db.check().unwrap();
        assert!(report.is_ok(), "{release}: {:?}", report.problems);
        let releases = |db: &Database| {
            let info = db.file_info().unwrap();
            let shown = |release: Option<trunkdb::Release>| release.map(|r| r.to_string());
            (shown(info.created_by), shown(info.written_by))
        };
        let named = named_releases(&release);
        assert_eq!(releases(&db), (named.clone(), named.clone()), "{release}");
        let names: Vec<String> = expected.iter().map(|(name, _, _)| name.clone()).collect();
        assert_eq!(db.collections().unwrap(), names, "{release}");

        for (name, documents, indexes) in &expected {
            let collection = db.collection::<Document>(name);
            assert_eq!(
                printed(collection.find(Filter::new()).unwrap()),
                printed(documents.clone()),
                "{release}: {name}"
            );
            let found: Vec<Document> = collection
                .indexes()
                .unwrap()
                .iter()
                .map(|index| {
                    let fields: Vec<Document> = index
                        .fields()
                        .iter()
                        .map(|f| Document::from(f.as_str()))
                        .collect();
                    doc(&format!(
                        r#"{{"collection": {}, "fields": {}, "unique": {}, "sparse": {}}}"#,
                        Document::from(name.as_str()),
                        Document::Array(fields),
                        index.is_unique(),
                        index.is_sparse(),
                    ))
                })
                .collect();
            assert_eq!(
                printed(found),
                printed(indexes.clone()),
                "{release}: {name}"
            );

            for index in indexes {
                for (filter, through_index) in filters_through(index, documents) {
                    assert!(
                        !through_index
                            || !matches!(collection.explain(&filter).unwrap(), QueryPlan::Scan),
                        "{release}: {name}: {filter} scans"
                    );
                    let sorted = filter.clone().apply(documents.clone());
                    same_answer(
                        collection.find(filter.clone()).unwrap(),
                        sorted,
                        first_field(index),
                        &format!("{release}: {name}: {filter}"),
                    );
                }
            }
        }
        drop(db);

        // Written to by this build, it's still whole, and reads back.
        let db = Database::open(&path).unwrap();
        for (name, documents, indexes) in &expected {
            let collection = db.collection::<Document>(name);
            let added = collection.insert(doc(r#"{"added": true}"#)).unwrap();
            if let Some(Document::Object(first)) = documents.first() {
                let id = match first.get("_id") {
                    Some(Document::Id(id)) => *id,
                    other => panic!("{release}: {name}: _id is {other:?}"),
                };
                assert!(collection.delete(&id).unwrap(), "{release}: {name}");
            }
            for index in indexes {
                let unique = field(index, "unique") == Some(&Document::Bool(true));
                let Some(Document::Array(fields)) = field(index, "fields") else {
                    panic!("{index}")
                };
                let Document::String(first) = &fields[0] else {
                    panic!("{index}")
                };
                let taken = documents.iter().skip(1).find_map(|d| field(d, first));
                if let (true, 1, Some(value)) = (unique, fields.len(), taken)
                    && *value != Document::Null
                {
                    let mut copy = trunkdb::Map::new();
                    copy.insert(first.clone(), value.clone());
                    assert!(
                        collection.insert(Document::Object(copy)).is_err(),
                        "{release}: {name}: a duplicate in {first} was stored"
                    );
                }
            }
            assert!(collection.delete(&added).unwrap(), "{release}: {name}");
        }
        drop(db);
        let db = Database::open(&path).unwrap();
        let report = db.check().unwrap();
        assert!(
            report.is_ok(),
            "{release}: after writing: {:?}",
            report.problems
        );
        assert_eq!(
            releases(&db),
            (named, Some(trunkdb::RELEASE.to_string())),
            "{release}: after writing, it's written by this build"
        );
    }
}
