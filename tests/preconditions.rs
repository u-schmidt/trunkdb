//! A batch's preconditions (SPEC §162): `require` and `require_none`,
//! checked at `commit` under the writer's lock, after the ops before
//! them; a failed one writes nothing.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use trunkdb::query::{Condition, Filter, Update};
use trunkdb::{Collection, Database, DocId, Document, Error};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("preconditions.trunkdb")).unwrap();
    (dir, db)
}

fn doc(text: &str) -> Document {
    text.parse().unwrap()
}

fn filter(text: &str) -> Filter {
    text.parse().unwrap()
}

fn field(doc: &Document, name: &str) -> i64 {
    match doc.get(name) {
        Some(Document::Int(n)) => *n,
        other => panic!("{name}: {other:?}"),
    }
}

/// `(op, matched)` of a `PreconditionFailed`, after checking its collection.
fn failed<T: std::fmt::Debug>(result: trunkdb::Result<T>, collection: &str) -> (usize, bool) {
    match result {
        Err(Error::PreconditionFailed {
            collection: c,
            op,
            matched,
            ..
        }) => {
            assert_eq!(c, collection);
            (op, matched)
        }
        other => panic!("expected a failed precondition, got {other:?}"),
    }
}

fn accounts(db: &Database, balances: &[i64]) -> (Collection<Document>, Vec<DocId>) {
    let accounts = db.collection::<Document>("accounts");
    let ids = (balances.iter())
        .map(|b| {
            accounts
                .insert(doc(&format!(r#"{{"balance": {b}}}"#)))
                .unwrap()
        })
        .collect();
    (accounts, ids)
}

fn balance(accounts: &Collection<Document>, id: DocId) -> i64 {
    field(&accounts.get(&id).unwrap().unwrap(), "balance")
}

#[test]
fn a_batch_writes_only_while_its_precondition_holds() {
    let (_dir, db) = open();
    let (accounts, ids) = accounts(&db, &[40, 0]);
    let (from, to) = (ids[0], ids[1]);
    let transfer = |amount: i64| {
        let mut batch = db.batch();
        batch
            .require(&accounts, Filter::new().id(from).gte("balance", amount))
            .unwrap();
        let take = Update::new().inc("balance", -amount);
        batch.update_fields(&accounts, &from, &take).unwrap();
        let give = Update::new().inc("balance", amount);
        batch.update_fields(&accounts, &to, &give).unwrap();
        assert_eq!(batch.len(), 3, "a precondition counts as an op");
        batch.commit()
    };
    transfer(25).unwrap();
    assert_eq!((balance(&accounts, from), balance(&accounts, to)), (15, 25));
    assert_eq!(failed(transfer(25), "accounts"), (0, false));
    // Nothing of the failed batch was written.
    assert_eq!((balance(&accounts, from), balance(&accounts, to)), (15, 25));
    transfer(15).unwrap();
    assert_eq!((balance(&accounts, from), balance(&accounts, to)), (0, 40));
}

#[test]
fn require_none_refuses_what_a_unique_index_cant() {
    let (_dir, db) = open();
    let users = db.collection::<Document>("users");
    users
        .insert(doc(r#"{"email": "ada@example.com", "active": false}"#))
        .unwrap();
    // Unique among active users only: no unique index says that.
    let register = |email: &str| {
        let mut batch = db.batch();
        batch.insert(&users, doc(r#"{"note": "before"}"#)).unwrap();
        let taken = Filter::new().eq("email", email).eq("active", true);
        batch.require_none(&users, taken).unwrap();
        batch
            .insert(
                &users,
                doc(&format!(r#"{{"email": "{email}", "active": true}}"#)),
            )
            .unwrap();
        batch.commit()
    };
    register("ada@example.com").unwrap();
    assert_eq!(users.count(Filter::new()).unwrap(), 3);
    // The op is counted from 0, the insert before it included.
    assert_eq!(failed(register("ada@example.com"), "users"), (1, true));
    assert_eq!(users.count(Filter::new()).unwrap(), 3, "nothing written");
}

#[test]
fn a_precondition_sees_the_ops_before_it_and_not_those_after() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    let id = items.insert(doc(r#"{"v": 1}"#)).unwrap();

    // After an insert: there.
    let mut batch = db.batch();
    batch.insert(&items, doc(r#"{"v": 7}"#)).unwrap();
    batch.require(&items, filter("v = 7")).unwrap();
    batch.commit().unwrap();

    // Before an insert: not yet there.
    let mut batch = db.batch();
    batch.require(&items, filter("v = 8")).unwrap();
    batch.insert(&items, doc(r#"{"v": 8}"#)).unwrap();
    assert_eq!(failed(batch.commit(), "items"), (0, false));

    // After an update: the new value.
    let mut batch = db.batch();
    batch
        .update_fields(&items, &id, &Update::new().set("v", 2))
        .unwrap();
    batch
        .require(&items, Filter::new().id(id).eq("v", 2))
        .unwrap();
    batch.require_none(&items, filter("v = 1")).unwrap();
    batch.commit().unwrap();

    // After a delete: gone.
    let mut batch = db.batch();
    batch.delete(&items, &id);
    batch.require(&items, Filter::new().id(id)).unwrap();
    assert_eq!(failed(batch.commit(), "items"), (1, false));
    assert!(
        items.get(&id).unwrap().is_some(),
        "the delete was rolled back"
    );
}

#[test]
fn only_conditions_are_checked_and_the_rest_is_refused() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    items.insert(doc(r#"{"v": 1}"#)).unwrap();
    let refused = [
        ("a sort", filter("v = 1").sort_asc("v")),
        (
            "a sort",
            filter("v = 1").sort_by_key(["v"], |_d: &Document| Some(1.0)),
        ),
        ("a skip", filter("v = 1").skip(1)),
        ("a limit", filter("v = 1").limit(1)),
        ("a selection", filter("v = 1").select(["v"])),
        ("a selection", filter("v = 1").except(["v"])),
    ];
    for (what, filter) in refused {
        let mut batch = db.batch();
        let error = batch
            .require(&items, filter.clone())
            .unwrap_err()
            .to_string();
        assert!(error.contains(what), "{what}: {error}");
        assert!(batch.require_none(&items, filter).is_err(), "{what}");
        assert!(batch.is_empty(), "{what}: the batch is as it was");
    }
    // A hint is part of how it reads, so it's followed, or the batch fails.
    let mut batch = db.batch();
    batch
        .require(&items, filter("v = 1").use_index("v"))
        .unwrap();
    assert!(matches!(batch.commit(), Err(Error::Hint { .. })));
    items.ensure_index("v").unwrap();
    let mut batch = db.batch();
    batch
        .require(&items, filter("v = 1").use_index("v"))
        .unwrap();
    batch.commit().unwrap();
}

#[test]
fn a_missing_collection_has_nothing_and_isnt_created() {
    let (_dir, db) = open();
    let nothing = db.collection::<Document>("nothing");
    let mut batch = db.batch();
    batch.require_none(&nothing, Filter::new()).unwrap();
    batch.commit().unwrap();
    let mut batch = db.batch();
    batch.require(&nothing, Filter::new()).unwrap();
    assert_eq!(failed(batch.commit(), "nothing"), (0, false));
    assert!(!db.collections().unwrap().iter().any(|c| c == "nothing"));
}

#[test]
fn a_batch_of_preconditions_alone_checks_and_writes_nothing() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    items.insert(doc(r#"{"v": 1}"#)).unwrap();
    let changes = db.subscribe().unwrap();
    let mut batch = db.batch();
    batch.require(&items, filter("v = 1")).unwrap();
    batch.require_none(&items, filter("v = 2")).unwrap();
    assert!(!batch.is_empty());
    batch.commit().unwrap();
    let mut batch = db.batch();
    batch.require(&items, filter("v = 1")).unwrap();
    batch.require(&items, filter("v = 2")).unwrap();
    assert_eq!(failed(batch.commit(), "items"), (1, false));
    items.insert(doc(r#"{"v": 3}"#)).unwrap();
    // The checks were no commits: the insert is the first one heard of.
    let first = changes.recv().unwrap();
    assert_eq!(first.changes.len(), 1);
    assert!(matches!(first.changes[0], trunkdb::Change::Inserted { .. }));
}

#[test]
fn a_custom_condition_that_panics_fails_the_batch_and_nothing_else() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    items.insert(doc(r#"{"v": 1}"#)).unwrap();
    let mut batch = db.batch();
    batch.insert(&items, doc(r#"{"v": 2}"#)).unwrap();
    let boom = Condition::custom(["v"], |_doc: &Document| panic!("boom"));
    batch.require(&items, Filter::new().and(boom)).unwrap();
    assert!(matches!(batch.commit(), Err(Error::ClosurePanicked { .. })));
    assert_eq!(items.count(Filter::new()).unwrap(), 1);
    items.insert(doc(r#"{"v": 3}"#)).unwrap(); // still usable
}

#[test]
fn a_typed_collection_takes_preconditions_too() {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Slot {
        room: String,
        hour: i64,
    }
    let (_dir, db) = open();
    let slots = db.collection::<Slot>("slots");
    let book = |hour: i64| {
        let mut batch = db.batch();
        let taken = Filter::new().eq("room", "a").eq("hour", hour);
        batch.require_none(&slots, taken).unwrap();
        batch
            .insert(
                &slots,
                Slot {
                    room: "a".into(),
                    hour,
                },
            )
            .unwrap();
        batch.commit()
    };
    book(9).unwrap();
    book(10).unwrap();
    assert_eq!(failed(book(9), "slots"), (0, true));
}

/// A small, fixed random source, so a failure can be run again.
struct Random(u64);

impl Random {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// A filter of the model test, as the model reads it and as trunkdb does.
#[derive(Debug, Clone)]
enum Test {
    K(i64),
    KAndVAtLeast(i64, i64),
    VBelow(i64),
    Tag(i64),
    IdAndV(DocId, i64),
}

impl Test {
    fn holds(&self, id: DocId, (k, v): (i64, i64)) -> bool {
        match *self {
            Test::K(a) => k == a,
            Test::KAndVAtLeast(a, b) => k == a && v >= b,
            Test::VBelow(b) => v < b,
            // `tags` is `[k, k + 1]`.
            Test::Tag(t) => t == k || t == k + 1,
            Test::IdAndV(of, b) => id == of && v == b,
        }
    }

    fn filter(&self) -> Filter {
        match *self {
            Test::K(a) => Filter::new().eq("k", a),
            Test::KAndVAtLeast(a, b) => Filter::new().eq("k", a).gte("v", b),
            Test::VBelow(b) => Filter::new().lt("v", b),
            Test::Tag(t) => Filter::new().eq("tags[*]", t),
            Test::IdAndV(id, b) => Filter::new().id(id).eq("v", b),
        }
    }
}

fn item(k: i64, v: i64) -> Document {
    doc(&format!(
        r#"{{"k": {k}, "v": {v}, "tags": [{k}, {}]}}"#,
        k + 1
    ))
}

fn stored(items: &Collection<Document>) -> BTreeMap<DocId, (i64, i64)> {
    (items.find(Filter::new()).unwrap().iter())
        .map(|d| match d.get("_id") {
            Some(Document::Id(id)) => (*id, (field(d, "k"), field(d, "v"))),
            other => panic!("{other:?}"),
        })
        .collect()
}

/// Random batches of inserts, updates, deletes and preconditions against
/// a model that applies them in order: the same outcome, the same failed
/// op, and the same documents after, with and without indexes the checks
/// read through.
#[test]
fn preconditions_match_a_model_through_random_batches() {
    let setups: [&[&[&str]]; 4] = [&[], &[&["k"]], &[&["k", "v"]], &[&["tags[*]"], &["v"]]];
    for (setup, indexes) in setups.iter().enumerate() {
        let (_dir, db) = open();
        let items = db.collection::<Document>("items");
        for fields in indexes.iter() {
            items.ensure_index(fields.to_vec()).unwrap();
        }
        let mut random = Random(0x9e37_79b9_7f4a_7c15 ^ setup as u64);
        let mut model = BTreeMap::new();
        for n in 0..8 {
            let (k, v) = (n % 4, n);
            model.insert(items.insert(item(k, v)).unwrap(), (k, v));
        }
        let (mut held, mut refused) = (0, 0);
        for round in 0..400 {
            let mut state = model.clone();
            let mut batch = db.batch();
            let mut expected: Result<(), (usize, bool)> = Ok(());
            for at in 0..1 + random.below(6) as usize {
                let ids: Vec<DocId> = state.keys().copied().collect();
                let some_id = |random: &mut Random| ids[random.below(ids.len() as u64) as usize];
                let choice = match ids.is_empty() {
                    true => random.below(2) * 4,
                    false => random.below(6),
                };
                match choice {
                    0 => {
                        let (k, v) = (random.below(4) as i64, random.below(10) as i64);
                        let id = batch.insert(&items, item(k, v)).unwrap();
                        state.insert(id, (k, v));
                    }
                    1 => {
                        let (id, v) = (some_id(&mut random), random.below(10) as i64);
                        batch
                            .update_fields(&items, &id, &Update::new().set("v", v))
                            .unwrap();
                        state.get_mut(&id).unwrap().1 = v;
                    }
                    2 => {
                        let id = some_id(&mut random);
                        batch.delete(&items, &id);
                        state.remove(&id);
                    }
                    _ => {
                        let (a, b) = (random.below(4) as i64, random.below(10) as i64);
                        let test = match random.below(5) {
                            0 => Test::K(a),
                            1 => Test::KAndVAtLeast(a, b),
                            2 => Test::VBelow(b),
                            3 => Test::Tag(a),
                            _ if ids.is_empty() => Test::K(a),
                            _ => Test::IdAndV(some_id(&mut random), b),
                        };
                        let matched = state.iter().any(|(id, kv)| test.holds(*id, *kv));
                        let none = random.below(2) == 0;
                        match none {
                            true => batch.require_none(&items, test.filter()).unwrap(),
                            false => batch.require(&items, test.filter()).unwrap(),
                        }
                        if matched == none && expected.is_ok() {
                            expected = Err((at, matched));
                        }
                    }
                }
            }
            let outcome = batch.commit();
            match expected {
                Ok(()) => {
                    outcome.unwrap();
                    model = state;
                    held += 1;
                }
                Err(op) => {
                    assert_eq!(failed(outcome, "items"), op, "setup {setup}, round {round}");
                    refused += 1;
                }
            }
            assert_eq!(stored(&items), model, "setup {setup}, round {round}");
        }
        assert!(
            held > 50 && refused > 50,
            "setup {setup}: {held} held, {refused} refused"
        );
    }
}

/// Threads move money between three accounts, each move only while the
/// account it takes from covers it. Without the check under the lock,
/// two moves that read the same balance would both go through.
#[test]
fn concurrent_transfers_never_overdraw_and_keep_the_sum() {
    let (_dir, db) = open();
    let (accounts, ids) = accounts(&db, &[100, 100, 100]);
    let refused = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..4u64)
        .map(|t| {
            let (db, accounts, ids, refused) =
                (db.clone(), accounts.clone(), ids.clone(), refused.clone());
            std::thread::spawn(move || {
                let mut random = Random(0x2545_f491_4f6c_dd1d ^ (t + 1));
                for _ in 0..300 {
                    let from = ids[random.below(3) as usize];
                    let to = ids[random.below(3) as usize];
                    let amount = 1 + random.below(60) as i64;
                    // What the program read, and decided on.
                    if balance(&accounts, from) < amount {
                        continue;
                    }
                    let mut batch = db.batch();
                    batch
                        .require(&accounts, Filter::new().id(from).gte("balance", amount))
                        .unwrap();
                    let take = Update::new().inc("balance", -amount);
                    batch.update_fields(&accounts, &from, &take).unwrap();
                    let give = Update::new().inc("balance", amount);
                    batch.update_fields(&accounts, &to, &give).unwrap();
                    match batch.commit() {
                        Ok(_) => {}
                        Err(Error::PreconditionFailed { .. }) => {
                            refused.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(other) => panic!("{other}"),
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let balances: Vec<i64> = ids.iter().map(|id| balance(&accounts, *id)).collect();
    assert!(balances.iter().all(|b| *b >= 0), "{balances:?}");
    assert_eq!(balances.iter().sum::<i64>(), 300);
}

/// Threads count up one document by reading it and writing it back
/// whole, each write held to the version it read: none is lost.
#[test]
fn a_version_kept_by_the_program_loses_no_update() {
    let (_dir, db) = open();
    let counters = db.collection::<Document>("counters");
    let id = counters
        .insert(doc(r#"{"count": 0, "version": 0}"#))
        .unwrap();
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (db, counters) = (db.clone(), counters.clone());
            std::thread::spawn(move || {
                for _ in 0..100 {
                    // Each failure is another thread's success, so a
                    // thousand in a row mean the check is wrong.
                    for attempt in 0.. {
                        assert!(attempt < 1000, "a precondition that never holds");
                        let read = counters.get(&id).unwrap().unwrap();
                        let (count, version) = (field(&read, "count"), field(&read, "version"));
                        let written = doc(&format!(
                            r#"{{"count": {}, "version": {}}}"#,
                            count + 1,
                            version + 1
                        ));
                        let mut batch = db.batch();
                        batch
                            .require(&counters, Filter::new().id(id).eq("version", version))
                            .unwrap();
                        batch.update(&counters, &id, written).unwrap();
                        match batch.commit() {
                            Ok(_) => break,
                            Err(Error::PreconditionFailed { .. }) => continue, // read again
                            Err(other) => panic!("{other}"),
                        }
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let last = counters.get(&id).unwrap().unwrap();
    assert_eq!((field(&last, "count"), field(&last, "version")), (400, 400));
}
