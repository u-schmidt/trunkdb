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

/// Without an index, writes to the collection more often than
/// `clock_every` don't put the reread off: a silent device is reported
/// while another reports every 20 ms.
#[test]
fn writes_more_often_than_clock_every_dont_put_it_off() {
    let (_dir, db) = open(OpenOptions::default().clock_every(Duration::from_millis(200)));
    let devices = db.collection::<Document>("devices");
    let van = devices.insert(device("van", DateTime::now())).unwrap();
    let car = devices.insert(device("car", DateTime::now())).unwrap();
    let live = devices
        .watch(Filter::new().older_than("last_seen", AGE))
        .unwrap();
    let started = std::time::Instant::now();
    let entered = loop {
        assert!(
            started.elapsed() < PATIENCE,
            "never reported while written to"
        );
        devices
            .update(&car, device("car", DateTime::now()))
            .unwrap();
        if let Some(event) = live.try_recv() {
            break event.unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let Live::Entered { id, doc, .. } = entered else {
        panic!("not entered: {entered:?}");
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
    // Ages a second, the readings half of it apart: they age out at
    // moments of their own unless the clock thread is held up half a
    // second, when one reread would see both gone at once.
    let age = Duration::from_secs(1);
    reading("van");
    let per_device = Aggregate::new().group_by("device").count().named("n");
    let live = readings
        .watch_aggregate(Filter::new().newer_than("last_seen", age), &per_device)
        .unwrap();
    assert!(matches!(next_row(&live), LiveRow::Entered { .. }));
    std::thread::sleep(age / 2);
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

/// A read-only open beside the writer `_writer` holds, at `dir`,
/// looking every 5 ms, `clock_every` as `options` says.
fn beside(dir: &tempfile::TempDir, options: OpenOptions) -> Database {
    let options = options
        .read_only(true)
        .watch_every(Duration::from_millis(5));
    Database::open_with(dir.path().join("clock.trunkdb"), options).unwrap()
}

/// Beside a writer (SPEC §205), the look reads a live query against the
/// clock again at its moment, with nothing committed: a device goes
/// silent, through an index, `Left` when the writer has it report again.
#[test]
fn beside_a_writer_a_device_goes_silent() {
    let (dir, db) = open(OpenOptions::default());
    let devices = db.collection::<Document>("devices");
    devices.ensure_index("last_seen").unwrap();
    let van = devices.insert(device("van", DateTime::now())).unwrap();
    let reader = beside(
        &dir,
        OpenOptions::default().clock_every(Duration::from_secs(3600)),
    );
    let live = (reader.collection::<Document>("devices"))
        .watch(Filter::new().older_than("last_seen", AGE))
        .unwrap();
    assert!(live.try_recv().is_none(), "seen just now: not silent");
    let Live::Entered { id, doc, .. } = next(&live) else {
        panic!("not entered");
    };
    assert_eq!(id, van);
    assert!(before(DateTime::now(), AGE) > seen(&doc));
    let again = DateTime::now();
    devices.update(&van, device("van", again)).unwrap();
    assert!(matches!(next(&live), Live::Left { id, .. } if id == van));
    let Live::Entered { doc, .. } = next(&live) else {
        panic!("not silent again");
    };
    assert_eq!(seen(&doc), again, "as the writer left it");
    assert!(before(DateTime::now(), AGE) > again);
}

/// Beside a writer, without an index: every `clock_every`, as in the
/// writer's process.
#[test]
fn beside_a_writer_without_an_index() {
    let (dir, db) = open(OpenOptions::default());
    let devices = db.collection::<Document>("devices");
    let van = devices.insert(device("van", DateTime::now())).unwrap();
    let reader = beside(
        &dir,
        OpenOptions::default().clock_every(Duration::from_millis(50)),
    );
    let live = (reader.collection::<Document>("devices"))
        .watch(Filter::new().older_than("last_seen", AGE))
        .unwrap();
    let Live::Entered { id, doc, .. } = next(&live) else {
        panic!("not entered");
    };
    assert_eq!(id, van);
    assert!(before(DateTime::now(), AGE) > seen(&doc));
}

/// Beside a writer, a live aggregate against the clock runs again at
/// its moment: a device's count falls as its readings age, then its row
/// leaves.
#[test]
fn beside_a_writer_a_live_aggregate_ages_out() {
    let (dir, db) = open(OpenOptions::default());
    let readings = db.collection::<Document>("readings");
    readings.ensure_index("last_seen").unwrap();
    readings
        .insert(device("van", before(DateTime::now(), AGE / 2)))
        .unwrap();
    readings.insert(device("van", DateTime::now())).unwrap();
    let reader = beside(&dir, OpenOptions::default());
    let per_device = Aggregate::new().group_by("name").count().named("n");
    let live = (reader.collection::<Document>("readings"))
        .watch_aggregate(Filter::new().newer_than("last_seen", AGE), &per_device)
        .unwrap();
    let count = |row: &LiveRow| match row {
        LiveRow::Entered { row, .. } | LiveRow::Changed { row, .. } => row.get("n").cloned(),
        _ => None,
    };
    assert_eq!(count(&next_row(&live)), Some(Document::Int(2)));
    assert_eq!(count(&next_row(&live)), Some(Document::Int(1)));
    assert!(matches!(next_row(&live), LiveRow::Left { row: None, .. }));
}

/// Beside a writer, a look reads nothing where no moment has come for
/// it and nothing was committed: before the device goes silent, and
/// after, with nothing ahead, while another live query, without an
/// index, is read again every look. Conditions of the test's own count
/// the documents a live query and a live aggregate are given.
#[test]
fn beside_a_writer_a_look_before_the_moment_reads_nothing() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let age = Duration::from_secs(1);
    let (dir, db) = open(OpenOptions::default());
    let devices = db.collection::<Document>("devices");
    devices.ensure_index("last_seen").unwrap();
    devices.insert(device("van", DateTime::now())).unwrap();
    let reader = beside(
        &dir,
        OpenOptions::default().clock_every(Duration::from_millis(5)),
    );
    let read = Arc::new(AtomicUsize::new(0));
    let counting = |read: &Arc<AtomicUsize>| {
        let counted = read.clone();
        Filter::new()
            .older_than("last_seen", age)
            .and(trunkdb::query::Condition::custom(["name"], move |_| {
                counted.fetch_add(1, Ordering::Relaxed);
                true
            }))
    };
    let devices = reader.collection::<Document>("devices");
    let live = devices.watch(counting(&read)).unwrap();
    let silent = Aggregate::new().count().named("n");
    let counted = devices.watch_aggregate(counting(&read), &silent).unwrap();
    let LiveRow::Entered { .. } = next_row(&counted) else {
        panic!("no first row");
    };
    let _often = (reader.collection::<Document>("others"))
        .watch(Filter::new().older_than("at", age))
        .unwrap();
    let steady = |what: &str| {
        std::thread::sleep(Duration::from_millis(50));
        let first = read.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(read.load(Ordering::Relaxed), first, "twenty looks {what}");
    };
    steady("before the moment");
    assert!(matches!(next(&live), Live::Entered { .. }));
    let LiveRow::Changed { row, .. } = next_row(&counted) else {
        panic!("the count not changed");
    };
    assert_eq!(row.get("n"), Some(&Document::Int(1)));
    steady("with nothing ahead");
    drop(db);
}

/// Beside a writer, a live query against the clock started after the
/// look has read the writer's last commit: its moment is found when it
/// starts, as no commit comes to find it.
#[test]
fn beside_a_writer_a_live_query_started_later() {
    let (dir, db) = open(OpenOptions::default());
    let devices = db.collection::<Document>("devices");
    devices.ensure_index("last_seen").unwrap();
    let van = devices.insert(device("van", DateTime::now())).unwrap();
    let reader = beside(
        &dir,
        OpenOptions::default().clock_every(Duration::from_secs(3600)),
    );
    let devices = reader.collection::<Document>("devices");
    let all = devices.watch(Filter::new()).unwrap();
    assert!(matches!(next(&all), Live::Entered { .. }));
    std::thread::sleep(Duration::from_millis(50));
    let live = devices
        .watch(Filter::new().older_than("last_seen", AGE))
        .unwrap();
    let Live::Entered { id, .. } = next(&live) else {
        panic!("not entered");
    };
    assert_eq!(id, van);
    drop(db);
}

/// A read-only open in a directory it may not write, where no writer has
/// made the lock files, is alone (SPEC §185.6, §205): nothing commits and
/// nothing looks, and `trunkdb-clock` alone reads its live queries and
/// aggregates again.
#[cfg(unix)]
#[test]
fn a_read_only_open_alone_sees_a_device_go_silent() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, db) = open(OpenOptions::default());
    let devices = db.collection::<Document>("devices");
    devices.ensure_index("last_seen").unwrap();
    let van = devices.insert(device("van", DateTime::now())).unwrap();
    drop(devices);
    drop(db);
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name != "clock.trunkdb" && name != "clock.trunkdb.wal" {
            std::fs::remove_file(path).unwrap();
        }
    }
    let set =
        |mode| std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
    set(0o555);
    let root = std::fs::File::create(dir.path().join("probe")).is_ok();
    let reader = Database::open_with(
        dir.path().join("clock.trunkdb"),
        OpenOptions::default().read_only(true),
    );
    set(0o755);
    if root {
        return;
    }
    let reader = reader.unwrap();
    assert!(
        std::fs::read_dir(dir.path()).unwrap().count() <= 2,
        "no lock files: alone"
    );
    expect_silent(&reader, van);
}

