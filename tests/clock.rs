//! The clock in a filter (SPEC §203): `now`, and `now` moved by a
//! duration, in a comparison; read once per read, then compared as any
//! date-time. Times here are minutes apart from the edges, so a test
//! that runs for seconds gives the same answer.

use std::time::Duration;
use trunkdb::query::{Aggregate, Condition, Filter, Offset, Op};
use trunkdb::{Collection, Database, DateTime, Document, Error};

const MINUTE: Duration = Duration::from_secs(60);

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("clock.trunkdb")).unwrap();
    (dir, db)
}

/// `minutes` from now: before it where negative.
fn ago(minutes: i64) -> DateTime {
    let now = DateTime::now();
    DateTime::from_unix(now.unix_seconds() - minutes * 60, now.subsec_nanos()).unwrap()
}

/// A device's document, `seen` minutes ago.
fn device(name: &str, seen: i64) -> Document {
    let mut doc: Document = format!(r#"{{"name": "{name}"}}"#).parse().unwrap();
    if let Document::Object(map) = &mut doc {
        map.insert("last_seen".into(), Document::DateTime(ago(seen)));
    }
    doc
}

/// Four devices, seen an hour, 20 minutes and a minute ago, and one an
/// hour from now (a clock ahead); and one without a time, and one whose
/// time is a number.
fn devices(db: &Database) -> Collection<Document> {
    let devices = db.collection::<Document>("devices");
    for (name, seen) in [("hour", 60), ("twenty", 20), ("minute", 1), ("ahead", -60)] {
        devices.insert(device(name, seen)).unwrap();
    }
    devices
        .insert(r#"{"name": "none"}"#.parse().unwrap())
        .unwrap();
    let number = r#"{"name": "number", "last_seen": 1700000000}"#;
    devices.insert(number.parse().unwrap()).unwrap();
    devices
}

fn names(docs: Vec<Document>) -> Vec<String> {
    let mut names: Vec<String> = (docs.iter())
        .map(|d| {
            d.get("name")
                .and_then(Document::as_str)
                .unwrap()
                .to_string()
        })
        .collect();
    names.sort();
    names
}

fn found(devices: &Collection<Document>, filter: Filter) -> Vec<String> {
    names(devices.find(filter).unwrap())
}

#[test]
fn older_and_newer_than_split_the_date_times() {
    let (_dir, db) = open();
    let devices = devices(&db);
    let ten = 10 * MINUTE;
    let older = Filter::new().older_than("last_seen", ten);
    let newer = Filter::new().newer_than("last_seen", ten);
    assert_eq!(found(&devices, older.clone()), ["hour", "twenty"]);
    assert_eq!(found(&devices, newer.clone()), ["ahead", "minute"]);
    // A missing time, and one that isn't a date-time, in neither.
    assert_eq!(devices.count(older).unwrap(), 2);
    assert_eq!(devices.count(newer).unwrap(), 2);
    // `now` alone, and after it.
    let past = Filter::new().and(Condition::against_now("last_seen", Op::Lt, Offset::NONE));
    assert_eq!(found(&devices, past), ["hour", "minute", "twenty"]);
    let hour = Offset::after(60 * MINUTE + MINUTE);
    let within = Filter::new().and(Condition::against_now("last_seen", Op::Lte, hour));
    assert_eq!(
        found(&devices, within),
        ["ahead", "hour", "minute", "twenty"]
    );
}

/// Every way a program reads: the answer is the same through an index,
/// and the index is read only over the range the time gives.
#[test]
fn every_read_takes_it_and_an_index_bounds_it() {
    let (_dir, db) = open();
    let devices = devices(&db);
    let silent = || Filter::new().older_than("last_seen", 10 * MINUTE);
    let before = DateTime::now();
    let scanned = devices.explain_run(silent()).unwrap();
    assert!(
        scanned
            .now
            .is_some_and(|now| now >= before && now <= DateTime::now())
    );
    assert_eq!(scanned.stats.documents_returned, 2);

    devices.ensure_index("last_seen").unwrap();
    assert_eq!(found(&devices, silent()), ["hour", "twenty"]);
    let explained = devices.explain_run(silent()).unwrap();
    assert_eq!(explained.reads.len(), 1, "{explained}");
    // The two before the edge, and none after it.
    assert_eq!(explained.stats.keys_examined, 2, "{explained}");
    assert!(explained.to_string().contains("now: "), "{explained}");
    assert!(matches!(
        devices.explain(&silent()).unwrap(),
        trunkdb::query::QueryPlan::Index { .. }
    ));

    assert_eq!(devices.cursor(silent()).unwrap().count(), 2);
    let newest = devices.find_one(silent().sort_desc("last_seen")).unwrap();
    assert_eq!(names(newest.into_iter().collect()), ["twenty"]);
    let snapshot = db.snapshot().unwrap();
    let view = snapshot.collection::<Document>("devices");
    assert_eq!(names(view.find(silent()).unwrap()), ["hour", "twenty"]);
    assert_eq!(view.count(silent()).unwrap(), 2);
    // Without a `now`, no time is read.
    assert_eq!(devices.explain_run(Filter::new()).unwrap().now, None);
}

#[test]
fn writes_and_batches_take_it() {
    let (_dir, db) = open();
    let devices = devices(&db);
    let silent = || Filter::new().older_than("last_seen", 10 * MINUTE);

    let mut batch = db.batch();
    batch.require(&devices, silent()).unwrap();
    batch
        .require_none(
            &devices,
            Filter::new().older_than("last_seen", 2 * 60 * MINUTE),
        )
        .unwrap();
    batch.delete_many(&devices, silent());
    let written = batch.commit().unwrap();
    assert_eq!(written.counts().last(), Some(&2));
    assert_eq!(devices.count(silent()).unwrap(), 0);

    // None silent now: the precondition fails, and nothing is written.
    let mut batch = db.batch();
    batch.require(&devices, silent()).unwrap();
    assert!(matches!(
        batch.commit(),
        Err(Error::PreconditionFailed { matched: false, .. })
    ));

    let update = trunkdb::query::Update::new().set("checked", true);
    let recent = Filter::new().newer_than("last_seen", 10 * MINUTE);
    assert_eq!(devices.update_fields(recent.clone(), &update).unwrap(), 2);
    assert_eq!(devices.count(recent.eq("checked", true)).unwrap(), 2);
    assert_eq!(
        devices
            .delete_many(Filter::new().newer_than("last_seen", 10 * MINUTE))
            .unwrap(),
        2
    );
}

#[test]
fn aggregates_take_it_in_the_filter_columns_and_having() {
    let (_dir, db) = open();
    let devices = devices(&db);
    let per_device = Aggregate::new()
        .count()
        .filter(Condition::older_than("last_seen", 10 * MINUTE))
        .named("silent")
        .count()
        .named("all");
    let rows = devices.aggregate(Filter::new(), &per_device).unwrap();
    assert_eq!(rows[0].get("silent"), Some(&Document::Int(2)));
    assert_eq!(rows[0].get("all"), Some(&Document::Int(6)));

    let last_seen = Aggregate::new()
        .group_by("name")
        .max("last_seen")
        .named("last")
        .having(Condition::older_than("last", 10 * MINUTE));
    let rows = devices
        .aggregate(Filter::new().exists("last_seen"), &last_seen)
        .unwrap();
    assert_eq!(names(rows), ["hour", "twenty"]);

    let recent = Filter::new().newer_than("last_seen", 10 * MINUTE);
    let all = Aggregate::new().count().named("n");
    let rows = devices.aggregate(recent.clone(), &all).unwrap();
    assert_eq!(rows[0].get("n"), Some(&Document::Int(2)));
    let explained = devices.explain_aggregate(recent, &all).unwrap();
    assert!(explained.now.is_some());
}

/// `now_is` says the time itself: the same answer whenever it runs.
#[test]
fn a_program_can_say_the_time() {
    let (_dir, db) = open();
    let devices = devices(&db);
    let silent = Filter::new().older_than("last_seen", 10 * MINUTE);
    // Two hours ago, nothing had been silent for ten minutes yet.
    assert_eq!(
        found(&devices, silent.clone().now_is(ago(120))),
        Vec::<String>::new()
    );
    // In a day, all four will have been.
    let names = found(&devices, silent.now_is(ago(-24 * 60)));
    assert_eq!(names, ["ahead", "hour", "minute", "twenty"]);
}

/// In memory, as `find` checks a document: the time read when it checks.
#[test]
fn checked_in_memory_too() {
    let silent = Filter::new().older_than("last_seen", 10 * MINUTE);
    assert!(silent.matches(&device("old", 30)));
    assert!(!silent.matches(&device("new", 1)));
    assert!(Condition::newer_than("last_seen", 10 * MINUTE).matches(&device("new", 1)));
    let docs = vec![device("old", 30), device("new", 1)];
    assert_eq!(names(silent.apply(docs.clone())), ["old"]);
    let n = Aggregate::new()
        .count()
        .filter(Condition::older_than("last_seen", 10 * MINUTE))
        .named("n");
    assert_eq!(n.apply(docs).unwrap()[0].get("n"), Some(&Document::Int(1)));
    // Inside `has`, and on a path with `[*]`.
    let pings = format!(r#"{{"pings": [{{"at": {{"$date": "{}"}}}}]}}"#, ago(1));
    let pings: Document = pings.parse().unwrap();
    let text = |text: &str| text.parse::<Filter>().unwrap();
    assert!(text("pings has (at >= now - 5m)").matches(&pings));
    assert!(text("pings[*].at >= now - 5m").matches(&pings));
    assert!(!text("pings[*].at < now - 5m").matches(&pings));
}

#[test]
fn the_text_reads_and_prints_it() {
    for text in [
        "last_seen < now - 10m",
        "at >= now - 5m",
        "due < now + 1h",
        "at > now",
        "at <= now - 90s",
        "a has (at >= now - 1d)",
        "not at < now - 1ms",
    ] {
        let filter: Filter = text.parse().unwrap();
        assert_eq!(filter.to_string(), text);
    }
    // Written in the longest whole unit, as a window's duration.
    let filter: Filter = "at>=now-300s".parse().unwrap();
    assert_eq!(filter.to_string(), "at >= now - 5m");
    assert_eq!(
        "at >= now - 5m".parse::<Filter>().unwrap().to_string(),
        Filter::new().newer_than("at", 5 * MINUTE).to_string()
    );
    // `now + 0s` is `now`.
    assert_eq!(
        "at < now + 0s".parse::<Filter>().unwrap().to_string(),
        "at < now"
    );
    // A field named `now` is a field.
    let filter: Filter = "now < now".parse().unwrap();
    assert_eq!(filter.to_string(), "now < now");
}

#[test]
fn what_it_refuses() {
    for (text, says) in [
        (
            "at = now",
            "`=` against `now` holds for a nanosecond at most",
        ),
        ("at != now - 1m", "`!=` against `now`"),
        ("at contains now", "`contains` against `now`"),
        ("at < now - 10", "a unit after the number"),
        ("at < now -", "a duration after `now`"),
        ("at < now - :age", "a duration after `now`"),
    ] {
        let e = text.parse::<Filter>().unwrap_err().to_string();
        assert!(e.contains(says), "{text}: {e}");
    }
    let (_dir, db) = open();
    let devices = devices(&db);
    let exactly = Filter::new().and(Condition::against_now("last_seen", Op::Eq, Offset::NONE));
    for result in [
        devices.find(exactly.clone()).map(drop),
        devices.count(exactly.clone()).map(drop),
        devices.explain_run(exactly.clone()).map(drop),
        devices.delete_many(exactly.clone()).map(drop),
    ] {
        match result {
            Err(Error::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
            other => panic!("{other:?}"),
        }
    }
    // A batch's precondition, refused as the batch runs it.
    let mut batch = db.batch();
    batch.require(&devices, exactly.clone()).unwrap();
    assert!(matches!(
        batch.commit(),
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::InvalidInput
    ));
    let column = Aggregate::new()
        .count()
        .filter(Condition::against_now("last_seen", Op::Ne, Offset::NONE))
        .named("n");
    assert!(matches!(
        devices.aggregate(Filter::new(), &column),
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::InvalidInput
    ));
}

/// A live query or aggregate against the clock is taken (SPEC §204):
/// in the filter, and in a column's condition.
#[test]
fn a_live_query_against_now_is_taken() {
    let (_dir, db) = open();
    let devices = devices(&db);
    let silent = Filter::new().older_than("last_seen", 10 * MINUTE);
    let live = devices.watch(silent.clone()).unwrap();
    let entered: Vec<Document> = std::iter::from_fn(|| live.try_recv())
        .map(|event| match event.unwrap() {
            trunkdb::Live::Entered { doc, .. } => doc,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(names(entered), ["hour", "twenty"]);
    let n = Aggregate::new().count().named("n");
    assert!(devices.watch_aggregate(silent, &n).is_ok());
    let column = Aggregate::new()
        .count()
        .filter(Condition::older_than("last_seen", 10 * MINUTE))
        .named("n");
    assert!(devices.watch_aggregate(Filter::new(), &column).is_ok());
}

#[test]
fn offsets() {
    assert_eq!(Offset::after(Duration::ZERO), Offset::NONE);
    assert_eq!(Offset::before(Duration::ZERO), Offset::NONE);
    let noon: DateTime = "2026-11-16T12:00:00Z".parse().unwrap();
    assert_eq!(
        Offset::before(10 * MINUTE).from(noon).to_string(),
        "2026-11-16T11:50:00Z"
    );
    assert_eq!(
        Offset::after(60 * MINUTE).from(noon).to_string(),
        "2026-11-16T13:00:00Z"
    );
    assert!(Offset::after(MINUTE).is_after());
    assert_eq!(Offset::before(MINUTE).duration(), MINUTE);
}

/// On the edge: `older_than` is `<`, `newer_than` is `>=`, so the instant
/// exactly `age` before now is newer, to the nanosecond.
#[test]
fn the_edge_is_newer() {
    let noon: DateTime = "2026-11-16T12:00:00Z".parse().unwrap();
    let at = |text: &str| -> Document {
        format!(r#"{{"at": {{"$date": "{text}"}}}}"#)
            .parse()
            .unwrap()
    };
    let older = Filter::new().older_than("at", 10 * MINUTE).now_is(noon);
    let newer = Filter::new().newer_than("at", 10 * MINUTE).now_is(noon);
    for (text, is_older) in [
        ("2026-11-16T11:49:59.999999999Z", true),
        ("2026-11-16T11:50:00Z", false),
        ("2026-11-16T11:50:00.000000001Z", false),
    ] {
        assert_eq!(older.matches(&at(text)), is_older, "{text}");
        assert_eq!(newer.matches(&at(text)), !is_older, "{text}");
    }
}

/// A comparison with the clock anywhere in a filter counts: inside
/// `has`, a NOT, an OR. A live query takes each.
#[test]
fn nested_comparisons_with_the_clock_count() {
    let (_dir, db) = open();
    let devices = devices(&db);
    for text in [
        "pings has (at >= now - 5m)",
        "not last_seen < now - 1m",
        "name = \"x\" or last_seen < now",
    ] {
        let filter: Filter = text.parse().unwrap();
        assert!(devices.watch(filter.clone()).is_ok(), "{text}");
        // And a read takes the time: `explain_run` says it.
        assert!(devices.explain_run(filter).unwrap().now.is_some(), "{text}");
    }
}

/// One time for a whole check in memory: `Aggregate::apply` makes its
/// columns' matchers before it reads the documents and `having`'s after.
/// A document a second ahead, read by an iterator slower than that, is
/// counted as at or after now by a column; with the time read again for
/// `having`, the same time would also be before now, and the row would
/// come out. A second, so a machine that stalls between making the
/// document and reading the clock doesn't age it first.
#[test]
fn a_slow_check_in_memory_compares_with_one_time() {
    // A document a second ahead of now, made fresh for each call.
    let ahead = || -> Document {
        let now = DateTime::now();
        let nanos = i128::from(now.unix_seconds()) * 1_000_000_000
            + i128::from(now.subsec_nanos())
            + 1_000_000_000;
        let secs = nanos.div_euclid(1_000_000_000) as i64;
        let at = DateTime::from_unix(secs, nanos.rem_euclid(1_000_000_000) as u32).unwrap();
        format!(r#"{{"at": {{"$date": "{at}"}}}}"#).parse().unwrap()
    };
    let slowly = |doc: Document| {
        std::iter::once(doc).inspect(|_| std::thread::sleep(Duration::from_millis(1500)))
    };
    let aggregate = Aggregate::new()
        .count()
        .filter(Condition::against_now("at", Op::Gte, Offset::NONE))
        .named("n")
        .max("at")
        .named("last");
    let rows = aggregate.apply(slowly(ahead())).unwrap();
    assert_eq!(rows[0].get("n"), Some(&Document::Int(1)));
    // The one row counts it as not yet past, so its `last` isn't either.
    let contradiction = aggregate.having(Condition::against_now("last", Op::Lt, Offset::NONE));
    assert_eq!(
        contradiction.apply(slowly(ahead())).unwrap(),
        Vec::<Document>::new()
    );
}
