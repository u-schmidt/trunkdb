//! A condition of the application's own (SPEC §136): `Condition::custom`
//! in every read and write, the fields its closure gets, what it does to
//! a plan, and a panic in it, which leaves the database as it was.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use trunkdb::query::{Aggregate, Condition, Filter, QueryPlan, Update};
use trunkdb::{Collection, Database, Document, Error, Upserted};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("custom.trunkdb")).unwrap();
    (dir, db)
}

fn doc(text: &str) -> Document {
    text.parse().unwrap()
}

/// 100 shifts: `start` `n % 12`, `end` `start + n % 9`, `team` "a" or
/// "b", `n` the number. An index on `team`.
fn shifts(db: &Database) -> Collection<Document> {
    let shifts = db.collection::<Document>("shifts");
    for n in 0..100i64 {
        let start = n % 12;
        let team = if n % 2 == 0 { "a" } else { "b" };
        shifts
            .insert(doc(&format!(
                r#"{{"n": {n}, "start": {start}, "end": {}, "team": "{team}"}}"#,
                start + n % 9
            )))
            .unwrap();
    }
    shifts.ensure_index("team").unwrap();
    shifts
}

fn number(doc: &Document, field: &str) -> i64 {
    doc.get(field).and_then(Document::as_i64).unwrap()
}

/// Longer than six hours: what no operator says, as it compares two
/// fields.
fn long() -> Condition {
    Condition::custom(["start", "end"], |doc| {
        let hours = |field| doc.get(field).and_then(Document::as_i64);
        matches!((hours("start"), hours("end")), (Some(start), Some(end)) if end - start > 6)
    })
}

/// The shifts `long` passes, as their `n`s, found without it.
fn long_ns(shifts: &Collection<Document>) -> Vec<i64> {
    let mut ns: Vec<i64> = (shifts.find(Filter::new()).unwrap().iter())
        .filter(|d| number(d, "end") - number(d, "start") > 6)
        .map(|d| number(d, "n"))
        .collect();
    ns.sort();
    ns
}

fn ns(found: &[Document]) -> Vec<i64> {
    let mut ns: Vec<i64> = found.iter().map(|d| number(d, "n")).collect();
    ns.sort();
    ns
}