/// A read-only open after the writer closed, its lock files there: the
/// look keeps time, as beside a writer.
#[test]
fn a_read_only_open_after_the_writer_closed() {
    let (dir, db) = open(OpenOptions::default());
    let devices = db.collection::<Document>("devices");
    devices.ensure_index("last_seen").unwrap();
    let van = devices.insert(device("van", DateTime::now())).unwrap();
    drop(devices);
    drop(db);
    let reader = Database::open_with(
        dir.path().join("clock.trunkdb"),
        OpenOptions::default().read_only(true),
    )
    .unwrap();
    expect_silent(&reader, van);
}

/// `van`, seen just now, goes silent in `reader`'s live query, and
/// leaves its live aggregate's count.
fn expect_silent(reader: &Database, van: trunkdb::DocId) {
    let devices = reader.collection::<Document>("devices");
    let live = devices
        .watch(Filter::new().older_than("last_seen", AGE))
        .unwrap();
    let n = Aggregate::new().count().named("n");
    let counted = devices
        .watch_aggregate(Filter::new().newer_than("last_seen", AGE), &n)
        .unwrap();
    assert!(live.try_recv().is_none(), "seen just now: not silent");
    let Live::Entered { id, doc, .. } = next(&live) else {
        panic!("not entered");
    };
    assert_eq!(id, van);
    assert!(before(DateTime::now(), AGE) > seen(&doc));
    let LiveRow::Entered { row, .. } = next_row(&counted) else {
        panic!("no first row");
    };
    assert_eq!(row.get("n"), Some(&Document::Int(1)));
    let LiveRow::Changed { row, .. } = next_row(&counted) else {
        panic!("the count not changed");
    };
    assert_eq!(row.get("n"), Some(&Document::Int(0)));
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

/// A condition of the test's own, true for every document, that runs
/// `inside` in a turn of `trunkdb-clock` only (SPEC §207).
fn in_a_tick(inside: impl Fn() + Send + Sync + 'static) -> trunkdb::query::Condition {
    trunkdb::query::Condition::custom(["name"], move |_| {
        if std::thread::current().name() == Some("trunkdb-clock") {
            inside();
        }
        true
    })
}

/// The last handle dropped while `trunkdb-clock` reads a live query
/// again: the drop waits for the turn, so the file is closed when it
/// returns, and opens again at once (SPEC §207). Found by the fuzzer's
/// reopen, which a turn under way had refused as open for writing.
#[test]
fn the_last_handle_closes_the_file_in_the_middle_of_a_tick() {
    let (dir, db) = open(OpenOptions::default().clock_every(Duration::from_millis(5)));
    let devices = db.collection::<Document>("devices");
    devices.insert(device("van", DateTime::now())).unwrap();
    let (inside, ticking) = std::sync::mpsc::sync_channel(1);
    let inside = std::sync::Mutex::new(inside);
    let filter = Filter::new()
        .newer_than("last_seen", Duration::from_secs(3600))
        .and(in_a_tick(move || {
            let _ = inside.lock().unwrap().try_send(());
            std::thread::sleep(Duration::from_millis(300));
        }));
    let live = devices.watch(filter).unwrap();
    ticking.recv_timeout(PATIENCE).expect("a tick");
    drop(devices);
    drop(db);
    Database::open(dir.path().join("clock.trunkdb")).unwrap();
    drop(live);
}

/// A condition that drops the application's last handle, in a turn of
/// `trunkdb-clock`: the drop doesn't wait for its own turn, and the file
/// is closed when the turn ends (SPEC §207).
#[test]
fn the_last_handle_dropped_in_a_tick_closes_the_file_after_it() {
    use std::sync::{Arc, Mutex};
    let (dir, db) = open(OpenOptions::default().clock_every(Duration::from_millis(5)));
    let devices = db.collection::<Document>("devices");
    devices.insert(device("van", DateTime::now())).unwrap();
    let held: Arc<Mutex<Option<Database>>> = Arc::default();
    let (gone, dropped) = std::sync::mpsc::sync_channel(1);
    let gone = Mutex::new(gone);
    let holding = held.clone();
    let filter = Filter::new()
        .newer_than("last_seen", Duration::from_secs(3600))
        .and(in_a_tick(move || {
            if let Some(last) = holding.lock().unwrap().take() {
                drop(last);
                let _ = gone.lock().unwrap().try_send(());
            }
        }));
    let live = devices.watch(filter).unwrap();
    drop(devices);
    *held.lock().unwrap() = Some(db);
    dropped.recv_timeout(PATIENCE).expect("dropped in a tick");
    let path = dir.path().join("clock.trunkdb");
    let started = std::time::Instant::now();
    while let Err(e) = Database::open(&path) {
        assert!(started.elapsed() < PATIENCE, "never closed: {e:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(live);
}
