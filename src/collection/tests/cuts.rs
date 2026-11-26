//! Group fields cut to a unit, answered from an index's entries (SPEC
//! §209.6, step 2): each entry's group cut from its key, the last group
//! known by its cut, whole leaves of one bucket counted at once — every
//! answer held against `Aggregate::apply` over the documents.

use super::*;
use crate::query::{Aggregate, Trunc};
use crate::{Date, DateTime};
use std::time::Duration;

fn every(secs: u64) -> Trunc {
    Trunc::every(Duration::from_secs(secs))
}

/// 2026-01-01T00:00:00Z.
const START: i64 = 1_767_225_600;

/// What `aggregate` gives over `filter`, held against the same aggregate
/// applied to the documents the filter finds; with how it ran and how
/// many leaves it counted whole.
fn run(
    readings: &Collection<Document>,
    filter: Filter,
    aggregate: &Aggregate,
) -> (Vec<Document>, crate::explain::Explained, usize) {
    LEAVES_COUNTED.with(|c| c.set(0));
    let rows = readings.aggregate(filter.clone(), aggregate).unwrap();
    let leaves = LEAVES_COUNTED.with(|c| c.get());
    let expected = aggregate
        .apply(readings.find(filter.clone()).unwrap())
        .unwrap();
    assert_eq!(rows, expected, "{aggregate}");
    let run = readings.explain_aggregate(filter, aggregate).unwrap();
    (rows, run, leaves)
}

/// `n` readings a second apart from `START`, of servers `s0` to `s4`
/// in turn.
fn readings(db: &Database, n: i64) -> Collection<Document> {
    let readings = db.collection::<Document>("readings");
    let mut batch = db.batch();
    for i in 0..n {
        let doc = object(vec![
            (
                "at",
                Document::DateTime(DateTime::from_unix(START + i, 0).unwrap()),
            ),
            ("server", Document::String(format!("s{}", i % 5))),
        ]);
        batch.insert(&readings, doc).unwrap();
    }
    batch.commit().unwrap();
    readings
}

#[test]
fn a_cut_field_is_answered_from_entries_and_whole_leaves() {
    let (_dir, db) = fresh_db();
    const N: i64 = 20_000;
    let readings = readings(&db, N);
    readings.ensure_index("at").unwrap();
    let count = |unit| {
        Aggregate::new()
            .group_by_trunc("at", unit)
            .count()
            .named("n")
    };

    // Per hour, every document: the index holds them all, its leaves
    // inside one hour counted whole, no document read.
    let (rows, r, leaves) = run(&readings, Filter::new(), &count(every(3600)));
    assert_eq!(rows.len(), 6);
    assert_eq!(
        (r.stats.documents_examined, r.stats.from_entries),
        (0, N as u64),
        "{r:?}"
    );
    assert!(leaves > 40, "{leaves}");

    // Per minute, a leaf holds several: walked entry by entry, still
    // from the keys.
    let since = Filter::new().gte("at", DateTime::from_unix(START + 7_000, 0).unwrap());
    let (rows, r, _) = run(&readings, since.clone(), &count(every(60)));
    assert_eq!(rows.len(), 218);
    assert_eq!(r.stats.documents_examined, 0, "{r:?}");

    // Per day as a date, a range of the index: one row.
    let (rows, r, leaves) = run(
        &readings,
        since,
        &Aggregate::new().group_by_date("at").count().named("n"),
    );
    assert_eq!(
        rows[0].get("at"),
        Some(&Document::Date("2026-01-01".parse().unwrap()))
    );
    assert_eq!(rows[0].get("n"), Some(&Document::Int(N - 7_000)));
    assert_eq!(r.stats.documents_examined, 0, "{r:?}");
    assert!(leaves > 40, "{leaves}");

    // Beside a column on the same field: its value as stored, not cut.
    let (rows, r, _) = run(
        &readings,
        Filter::new(),
        &count(every(3600))
            .min("at")
            .named("first")
            .max("at")
            .named("last"),
    );
    let at = |secs| Document::DateTime(DateTime::from_unix(START + secs, 0).unwrap());
    assert_eq!(rows[1].get("first"), Some(&at(3600)));
    assert_eq!(rows[1].get("last"), Some(&at(7199)));
    assert_eq!(r.stats.documents_examined, 0, "{r:?}");
}

