//! A window on an aggregate, and live aggregates (SPEC §139):
//! `Aggregate::window`, the documents within a duration of the newest,
//! and `Collection::watch_aggregate`, an aggregate run again as each
//! commit to its collection commits, reporting how its rows changed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use trunkdb::query::{Aggregate, Condition, Filter};
use trunkdb::{Collection, Database, DateTime, Document, Error, LiveAggregate, LiveRow};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("live_aggregates.trunkdb")).unwrap();
    (dir, db)
}

fn readings(db: &Database) -> Collection<Document> {
    db.collection::<Document>("readings")
}

/// `secs` seconds after the epoch.
fn t(secs: i64) -> DateTime {
    DateTime::from_unix(secs, 0).unwrap()
}

/// A reading of `server`'s load at `secs`.
fn reading(server: &str, load: f64, secs: i64) -> Document {
    let mut doc: Document = format!(r#"{{"server": "{server}", "load": {load:?}}}"#)
        .parse()
        .unwrap();
    if let Document::Object(map) = &mut doc {
        map.insert("at".into(), Document::DateTime(t(secs)));
    }
    doc
}

fn mean_per_server() -> Aggregate {
    Aggregate::new()
        .group_by("server")
        .count()
        .named("n")
        .avg("load")
        .named("mean")
}

fn five_minutes() -> Duration {
    Duration::from_secs(300)
}

/// A row as `(server, n)`, for whole comparisons.
fn short(row: &Document) -> (String, i64) {
    (
        row.get("server")
            .and_then(Document::as_str)
            .unwrap_or("")
            .to_string(),
        row.get("n").and_then(Document::as_i64).unwrap(),
    )
}

fn rows(found: &[Document]) -> Vec<(String, i64)> {
    found.iter().map(short).collect()
}

fn s(server: &str, n: i64) -> (String, i64) {
    (server.to_string(), n)
}

/// The same rows from every way of running `aggregate`: the database,
/// a snapshot, and `Aggregate::apply` over what the filter finds.
fn every_way(db: &Database, filter: Filter, aggregate: &Aggregate) -> Vec<(String, i64)> {
    let readings = readings(db);
    let stored = readings.aggregate(filter.clone(), aggregate).unwrap();
    let snapshot = db.snapshot().unwrap();
    let viewed = snapshot
        .collection::<Document>("readings")
        .aggregate(filter.clone(), aggregate)
        .unwrap();
    let in_memory = aggregate.apply(readings.find(filter).unwrap()).unwrap();
    assert_eq!(stored, viewed, "a snapshot");
    assert_eq!(stored, in_memory, "in memory");
    rows(&stored)
}

#[test]
fn a_window_keeps_what_is_close_to_the_newest() {
    let (_dir, db) = open();
    let readings = readings(&db);
    for doc in [
        reading("a", 0.5, 1000),
        reading("a", 0.7, 1300), // 300 s before the newest: in
        reading("a", 0.9, 1600), // the newest
        reading("b", 0.2, 1299), // 301 s before: out
        reading("b", 0.4, 1500),
    ] {
        readings.insert(doc).unwrap();
    }
    // No `DateTime` there: out, whatever the value.
    readings
        .insert(r#"{"server": "c", "load": 1.0, "at": 99999}"#.parse().unwrap())
        .unwrap();
    readings
        .insert(r#"{"server": "c", "load": 1.0}"#.parse().unwrap())
        .unwrap();
    let windowed = mean_per_server().window("at", five_minutes());
    assert_eq!(
        every_way(&db, Filter::new(), &windowed),
        [s("a", 2), s("b", 1)]
    );
    let means = readings.aggregate(Filter::new(), &windowed).unwrap();
    assert_eq!(means[0].get("mean").and_then(Document::as_f64), Some(0.8));
    // Through an index on the path: the same.
    readings.ensure_index("at").unwrap();
    assert_eq!(
        every_way(&db, Filter::new(), &windowed),
        [s("a", 2), s("b", 1)]
    );
    // Nanoseconds at the edge.
    let edge = DateTime::from_unix(1300, 1).unwrap();
    readings.delete_many(Filter::new()).unwrap();
    for (server, at) in [("a", t(1600)), ("b", t(1300)), ("c", edge)] {
        let mut doc = reading(server, 0.0, 0);
        if let Document::Object(map) = &mut doc {
            map.insert("at".into(), Document::DateTime(at));
        }
        readings.insert(doc).unwrap();
    }
    let narrow = mean_per_server().window("at", Duration::new(299, 999_999_999));
    assert_eq!(
        every_way(&db, Filter::new(), &narrow),
        [s("a", 1), s("c", 1)]
    );
    // A window longer than all of time keeps every date-time.
    let all = mean_per_server().window("at", Duration::MAX);
    assert_eq!(
        every_way(&db, Filter::new(), &all),
        [s("a", 1), s("b", 1), s("c", 1)]
    );
}

#[test]
fn the_newest_is_the_whole_aggregates_among_the_filters_matches() {
    let (_dir, db) = open();
    let readings = readings(&db);
    readings.insert(reading("a", 0.5, 1000)).unwrap();
    readings.insert(reading("b", 0.5, 2000)).unwrap();
    readings.insert(reading("c", 0.5, 9000)).unwrap();
    let windowed = mean_per_server().window("at", five_minutes());
    // One newest for all: `a` and `b` stopped reporting long before `c`.
    assert_eq!(every_way(&db, Filter::new(), &windowed), [s("c", 1)]);
    // The newest among the matches: `c` filtered out, `b` is newest.
    let not_c = Filter::new().ne("server", "c");
    assert_eq!(every_way(&db, not_c.clone(), &windowed), [s("b", 1)]);
    // A window by count beside it is refused: by time or by count.
    for picked in [
        not_c.clone().sort_desc("at"),
        not_c.clone().skip(1),
        not_c.clone().limit(2),
    ] {
        assert!(readings.aggregate(picked.clone(), &windowed).is_err());
        assert!(readings.watch_aggregate(picked, &windowed).is_err());
    }
    // No date-time at all: nothing, so no group, and one empty row
    // without `group_by`.
    let none = mean_per_server().window("when", five_minutes());
    assert_eq!(every_way(&db, Filter::new(), &none), []);
    let count = Aggregate::new()
        .count()
        .named("n")
        .window("when", five_minutes());
    assert_eq!(every_way(&db, Filter::new(), &count), [s("", 0)]);
}

#[test]
fn a_window_reads_through_an_index_on_its_path() {
    let (_dir, db) = open();
    let readings = readings(&db);
    for i in 0..500 {
        readings
            .insert(reading(["a", "b"][i % 2], 0.5, i as i64 * 10))
            .unwrap();
    }
    readings.ensure_index("at").unwrap();
    let windowed = mean_per_server().window("at", Duration::from_secs(100));
    let explained = readings
        .explain_aggregate(Filter::new(), &windowed)
        .unwrap();
    // The newest, then the 11 readings of the window: not 500.
    assert!(
        explained.stats.documents_examined <= 12,
        "{} examined",
        explained.stats.documents_examined
    );
    assert_eq!(
        rows(&readings.aggregate(Filter::new(), &windowed).unwrap()),
        [s("a", 5), s("b", 6)]
    );
}

#[test]
fn a_windows_path_must_name_a_field() {
    let (_dir, db) = open();
    let readings = readings(&db);
    for path in ["", "at[*]", "_id"] {
        let windowed = mean_per_server().window(path, five_minutes());
        assert!(
            readings.aggregate(Filter::new(), &windowed).is_err(),
            "{path:?}"
        );
        assert!(windowed.apply([]).is_err(), "{path:?}");
        assert!(readings.watch_aggregate(Filter::new(), &windowed).is_err());
    }
}

/// A live aggregate's event as `(kind, the row's server and n)`; for a
/// row gone, `"gone"` and its group's server.
type Short = (&'static str, Option<(String, i64)>);

fn short_group(group: &Document) -> String {
    let server = group.get("server").and_then(Document::as_str);
    server.unwrap_or("").to_string()
}

fn gone(server: &str) -> Short {
    ("gone", Some(s(server, 0)))
}

fn event(live: LiveRow) -> Short {
    match &live {
        LiveRow::Entered { row, .. } => ("entered", Some(short(row))),
        // Gone: no row, the group's values alone.
        LiveRow::Left {
            row: None, group, ..
        } => ("gone", Some((short_group(group), 0))),
        LiveRow::Left { row: Some(row), .. } => ("left", Some(short(row))),
        LiveRow::Changed { row, .. } => ("changed", Some(short(row))),
        _ => unreachable!(),
    }
}

fn waiting(live: &LiveAggregate) -> Vec<Short> {
    std::iter::from_fn(|| live.try_recv())
        .map(|e| event(e.unwrap()))
        .collect()
}

fn ev(kind: &'static str, server: &str, n: i64) -> Short {
    (kind, Some(s(server, n)))
}

fn busy() -> Aggregate {
    mean_per_server().having(Condition::gt("mean", 0.8))
}

#[test]
fn every_row_comes_first() {
    let (_dir, db) = open();
    let readings = readings(&db);
    readings.insert(reading("b", 0.5, 1)).unwrap();
    readings.insert(reading("a", 0.5, 2)).unwrap();
    readings.insert(reading("a", 0.5, 3)).unwrap();
    let live = readings
        .watch_aggregate(Filter::new(), &mean_per_server())
        .unwrap();
    assert_eq!(
        waiting(&live),
        [ev("entered", "a", 2), ev("entered", "b", 1)]
    );
    // Without `group_by`, one row, there for no documents too.
    let empty = db.collection::<Document>("empty");
    let count = Aggregate::new().count().named("n");
    let live = empty.watch_aggregate(Filter::new(), &count).unwrap();
    assert_eq!(waiting(&live), [ev("entered", "", 0)]);
    empty.insert(reading("a", 0.0, 0)).unwrap();
    assert_eq!(waiting(&live), [ev("changed", "", 1)]);
}

#[test]
fn a_row_crossing_the_having_enters_and_leaves() {
    let (_dir, db) = open();
    let readings = readings(&db);
    let live = readings.watch_aggregate(Filter::new(), &busy()).unwrap();
    assert_eq!(waiting(&live), []);
    // Under the limit: no row; over it: in.
    readings.insert(reading("a", 0.5, 1)).unwrap();
    assert_eq!(waiting(&live), []);
    let high = readings.insert(reading("a", 1.5, 2)).unwrap();
    assert_eq!(waiting(&live), [ev("entered", "a", 2)]);
    // Still over, with other values.
    readings.insert(reading("a", 1.0, 3)).unwrap();
    assert_eq!(waiting(&live), [ev("changed", "a", 3)]);
    // Under again: left, with the row as it is now.
    readings.insert(reading("a", 0.0, 4)).unwrap();
    assert_eq!(waiting(&live), [ev("left", "a", 4)]);
    // Over, then the group's documents all gone: left, no row.
    readings.delete(&high).unwrap();
    readings.insert(reading("a", 5.0, 5)).unwrap();
    assert_eq!(waiting(&live), [ev("entered", "a", 4)]);
    readings
        .delete_many(Filter::new().eq("server", "a"))
        .unwrap();
    assert_eq!(waiting(&live), [gone("a")]);
}

#[test]
fn changed_only_where_a_value_changed() {
    let (_dir, db) = open();
    let readings = readings(&db);
    let mean = Aggregate::new()
        .group_by("server")
        .avg("load")
        .named("mean");
    let live = readings.watch_aggregate(Filter::new(), &mean).unwrap();
    readings.insert(reading("a", 0.5, 1)).unwrap();
    assert!(matches!(
        &waiting_rows(&live)[..],
        [LiveRow::Entered { .. }]
    ));
    // The same mean: no event. Another: one.
    readings.insert(reading("a", 0.5, 2)).unwrap();
    assert!(waiting_rows(&live).is_empty());
    readings.insert(reading("a", 2.0, 3)).unwrap();
    assert!(matches!(
        &waiting_rows(&live)[..],
        [LiveRow::Changed { .. }]
    ));
    // A group of `5` is one with `5.0`: no row left or entered as the
    // first document, whose value the row shows, changes type.
    let by_level = Aggregate::new().group_by("level").count().named("n");
    let levels = db.collection::<Document>("levels");
    let five = levels.insert(r#"{"level": 5}"#.parse().unwrap()).unwrap();
    let live = levels.watch_aggregate(Filter::new(), &by_level).unwrap();
    waiting_rows(&live);
    levels.insert(r#"{"level": 5.0}"#.parse().unwrap()).unwrap();
    levels.delete(&five).unwrap();
    let events = waiting_rows(&live);
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(
        events.iter().all(|e| matches!(e, LiveRow::Changed { .. })),
        "{events:?}"
    );
    // An object's fields in another order are the same value (§124): no
    // event as the first document, whose order the row shows, changes.
    let shapes = db.collection::<Document>("shapes");
    let first = shapes
        .insert(r#"{"size": {"w": 1, "h": 2}}"#.parse().unwrap())
        .unwrap();
    let by_size = Aggregate::new()
        .group_by("size")
        .max("size")
        .named("biggest");
    let live = shapes.watch_aggregate(Filter::new(), &by_size).unwrap();
    waiting_rows(&live);
    shapes
        .insert(r#"{"size": {"h": 2, "w": 1}}"#.parse().unwrap())
        .unwrap();
    shapes.delete(&first).unwrap();
    assert_eq!(waiting_rows(&live), []);
}

fn waiting_rows(live: &LiveAggregate) -> Vec<LiveRow> {
    std::iter::from_fn(|| live.try_recv())
        .map(Result::unwrap)
        .collect()
}

#[test]
fn no_crossing_is_lost_to_a_late_reader() {
    let (_dir, db) = open();
    let readings = readings(&db);
    let live = readings.watch_aggregate(Filter::new(), &busy()).unwrap();
    let one = readings.insert(reading("a", 0.9, 1)).unwrap();
    readings.delete(&one).unwrap();
    readings.insert(reading("a", 0.9, 2)).unwrap();
    readings.insert(reading("a", 0.1, 3)).unwrap();
    assert_eq!(
        waiting(&live),
        [
            ev("entered", "a", 1),
            gone("a"),
            ev("entered", "a", 1),
            ev("left", "a", 2),
        ]
    );
}

#[test]
fn only_a_commit_to_its_collection_runs_it_again() {
    let (_dir, db) = open();
    let readings = readings(&db);
    let others = db.collection::<Document>("others");
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = {
        let calls = Arc::clone(&calls);
        Filter::new().and(Condition::custom(["server"], move |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            true
        }))
    };
    readings.insert(reading("a", 0.5, 1)).unwrap();
    let live = readings
        .watch_aggregate(counted, &mean_per_server())
        .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1, "the first run");
    others.insert(reading("a", 0.5, 1)).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1, "another collection");
    readings.insert(reading("a", 0.5, 2)).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 3, "run again, whole");
    assert_eq!(
        waiting(&live),
        [ev("entered", "a", 1), ev("changed", "a", 2)]
    );
}

#[test]
fn the_busiest_three_are_live_too() {
    let (_dir, db) = open();
    let readings = readings(&db);
    for (server, n) in [("a", 4), ("b", 3), ("c", 2), ("d", 1)] {
        for i in 0..n {
            readings.insert(reading(server, 0.5, i)).unwrap();
        }
    }
    let busiest = mean_per_server().sort_desc("n").limit(3);
    let live = readings.watch_aggregate(Filter::new(), &busiest).unwrap();
    assert_eq!(
        waiting(&live),
        [
            ev("entered", "a", 4),
            ev("entered", "b", 3),
            ev("entered", "c", 2)
        ]
    );
    // `d` passes `c`: `c` leaves, still there, and `d` enters.
    readings.insert(reading("d", 0.5, 9)).unwrap();
    assert_eq!(waiting(&live), []); // tied at 2: `c` sorts first still
    readings.insert(reading("d", 0.5, 10)).unwrap();
    assert_eq!(waiting(&live), [ev("left", "c", 2), ev("entered", "d", 3)]);
    // `b` passes `a`: a new order within the three, and `b` changed.
    readings.insert(reading("b", 0.5, 11)).unwrap();
    readings.insert(reading("b", 0.5, 12)).unwrap();
    assert_eq!(
        waiting(&live),
        [ev("changed", "b", 4), ev("changed", "b", 5)]
    );
    // The filter's limit too: the mean of the last two readings.
    let last_two = Filter::new().sort_desc("at").limit(2);
    let mean = Aggregate::new()
        .count()
        .named("n")
        .avg("load")
        .named("mean");
    let live = readings.watch_aggregate(last_two, &mean).unwrap();
    assert_eq!(waiting(&live), [ev("entered", "", 2)]);
    readings.insert(reading("e", 0.5, 99)).unwrap();
    assert_eq!(waiting(&live), [], "two readings still, the same mean");
    readings.insert(reading("e", 1.5, 100)).unwrap();
    assert_eq!(waiting(&live), [ev("changed", "", 2)]);
}

#[test]
fn a_server_gone_silent_ages_out_of_the_window() {
    let (_dir, db) = open();
    let readings = readings(&db);
    readings.ensure_index("at").unwrap();
    let windowed = busy().window("at", five_minutes());
    let live = readings.watch_aggregate(Filter::new(), &windowed).unwrap();
    readings.insert(reading("a", 0.9, 1000)).unwrap();
    readings.insert(reading("b", 0.9, 1000)).unwrap();
    assert_eq!(
        waiting(&live),
        [ev("entered", "a", 1), ev("entered", "b", 1)]
    );
    // `b` stops; `a` goes on reporting, and the window moves with it.
    readings.insert(reading("a", 0.9, 1200)).unwrap();
    assert_eq!(waiting(&live), [ev("changed", "a", 2)]);
    readings.insert(reading("a", 0.9, 1301)).unwrap();
    // `b`'s reading is out, and so is `a`'s first: still two readings
    // of `a`, the same mean, so its row hasn't changed.
    assert_eq!(waiting(&live), [gone("b")]);
}

#[test]
fn a_panic_ends_that_live_aggregate_alone() {
    let (_dir, db) = open();
    let readings = readings(&db);
    readings.insert(reading("a", 0.5, 1)).unwrap();
    let fragile = Filter::new().and(Condition::custom(["load"], |doc| {
        if doc.get("load").and_then(Document::as_f64) == Some(13.0) {
            panic!("unlucky load");
        }
        true
    }));
    let doomed = readings
        .watch_aggregate(fragile.clone(), &mean_per_server())
        .unwrap();
    let steady = readings
        .watch_aggregate(Filter::new(), &mean_per_server())
        .unwrap();
    waiting(&doomed);
    waiting(&steady);
    let unlucky = readings.insert(reading("a", 13.0, 2)).unwrap();
    match doomed.try_recv() {
        Some(Err(Error::ClosurePanicked { message, .. })) => assert_eq!(message, "unlucky load"),
        other => panic!("not ClosurePanicked: {other:?}"),
    }
    assert!(doomed.try_recv().is_none());
    // Ended: the next commit runs it no more.
    readings.insert(reading("a", 0.5, 3)).unwrap();
    assert!(doomed.try_recv().is_none(), "ended");
    // The commit stands; the other goes on.
    assert_eq!(
        waiting(&steady),
        [ev("changed", "a", 2), ev("changed", "a", 3)]
    );
    assert!(readings.get(&unlucky).unwrap().is_some());
    db.check().unwrap();
    // A panic in the first run fails the call.
    assert!(matches!(
        readings.watch_aggregate(fragile, &mean_per_server()),
        Err(Error::ClosurePanicked { .. })
    ));
}

#[test]
fn a_dropped_collection_leaves_its_rows() {
    let (_dir, db) = open();
    let readings = readings(&db);
    readings.insert(reading("a", 0.5, 1)).unwrap();
    readings.insert(reading("b", 0.5, 1)).unwrap();
    let live = readings
        .watch_aggregate(Filter::new(), &mean_per_server())
        .unwrap();
    let total = readings
        .watch_aggregate(Filter::new(), &Aggregate::new().count().named("n"))
        .unwrap();
    waiting(&live);
    waiting(&total);
    assert!(db.drop_collection("readings").unwrap());
    assert_eq!(waiting(&live), [gone("a"), gone("b")]);
    // Without `group_by`, the one row stays, as `aggregate` says.
    assert_eq!(waiting(&total), [ev("changed", "", 0)]);
    readings.insert(reading("c", 0.5, 1)).unwrap();
    assert_eq!(waiting(&live), [ev("entered", "c", 1)]);
}

#[test]
fn refused_as_aggregate_refuses() {
    let (_dir, db) = open();
    let readings = readings(&db);
    let twice = Aggregate::new().count().named("n").count().named("n");
    assert!(readings.aggregate(Filter::new(), &twice).is_err());
    assert!(readings.watch_aggregate(Filter::new(), &twice).is_err());
    // A typed collection watches the same rows.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Reading {
        server: String,
    }
    let typed = db.collection::<Reading>("readings");
    typed.insert(Reading { server: "a".into() }).unwrap();
    let live = typed
        .watch_aggregate(
            Filter::new(),
            &Aggregate::new().group_by("server").count().named("n"),
        )
        .unwrap();
    assert_eq!(waiting(&live), [ev("entered", "a", 1)]);
}

#[test]
fn iterating_ends_when_the_database_is_dropped() {
    let (_dir, db) = open();
    let readings = readings(&db);
    let live = readings
        .watch_aggregate(Filter::new(), &mean_per_server())
        .unwrap();
    readings.insert(reading("a", 0.5, 1)).unwrap();
    readings.insert(reading("b", 0.5, 1)).unwrap();
    drop(readings);
    drop(db);
    assert_eq!(live.count(), 2);
}