#[test]
fn every_read_takes_it() {
    let (_dir, db) = open();
    let shifts = shifts(&db);
    let expected = long_ns(&shifts);
    assert_eq!(expected.len(), 22, "a test that passes some, not all");
    let filter = || Filter::new().and(long());

    assert_eq!(ns(&shifts.find(filter()).unwrap()), expected);
    assert_eq!(shifts.count(filter()).unwrap(), expected.len());
    let first = shifts.find_one(filter().sort_asc("n")).unwrap().unwrap();
    assert_eq!(number(&first, "n"), expected[0]);
    let streamed: Vec<Document> = (shifts.cursor(filter()).unwrap())
        .collect::<trunkdb::Result<_>>()
        .unwrap();
    assert_eq!(ns(&streamed), expected);
    // Sorted, skipped, limited: after the closure, as after any condition.
    let page = shifts
        .find(filter().sort_desc("n").skip(2).limit(3))
        .unwrap();
    let mut wanted: Vec<i64> = expected.iter().rev().skip(2).take(3).copied().collect();
    wanted.sort();
    assert_eq!(ns(&page), wanted);
    // In memory, as `find` checks.
    let all = shifts.find(Filter::new()).unwrap();
    assert_eq!(ns(&filter().apply(all.clone())), expected);
    assert_eq!(
        all.iter().filter(|d| long().matches(d)).count(),
        expected.len()
    );

    // ORed, negated, nested in an `elem_match`.
    let either = shifts
        .find(Filter::new().and(long() | Condition::eq("n", 0)))
        .unwrap();
    assert_eq!(ns(&either).len(), expected.len() + 1, "n 0 is a short one");
    let short = shifts.find(Filter::new().and(!long())).unwrap();
    assert_eq!(short.len(), 100 - expected.len());

    // An aggregate's filter, and a column's.
    let rows = shifts
        .aggregate(
            Filter::new().and(long()),
            &Aggregate::new().count().named("long"),
        )
        .unwrap();
    assert_eq!(rows, [doc(&format!(r#"{{"long": {}}}"#, expected.len()))]);
    let rows = shifts
        .aggregate(
            Filter::new(),
            &Aggregate::new()
                .group_by("team")
                .count()
                .named("all")
                .count()
                .filter(long())
                .named("long"),
        )
        .unwrap();
    let long_in = |team: &str| {
        (all.iter())
            .filter(|d| d.get("team").and_then(Document::as_str) == Some(team))
            .filter(|d| long().matches(d))
            .count()
    };
    assert_eq!(
        rows,
        [
            doc(&format!(
                r#"{{"team": "a", "all": 50, "long": {}}}"#,
                long_in("a")
            )),
            doc(&format!(
                r#"{{"team": "b", "all": 50, "long": {}}}"#,
                long_in("b")
            )),
        ]
    );
}

#[test]
fn an_element_of_an_array_is_its_document() {
    let (_dir, db) = open();
    let orders = db.collection::<Document>("orders");
    orders
        .insert(doc(
            r#"{"n": 1, "items": [{"qty": 2, "price": 5}, {"qty": 1, "price": 3}]}"#,
        ))
        .unwrap();
    orders
        .insert(doc(r#"{"n": 2, "items": [{"qty": 4, "price": 4}]}"#))
        .unwrap();
    // An item worth more than 12: paths start at the element (§46).
    let dear = Condition::custom(["qty", "price"], |item| {
        let get = |field| item.get(field).and_then(Document::as_i64).unwrap_or(0);
        get("qty") * get("price") > 12
    });
    let found = orders
        .find(Filter::new().and(Condition::elem_match("items", dear)))
        .unwrap();
    assert_eq!(ns(&found), [2]);
}

#[test]
fn the_closure_gets_the_fields_it_names_and_no_others() {
    let (_dir, db) = open();
    let things = db.collection::<Document>("things");
    things
        .insert(doc(
            r#"{"a": 1, "b": {"c": 2, "d": 3}, "e": 4, "f": [5, 6]}"#,
        ))
        .unwrap();
    things.ensure_index("e").unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let saw = |fields: &'static [&'static str]| {
        let seen = Arc::clone(&seen);
        Condition::custom(fields.iter().copied(), move |doc| {
            seen.lock().unwrap().push(doc.clone());
            true
        })
    };
    let take = || std::mem::take(&mut *seen.lock().unwrap());

    // The first step of each path, the whole value there.
    things.find(Filter::new().and(saw(&["b.c", "a"]))).unwrap();
    assert_eq!(take(), [doc(r#"{"a": 1, "b": {"c": 2, "d": 3}}"#)]);
    // Not the other conditions' fields, though the read decoded them.
    let filter = Filter::new()
        .eq("e", 4)
        .and(Condition::exists("f"))
        .and(saw(&["a"]));
    assert_eq!(things.count(filter.clone()).unwrap(), 1);
    assert_eq!(take(), [doc(r#"{"a": 1}"#)]);
    // Nor the sort's.
    things.find(filter.clone().sort_asc("b.d")).unwrap();
    assert_eq!(take(), [doc(r#"{"a": 1}"#)]);
    // Checked in memory, from the whole document: the same.
    assert!(filter.matches(&things.find(Filter::new()).unwrap()[0]));
    assert_eq!(take(), [doc(r#"{"a": 1}"#)]);
    // A field the document hasn't is missing, as in the document.
    things.find(Filter::new().and(saw(&["a", "z"]))).unwrap();
    assert_eq!(take(), [doc(r#"{"a": 1}"#)]);
    // None named: nothing. `""` or `[*]` first: the whole document.
    things.find(Filter::new().and(saw(&[]))).unwrap();
    assert_eq!(take(), [doc("{}")]);
    things.find(Filter::new().and(saw(&[""]))).unwrap();
    let whole = take();
    assert_eq!(whole.len(), 1);
    assert_eq!(whole[0].as_object().unwrap().len(), 5, "with its _id");
}

#[test]
fn it_never_bounds_a_read_but_runs_on_what_the_others_leave() {
    let (_dir, db) = open();
    let shifts = shifts(&db);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = {
        let calls = Arc::clone(&calls);
        Condition::custom(["team"], move |doc| {
            calls.fetch_add(1, Ordering::Relaxed);
            doc.get("team").and_then(Document::as_str) == Some("a")
        })
    };
    // Alone it scans, though it reads an indexed field.
    let alone = Filter::new().and(counted.clone());
    assert_eq!(shifts.explain(&alone).unwrap(), QueryPlan::Scan);
    assert_eq!(shifts.count(alone).unwrap(), 50);
    assert_eq!(calls.swap(0, Ordering::Relaxed), 100);
    // ANDed with a condition that bounds one, on the 50 it leaves.
    let bounded = Filter::new().eq("team", "b").and(counted);
    assert!(matches!(
        shifts.explain(&bounded).unwrap(),
        QueryPlan::Index { field, .. } if field == "team"
    ));
    assert_eq!(shifts.count(bounded).unwrap(), 0);
    assert_eq!(calls.load(Ordering::Relaxed), 50);
}

#[test]
fn every_write_takes_it() {
    let (_dir, db) = open();
    let shifts = shifts(&db);
    let expected = long_ns(&shifts);

    let changed = shifts
        .update_fields(Filter::new().and(long()), &Update::new().set("long", true))
        .unwrap();
    assert_eq!(changed, expected.len());
    assert_eq!(
        ns(&shifts.find(Filter::new().eq("long", true)).unwrap()),
        expected
    );
    let changed = shifts
        .update_many(Filter::new().and(long()), |d| {
            if let Document::Object(map) = d {
                map.insert("checked".into(), Document::Bool(true));
            }
        })
        .unwrap();
    assert_eq!(changed, expected.len());
    assert_eq!(
        shifts.count(Filter::new().eq("checked", true)).unwrap(),
        expected.len()
    );
    // One match: updated; none: inserted.
    let one = Condition::custom(["n"], |d| d.get("n").and_then(Document::as_i64) == Some(5));
    let upserted = shifts
        .upsert(
            Filter::new().and(one),
            doc(r#"{"n": 5, "start": 0, "end": 1, "team": "a"}"#),
        )
        .unwrap();
    assert!(matches!(upserted, Upserted::Updated(_)));
    let none = Condition::custom(["n"], |d| {
        d.get("n").and_then(Document::as_i64) == Some(500)
    });
    let upserted = shifts
        .upsert(Filter::new().and(none), doc(r#"{"n": 500}"#))
        .unwrap();
    assert!(matches!(upserted, Upserted::Inserted(_)));
    let deleted = shifts.delete_many(Filter::new().and(long())).unwrap();
    assert_eq!(deleted, expected.len());
    assert_eq!(shifts.count(Filter::new()).unwrap(), 101 - expected.len());
}

/// A closure that panics once its document's `n` is `at`, with its own
/// message.
fn panics_at(at: i64) -> Condition {
    Condition::custom(["n"], move |doc| {
        let n = doc.get("n").and_then(Document::as_i64).unwrap_or(-1);
        if n == at {
            panic!("no shift {n}");
        }
        true
    })
}

fn panicked(result: trunkdb::Result<impl std::fmt::Debug>) -> String {
    match result {
        Err(Error::ConditionPanicked { message, .. }) => message,
        other => panic!("not ConditionPanicked: {other:?}"),
    }
}

#[test]
fn a_panic_fails_the_call_and_the_database_goes_on() {
    let (_dir, db) = open();
    let shifts = shifts(&db);
    let filter = || Filter::new().and(panics_at(40));
    let all = shifts.find(Filter::new()).unwrap();

    // Each read: the error, with the panic's message.
    assert_eq!(panicked(shifts.find(filter())), "no shift 40");
    assert_eq!(panicked(shifts.count(filter())), "no shift 40");
    assert_eq!(
        panicked(shifts.find_one(filter().eq("n", 40))),
        "no shift 40"
    );
    assert_eq!(panicked(shifts.explain_run(filter())), "no shift 40");
    let aggregate = Aggregate::new().count().named("n");
    assert_eq!(
        panicked(shifts.aggregate(filter(), &aggregate)),
        "no shift 40"
    );
    let column = Aggregate::new().count().filter(panics_at(40)).named("n");
    assert_eq!(
        panicked(shifts.aggregate(Filter::new(), &column)),
        "no shift 40"
    );
    let streamed: trunkdb::Result<Vec<Document>> = shifts.cursor(filter()).unwrap().collect();
    assert_eq!(panicked(streamed), "no shift 40");
    // A message made with `format!` too, and a panic without one.
    let formatted = Condition::custom(["n"], |doc| panic!("{} it is", doc.get("n").unwrap()));
    assert_eq!(
        panicked(shifts.find(Filter::new().and(formatted))),
        "0 it is"
    );
    let silent = Condition::custom(["n"], |_| std::panic::panic_any(7u8));
    assert_eq!(
        panicked(shifts.count(Filter::new().and(silent))),
        "a panic without a message"
    );

    // Each write: the error, nothing written. Under a `not` a panic
    // taken as `false` would delete every document.
    assert_eq!(
        panicked(shifts.delete_many(Filter::new().and(!panics_at(40)))),
        "no shift 40"
    );
    let set = Update::new().set("touched", true);
    assert_eq!(
        panicked(shifts.update_fields(filter(), &set)),
        "no shift 40"
    );
    assert_eq!(
        panicked(shifts.update_many(filter(), |d| *d = Document::Null)),
        "no shift 40"
    );
    assert_eq!(
        panicked(shifts.upsert(filter(), doc(r#"{"n": 1000}"#))),
        "no shift 40"
    );
    assert_eq!(shifts.find(Filter::new()).unwrap(), all, "nothing written");

    // And the database goes on: a write, a read, a write by a closure.
    shifts.insert(doc(r#"{"n": 100}"#)).unwrap();
    assert_eq!(shifts.count(Filter::new()).unwrap(), 101);
    let deleted = shifts
        .delete_many(Filter::new().and(panics_at(-2)).eq("n", 100))
        .unwrap();
    assert_eq!(deleted, 1);
    db.check().unwrap();
}

#[test]
fn a_panic_checked_in_memory_goes_on() {
    let all = vec![doc(r#"{"n": 1}"#), doc(r#"{"n": 2}"#)];
    let filter = Filter::new().and(panics_at(2));
    let caught = std::panic::catch_unwind(|| filter.apply(all.clone()));
    let message = caught.unwrap_err();
    assert_eq!(message.downcast_ref::<String>().unwrap(), "no shift 2");
    assert!(std::panic::catch_unwind(|| filter.matches(&all[1])).is_err());
    assert!(std::panic::catch_unwind(|| panics_at(1).matches(&all[0])).is_err());
    let aggregate = Aggregate::new().count().filter(panics_at(1)).named("n");
    assert!(std::panic::catch_unwind(|| aggregate.apply(all.clone())).is_err());
}

#[test]
fn it_is_cloned_compared_and_shown() {
    let a = long();
    assert_eq!(a, a.clone(), "a clone shares the closure");
    assert_ne!(a, long(), "two closures, though written the same");
    let Condition::Custom(custom) = &a else {
        panic!("not Custom");
    };
    assert_eq!(custom.fields(), ["start", "end"]);
    assert_eq!(format!("{a:?}"), r#"Custom(custom(["start", "end"]))"#);
    // Text it has, to be read by a person; text it doesn't read back from.
    let filter = Filter::new().eq("team", "a").and(a);
    assert_eq!(filter.to_string(), r#"team = "a" and custom(start, end)"#);
    let refused = filter.to_string().parse::<Filter>().unwrap_err();
    assert!(
        refused
            .to_string()
            .starts_with("`custom(…)` is a closure in code"),
        "{refused}"
    );
}

#[test]
fn a_filter_with_one_moves_between_threads() {
    // `UnwindSafe` as before it could hold a closure (§136.3).
    fn shared<T: Send + Sync + Clone + std::panic::RefUnwindSafe + 'static>(_: &T) {}
    let (_dir, db) = open();
    let shifts = shifts(&db);
    let filter = Filter::new().and(long());
    shared(&filter);
    let expected = shifts.count(filter.clone()).unwrap();
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (shifts, filter) = (shifts.clone(), filter.clone());
            std::thread::spawn(move || shifts.count(filter).unwrap())
        })
        .collect();
    for thread in threads {
        assert_eq!(thread.join().unwrap(), expected);
    }
}

#[test]
fn a_subscriber_tests_what_changed() {
    // The hook's reader (§135) with the same condition `find` takes.
    let (_dir, db) = open();
    let shifts = db.collection::<Document>("shifts");
    let changes = db.subscribe().unwrap();
    let filter = Filter::new().and(long());
    let short = shifts.insert(doc(r#"{"start": 8, "end": 12}"#)).unwrap();
    let long_one = shifts.insert(doc(r#"{"start": 6, "end": 18}"#)).unwrap();
    let mut long_ids = Vec::new();
    for _ in 0..2 {
        for change in changes.recv().unwrap().changes {
            let id = change.id().unwrap();
            if filter.matches(&shifts.get(&id).unwrap().unwrap()) {
                long_ids.push(id);
            }
        }
    }
    assert_eq!(long_ids, [long_one]);
    let _ = short;
}
