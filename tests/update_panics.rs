//! A panic in `update_many`'s change, or in a `T`'s conversions, while
//! the writer's lock is held (SPEC §137): the call fails with
//! `Error::ClosurePanicked`, nothing is written, and the database goes on,
//! its lock not poisoned.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use trunkdb::query::{Filter, Update};
use trunkdb::{Database, Document, Error};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("update_panics.trunkdb")).unwrap();
    (dir, db)
}

fn panicked<T: std::fmt::Debug>(result: trunkdb::Result<T>) -> String {
    match result {
        Err(Error::ClosurePanicked { message, .. }) => message,
        other => panic!("not ClosurePanicked: {other:?}"),
    }
}

/// `n` is written as it is, except 13, which panics: a `Serialize` of
/// the application's that has a bug.
fn unlucky_out<S: Serializer>(n: &i64, out: S) -> Result<S::Ok, S::Error> {
    if *n == 13 {
        panic!("13 won't be written");
    }
    out.serialize_i64(*n)
}

/// `tag` is read as it is, except "bad", which panics: a `Deserialize`
/// of the application's that has a bug.
fn unlucky_in<'de, D: Deserializer<'de>>(from: D) -> Result<String, D::Error> {
    let tag = String::deserialize(from)?;
    if tag == "bad" {
        panic!("can't read a bad tag");
    }
    Ok(tag)
}

#[derive(Debug, Serialize, Deserialize)]
struct Task {
    #[serde(serialize_with = "unlucky_out")]
    n: i64,
    #[serde(deserialize_with = "unlucky_in")]
    tag: String,
}

/// Each way of writing a document after the panic, and a `check`.
fn goes_on(db: &Database) {
    let docs = db.collection::<Document>("tasks");
    docs.insert(r#"{"n": 100, "tag": "new"}"#.parse().unwrap())
        .unwrap();
    let changed = docs
        .update_many(Filter::new().eq("n", 100), |doc| {
            if let Document::Object(map) = doc {
                map.insert("seen".into(), Document::Bool(true));
            }
        })
        .unwrap();
    assert_eq!(changed, 1);
    docs.delete_many(Filter::new().eq("n", 100)).unwrap();
    db.check().unwrap();
}

fn stored(db: &Database) -> Vec<Document> {
    db.collection::<Document>("tasks")
        .find(Filter::new())
        .unwrap()
}

fn tasks(db: &Database) {
    let docs = db.collection::<Document>("tasks");
    for n in 0..5 {
        docs.insert(format!(r#"{{"n": {n}, "tag": "ok"}}"#).parse().unwrap())
            .unwrap();
    }
}

#[test]
fn a_change_that_panics_writes_nothing() {
    let (_dir, db) = open();
    tasks(&db);
    let before = stored(&db);
    let docs = db.collection::<Document>("tasks");
    // The first two changed, then the third panics: none of them land.
    let mut seen = 0;
    let result = docs.update_many(Filter::new().sort_asc("n"), |doc| {
        seen += 1;
        if seen == 3 {
            panic!("no third change");
        }
        if let Document::Object(map) = doc {
            map.insert("changed".into(), Document::Bool(true));
        }
    });
    assert_eq!(panicked(result), "no third change");
    assert_eq!(stored(&db), before, "nothing written");
    goes_on(&db);
}

#[test]
fn a_typed_change_or_conversion_that_panics_writes_nothing() {
    let (_dir, db) = open();
    tasks(&db);
    let before = stored(&db);
    let typed = db.collection::<Task>("tasks");

    // The change.
    let result = typed.update_many(Filter::new(), |task| {
        if task.n == 4 {
            panic!("not task 4");
        }
        task.tag = "done".into();
    });
    assert_eq!(panicked(result), "not task 4");
    // `T`'s `Serialize`, after the change.
    let result = typed.update_many(Filter::new(), |task| task.n += 10);
    assert_eq!(panicked(result), "13 won't be written");
    assert_eq!(stored(&db), before, "nothing written");
    goes_on(&db);

    // `T`'s `Deserialize`: of a match in `update_many`, and of what
    // `update_fields` made, checked to convert.
    let docs = db.collection::<Document>("tasks");
    docs.insert(r#"{"n": 50, "tag": "bad"}"#.parse().unwrap())
        .unwrap();
    let before = stored(&db);
    let result = typed.update_many(Filter::new().eq("n", 50), |task| task.n = 51);
    assert_eq!(panicked(result), "can't read a bad tag");
    let result = typed.update_fields(Filter::new().eq("n", 0), &Update::new().set("tag", "bad"));
    assert_eq!(panicked(result), "can't read a bad tag");
    assert_eq!(stored(&db), before, "nothing written");
    goes_on(&db);
}

#[test]
fn reads_of_a_t_panic_on_as_before() {
    // A read holds no writer's lock: `T`'s panic is the program's, as it
    // was. The database goes on all the same.
    let (_dir, db) = open();
    let docs = db.collection::<Document>("tasks");
    docs.insert(r#"{"n": 1, "tag": "bad"}"#.parse().unwrap())
        .unwrap();
    let typed = db.collection::<Task>("tasks");
    let read = std::panic::catch_unwind(|| typed.find(Filter::new()));
    assert!(read.is_err());
    goes_on(&db);
}