/// A cut field before another group field: a leaf's ends in one hour
/// say nothing of the servers between them, so no leaf is counted whole.
/// After one, as in `(server, at)`, the server's leaves are.
#[test]
fn only_a_cut_field_last_counts_leaves_whole() {
    let (_dir, db) = fresh_db();
    let readings = readings(&db, 20_000);
    readings.ensure_index(["at", "server"]).unwrap();
    let hour_then_server = Aggregate::new()
        .group_by_trunc("at", every(3600))
        .group_by("server")
        .count()
        .named("n");
    let (rows, r, leaves) = run(&readings, Filter::new(), &hour_then_server);
    assert_eq!(rows.len(), 30);
    assert_eq!(r.stats.documents_examined, 0, "{r:?}");
    assert_eq!(leaves, 0);

    readings.drop_index(["at", "server"]).unwrap();
    readings.ensure_index(["server", "at"]).unwrap();
    let server_then_hour = Aggregate::new()
        .group_by("server")
        .group_by_trunc("at", every(3600))
        .count()
        .named("n");
    let (rows, r, leaves) = run(&readings, Filter::new(), &server_then_hour);
    assert_eq!(rows.len(), 30);
    assert_eq!(r.stats.documents_examined, 0, "{r:?}");
    assert!(leaves > 20, "{leaves}");
}

/// A cut keeps order within a kind, not across them: a leaf from a
/// date-time to a date both cut to one day holds binary between, which
/// a cut leaves as it is. Its ends must be of one kind to count it whole.
#[test]
fn a_leaf_whose_ends_differ_in_kind_is_walked() {
    let (_dir, db) = fresh_db();
    let readings = db.collection::<Document>("readings");
    readings.ensure_index("at").unwrap();
    let day: Date = "2026-01-01".parse().unwrap();
    let mut batch = db.batch();
    for i in 0..100i64 {
        let at = DateTime::from_unix(START + 600 * i, 0).unwrap();
        batch
            .insert(&readings, object(vec![("at", Document::DateTime(at))]))
            .unwrap();
        batch
            .insert(&readings, object(vec![("at", Document::Date(day))]))
            .unwrap();
    }
    for b in 0..3u8 {
        batch
            .insert(&readings, object(vec![("at", Document::Binary(vec![b]))]))
            .unwrap();
    }
    batch.commit().unwrap();
    let per_day = Aggregate::new().group_by_date("at").count().named("n");
    let (rows, r, _) = run(&readings, Filter::new(), &per_day);
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert_eq!(r.stats.documents_examined, 0, "{r:?}");
}

/// Random readings of every kind at the cut field, random indexes and
/// filters, every aggregate held against its documents.
#[test]
fn random_cuts_agree_with_their_documents() {
    let mut rng = crate::testing::XorShift(0x2209_6211);
    for round in 0..12 {
        let (_dir, db) = fresh_db();
        let readings = db.collection::<Document>("readings");
        match round % 4 {
            0 => readings.ensure_index("at").unwrap(),
            1 => readings.ensure_index(["server", "at"]).unwrap(),
            2 => readings.ensure_index(["at", "server"]).unwrap(),
            _ => readings.ensure_index(["server", "at", "v"]).unwrap(),
        };
        let mut batch = db.batch();
        for _ in 0..(500 + rng.below(3000)) {
            let secs = START + rng.below(40 * 86_400) as i64;
            let at = match rng.below(20) {
                0 => Document::Date(DateTime::from_unix(secs, 0).unwrap().date()),
                1 => Document::Int(rng.below(5) as i64),
                2 => Document::Float(rng.below(5) as f64 + 0.5),
                3 => Document::String(format!("x{}", rng.below(3))),
                4 => Document::Null,
                5 => Document::Binary(vec![rng.below(3) as u8]),
                _ => Document::DateTime(
                    DateTime::from_unix(secs, rng.below(1_000_000_000) as u32).unwrap(),
                ),
            };
            let mut fields = vec![
                ("server", Document::String(format!("s{}", rng.below(4)))),
                ("v", Document::Int(rng.below(10) as i64)),
            ];
            if rng.below(30) > 0 {
                fields.push(("at", at));
            }
            batch.insert(&readings, object(fields)).unwrap();
        }
        batch.commit().unwrap();
        let units = [
            every(1),
            every(60),
            every(3600),
            every(86_400),
            Trunc::Week,
            Trunc::Month,
        ];
        for _ in 0..8 {
            let unit = units[rng.below(units.len())];
            let mut aggregate = match rng.below(4) {
                0 => Aggregate::new().group_by_trunc("at", unit),
                1 => Aggregate::new().group_by_date("at"),
                2 => Aggregate::new()
                    .group_by("server")
                    .group_by_trunc("at", unit),
                _ => Aggregate::new()
                    .group_by_trunc("at", unit)
                    .group_by("server"),
            };
            aggregate = aggregate.count().named("n");
            if rng.below(2) == 0 {
                aggregate = aggregate.min("at").named("first").max("v").named("top");
            }
            let filter = match rng.below(3) {
                0 => Filter::new(),
                1 => Filter::new().gte(
                    "at",
                    DateTime::from_unix(START + rng.below(40 * 86_400) as i64, 0).unwrap(),
                ),
                _ => Filter::new().eq("server", "s1"),
            };
            run(&readings, filter, &aggregate);
        }
    }
}
