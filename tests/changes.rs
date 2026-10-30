//! The hook after each commit (SPEC §135): `Database::subscribe`, one
//! report per committed batch, each document once with its net effect.

use std::time::Duration;
use trunkdb::query::{Filter, Update};
use trunkdb::{Change, Changes, Collection, Commit, Database, DocId, Document, OpenOptions};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("changes.trunkdb")).unwrap();
    (dir, db)
}

fn doc(text: &str) -> Document {
    text.parse().unwrap()
}

/// A change as `(kind, collection, id)`, for whole comparisons.
fn short(change: &Change) -> (&'static str, String, Option<DocId>) {
    let kind = match change {
        Change::Inserted { .. } => "inserted",
        Change::Updated { .. } => "updated",
        Change::Deleted { .. } => "deleted",
        Change::Dropped { .. } => "dropped",
        _ => "other",
    };
    (kind, change.collection().to_string(), change.id())
}

/// The next report, which must have come by now.
fn next(changes: &Changes) -> Vec<(&'static str, String, Option<DocId>)> {
    let commit = changes.try_recv().expect("a report");
    assert!(!commit.changes.is_empty());
    commit.changes.iter().map(short).collect()
}

fn none_left(changes: &Changes) {
    assert_eq!(changes.try_recv(), None);
}

fn change(
    kind: &'static str,
    collection: &str,
    id: DocId,
) -> (&'static str, String, Option<DocId>) {
    (kind, collection.to_string(), Some(id))
}

fn tasks(db: &Database) -> Collection<Document> {
    db.collection::<Document>("tasks")
}

