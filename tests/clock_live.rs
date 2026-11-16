//! Live queries and aggregates against the clock (SPEC §204): read again
//! by `trunkdb-clock` as time passes, at the next moment a result can
//! change, found through an index, or every `clock_every` without one.
//!
//! The ages here are a few hundred milliseconds. Each test asserts that
//! an event never comes before its moment, and gives it seconds to come
//! after: a slow machine is late, never early.

use std::time::Duration;
use trunkdb::query::{Aggregate, Filter};
use trunkdb::{Collection, Database, DateTime, Document, Error, Live, LiveRow, OpenOptions};

const AGE: Duration = Duration::from_millis(300);
const PATIENCE: Duration = Duration::from_secs(10);

fn open(options: OpenOptions) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with(dir.path().join("clock.trunkdb"), options).unwrap();
    (dir, db)
}

/// A device's document, seen at `at`.
fn device(name: &str, at: DateTime) -> Document {
    let mut doc: Document = format!(r#"{{"name": "{name}"}}"#).parse().unwrap();
    if let Document::Object(map) = &mut doc {
        map.insert("last_seen".into(), Document::DateTime(at));
    }
    doc
}

fn seen(doc: &Document) -> DateTime {
    match doc.get("last_seen") {
        Some(Document::DateTime(at)) => *at,
        other => panic!("no time: {other:?}"),
    }
}

fn name(doc: &Document) -> &str {
    doc.get("name").and_then(Document::as_str).unwrap()
}

/// `at` moved back by `by`.
fn before(at: DateTime, by: Duration) -> DateTime {
    let nanos = i128::from(at.unix_seconds()) * 1_000_000_000 + i128::from(at.subsec_nanos())
        - by.as_nanos() as i128;
    DateTime::from_unix(
        nanos.div_euclid(1_000_000_000) as i64,
        nanos.rem_euclid(1_000_000_000) as u32,
    )
    .unwrap()
}

/// The next event, within `PATIENCE`.
fn next<T: std::fmt::Debug>(live: &trunkdb::LiveQuery<T>) -> Live<T> {
    live.recv_timeout(PATIENCE)
        .expect("an event in time")
        .unwrap()
}

fn next_row(live: &trunkdb::LiveAggregate) -> LiveRow {
    live.recv_timeout(PATIENCE)
        .expect("an event in time")
        .unwrap()
}

/// A device that stops reporting is reported silent, by the clock, no
/// sooner than its age after it was seen; back when it reports again; and
/// silent again later. Through an index on `last_seen`.
#[test]
fn a_device_that_goes_silent_is_reported() {
    let (_dir, db) = open(OpenOptions::default());
    let devices = db.collection::<Document>("devices");
    devices.ensure_index("last_seen").unwrap();
    let van = devices.insert(device("van", DateTime::now())).unwrap();
    let live = devices
        .watch(Filter::new().older_than("last_seen", AGE))
        .unwrap();
    assert!(live.try_recv().is_none(), "seen just now: not silent");

    for _ in 0..2 {
        let Live::Entered { id, doc, .. } = next(&live) else {
            panic!("not entered");
        };
        assert_eq!(id, van);
        assert!(before(DateTime::now(), AGE) > seen(&doc), "never early");

        // It reports again: back, by the commit.
        devices
            .update(&van, device("van", DateTime::now()))
            .unwrap();
        let Live::Left { id, doc, .. } = next(&live) else {
            panic!("not left");
        };
        assert_eq!(id, van);
        assert_eq!(name(&doc.unwrap()), "van");
    }
}

/// Through an index the moment is found, not waited for: with
/// `clock_every` an hour, it still comes at its moment. Watched before
/// any device is there, the moment is found anew as one is written.
#[test]
fn an_index_finds_the_moment() {
    let (_dir, db) = open(OpenOptions::default().clock_every(Duration::from_secs(3600)));
    let devices = db.collection::<Document>("devices");
    devices.ensure_index("last_seen").unwrap();
    let live = devices
        .watch(Filter::new().older_than("last_seen", AGE))
        .unwrap();
    let van = devices.insert(device("van", DateTime::now())).unwrap();
    let Live::Entered { id, doc, .. } = next(&live) else {
        panic!("not entered");
    };
    assert_eq!(id, van);
    assert!(before(DateTime::now(), AGE) > seen(&doc), "never early");
}

/// The first events are the devices silent already; a newer one ages in
/// after them, each at its own moment.
#[test]
fn the_silent_ones_come_first_then_each_at_its_moment() {
    let (_dir, db) = open(OpenOptions::default());
    let devices = db.collection::<Document>("devices");
    devices.ensure_index("last_seen").unwrap();
    let now = DateTime::now();
    devices
        .insert(device("old", before(now, AGE * 10)))
        .unwrap();
    let first = devices.insert(device("first", now)).unwrap();
    let live = devices
        .watch(Filter::new().older_than("last_seen", AGE))
        .unwrap();
    let Live::Entered { doc, .. } = next(&live) else {
        panic!("not entered");
    };
    assert_eq!(name(&doc), "old");
    let second = devices.insert(device("second", DateTime::now())).unwrap();
    let mut order = Vec::new();
    for _ in 0..2 {
        let Live::Entered { id, doc, .. } = next(&live) else {
            panic!("not entered");
        };
        assert!(before(DateTime::now(), AGE) > seen(&doc), "never early");
        order.push(id);
    }
    assert_eq!(order, [first, second]);
}

/// The other side: `newer_than` loses each document as it ages out.
#[test]
fn newer_than_loses_them_as_they_age() {
    let (_dir, db) = open(OpenOptions::default());
    let devices = db.collection::<Document>("devices");
    devices.ensure_index("last_seen").unwrap();
    let van = devices.insert(device("van", DateTime::now())).unwrap();
    let live = devices
        .watch(Filter::new().newer_than("last_seen", AGE))
        .unwrap();
    assert!(matches!(next(&live), Live::Entered { id, .. } if id == van));
    let Live::Left { id, doc, .. } = next(&live) else {
        panic!("not left");
    };
    assert_eq!(id, van);
    assert!(before(DateTime::now(), AGE) > seen(doc.as_ref().unwrap()));
}

/// Without an index, it is read again every `clock_every`: late by up to
/// that, never early.
#[test]
fn without_an_index_every_clock_every() {
    let every = Duration::from_millis(50);
    let (_dir, db) = open(OpenOptions::default().clock_every(every));
    let devices = db.collection::<Document>("devices");
    let van = devices.insert(device("van", DateTime::now())).unwrap();
    let live = devices
        .watch(Filter::new().older_than("last_seen", AGE))
        .unwrap();
    let Live::Entered { id, doc, .. } = next(&live) else {
        panic!("not entered");
    };
    assert_eq!(id, van);
    assert!(before(DateTime::now(), AGE) > seen(&doc));
}

/// Inside `has`, whose path starts at an element, no seek finds the
/// moment: read again every `clock_every`, as without an index.
#[test]
fn inside_has_every_clock_every() {
    let (_dir, db) = open(OpenOptions::default().clock_every(Duration::from_millis(50)));
    let devices = db.collection::<Document>("devices");
    let pinged = DateTime::now();
    let doc: Document = format!(r#"{{"pings": [{{"at": {{"$date": "{pinged}"}}}}]}}"#)
        .parse()
        .unwrap();
    let id = devices.insert(doc).unwrap();
    let live = devices
        .watch("pings has (at >= now - 300ms)".parse().unwrap())
        .unwrap();
    assert!(matches!(next(&live), Live::Entered { id: i, .. } if i == id));
    assert!(matches!(next(&live), Live::Left { id: i, .. } if i == id));
    assert!(before(DateTime::now(), AGE) > pinged);
}

/// A sorted list with a limit: the newest silent one at its head. The
/// clock moves a newer one in, which pushes the older out, at the places
/// a commit's events would name.
#[test]
fn a_sorted_list_moves_by_the_clock() {
    let (_dir, db) = open(OpenOptions::default());
    let devices = db.collection::<Document>("devices");
    devices.ensure_index("last_seen").unwrap();
    let old = devices
        .insert(device("old", before(DateTime::now(), AGE * 10)))
        .unwrap();
    let new = devices.insert(device("new", DateTime::now())).unwrap();
    let filter = Filter::new()
        .older_than("last_seen", AGE)
        .sort_desc("last_seen")
        .limit(1);
    let live = devices.watch(filter).unwrap();
    assert!(matches!(next(&live), Live::Entered { id, at: Some(0), .. } if id == old));
    assert!(matches!(next(&live), Live::Left { id, from: Some(0), .. } if id == old));
    assert!(matches!(next(&live), Live::Entered { id, at: Some(0), .. } if id == new));
}

/// Time moves a document in or out and changes none: no `Changed` from
/// the clock. A `select` cuts what it reports, as a commit's.
#[test]
fn the_clock_reports_as_a_commit_does() {
    let (_dir, db) = open(OpenOptions::default());
    let devices = db.collection::<Document>("devices");
    devices.ensure_index("last_seen").unwrap();
    devices.insert(device("van", DateTime::now())).unwrap();
    let live = devices
        .watch(Filter::new().older_than("last_seen", AGE).select(["name"]))
        .unwrap();
    let Live::Entered { doc, .. } = next(&live) else {
        panic!("not entered");
    };
    assert_eq!(name(&doc), "van");
    assert_eq!(doc.get("last_seen"), None);
    assert!(live.recv_timeout(AGE * 2).is_none(), "nothing more");
}

/// On the readings alone: a live aggregate of the last moments of each
/// device's readings. A device that stops reporting ages out, its row
/// `Left` with none; a count falls as readings age, `Changed`.
#[test]
fn a_live_aggregate_ages_out() {
    let (_dir, db) = open(OpenOptions::default());
    let readings = db.collection::<Document>("readings");
    readings.ensure_index("last_seen").unwrap();
    let reading = |name: &str| {
        let mut doc = device(name, DateTime::now());
        if let Document::Object(map) = &mut doc {
            map.insert("device".into(), name.into());
        }
        readings.insert(doc).unwrap();
    };
    reading("van");
    let per_device = Aggregate::new().group_by("device").count().named("n");
    let live = readings
        .watch_aggregate(Filter::new().newer_than("last_seen", AGE), &per_device)
        .unwrap();
    assert!(matches!(next_row(&live), LiveRow::Entered { .. }));
    // Apart enough that the two age out at moments of their own.
    std::thread::sleep(AGE / 3);
    reading("van");
    let LiveRow::Changed { row, .. } = next_row(&live) else {
        panic!("not changed");
    };
    assert_eq!(row.get("n"), Some(&Document::Int(2)));
    // The first reading ages out first: the count falls, by the clock.
    let LiveRow::Changed { row, .. } = next_row(&live) else {
        panic!("not changed");
    };
    assert_eq!(row.get("n"), Some(&Document::Int(1)));
    let LiveRow::Left { group, row, .. } = next_row(&live) else {
        panic!("not left");
    };
    assert_eq!(group.get("device"), Some(&Document::from("van")));
    assert_eq!(row, None);
}

/// A column's condition against the clock, and `having`'s, which no
/// seek serves: read again every `clock_every`.
#[test]
fn a_column_and_having_against_the_clock() {
    let (_dir, db) = open(OpenOptions::default().clock_every(Duration::from_millis(50)));
    let devices = db.collection::<Document>("devices");
    devices.insert(device("van", DateTime::now())).unwrap();
    let silent = Aggregate::new()
        .count()
        .filter(trunkdb::query::Condition::older_than("last_seen", AGE))
        .named("silent");
    let live = devices.watch_aggregate(Filter::new(), &silent).unwrap();
    let LiveRow::Entered { row, .. } = next_row(&live) else {
        panic!("not entered");
    };
    assert_eq!(row.get("silent"), Some(&Document::Int(0)));
    let LiveRow::Changed { row, .. } = next_row(&live) else {
        panic!("not changed");
    };
    assert_eq!(row.get("silent"), Some(&Document::Int(1)));
}

/// `having`'s comparison is on rows, which no seek finds: read again
/// every `clock_every`.
#[test]
fn having_against_the_clock() {
    let (_dir, db) = open(OpenOptions::default().clock_every(Duration::from_millis(50)));
    let devices = db.collection::<Document>("devices");
    devices.ensure_index("last_seen").unwrap();
    let last = Aggregate::new()
        .group_by("name")
        .max("last_seen")
        .named("last")
        .having(trunkdb::query::Condition::older_than("last", AGE));
    let live = devices.watch_aggregate(Filter::new(), &last).unwrap();
    let seen = DateTime::now();
    devices.insert(device("van", seen)).unwrap();
    let LiveRow::Entered { row, .. } = next_row(&live) else {
        panic!("not entered");
    };
    assert_eq!(row.get("name"), Some(&Document::from("van")));
    assert!(before(DateTime::now(), AGE) > seen, "never early");
}

/// A sorted list that only loses: the newest two of the last moments,
/// each leaving as it ages, nothing coming in.
#[test]
fn a_sorted_list_only_loses() {
    let (_dir, db) = open(OpenOptions::default());
    let devices = db.collection::<Document>("devices");
    devices.ensure_index("last_seen").unwrap();
    let van = devices.insert(device("van", DateTime::now())).unwrap();
    let filter = Filter::new()
        .newer_than("last_seen", AGE)
        .sort_desc("last_seen")
        .limit(2);
    let live = devices.watch(filter).unwrap();
    assert!(matches!(next(&live), Live::Entered { id, at: Some(0), .. } if id == van));
    let Live::Left { id, doc, from, .. } = next(&live) else {
        panic!("not left");
    };
    assert_eq!((id, from), (van, Some(0)));
    assert_eq!(name(&doc.unwrap()), "van");
}

/// A write in between doesn't confuse it: the commit checks the written
/// document against the time it reads, and the clock the rest.
#[test]
fn commits_and_the_clock_take_turns() {
    let (_dir, db) = open(OpenOptions::default());
    let devices = db.collection::<Document>("devices");
    devices.ensure_index("last_seen").unwrap();
    let van = devices.insert(device("van", DateTime::now())).unwrap();
    let live = devices
        .watch(Filter::new().older_than("last_seen", AGE))
        .unwrap();
    // Written silent: entered by the commit.
    let old = devices
        .insert(device("old", before(DateTime::now(), AGE * 10)))
        .unwrap();
    assert!(matches!(next(&live), Live::Entered { id, .. } if id == old));
    // An unrelated write while the van ages: nothing for it.
    let elsewhere = db.collection::<Document>("elsewhere");
    elsewhere.insert(r#"{"x": 1}"#.parse().unwrap()).unwrap();
    assert!(matches!(next(&live), Live::Entered { id, .. } if id == van));
    devices.delete(&old).unwrap();
    assert!(matches!(next(&live), Live::Left { id, doc: None, .. } if id == old));
    assert!(live.recv_timeout(AGE).is_none(), "nothing more");
}

/// A read-only open has nothing yet that reads a live query again as time
/// passes (§202's step 3): refused there, with `NotLive`.
#[test]
fn a_read_only_open_refuses_it_for_now() {
    let (dir, db) = open(OpenOptions::default());
    let devices = db.collection::<Document>("devices");
    devices.insert(device("van", DateTime::now())).unwrap();
    let reader = Database::open_with(
        dir.path().join("clock.trunkdb"),
        OpenOptions::default().read_only(true),
    )
    .unwrap();
    let devices = reader.collection::<Document>("devices");
    let silent = Filter::new().older_than("last_seen", AGE);
    assert!(matches!(
        devices.watch(silent.clone()),
        Err(Error::NotLive { .. })
    ));
    let n = Aggregate::new().count().named("n");
    assert!(matches!(
        devices.watch_aggregate(silent, &n),
        Err(Error::NotLive { .. })
    ));
    // Without the clock, as before.
    assert!(devices.watch(Filter::new()).is_ok());
    drop(db);
}

/// `=` against `now` is refused as by any read, before anything is kept.
#[test]
fn equal_to_now_is_refused() {
    let (_dir, db) = open(OpenOptions::default());
    let devices: Collection<Document> = db.collection("devices");
    let filter = Filter::new().and(trunkdb::query::Condition::against_now(
        "last_seen",
        trunkdb::query::Op::Eq,
        trunkdb::query::Offset::NONE,
    ));
    let Err(Error::Io(e)) = devices.watch(filter) else {
        panic!("not refused");
    };
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
}
