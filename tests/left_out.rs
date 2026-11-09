//! An index that counts the documents it leaves out (SPEC §181): a walk
//! in its order scans for them only when there are some, and finds the
//! same documents either way.

use std::path::Path;

use trunkdb::query::{Filter, Update};
use trunkdb::{Collection, Database, Document};

fn open(dir: &Path) -> Database {
    Database::open(dir.join("left_out.trunkdb")).unwrap()
}

fn doc(json: &str) -> Document {
    json.parse().unwrap_or_else(|e| panic!("{json}: {e}"))
}

fn filter(text: &str) -> Filter {
    text.parse().unwrap_or_else(|e| panic!("{text}: {e}"))
}

/// What `text` read: index entries, documents, and whether it scanned
/// every document after the walk.
fn reads(docs: &Collection<Document>, text: &str) -> (u64, u64, bool) {
    let run = docs.explain_run(filter(text)).unwrap();
    let scanned = run.reads.iter().any(|read| read.index == "_id");
    (
        run.stats.keys_examined,
        run.stats.documents_examined,
        scanned,
    )
}

/// The names `text` finds, in its order.
fn names(docs: &Collection<Document>, text: &str) -> Vec<String> {
    (docs.find(filter(text)).unwrap().iter())
        .map(|doc| match doc.get("name") {
            Some(Document::String(name)) => name.clone(),
            other => panic!("name: {other:?}"),
        })
        .collect()
}

const NOT_SALAD: &str = r#"category != "Salad" order by category limit 50"#;