/// Each way of writing a document is reported, once per call, as the
/// batch it is.
#[test]
fn every_write_is_reported_as_its_batch() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let notes = db.collection::<Document>("notes");
    let changes = db.subscribe().unwrap();

    let a = tasks.insert(doc(r#"{"n": 1, "status": "open"}"#)).unwrap();
    assert_eq!(next(&changes), [change("inserted", "tasks", a)]);
    tasks
        .update(&a, doc(r#"{"n": 1, "status": "done"}"#))
        .unwrap();
    assert_eq!(next(&changes), [change("updated", "tasks", a)]);

    // A batch across collections: one report, in the order written.
    let mut batch = db.batch();
    let b = batch
        .insert(&tasks, doc(r#"{"n": 2, "status": "open"}"#))
        .unwrap();
    let c = batch.insert(&notes, doc(r#"{"on": 2}"#)).unwrap();
    batch
        .update(&tasks, &a, doc(r#"{"n": 1, "status": "open"}"#))
        .unwrap();
    batch.commit().unwrap();
    assert_eq!(
        next(&changes),
        [
            change("inserted", "tasks", b),
            change("inserted", "notes", c),
            change("updated", "tasks", a)
        ]
    );

    // The writes that find what they change.
    assert_eq!(
        tasks
            .update_many(Filter::new().eq("status", "open"), |d| {
                if let Document::Object(fields) = d {
                    fields.insert("seen".into(), Document::Bool(true));
                }
            })
            .unwrap(),
        2
    );
    assert_eq!(
        next(&changes),
        [change("updated", "tasks", a), change("updated", "tasks", b)]
    );
    tasks
        .update_fields(Filter::new().eq("n", 2), &Update::new().inc("n", 10))
        .unwrap();
    assert_eq!(next(&changes), [change("updated", "tasks", b)]);
    let upserted = tasks
        .upsert(Filter::new().eq("n", 3), doc(r#"{"n": 3}"#))
        .unwrap();
    assert_eq!(next(&changes), [change("inserted", "tasks", upserted.id())]);
    tasks
        .upsert(Filter::new().eq("n", 3), doc(r#"{"n": 3, "again": true}"#))
        .unwrap();
    assert_eq!(next(&changes), [change("updated", "tasks", upserted.id())]);
    assert_eq!(tasks.delete_many(Filter::new().gte("n", 3)).unwrap(), 2);
    assert_eq!(
        next(&changes),
        [
            change("deleted", "tasks", b),
            change("deleted", "tasks", upserted.id())
        ]
    );
    tasks.delete(&a).unwrap();
    assert_eq!(next(&changes), [change("deleted", "tasks", a)]);

    // A collection dropped: one change, not one per document.
    assert!(db.drop_collection("notes").unwrap());
    assert_eq!(next(&changes), [("dropped", "notes".to_string(), None)]);
    none_left(&changes);
}

/// A batch reports what it left each document as, compared with before.
#[test]
fn a_batch_reports_each_documents_net_effect() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let kept = tasks.insert(doc(r#"{"n": 1}"#)).unwrap();
    let changes = db.subscribe().unwrap();

    let mut batch = db.batch();
    let new = batch.insert(&tasks, doc(r#"{"n": 2}"#)).unwrap();
    batch.update(&tasks, &new, doc(r#"{"n": 3}"#)).unwrap();
    let passing = batch.insert(&tasks, doc(r#"{"n": 4}"#)).unwrap();
    batch.delete(&tasks, &passing);
    batch.update(&tasks, &kept, doc(r#"{"n": 5}"#)).unwrap();
    batch.delete(&tasks, &kept);
    batch.commit().unwrap();
    assert_eq!(
        next(&changes),
        [
            change("inserted", "tasks", new),
            change("deleted", "tasks", kept)
        ]
    );

    // A batch that leaves every document as it was isn't reported.
    let mut batch = db.batch();
    let gone = batch.insert(&tasks, doc(r#"{"n": 6}"#)).unwrap();
    batch.delete(&tasks, &gone);
    batch.commit().unwrap();
    none_left(&changes);
}

/// What changes no document, or fails, isn't reported.
#[test]
fn what_changes_no_document_is_not_reported() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let a = tasks.insert(doc(r#"{"n": 1}"#)).unwrap();
    let changes = db.subscribe().unwrap();

    tasks.ensure_index("n").unwrap();
    tasks.drop_index("n").unwrap();
    db.compact().unwrap();
    db.checkpoint().unwrap();
    db.check().unwrap();
    // Found nothing, or nothing to change.
    assert_eq!(tasks.delete_many(Filter::new().eq("n", 9)).unwrap(), 0);
    assert_eq!(
        tasks
            .update_fields(Filter::new(), &Update::new().set("n", 1))
            .unwrap(),
        0
    );
    assert!(!tasks.update(&DocId::NIL, doc(r#"{"n": 2}"#)).unwrap());
    assert!(!db.drop_collection("missing").unwrap());
    // Failed: a duplicate id rolls the whole batch back.
    let mut batch = db.batch();
    batch.insert(&tasks, doc(r#"{"n": 3}"#)).unwrap();
    // The stored document again, its `_id` with it.
    let stored = tasks.get(&a).unwrap().unwrap();
    batch.insert(&tasks, stored).unwrap();
    assert!(batch.commit().is_err());
    let mut batch = db.batch();
    batch
        .update(&tasks, &DocId::NIL, doc(r#"{"n": 4}"#))
        .unwrap();
    assert!(batch.commit().is_err());
    none_left(&changes);
}

/// Reports start with the first commit after `subscribe`; every
/// subscriber gets each one; one dropped doesn't stop the others.
#[test]
fn each_subscriber_gets_every_commit_after_it() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    tasks.insert(doc(r#"{"n": 0}"#)).unwrap();
    let first = db.subscribe().unwrap();
    let a = tasks.insert(doc(r#"{"n": 1}"#)).unwrap();
    let second = db.clone().subscribe().unwrap();
    let b = tasks.insert(doc(r#"{"n": 2}"#)).unwrap();
    let dropped = db.subscribe().unwrap();
    drop(dropped);
    let c = tasks.insert(doc(r#"{"n": 3}"#)).unwrap();

    for (id, report) in [
        (a, &first),
        (b, &first),
        (c, &first),
        (b, &second),
        (c, &second),
    ] {
        assert_eq!(next(report), [change("inserted", "tasks", id)]);
    }
    none_left(&first);
    none_left(&second);
}

/// A report is sent once the batch is durable and new reads see it: a
/// read on the subscriber's thread finds what it names. The reports come
/// in the order the batches committed, from writers on several threads.
#[test]
fn a_report_comes_after_its_commit_in_commit_order() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let changes = db.subscribe().unwrap();
    let reader = {
        let tasks = tasks.clone();
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            for commit in changes {
                for change in commit.changes {
                    if let Change::Inserted { id, .. } = change {
                        // Committed before the report: it's there.
                        let found = tasks.get(&id).unwrap().expect("committed");
                        seen.push(found);
                    }
                }
                if seen.len() == 400 {
                    break;
                }
            }
            seen
        })
    };
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let tasks = tasks.clone();
            std::thread::spawn(move || {
                for n in 0..100 {
                    tasks
                        .insert(doc(&format!(r#"{{"writer": {w}, "n": {n}}}"#)))
                        .unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let seen = reader.join().unwrap();
    // In commit order, each writer's in its own order.
    for w in 0..4 {
        let ns: Vec<i64> = seen
            .iter()
            .filter_map(|d| match d {
                Document::Object(f) if f.get("writer") == Some(&Document::Int(w)) => {
                    match f.get("n") {
                        Some(Document::Int(n)) => Some(*n),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect();
        assert_eq!(ns, (0..100).collect::<Vec<_>>());
    }
}

/// A subscriber may write from its own thread, here a log entry for each
/// task: a writer never waits for a subscriber, so this can't deadlock,
/// and its own writes are reported too.
#[test]
fn a_subscriber_that_writes_does_not_hold_up_the_writer() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let changes = db.subscribe().unwrap();
    let log = db.collection::<Document>("log");
    let follower = std::thread::spawn(move || {
        let mut logged = 0;
        while logged < 50 {
            let commit = changes
                .recv_timeout(Duration::from_secs(10))
                .expect("a report");
            for change in commit.changes {
                if change.collection() == "tasks" {
                    log.insert(doc(r#"{"saw": "a task"}"#)).unwrap();
                    logged += 1;
                }
            }
        }
        logged
    });
    for n in 0..50 {
        tasks.insert(doc(&format!(r#"{{"n": {n}}}"#))).unwrap();
    }
    assert_eq!(follower.join().unwrap(), 50);
    assert_eq!(
        db.collection::<Document>("log")
            .count(Filter::new())
            .unwrap(),
        50
    );
}

/// Iterating ends when the database is dropped, every handle on it, once
/// the reports waiting are read.
#[test]
fn iterating_ends_when_the_database_is_dropped() {
    let (_dir, db) = open();
    let changes = db.subscribe().unwrap();
    let tasks = tasks(&db);
    tasks.insert(doc(r#"{"n": 1}"#)).unwrap();
    tasks.insert(doc(r#"{"n": 2}"#)).unwrap();
    let counter = std::thread::spawn(move || changes.count());
    drop(tasks);
    drop(db);
    assert_eq!(counter.join().unwrap(), 2);
}

/// Typed collections, `import`, and a database opened read-only, which
/// commits nothing.
#[test]
fn imports_typed_writes_and_read_only_opens() {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Task {
        #[serde(rename = "_id")]
        id: Option<DocId>,
        title: String,
    }
    let (dir, db) = open();
    let typed = db.collection::<Task>("typed");
    let changes = db.subscribe().unwrap();
    let id = typed
        .insert(Task {
            id: None,
            title: "a".into(),
        })
        .unwrap();
    assert_eq!(next(&changes), [change("inserted", "typed", id)]);

    let mut exported = Vec::new();
    db.export(&mut exported).unwrap();
    let other = Database::open(dir.path().join("other.trunkdb")).unwrap();
    let imported = other.subscribe().unwrap();
    other.import(&exported[..]).unwrap();
    assert_eq!(next(&imported), [change("inserted", "typed", id)]);
    none_left(&imported);
    drop(other);

    drop(changes);
    drop(typed);
    drop(db);
    let read_only = Database::open_with(
        dir.path().join("changes.trunkdb"),
        OpenOptions::default().read_only(true),
    )
    .unwrap();
    let changes = read_only.subscribe().unwrap();
    assert!(
        read_only
            .collection::<Document>("typed")
            .insert(doc("{}"))
            .is_err()
    );
    none_left(&changes);
}

/// `Commit` and `Change` are plain values: compared, cloned, printed.
#[test]
fn a_commit_is_a_plain_value() {
    let (_dir, db) = open();
    let changes = db.subscribe().unwrap();
    tasks(&db).insert(doc("{}")).unwrap();
    let commit: Commit = changes.recv().unwrap();
    assert_eq!(commit.clone(), commit);
    assert!(format!("{commit:?}").contains("Inserted"));
}
