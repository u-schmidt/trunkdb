//! Field names stored once (SPEC §170): the file's dictionary of names,
//! kept in the catalog, written with the batch that brings a name, and
//! read back at every open, by every read.

use trunkdb::query::Filter;
use trunkdb::{Database, Document, Map};

fn doc(json: &str) -> Document {
    json.parse().unwrap_or_else(|e| panic!("{json}: {e}"))
}

fn all(db: &Database, name: &str) -> Vec<String> {
    let mut found: Vec<String> = db
        .collection::<Document>(name)
        .find(Filter::new())
        .unwrap()
        .iter()
        .map(|d| {
            let Document::Object(map) = d else { panic!() };
            let mut map = map.clone();
            map.remove("_id");
            Document::Object(map).to_string()
        })
        .collect();
    found.sort();
    found
}

fn checked(db: &Database) {
    let report = db.check().unwrap();
    assert!(report.is_ok(), "{:?}", report.problems);
}

/// Names learned in a batch that fails are forgotten with it: the next
/// batch numbers its own names from where the dictionary stood, and a
/// reopen reads them all.
#[test]
fn names_survive_a_reopen_and_go_with_a_failed_batch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let db = Database::open(&path).unwrap();
    let things = db.collection::<Document>("things");
    things
        .insert(doc(r#"{"a": 1, "b": {"c": [{"d": "x"}]}}"#))
        .unwrap();

    let mut batch = db.batch();
    batch
        .insert(&things, doc(r#"{"lost": 1, "also lost": {"x": 2}}"#))
        .unwrap();
    batch.require(&things, Filter::new().eq("a", 99)).unwrap();
    assert!(batch.commit().is_err());

    things.insert(doc(r#"{"e": 2, "b": {"f": true}}"#)).unwrap();
    db.collection::<Document>("others")
        .insert(doc(r#"{"a": "shared with things", "g": null}"#))
        .unwrap();
    let before = (all(&db, "things"), all(&db, "others"));
    assert_eq!(before.0.len(), 2, "the failed batch's insert is gone");
    drop(things);
    drop(db);

    let db = Database::open(&path).unwrap();
    assert_eq!((all(&db, "things"), all(&db, "others")), before);
    assert_eq!(db.file_info().unwrap().format_version, 17);
    checked(&db);
}

/// Past the dictionary's limits, a key is stored as it is: a name too
/// long, and every name after the first 4,096.
#[test]
fn names_past_the_limits_are_stored_inline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let db = Database::open(&path).unwrap();
    let things = db.collection::<Document>("things");
    let mut wide = Map::new();
    for n in 0..5000 {
        wide.insert(format!("key{n}"), Document::Int(n));
    }
    wide.insert("long ".repeat(20), Document::from("a long name"));
    let wide = Document::Object(wide);
    things.insert(wide.clone()).unwrap();
    things
        .insert(doc(r#"{"after the limit": [1, 2, {"nested too": 3}]}"#))
        .unwrap();
    let before = all(&db, "things");
    drop(things);
    drop(db);

    let db = Database::open(&path).unwrap();
    assert_eq!(all(&db, "things"), before);
    let things = db.collection::<Document>("things");
    let found = things
        .find(
            Filter::new()
                .eq("key4999", 4999)
                .select(["key4999", "key0"]),
        )
        .unwrap();
    assert_eq!(found.len(), 1);
    checked(&db);
}

/// A snapshot reads its documents while later batches add names, and
/// typed reads, selections and sorts read the numbered keys.
#[test]
fn every_kind_of_read_reads_numbered_keys() {
    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Thing {
        name: String,
        size: i64,
        inner: Inner,
    }
    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Inner {
        deep: Vec<i64>,
    }

    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
    let things = db.collection::<Document>("things");
    for n in 0..50 {
        things
            .insert(doc(&format!(
                r#"{{"name": "t{n}", "size": {}, "inner": {{"deep": [{n}, -{n}]}}}}"#,
                n * 1000 - 20_000
            )))
            .unwrap();
    }
    things.ensure_index("size").unwrap();
    let snapshot = db.snapshot().unwrap();
    for n in 0..50 {
        things
            .insert(doc(&format!(r#"{{"new{n}": {n}, "name": "late"}}"#)))
            .unwrap();
    }
    let view = snapshot.collection::<Document>("things");
    assert_eq!(view.find(Filter::new()).unwrap().len(), 50);

    let typed = db.collection::<Thing>("things");
    let found = typed
        .find(Filter::new().gt("size", 20_000).sort_desc("size"))
        .unwrap();
    assert_eq!(
        found.first(),
        Some(&Thing {
            name: "t49".into(),
            size: 29_000,
            inner: Inner {
                deep: vec![49, -49]
            },
        })
    );
    let selected = things
        .find(Filter::new().eq("name", "t3").select(["inner.deep"]))
        .unwrap();
    let Document::Object(map) = &selected[0] else {
        panic!()
    };
    assert_eq!(map.get("inner").unwrap().to_string(), r#"{"deep":[3,-3]}"#);
    assert_eq!(things.count(Filter::new().eq("name", "late")).unwrap(), 50);
    checked(&db);
}

/// `compact` stores every document anew, in a dictionary of the names
/// still in use.
#[test]
fn compact_keeps_what_the_documents_say() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let db = Database::open(&path).unwrap();
    let things = db.collection::<Document>("things");
    let mut ids = Vec::new();
    for n in 0..300 {
        ids.push(
            things
                .insert(doc(&format!(r#"{{"gone{n}": 1, "kept": {n}}}"#)))
                .unwrap(),
        );
    }
    for id in &ids[..299] {
        things.delete(id).unwrap();
    }
    let before = all(&db, "things");
    db.compact().unwrap();
    assert_eq!(all(&db, "things"), before);
    checked(&db);
    drop(things);
    drop(db);
    let db = Database::open(&path).unwrap();
    assert_eq!(all(&db, "things"), before);
    checked(&db);
}
