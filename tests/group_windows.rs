//! A window for each group (SPEC §174): `Aggregate::newest_per_group`,
//! each group's last `n` documents, and `window_per_group`, each
//! group's documents within a duration of its own newest; one-off,
//! through a snapshot, in memory and live.

use std::collections::BTreeMap;
use std::time::Duration;
use trunkdb::query::{Aggregate, Condition, Filter};
use trunkdb::{Collection, Database, DateTime, Document, LiveAggregate, LiveRow};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("group_windows.trunkdb")).unwrap();
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
fn reading(server: &str, load: i64, secs: i64) -> Document {
    let mut doc: Document = format!(r#"{{"server": "{server}", "load": {load}}}"#)
        .parse()
        .unwrap();
    if let Document::Object(map) = &mut doc {
        map.insert("at".into(), Document::DateTime(t(secs)));
    }
    doc
}

fn insert(readings: &Collection<Document>, all: &[(&str, i64, i64)]) {
    for &(server, load, secs) in all {
        readings.insert(reading(server, load, secs)).unwrap();
    }
}

/// Per server, how many readings and their total load.
fn per_server() -> Aggregate {
    Aggregate::new()
        .group_by("server")
        .count()
        .named("n")
        .sum("load")
        .named("total")
}

/// A row as `(server, n, total)`.
type Short = (String, i64, i64);

fn short(row: &Document) -> Short {
    let server = row.get("server").and_then(Document::as_str);
    (
        server.unwrap_or("").to_string(),
        row.get("n").and_then(Document::as_i64).unwrap(),
        row.get("total").and_then(Document::as_i64).unwrap(),
    )
}

fn s(server: &str, n: i64, total: i64) -> Short {
    (server.to_string(), n, total)
}

/// The same rows from every way of running `aggregate`: the database,
/// a snapshot, `Aggregate::apply` over what the filter finds, and the
/// aggregate's text read back.
fn every_way(db: &Database, filter: Filter, aggregate: &Aggregate) -> Vec<Short> {
    let readings = readings(db);
    let stored = readings.aggregate(filter.clone(), aggregate).unwrap();
    let snapshot = db.snapshot().unwrap();
    let viewed = (snapshot.collection::<Document>("readings"))
        .aggregate(filter.clone(), aggregate)
        .unwrap();
    let found = readings.find(filter.clone()).unwrap();
    let in_memory = aggregate.apply(found).unwrap();
    let text: Aggregate = aggregate.to_string().parse().unwrap();
    let read = readings.aggregate(filter, &text).unwrap();
    assert_eq!(stored, viewed, "a snapshot");
    assert_eq!(stored, in_memory, "in memory");
    assert_eq!(stored, read, "as text: {aggregate}");
    stored.iter().map(short).collect()
}

#[test]
fn each_group_keeps_its_newest_n() {
    let (_dir, db) = open();
    let readings = readings(&db);
    // `a` reports often, `b` seldom, `c` once, long ago.
    insert(
        &readings,
        &[
            ("c", 1000, 10),
            ("b", 100, 100),
            ("a", 1, 500),
            ("b", 200, 600),
            ("a", 2, 900),
            ("a", 4, 950),
            ("b", 400, 990),
            ("a", 8, 999),
        ],
    );
    let last_two = per_server().newest_per_group("at", 2);
    assert_eq!(
        every_way(&db, Filter::new(), &last_two),
        [s("a", 2, 12), s("b", 2, 600), s("c", 1, 1000)]
    );
    // The filter picks first; the window then counts in what is left.
    let not_late = Filter::new().lt("at", t(990));
    assert_eq!(
        every_way(&db, not_late, &last_two),
        [s("a", 2, 6), s("b", 2, 300), s("c", 1, 1000)]
    );
    // Zero keeps nothing, so no group; more than there are keeps all.
    let none = per_server().newest_per_group("at", 0);
    assert_eq!(every_way(&db, Filter::new(), &none), []);
    let all = per_server().newest_per_group("at", 99);
    assert_eq!(
        every_way(&db, Filter::new(), &all),
        [s("a", 4, 15), s("b", 3, 700), s("c", 1, 1000)]
    );
    // Without `group_by`, every match is one group: the newest two.
    let total = Aggregate::new()
        .count()
        .named("n")
        .sum("load")
        .named("total")
        .newest_per_group("at", 2);
    assert_eq!(every_way(&db, Filter::new(), &total), [s("", 2, 408)]);
}

#[test]
fn each_group_keeps_what_is_close_to_its_own_newest() {
    let (_dir, db) = open();
    let readings = readings(&db);
    readings.ensure_index("at").unwrap();
    insert(
        &readings,
        &[
            ("b", 100, 100),
            ("b", 200, 399),
            ("b", 400, 400),
            ("a", 1, 600),
            ("a", 2, 700),
            ("a", 4, 1000),
        ],
    );
    let windowed = per_server().window_per_group("at", Duration::from_secs(300));
    // `b`'s newest is at 400: 100 is exactly 300 before it, and in.
    assert_eq!(
        every_way(&db, Filter::new(), &windowed),
        [s("a", 2, 6), s("b", 3, 700)]
    );
    // The whole aggregate's window, beside it, would leave `b` out.
    let whole = per_server().window("at", Duration::from_secs(300));
    assert_eq!(every_way(&db, Filter::new(), &whole), [s("a", 2, 6)]);
    // Both: the window of all first, as a condition, then each group's.
    let both = whole.clone().newest_per_group("at", 1);
    assert_eq!(every_way(&db, Filter::new(), &both), [s("a", 1, 4)]);
    let narrow = per_server().window_per_group("at", Duration::new(299, 999_999_999));
    assert_eq!(
        every_way(&db, Filter::new(), &narrow),
        [s("a", 1, 4), s("b", 2, 600)]
    );
    // The later of the two replaces the earlier.
    let replaced = per_server()
        .window_per_group("at", Duration::from_secs(1))
        .newest_per_group("at", 3);
    assert_eq!(
        every_way(&db, Filter::new(), &replaced),
        [s("a", 3, 7), s("b", 3, 700)]
    );
}

/// A count orders any value a sort orders, and the later id wins a
/// tie; `null` and missing are out. A time keeps date-times only.
#[test]
fn what_orders_and_what_is_out() {
    let (_dir, db) = open();
    let readings = readings(&db);
    for (load, seq) in [
        (1, r#"3"#),
        (2, r#"7"#),
        (4, r#"7.0"#),
        (8, r#"null"#),
        (16, r#""late""#),
        (32, r#"5"#),
    ] {
        let doc = format!(r#"{{"server": "a", "load": {load}, "seq": {seq}}}"#);
        readings.insert(doc.parse().unwrap()).unwrap();
    }
    readings
        .insert(r#"{"server": "a", "load": 64}"#.parse().unwrap())
        .unwrap();
    // A string sorts after numbers; 7.0 equals 7 and came later.
    let by_seq = |n| per_server().newest_per_group("seq", n);
    assert_eq!(every_way(&db, Filter::new(), &by_seq(1)), [s("a", 1, 16)]);
    assert_eq!(every_way(&db, Filter::new(), &by_seq(2)), [s("a", 2, 20)]);
    assert_eq!(every_way(&db, Filter::new(), &by_seq(3)), [s("a", 3, 22)]);
    assert_eq!(every_way(&db, Filter::new(), &by_seq(9)), [s("a", 5, 55)]);
    // No date-time at `seq`: a window by time keeps nothing.
    let by_time = per_server().window_per_group("seq", Duration::from_secs(1 << 40));
    assert_eq!(every_way(&db, Filter::new(), &by_time), []);
}

/// Groups as `group_by` makes them: `5` and `5.0` one, a missing value
/// with `null`.
#[test]
fn groups_are_the_aggregates_groups() {
    let (_dir, db) = open();
    let readings = readings(&db);
    for (server, load, secs) in [
        ("5", 1, 1),
        ("5.0", 2, 2),
        ("null", 4, 3),
        ("", 8, 4),
        ("5", 16, 5),
        ("[5]", 32, 6),
        ("[5.0]", 64, 7),
    ] {
        let mut doc: Document = match server {
            "" => format!(r#"{{"load": {load}}}"#),
            _ => format!(r#"{{"server": {server}, "load": {load}}}"#),
        }
        .parse()
        .unwrap();
        if let Document::Object(map) = &mut doc {
            map.insert("at".into(), Document::DateTime(t(secs)));
        }
        readings.insert(doc).unwrap();
    }
    let newest = per_server().newest_per_group("at", 1);
    assert_eq!(
        every_way(&db, Filter::new(), &newest),
        [s("", 1, 8), s("", 1, 16), s("", 1, 64)]
    );
    let rows = readings.aggregate(Filter::new(), &newest).unwrap();
    assert_eq!(rows[0].get("server"), Some(&Document::Null));
    assert_eq!(rows[1].get("server"), Some(&Document::Int(5)));
    // Arrays as a filter's `=` sees them: `[5]` and `[5.0]` one group,
    // shown as the one kept has it.
    let five: Document = "[5.0]".parse().unwrap();
    assert_eq!(rows[2].get("server"), Some(&five));
}

/// Where an index holds every field, or a count alone needs no
/// document, the window is still kept: the documents are read.
#[test]
fn entries_and_counts_alone_keep_it_too() {
    let (_dir, db) = open();
    let readings = readings(&db);
    readings.ensure_index(["server", "at"]).unwrap();
    insert(
        &readings,
        &[("a", 1, 1), ("a", 2, 2), ("a", 4, 3), ("b", 8, 4)],
    );
    let n = |rows: Vec<Document>| -> Vec<i64> {
        (rows.iter())
            .map(|row| row.get("n").and_then(Document::as_i64).unwrap())
            .collect()
    };
    let newest = Aggregate::new()
        .group_by("server")
        .count()
        .named("n")
        .max("at")
        .named("latest")
        .newest_per_group("at", 2);
    let rows = readings.aggregate(Filter::new(), &newest).unwrap();
    assert_eq!(
        rows,
        newest.apply(readings.find(Filter::new()).unwrap()).unwrap()
    );
    assert_eq!(n(rows), [2, 1]);
    let alone = Aggregate::new()
        .count()
        .named("n")
        .newest_per_group("at", 3);
    assert_eq!(n(readings.aggregate(Filter::new(), &alone).unwrap()), [3]);
    let timed = Aggregate::new()
        .count()
        .named("n")
        .window_per_group("at", Duration::from_secs(1));
    assert_eq!(n(readings.aggregate(Filter::new(), &timed).unwrap()), [2]);
}

/// A reading in the model: `(secs, the order made, load)`.
type Kept = (i64, usize, i64);

/// Many random readings against a model: per server, sorted newest
/// first by hand, cut by count and by time.
#[test]
fn random_readings_match_a_model() {
    struct Random(u64);
    impl Random {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }
    let mut random = Random(0x2545_F491_4F6C_DD1D);
    for round in 0..6 {
        let (_dir, db) = open();
        let readings = readings(&db);
        if round % 2 == 1 {
            readings.ensure_index("at").unwrap();
        }
        // (server, load, secs); secs repeat, so ties are many.
        let mut all: Vec<(String, i64, i64)> = Vec::new();
        for _ in 0..80 + random.below(80) {
            let server = ["a", "b", "c", "d"][random.below(4) as usize];
            let load = random.below(1000) as i64;
            let secs = random.below(60) as i64 + 100 * random.below(3) as i64;
            all.push((server.to_string(), load, secs));
            readings.insert(reading(server, load, secs)).unwrap();
        }
        let mut per: BTreeMap<&str, Vec<Kept>> = BTreeMap::new();
        for (seq, (server, load, secs)) in all.iter().enumerate() {
            per.entry(server).or_default().push((*secs, seq, *load));
        }
        for list in per.values_mut() {
            list.sort_by(|a, b| b.cmp(a));
        }
        let model = |keep: &dyn Fn(&[Kept]) -> usize| -> Vec<Short> {
            (per.iter())
                .map(|(server, list)| {
                    let kept = &list[..keep(list)];
                    let total = kept.iter().map(|(_, _, load)| load).sum();
                    s(server, kept.len() as i64, total)
                })
                .filter(|(_, n, _)| *n > 0)
                .collect()
        };
        for n in [0, 1, 2, 5, 30] {
            let counted = per_server().newest_per_group("at", n);
            let expected = model(&|list| list.len().min(n));
            let found = every_way(&db, Filter::new(), &counted);
            assert_eq!(found, expected, "round {round}, newest {n}");
        }
        for secs in [0, 7, 59, 100, 250] {
            let timed = per_server().window_per_group("at", Duration::from_secs(secs));
            let expected = model(&|list| {
                (list.iter())
                    .take_while(|(at, _, _)| *at >= list[0].0 - secs as i64)
                    .count()
            });
            let found = every_way(&db, Filter::new(), &timed);
            assert_eq!(found, expected, "round {round}, within {secs}s");
        }
    }
}

#[test]
fn refused_where_it_cant_say_what_it_keeps() {
    let (_dir, db) = open();
    let readings = readings(&db);
    insert(&readings, &[("a", 1, 1)]);
    let newest = per_server().newest_per_group("at", 2);
    // Beside the filter's sort, skip or limit: which first?
    for picked in [
        Filter::new().sort_desc("at"),
        Filter::new().skip(1),
        Filter::new().limit(10),
    ] {
        let e = readings.aggregate(picked.clone(), &newest).unwrap_err();
        assert!(e.to_string().contains("window per group"), "{e}");
        assert!(readings.watch_aggregate(picked, &newest).is_err());
    }
    // A path that names no field.
    for path in ["", "at[*]", "_id"] {
        let wrong = per_server().newest_per_group(path, 2);
        assert!(readings.aggregate(Filter::new(), &wrong).is_err(), "{path}");
        assert!(wrong.apply([]).is_err(), "{path:?}");
        let timed = per_server().window_per_group(path, Duration::from_secs(1));
        assert!(readings.aggregate(Filter::new(), &timed).is_err(), "{path}");
    }
    // A group field or the window's path inside an unwound array: a
    // document would be in several groups, or ordered by several values.
    let unwound = |group: &str, path: &str| {
        Aggregate::new()
            .unwind("parts")
            .group_by(group)
            .count()
            .named("n")
            .newest_per_group(path, 1)
    };
    for (group, path) in [
        ("parts", "at"),
        ("parts.kind", "at"),
        ("server", "parts.at"),
    ] {
        let e = (readings.aggregate(Filter::new(), &unwound(group, path))).unwrap_err();
        assert!(e.to_string().contains("one group"), "{group} {path}: {e}");
    }
    // Unwinding what neither reads is fine: a document's elements stay
    // in its group, kept or left out together.
    let doc = r#"{"server": "a", "parts": [1, 2, 3], "load": 5}"#;
    readings.insert(doc.parse().unwrap()).unwrap();
    let rows = readings
        .aggregate(Filter::new(), &unwound("server", "load"))
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("n"), Some(&Document::Int(3)));
}

#[test]
fn the_text_reads_back() {
    for (text, written) in [
        (
            "window at 10 per group count() as n group by server",
            "window at 10 per group count() as n group by server",
        ),
        (
            "window at 300s per group count() as n",
            "window at 5m per group count() as n",
        ),
        (
            "window at 10 per group window at 1h count() as n",
            "window at 1h window at 10 per group count() as n",
        ),
    ] {
        let read: Aggregate = text.parse().unwrap();
        assert_eq!(read.to_string(), written);
    }
    for (text, why) in [
        ("window at 10 count() as n", "or `per group` after a count"),
        (
            "window at 5min count() as n",
            "or `per group` after a count",
        ),
        ("window at 10 per count() as n", "expected `group`"),
        (
            "window at 1 per group window at 2 per group count() as n",
            "two windows per group",
        ),
        (
            "window at 1h window at 2h count() as n",
            "two windows of all",
        ),
    ] {
        let e = text.parse::<Aggregate>().unwrap_err();
        assert!(e.to_string().contains(why), "{text}: {e}");
    }
    // A parameter stands for a count, never for a duration.
    let template: trunkdb::query::Template<Aggregate> =
        "window at :n per group count() as c group by server"
            .parse()
            .unwrap();
    let bound = template
        .bind(&trunkdb::query::Params::new().set("n", 3))
        .unwrap();
    assert_eq!(
        bound.to_string(),
        "window at 3 per group count() as c group by server"
    );
    let e = "window at :n count() as c"
        .parse::<trunkdb::query::Template<Aggregate>>()
        .unwrap_err();
    assert!(e.to_string().contains("a parameter can't stand"), "{e}");
}

/// A live aggregate's event as `(kind, server, n)`; `n` 0 for a row
/// gone.
fn event(live: LiveRow) -> (&'static str, String, i64) {
    let server = |group: &Document| {
        let server = group.get("server").and_then(Document::as_str);
        server.unwrap_or("").to_string()
    };
    match &live {
        LiveRow::Entered { row, .. } => ("entered", short(row).0, short(row).1),
        LiveRow::Left {
            row: None, group, ..
        } => ("gone", server(group), 0),
        LiveRow::Left { row: Some(row), .. } => ("left", short(row).0, short(row).1),
        LiveRow::Changed { row, .. } => ("changed", short(row).0, short(row).1),
        _ => unreachable!(),
    }
}

fn waiting(live: &LiveAggregate) -> Vec<(&'static str, String, i64)> {
    std::iter::from_fn(|| live.try_recv())
        .map(|e| event(e.unwrap()))
        .collect()
}

fn ev(kind: &'static str, server: &str, n: i64) -> (&'static str, String, i64) {
    (kind, server.to_string(), n)
}

/// SPEC §174: live, a server gone silent keeps its last readings, and
/// its alert, where the window of all (§139) ages it out.
#[test]
fn live_a_silent_server_keeps_its_last_readings() {
    let (_dir, db) = open();
    let readings = readings(&db);
    let busy = per_server()
        .avg("load")
        .named("mean")
        .window_per_group("at", Duration::from_secs(300))
        .having(Condition::gt("mean", 80));
    let live = readings.watch_aggregate(Filter::new(), &busy).unwrap();
    insert(&readings, &[("a", 90, 1000), ("b", 90, 1000)]);
    assert_eq!(
        waiting(&live),
        [ev("entered", "a", 1), ev("entered", "b", 1)]
    );
    // `b` stops; `a` goes on: `b`'s row stays as it is. `a`'s first
    // reading leaves its window as the third comes: the same row.
    insert(&readings, &[("a", 90, 1200), ("a", 90, 1301)]);
    assert_eq!(waiting(&live), [ev("changed", "a", 2)]);
    // `a` calms down over its own last readings; `b` is still busy.
    insert(&readings, &[("a", 0, 1302)]);
    assert_eq!(waiting(&live), [ev("left", "a", 3)]);
    let rows = readings.aggregate(Filter::new(), &busy).unwrap();
    assert_eq!(rows.iter().map(short).collect::<Vec<_>>(), [s("b", 1, 90)]);

    // By count: the last two readings of each.
    let last_two = per_server().newest_per_group("at", 2);
    let live = readings.watch_aggregate(Filter::new(), &last_two).unwrap();
    assert_eq!(
        waiting(&live),
        [ev("entered", "a", 2), ev("entered", "b", 1)]
    );
    insert(&readings, &[("b", 10, 2000), ("b", 20, 2001)]);
    assert_eq!(
        waiting(&live),
        [ev("changed", "b", 2), ev("changed", "b", 2)]
    );
}
