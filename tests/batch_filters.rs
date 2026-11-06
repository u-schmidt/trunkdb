//! Writes by a filter in a batch (SPEC §168): `update_many` and
//! `delete_many`, run at `commit` at their place, under the writer's
//! lock; what each op wrote, and a dry run that writes nothing.

use std::collections::BTreeMap;
use trunkdb::query::{Filter, Update};
use trunkdb::{Collection, Database, DocId, Document, Error};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("batch.trunkdb")).unwrap();
    (dir, db)
}

fn doc(text: &str) -> Document {
    text.parse().unwrap()
}

fn filter(text: &str) -> Filter {
    text.parse().unwrap()
}

fn int(doc: &Document, name: &str) -> i64 {
    match doc.get(name) {
        Some(Document::Int(n)) => *n,
        other => panic!("{name}: {other:?}"),
    }
}

/// Each document's `(k, v)` by id.
fn stored(items: &Collection<Document>) -> BTreeMap<DocId, (i64, i64)> {
    (items.find(Filter::new()).unwrap().iter())
        .map(|d| {
            let Some(Document::Id(id)) = d.get("_id") else {
                panic!("no _id: {d:?}");
            };
            (*id, (int(d, "k"), int(d, "v")))
        })
        .collect()
}

/// The roadmap's case: every overdue task marked, and a log entry of how
/// many, in one commit; ops after see what the filter's op wrote, ops
/// before it don't stop it from seeing what they wrote.
#[test]
fn update_many_and_delete_many_run_at_their_place() {
    let (_dir, db) = open();
    let tasks = db.collection::<Document>("tasks");
    let log = db.collection::<Document>("log");
    for (due, done) in [(1, false), (2, true), (3, false), (9, false)] {
        tasks
            .insert(doc(&format!(r#"{{"due": {due}, "done": {done}}}"#)))
            .unwrap();
    }

    let mut batch = db.batch();
    batch
        .insert(&tasks, doc(r#"{"due": 4, "done": false}"#))
        .unwrap();
    batch
        .update_many(
            &tasks,
            filter("due < 5 and done = false"),
            &Update::new().set("overdue", true),
        )
        .unwrap();
    batch
        .insert(&tasks, doc(r#"{"due": 0, "done": false}"#))
        .unwrap();
    batch.insert(&log, doc(r#"{"what": "marked"}"#)).unwrap();
    batch.delete_many(&tasks, filter("done = true"));
    batch.require(&tasks, filter("overdue = true")).unwrap();
    assert_eq!(batch.len(), 6);
    let written = batch.commit().unwrap();
    assert_eq!(written.counts(), [1, 3, 1, 1, 1, 0]);
    assert_eq!(written.total(), 7);

    // The insert before the update was marked; the one after wasn't.
    let marked: Vec<i64> = (tasks.find(filter("overdue = true order by due")).unwrap())
        .iter()
        .map(|d| int(d, "due"))
        .collect();
    assert_eq!(marked, [1, 3, 4]);
    assert_eq!(tasks.count(filter("done = true")).unwrap(), 0);
    assert_eq!(tasks.count(Filter::new()).unwrap(), 5);
    assert_eq!(log.count(Filter::new()).unwrap(), 1);
}

/// As for `Collection::update_fields` and `delete_many`: a sort, a skip
/// and a limit choose which documents, "the oldest two"; a document the
/// operators leave as it was isn't written or counted.
#[test]
fn a_sort_and_a_limit_choose_and_unchanged_documents_dont_count() {
    let (_dir, db) = open();
    let jobs = db.collection::<Document>("jobs");
    jobs.ensure_index("at").unwrap();
    for at in [5, 1, 4, 2, 3] {
        jobs.insert(doc(&format!(r#"{{"at": {at}, "state": "queued"}}"#)))
            .unwrap();
    }
    let mut batch = db.batch();
    batch
        .update_many(
            &jobs,
            filter(r#"state = "queued" order by at limit 2"#),
            &Update::new().set("state", "running"),
        )
        .unwrap();
    // Already running: changed by nothing, so not written.
    batch
        .update_many(
            &jobs,
            filter("at <= 2"),
            &Update::new().set("state", "running"),
        )
        .unwrap();
    batch.delete_many(&jobs, filter("order by at desc limit 1 offset 1"));
    let written = batch.commit().unwrap();
    assert_eq!(written.counts(), [2, 0, 1]);
    let left: Vec<(i64, String)> = (jobs.find(filter("order by at")).unwrap())
        .iter()
        .map(|d| match d.get("state") {
            Some(Document::String(s)) => (int(d, "at"), s.clone()),
            other => panic!("{other:?}"),
        })
        .collect();
    let left: Vec<(i64, &str)> = left.iter().map(|(at, s)| (*at, s.as_str())).collect();
    assert_eq!(
        left,
        [(1, "running"), (2, "running"), (3, "queued"), (5, "queued")]
    );
}

/// What each kind of op says it wrote.
#[test]
fn every_op_says_what_it_wrote() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    let a = items.insert(doc(r#"{"k": 1, "v": 1}"#)).unwrap();
    let mut batch = db.batch();
    let b = batch.insert(&items, doc(r#"{"k": 2, "v": 2}"#)).unwrap();
    batch
        .update(&items, &b, doc(r#"{"k": 2, "v": 3}"#))
        .unwrap();
    batch
        .update_fields(&items, &a, &Update::new().set("v", 1))
        .unwrap();
    batch
        .update_fields(&items, &a, &Update::new().set("v", 5))
        .unwrap();
    batch.require(&items, filter("v = 5")).unwrap();
    batch.require_none(&items, filter("v = 1")).unwrap();
    batch.delete(&items, &b);
    batch.delete_many(&items, filter("k = 7"));
    let written = batch.commit().unwrap();
    assert_eq!(written.counts(), [1, 1, 0, 1, 0, 0, 1, 0]);
    assert_eq!(db.batch().commit().unwrap().counts(), [] as [usize; 0]);
}

/// One op failing rolls back everything, the filters' writes too: an
/// operator that can't be made, a precondition after them, a unique
/// index refusing what an `update_many` makes.
#[test]
fn a_failing_op_rolls_back_the_filters_writes() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    items
        .ensure_index_with("code", trunkdb::IndexOptions::new().unique())
        .unwrap();
    for (k, code) in [(1, "a"), (1, "b"), (2, "c")] {
        items
            .insert(doc(&format!(r#"{{"k": {k}, "code": "{code}", "n": 1}}"#)))
            .unwrap();
    }
    let before = items.find(filter("order by code")).unwrap();

    let mut cant = db.batch();
    cant.delete_many(&items, filter("k = 2"));
    cant.update_many(&items, filter("k = 1"), &Update::new().inc("code", 1))
        .unwrap();
    assert!(matches!(cant.commit(), Err(Error::Update { .. })));

    let mut refused = db.batch();
    refused
        .update_many(&items, Filter::new(), &Update::new().inc("n", 1))
        .unwrap();
    refused.delete_many(&items, filter("k = 1"));
    refused.require(&items, filter("k = 1")).unwrap();
    let Err(Error::PreconditionFailed { op, .. }) = refused.commit() else {
        panic!("expected a failed precondition");
    };
    assert_eq!(op, 2);

    let mut unique = db.batch();
    unique.delete_many(&items, filter("k = 2"));
    unique
        .update_many(&items, filter("k = 1"), &Update::new().set("code", "z"))
        .unwrap();
    assert!(unique.commit().is_err(), "two documents with code z");

    assert_eq!(items.find(filter("order by code")).unwrap(), before);
}

/// An update refused as written is refused when added; on a typed
/// collection, a changed document that no longer converts fails the
/// batch.
#[test]
fn refused_when_added_or_when_it_no_longer_converts() {
    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Item {
        k: i64,
        v: i64,
    }
    let (_dir, db) = open();
    let items = db.collection::<Item>("items");
    items.insert(Item { k: 1, v: 1 }).unwrap();

    let mut batch = db.batch();
    let Err(Error::Io(e)) = batch.update_many(&items, Filter::new(), &Update::new().unset("_id"))
    else {
        panic!("expected InvalidInput");
    };
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
    assert!(batch.is_empty());

    batch
        .update_many(&items, filter("k = 1"), &Update::new().inc("v", 1))
        .unwrap();
    batch
        .update_many(&items, filter("k = 1"), &Update::new().set("v", "high"))
        .unwrap();
    assert!(matches!(batch.commit(), Err(Error::Document(_))));
    assert_eq!(items.find(Filter::new()).unwrap(), [Item { k: 1, v: 1 }]);

    let mut fine = db.batch();
    fine.update_many(&items, filter("k = 1"), &Update::new().inc("v", 1))
        .unwrap();
    fine.commit().unwrap();
    assert_eq!(items.find(Filter::new()).unwrap(), [Item { k: 1, v: 2 }]);
}

/// A collection that doesn't exist has nothing to change or delete, and
/// isn't made by trying.
#[test]
fn a_missing_collection_has_nothing_to_write() {
    let (_dir, db) = open();
    let none = db.collection::<Document>("none");
    let mut batch = db.batch();
    batch
        .update_many(&none, Filter::new(), &Update::new().set("a", 1))
        .unwrap();
    batch.delete_many(&none, Filter::new());
    assert_eq!(batch.commit().unwrap().counts(), [0, 0]);
    assert!(db.collections().unwrap().is_empty());
}

/// SPEC §168.4: a dry run says what `commit` would, and fails as it
/// would, writing nothing and telling no subscriber; the batch commits
/// after it as before.
#[test]
fn a_dry_run_says_what_commit_would_and_writes_nothing() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    items.ensure_index("k").unwrap();
    for k in [1, 1, 2, 3] {
        items
            .insert(doc(&format!(r#"{{"k": {k}, "v": 0}}"#)))
            .unwrap();
    }
    let before = stored(&items);
    let changes = db.subscribe().unwrap();

    let mut batch = db.batch();
    batch.insert(&items, doc(r#"{"k": 1, "v": 0}"#)).unwrap();
    batch
        .update_many(&items, filter("k = 1"), &Update::new().inc("v", 1))
        .unwrap();
    batch.delete_many(&items, filter("k >= 3"));
    let rehearsed = batch.dry_run().unwrap();
    assert_eq!(rehearsed.counts(), [1, 3, 1]);
    assert_eq!(stored(&items), before, "nothing written");
    assert_eq!(items.count(filter("k = 1")).unwrap(), 2, "nor in the index");
    assert!(changes.try_recv().is_none(), "no one hears of it");
    assert_eq!(batch.dry_run().unwrap(), rehearsed, "again, the same");

    let mut failing = db.batch();
    failing.delete_many(&items, filter("k = 2"));
    failing.require(&items, filter("k = 2")).unwrap();
    assert!(matches!(
        failing.dry_run(),
        Err(Error::PreconditionFailed { op: 1, .. })
    ));
    assert_eq!(stored(&items), before);

    assert_eq!(batch.commit().unwrap(), rehearsed);
    assert_eq!(items.count(filter("k = 1")).unwrap(), 3);
    assert!(changes.try_recv().is_some());
    drop(changes);

    // Closed and opened again: nothing of the dry runs is in the file.
    let path = _dir.path().join("batch.trunkdb");
    drop((items, db, failing));
    let db = Database::open(&path).unwrap();
    assert!(db.check().unwrap().is_ok());
    assert_eq!(
        db.collection::<Document>("items")
            .count(Filter::new())
            .unwrap(),
        4
    );
}

/// A read-only database refuses a dry run as it refuses a commit.
#[test]
fn a_read_only_database_refuses_a_dry_run() {
    let (dir, db) = open();
    let items = db.collection::<Document>("items");
    items.insert(doc(r#"{"k": 1}"#)).unwrap();
    drop((items, db));
    let path = dir.path().join("batch.trunkdb");
    let db = Database::open_with(&path, trunkdb::OpenOptions::default().read_only(true)).unwrap();
    let items = db.collection::<Document>("items");
    let mut batch = db.batch();
    batch.delete_many(&items, Filter::new());
    assert!(matches!(batch.dry_run(), Err(Error::ReadOnly)));
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

/// Random batches of inserts, `update_many`s, `delete_many`s and
/// `require`s, with and without an index on `k`, against a model that
/// applies each op to a map in order: the counts, the dry run's counts,
/// the failed op and the state after each batch agree.
#[test]
fn random_batches_match_a_model() {
    for setup in 0..2 {
        let (_dir, db) = open();
        let items = db.collection::<Document>("items");
        if setup == 1 {
            items.ensure_index("k").unwrap();
        }
        let mut random = Random(0x9e37_79b9_7f4a_7c15 + setup);
        let mut model: BTreeMap<DocId, (i64, i64)> = BTreeMap::new();
        let (mut held, mut refused) = (0, 0);
        for round in 0..300 {
            let mut state = model.clone();
            let mut batch = db.batch();
            let mut counts = Vec::new();
            let mut expected: Result<(), usize> = Ok(());
            for at in 0..1 + random.below(6) as usize {
                let (a, b) = (random.below(4) as i64, random.below(4) as i64);
                match random.below(6) {
                    0 | 1 => {
                        let v = random.below(10) as i64;
                        let id = batch
                            .insert(&items, doc(&format!(r#"{{"k": {a}, "v": {v}}}"#)))
                            .unwrap();
                        state.insert(id, (a, v));
                        counts.push(1);
                    }
                    2 => {
                        // Every match changes: `v` goes up.
                        let update = Update::new().inc("v", 1);
                        batch
                            .update_many(&items, filter(&format!("k = {a}")), &update)
                            .unwrap();
                        let mut n = 0;
                        for (k, v) in state.values_mut() {
                            if *k == a {
                                *v += 1;
                                n += 1;
                            }
                        }
                        counts.push(n);
                    }
                    3 => {
                        // Moves `k` from a to b: nothing written when a = b.
                        let update = Update::new().set("k", b);
                        let range = filter(&format!("k = {a} and v >= {}", b * 2));
                        batch.update_many(&items, range, &update).unwrap();
                        let mut n = 0;
                        for (k, v) in state.values_mut() {
                            if *k == a && *v >= b * 2 {
                                n += usize::from(a != b);
                                *k = b;
                            }
                        }
                        counts.push(n);
                    }
                    4 => {
                        batch.delete_many(&items, filter(&format!("v >= {}", a * 3)));
                        let doomed: Vec<DocId> = (state.iter())
                            .filter(|(_, (_, v))| *v >= a * 3)
                            .map(|(id, _)| *id)
                            .collect();
                        for id in &doomed {
                            state.remove(id);
                        }
                        counts.push(doomed.len());
                    }
                    _ => {
                        batch.require(&items, filter(&format!("k = {a}"))).unwrap();
                        if !state.values().any(|(k, _)| *k == a) && expected.is_ok() {
                            expected = Err(at);
                        }
                        counts.push(0);
                    }
                }
            }
            let rehearsed = batch.dry_run();
            assert_eq!(stored(&items), model, "a dry run writes nothing");
            let outcome = batch.commit();
            match expected {
                Ok(()) => {
                    let written = outcome.unwrap();
                    assert_eq!(written.counts(), counts, "setup {setup}, round {round}");
                    assert_eq!(rehearsed.unwrap(), written);
                    model = state;
                    held += 1;
                }
                Err(at) => {
                    for result in [outcome.map(drop), rehearsed.map(drop)] {
                        let Err(Error::PreconditionFailed { op, .. }) = result else {
                            panic!("setup {setup}, round {round}: expected op {at} to fail");
                        };
                        assert_eq!(op, at, "setup {setup}, round {round}");
                    }
                    refused += 1;
                }
            }
            assert_eq!(stored(&items), model, "setup {setup}, round {round}");
        }
        assert!(
            held > 50 && refused > 30,
            "setup {setup}: {held} held, {refused} refused"
        );
        assert!(db.check().unwrap().is_ok());
    }
}

/// Workers each take the next queued job and log it, in one batch, with
/// nothing between finding the job and taking it: every job is taken
/// once, and every log entry names a job taken.
#[test]
fn concurrent_workers_take_each_job_once() {
    let (_dir, db) = open();
    let jobs = db.collection::<Document>("jobs");
    jobs.ensure_index("state").unwrap();
    for n in 0..200 {
        jobs.insert(doc(&format!(r#"{{"n": {n}, "state": "queued"}}"#)))
            .unwrap();
    }
    let threads: Vec<_> = (0..4)
        .map(|worker| {
            let db = db.clone();
            std::thread::spawn(move || {
                let (jobs, log) = (db.collection::<Document>("jobs"), db.collection("log"));
                let mut taken = 0;
                loop {
                    let queued = filter(r#"state = "queued""#);
                    let mut batch = db.batch();
                    batch.require(&jobs, queued).unwrap();
                    batch
                        .update_many(
                            &jobs,
                            filter(r#"state = "queued" order by n limit 1"#),
                            &Update::new().set("state", "taken").set("by", worker),
                        )
                        .unwrap();
                    batch
                        .insert(&log, doc(&format!(r#"{{"by": {worker}}}"#)))
                        .unwrap();
                    match batch.commit() {
                        Ok(written) => assert_eq!(written.counts(), [0, 1, 1]),
                        Err(Error::PreconditionFailed { .. }) => break taken,
                        Err(other) => panic!("{other}"),
                    }
                    taken += 1;
                }
            })
        })
        .collect();
    let taken: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
    assert_eq!(taken, 200);
    assert_eq!(jobs.count(filter(r#"state = "taken""#)).unwrap(), 200);
    for worker in 0..4 {
        let by_jobs = jobs.count(filter(&format!("by = {worker}"))).unwrap();
        let by_log = db
            .collection::<Document>("log")
            .count(filter(&format!("by = {worker}")))
            .unwrap();
        assert_eq!(by_jobs, by_log, "worker {worker}");
    }
}
