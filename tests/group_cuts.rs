//! Groups by a date-time cut to a unit (SPEC §209, §210):
//! `Aggregate::group_by_trunc` and `group_by_date`, `trunc(path, unit)`
//! and `date(path)` in the text; one-off, through a snapshot, in memory,
//! beside an index and live.

use std::time::Duration;
use trunkdb::query::{Aggregate, Filter, Trunc};
use trunkdb::{Collection, Database, Date, DateTime, Document, LiveRow};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("group_cuts.trunkdb")).unwrap();
    (dir, db)
}

fn readings(db: &Database) -> Collection<Document> {
    db.collection::<Document>("readings")
}

fn at(text: &str) -> DateTime {
    text.parse().unwrap()
}

/// A reading of `server` with `value` at `at`.
fn reading(server: &str, at: Document) -> Document {
    let mut doc: Document = format!(r#"{{"server": "{server}"}}"#).parse().unwrap();
    if let Document::Object(map) = &mut doc {
        map.insert("at".into(), at);
    }
    doc
}

fn insert(readings: &Collection<Document>, all: &[(&str, &str)]) {
    for &(server, time) in all {
        readings
            .insert(reading(server, Document::DateTime(at(time))))
            .unwrap();
    }
}

fn minute() -> Trunc {
    Trunc::every(Duration::from_secs(60))
}

/// Every row as `(the group's value at "at" as text, n)`.
fn short(rows: &[Document]) -> Vec<(String, i64)> {
    (rows.iter())
        .map(|row| {
            let value = row.get("at").cloned().unwrap_or(Document::Null);
            let n = row.get("n").and_then(Document::as_i64).unwrap();
            (value.to_string(), n)
        })
        .collect()
}

/// The same rows from every way of running `aggregate`: the database,
/// a snapshot, `Aggregate::apply` over what the filter finds, and the
/// aggregate's text read back.
fn every_way(db: &Database, aggregate: &Aggregate) -> Vec<Document> {
    let readings = readings(db);
    let stored = readings.aggregate(Filter::new(), aggregate).unwrap();
    let snapshot = db.snapshot().unwrap();
    let viewed = (snapshot.collection::<Document>("readings"))
        .aggregate(Filter::new(), aggregate)
        .unwrap();
    let in_memory = aggregate
        .apply(readings.find(Filter::new()).unwrap())
        .unwrap();
    let text: Aggregate = aggregate.to_string().parse().unwrap();
    let from_text = readings.aggregate(Filter::new(), &text).unwrap();
    assert_eq!(stored, viewed);
    assert_eq!(stored, in_memory);
    assert_eq!(stored, from_text);
    stored
}

const MINUTES: [(&str, &str); 6] = [
    ("a", "2026-11-23T14:37:52.123Z"),
    ("b", "2026-11-23T14:37:00Z"),
    ("a", "2026-11-23T14:37:59.999999999Z"),
    ("a", "2026-11-23T14:38:00Z"),
    ("b", "2026-11-23T14:39:30.5Z"),
    ("a", "2026-11-24T00:00:01Z"),
];

#[test]
fn readings_are_counted_per_minute() {
    let (_dir, db) = open();
    insert(&readings(&db), &MINUTES);
    let per_minute = Aggregate::new()
        .group_by_trunc("at", minute())
        .count()
        .named("n");
    assert_eq!(
        per_minute.to_string(),
        "count() as n group by trunc(at, 1m)"
    );
    let rows = every_way(&db, &per_minute);
    assert_eq!(
        short(&rows),
        [
            (r#"{"$date":"2026-11-23T14:37:00Z"}"#.to_string(), 3),
            (r#"{"$date":"2026-11-23T14:38:00Z"}"#.to_string(), 1),
            (r#"{"$date":"2026-11-23T14:39:00Z"}"#.to_string(), 1),
            (r#"{"$date":"2026-11-24T00:00:00Z"}"#.to_string(), 1),
        ]
    );
}

#[test]
fn a_cut_group_field_goes_beside_plain_ones() {
    let (_dir, db) = open();
    insert(&readings(&db), &MINUTES);
    let aggregate: Aggregate = "count() as n group by server, trunc(at, 1h)"
        .parse()
        .unwrap();
    let rows = every_way(&db, &aggregate);
    let shown: Vec<String> = rows.iter().map(Document::to_string).collect();
    assert_eq!(
        shown,
        [
            r#"{"server":"a","at":{"$date":"2026-11-23T14:00:00Z"},"n":3}"#,
            r#"{"server":"a","at":{"$date":"2026-11-24T00:00:00Z"},"n":1}"#,
            r#"{"server":"b","at":{"$date":"2026-11-23T14:00:00Z"},"n":2}"#,
        ]
    );
}

#[test]
fn a_date_groups_by_day_as_a_date() {
    let (_dir, db) = open();
    insert(&readings(&db), &MINUTES);
    let per_day = Aggregate::new().group_by_date("at").count().named("n");
    let rows = every_way(&db, &per_day);
    let day = |text: &str| Document::Date(text.parse::<Date>().unwrap());
    assert_eq!(rows[0].get("at"), Some(&day("2026-11-23")));
    assert_eq!(rows[1].get("at"), Some(&day("2026-11-24")));
    assert_eq!(short(&rows).iter().map(|r| r.1).collect::<Vec<_>>(), [5, 1]);
    // `1d` gives midnight as a date-time instead.
    let midnight = Aggregate::new()
        .group_by_trunc("at", Trunc::every(Duration::from_secs(86_400)))
        .count()
        .named("n");
    let rows = every_way(&db, &midnight);
    assert_eq!(
        rows[0].get("at"),
        Some(&Document::DateTime(at("2026-11-23T00:00:00Z")))
    );
}

/// A value that isn't a date-time is grouped as it is, each its own
/// group, in the order a sort uses: null and missing first.
#[test]
fn other_values_are_grouped_as_they_are() {
    let (_dir, db) = open();
    let readings = readings(&db);
    insert(&readings, &MINUTES[..2]);
    readings.insert(reading("c", Document::Int(5))).unwrap();
    readings
        .insert(reading("c", Document::from("14:37")))
        .unwrap();
    readings.insert(reading("c", Document::Null)).unwrap();
    readings
        .insert("{\"server\": \"d\"}".parse().unwrap())
        .unwrap();
    let rows = every_way(
        &db,
        &Aggregate::new()
            .group_by_trunc("at", minute())
            .count()
            .named("n"),
    );
    assert_eq!(
        short(&rows),
        [
            ("null".to_string(), 2),
            ("5".to_string(), 1),
            (r#""14:37""#.to_string(), 1),
            (r#"{"$date":"2026-11-23T14:37:00Z"}"#.to_string(), 2),
        ]
    );
}

/// An index on the cut field, or one that starts with every group
/// field, answers the groups from its entries (§212): the rows are the
/// documents', not each date-time a group of its own.
#[test]
fn an_index_on_the_field_gives_the_same_rows() {
    let (_dir, db) = open();
    let readings = readings(&db);
    readings.ensure_index(["at"]).unwrap();
    readings.ensure_index(["server", "at"]).unwrap();
    insert(&readings, &MINUTES);
    for text in [
        "count() as n group by trunc(at, 1m)",
        "count() as n, min(at) as first group by server, trunc(at, 1h)",
        "count() as n group by date(at)",
    ] {
        let aggregate: Aggregate = text.parse().unwrap();
        let rows = every_way(&db, &aggregate);
        let run = readings
            .explain_aggregate(Filter::new(), &aggregate)
            .unwrap();
        // From the index's entries since §209's step 2: no document read.
        assert_eq!(run.stats.documents_examined, 0, "{text}: {run:#?}");
        assert!(run.stats.from_entries > 0, "{text}: {run:#?}");
        assert!(rows.len() < MINUTES.len(), "{text}: {rows:?}");
    }
    let ranged = readings
        .aggregate(
            Filter::new().gte("at", at("2026-11-23T14:38:00Z")),
            &Aggregate::new()
                .group_by_trunc("at", minute())
                .count()
                .named("n"),
        )
        .unwrap();
    assert_eq!(short(&ranged).len(), 3);
}

/// Each group's own window, the groups being minutes.
#[test]
fn a_window_per_group_keeps_each_buckets_newest() {
    let (_dir, db) = open();
    insert(&readings(&db), &MINUTES);
    let aggregate = Aggregate::new()
        .newest_per_group("at", 1)
        .group_by_trunc("at", minute())
        .count()
        .named("n");
    let rows = every_way(&db, &aggregate);
    assert_eq!(
        short(&rows).iter().map(|r| r.1).collect::<Vec<_>>(),
        [1, 1, 1, 1]
    );
}

#[test]
fn a_live_aggregate_sees_a_new_minute_enter() {
    let (_dir, db) = open();
    let readings = readings(&db);
    insert(&readings, &MINUTES[..2]);
    let per_minute = Aggregate::new()
        .group_by_trunc("at", minute())
        .count()
        .named("n");
    let live = readings
        .watch_aggregate(Filter::new(), &per_minute)
        .unwrap();
    let first: Vec<LiveRow> = std::iter::from_fn(|| live.try_recv())
        .map(Result::unwrap)
        .collect();
    assert!(matches!(&first[..], [LiveRow::Entered { .. }]), "{first:?}");
    insert(&readings, &[("a", "2026-11-23T14:37:30Z")]);
    insert(&readings, &[("a", "2026-11-23T14:40:10Z")]);
    let events: Vec<LiveRow> = std::iter::from_fn(|| live.try_recv())
        .map(Result::unwrap)
        .collect();
    match &events[..] {
        [
            LiveRow::Changed { row: changed, .. },
            LiveRow::Entered { row, group, .. },
        ] => {
            assert_eq!(changed.get("n"), Some(&Document::Int(3)));
            let minute = Document::DateTime(at("2026-11-23T14:40:00Z"));
            assert_eq!(row.get("at"), Some(&minute));
            assert_eq!(group.get("at"), Some(&minute));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_zero_duration_and_a_path_twice_are_refused() {
    let (_dir, db) = open();
    let readings = readings(&db);
    insert(&readings, &MINUTES);
    for aggregate in [
        Aggregate::new().group_by_trunc("at", Trunc::every(Duration::ZERO)),
        Aggregate::new()
            .group_by("at")
            .group_by_trunc("at", minute()),
        Aggregate::new()
            .group_by_trunc("at", minute())
            .group_by_date("at"),
        Aggregate::new().group_by_trunc("at[*]", minute()),
        Aggregate::new()
            .group_by_trunc("at", minute())
            .count()
            .named("at"),
    ] {
        assert!(
            readings.aggregate(Filter::new(), &aggregate).is_err(),
            "{aggregate}"
        );
        assert!(aggregate.apply([]).is_err(), "{aggregate}");
        assert!(
            readings.watch_aggregate(Filter::new(), &aggregate).is_err(),
            "{aggregate}"
        );
    }
}

/// GUIDE.md's per-minute example: on `(at, server, load)`, `at >=
/// since` walks a range of it and no document is read; on `(server,
/// at)` one server's minutes are, with `server` fixed.
#[test]
fn the_guides_index_answers_its_example() {
    let aggregate = Aggregate::new()
        .group_by("server")
        .group_by_trunc("at", minute())
        .count()
        .named("n")
        .avg("load")
        .named("mean");
    let since = at("2026-11-23T14:38:00Z");
    let cases = [
        (vec!["at", "server", "load"], Filter::new().gte("at", since)),
        (
            vec!["server", "at", "load"],
            Filter::new().eq("server", "a").gte("at", since),
        ),
    ];
    for (index, filter) in cases {
        let (_dir, db) = open();
        let readings = readings(&db);
        readings.ensure_index(index.clone()).unwrap();
        insert(&readings, &MINUTES);
        let run = readings
            .explain_aggregate(filter.clone(), &aggregate)
            .unwrap();
        assert_eq!(run.stats.documents_examined, 0, "{index:?}: {run:#?}");
        assert!(run.stats.from_entries > 0, "{index:?}: {run:#?}");
        let rows = readings.aggregate(filter.clone(), &aggregate).unwrap();
        let found = readings.find(filter).unwrap();
        assert_eq!(rows, aggregate.apply(found).unwrap(), "{index:?}");
    }
}