fn recipes(db: &Database) -> Collection<Document> {
    let recipes = db.collection::<Document>("recipes");
    for (name, category) in [
        ("Tomato soup", "Soup"),
        ("Greek salad", "Salad"),
        ("Lasagne", "Main"),
        ("Tiramisu", "Dessert"),
        ("Minestrone", "Soup"),
        ("Curry", "Main"),
    ] {
        let json = format!(r#"{{"name": "{name}", "category": "{category}"}}"#);
        recipes.insert(doc(&json)).unwrap();
    }
    recipes.ensure_index("category").unwrap();
    recipes
}

#[test]
fn a_page_not_filled_reads_no_document_twice() {
    let dir = tempfile::tempdir().unwrap();
    let recipes = recipes(&open(dir.path()));
    // The walk's 6 entries, the 5 documents not decided out by theirs.
    assert_eq!(reads(&recipes, NOT_SALAD), (6, 5, false));
    assert_eq!(
        names(&recipes, NOT_SALAD),
        ["Tiramisu", "Lasagne", "Curry", "Tomato soup", "Minestrone"]
    );
}

#[test]
fn each_value_nothing_orders_brings_the_scan_back_until_it_goes() {
    let dir = tempfile::tempdir().unwrap();
    let recipes = recipes(&open(dir.path()));
    for value in [r#"["Soup"]"#, r#"{"a": 1}"#, r#"{"$float": "NaN"}"#] {
        let json = format!(r#"{{"name": "Odd", "category": {value}}}"#);
        let odd = recipes.insert(doc(&json)).unwrap();
        // After every ordered one, found only by the scan.
        assert_eq!(reads(&recipes, NOT_SALAD), (13, 12, true), "{value}");
        assert_eq!(names(&recipes, NOT_SALAD).last().unwrap(), "Odd");
        recipes.delete(&odd).unwrap();
        assert_eq!(reads(&recipes, NOT_SALAD), (6, 5, false), "{value}");
    }
}

#[test]
fn an_update_moves_a_document_in_and_out_of_the_count() {
    let dir = tempfile::tempdir().unwrap();
    let recipes = recipes(&open(dir.path()));
    let curry = filter(r#"name = "Curry""#);
    let set = |value: &str| {
        let update = Update::new().set("category", doc(value));
        assert_eq!(recipes.update_fields(curry.clone(), &update).unwrap(), 1);
    };
    set(r#"["Main", "Spicy"]"#);
    assert!(reads(&recipes, NOT_SALAD).2);
    // From one value nothing orders to another: still counted once.
    set(r#"{"main": true}"#);
    assert!(reads(&recipes, NOT_SALAD).2);
    set(r#""Main""#);
    assert!(!reads(&recipes, NOT_SALAD).2);
    // A missing field sorts as null, which the index holds.
    let unset = Update::new().unset("category");
    recipes.update_fields(curry.clone(), &unset).unwrap();
    assert!(!reads(&recipes, NOT_SALAD).2);
    assert_eq!(names(&recipes, NOT_SALAD)[0], "Curry");
}

#[test]
fn the_count_is_kept_across_a_reopen_and_a_failed_batch() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(dir.path());
        let recipes = recipes(&db);
        recipes
            .insert(doc(r#"{"name": "Odd", "category": [1]}"#))
            .unwrap();
    }
    let db = open(dir.path());
    let recipes = db.collection::<Document>("recipes");
    assert!(reads(&recipes, NOT_SALAD).2, "1 left out, read back");
    recipes.delete_many(filter(r#"name = "Odd""#)).unwrap();

    // A batch that would leave one out, and fails: the count stays 0.
    let mut batch = db.batch();
    batch
        .insert(&recipes, doc(r#"{"name": "Odd", "category": [1]}"#))
        .unwrap();
    batch
        .require_none(&recipes, filter(r#"name = "Curry""#))
        .unwrap();
    assert!(batch.commit().is_err());
    assert!(!reads(&recipes, NOT_SALAD).2);
    drop(recipes);
    drop(db);
    let db = open(dir.path());
    let recipes = db.collection::<Document>("recipes");
    assert!(!reads(&recipes, NOT_SALAD).2);
    assert!(db.check().unwrap().is_ok());
}

#[test]
fn an_index_built_on_documents_left_out_counts_them() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let docs = db.collection::<Document>("docs");
    for json in [r#"{"name": "a", "v": 1}"#, r#"{"name": "b", "v": [1]}"#] {
        docs.insert(doc(json)).unwrap();
    }
    docs.ensure_index("v").unwrap();
    let all = "v != 5 order by v limit 10";
    assert_eq!(names(&docs, all), ["a", "b"]);
    docs.delete_many(filter(r#"name = "b""#)).unwrap();
    assert!(!reads(&docs, all).2);
    assert!(db.check().unwrap().is_ok());
}

/// A compaction that rewrites the file counts each index as it builds
/// it anew: many deleted first, so that it gains pages.
#[test]
fn compaction_counts_an_index_anew() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let recipes = recipes(&db);
    let filler = db.collection::<Document>("filler");
    for odd in [true, false] {
        if odd {
            recipes
                .insert(doc(r#"{"name": "Odd", "category": [1]}"#))
                .unwrap();
        } else {
            recipes.delete_many(filter(r#"name = "Odd""#)).unwrap();
        }
        for n in 0..300 {
            filler
                .insert(doc(&format!(
                    r#"{{"n": {n}, "pad": "{}"}}"#,
                    "x".repeat(200)
                )))
                .unwrap();
        }
        filler.delete_many(Filter::new()).unwrap();
        let compacted = db.compact().unwrap();
        assert!(
            compacted.pages_after < compacted.pages_before,
            "{compacted:?}"
        );
        assert_eq!(reads(&recipes, NOT_SALAD).2, odd, "odd: {odd}");
    }
    assert_eq!(reads(&recipes, NOT_SALAD), (6, 5, false));
    assert!(db.check().unwrap().is_ok());
}

/// An index from before format 16 (SPEC §181.4): it doesn't know its
/// count, so the walk scans as it did; compaction counts it.
#[test]
fn an_older_files_index_scans_until_it_is_compacted() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/0.35.0/trunkdb.db");
    let path = dir.path().join("old.trunkdb");
    std::fs::copy(fixture, &path).unwrap();
    let db = Database::open(&path).unwrap();
    let tasks = db.collection::<Document>("tasks");
    let open = r#"status != "open" order by status limit 100000"#;
    let found = tasks.find(filter(open)).unwrap();
    assert!(reads(&tasks, open).2, "not counted: scans");
    db.compact().unwrap();
    assert!(!reads(&tasks, open).2, "counted, none left out");
    assert_eq!(tasks.find(filter(open)).unwrap(), found);
    assert!(db.check().unwrap().is_ok());
}

struct Random(u64);

impl Random {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Random writes of values of every kind, some that nothing orders:
/// each sorted page found through the index is the one a scan finds,
/// and `check` agrees with the count throughout.
#[test]
fn random_writes_keep_the_count_and_the_pages_right() {
    const VALUES: [&str; 9] = [
        "1",
        "2.5",
        r#""a""#,
        "null",
        "true",
        "[1, 2]",
        "[]",
        r#"{"x": 1}"#,
        r#"{"$float": "NaN"}"#,
    ];
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let docs = db.collection::<Document>("docs");
    docs.ensure_index("v").unwrap();
    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    let mut ids = Vec::new();
    for step in 0..400 {
        let value = VALUES[random.below(VALUES.len() as u64) as usize];
        match random.below(4) {
            0 if !ids.is_empty() => {
                let id = ids.swap_remove(random.below(ids.len() as u64) as usize);
                docs.delete(&id).unwrap();
            }
            1 if !ids.is_empty() => {
                let id = ids[random.below(ids.len() as u64) as usize];
                let update = Update::new().set("v", doc(value));
                docs.update_fields(Filter::new().id(id), &update).unwrap();
            }
            _ => {
                let json = format!(r#"{{"name": "{step}", "v": {value}}}"#);
                ids.push(docs.insert(doc(&json)).unwrap());
            }
        }
        if step % 20 == 0 {
            for text in [
                "v != 2.5 order by v limit 1000",
                "order by v desc limit 1000",
            ] {
                // As text: NaN is never equal to itself.
                let walked = format!("{:?}", docs.find(filter(text)).unwrap());
                let scanned = format!("{:?}", docs.find(filter(text).no_index()).unwrap());
                assert_eq!(walked, scanned, "{text} at step {step}");
            }
            assert!(db.check().unwrap().is_ok(), "step {step}");
        }
    }
}
