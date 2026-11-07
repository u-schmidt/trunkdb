//! Dates without a time and times without a date, through a collection
//! (SPEC §173): fields of their own types, an index read in their
//! order, the file reopened, an export imported, an update and an
//! aggregate.

use serde::{Deserialize, Serialize};
use trunkdb::query::{Aggregate, Filter, Kind, QueryPlan, Update};
use trunkdb::{Database, Date, DateTime, DocId, Document, Time};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Shift {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    id: Option<DocId>,
    day: Date,
    starts: Time,
    ends: Option<Time>,
    logged: DateTime,
}

fn date(text: &str) -> Date {
    text.parse().unwrap()
}

fn time(text: &str) -> Time {
    text.parse().unwrap()
}

/// Thirty shifts, a day apart from 2026-10-25 on, starting at 6:00,
/// 6:30, … and ending eight hours later, but the last ten not yet.
fn shifts() -> Vec<Shift> {
    let first = date("2026-10-25");
    (0..30i64)
        .map(|n| {
            let day = Date::from_days_since_epoch(first.days_since_epoch() + n).unwrap();
            let starts = Time::from_hms(6 + (n / 2) as u32 % 12, 30 * (n % 2) as u32, 0).unwrap();
            let ends =
                (n < 20).then(|| Time::from_hms(starts.hour() + 8, starts.minute(), 0).unwrap());
            Shift {
                id: None,
                day,
                starts,
                ends,
                logged: day.at(starts).unwrap(),
            }
        })
        .collect()
}

fn days(found: &[Shift]) -> Vec<String> {
    found.iter().map(|shift| shift.day.to_string()).collect()
}

#[test]
fn dates_and_times_are_stored_indexed_and_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shifts.trunkdb");
    let db = Database::open(&path).unwrap();
    let collection = db.collection::<Shift>("shifts");
    for shift in shifts() {
        collection.insert(shift).unwrap();
    }
    collection.ensure_index("day").unwrap();
    collection.ensure_index("starts").unwrap();

    // A range of days, through the index, in calendar order.
    let november = Filter::new()
        .gte("day", date("2026-11-01"))
        .lt("day", date("2026-11-04"));
    assert!(matches!(
        collection.explain(&november).unwrap(),
        QueryPlan::Index { .. }
    ));
    let found = collection.find(november.clone().sort_desc("day")).unwrap();
    assert_eq!(days(&found), ["2026-11-03", "2026-11-02", "2026-11-01"]);
    assert_eq!(collection.count(november.clone()).unwrap(), 3);

    // Times of day, through their own index; never a date-time.
    let early = Filter::new().lt("starts", time("07:00"));
    assert!(matches!(
        collection.explain(&early).unwrap(),
        QueryPlan::Index { .. }
    ));
    assert_eq!(collection.count(early).unwrap(), 4);
    let midnight = Document::DateTime(date("2026-11-01").at(Time::MIDNIGHT).unwrap());
    assert_eq!(
        collection.count(Filter::new().eq("day", midnight)).unwrap(),
        0
    );
    assert_eq!(
        collection
            .count(Filter::new().eq("day", "2026-11-01"))
            .unwrap(),
        0
    );

    // As text, kinds and all.
    let text: Filter = r#"day >= date("2026-11-20") and ends is null and starts is time"#
        .parse()
        .unwrap();
    assert_eq!(collection.count(text).unwrap(), 4);
    let unfinished: Filter = "ends is not time".parse().unwrap();
    assert_eq!(collection.count(unfinished).unwrap(), 10);
    assert_eq!(
        collection
            .count(Filter::new().is_kind("logged", Kind::DateTime))
            .unwrap(),
        30
    );
    assert_eq!(
        collection
            .count(Filter::new().is_kind("logged", Kind::Date))
            .unwrap(),
        0
    );

    // An update's `max` keeps the later date; one of another kind
    // doesn't compare, and is refused.
    let first = Filter::new().eq("day", date("2026-10-25"));
    collection
        .update_fields(first.clone(), &Update::new().max("day", date("2026-10-24")))
        .unwrap();
    assert_eq!(collection.find(first.clone()).unwrap().len(), 1);
    assert!(
        collection
            .update_fields(
                first.clone(),
                &Update::new().max("starts", date("2026-10-24"))
            )
            .is_err()
    );

    // An aggregate's `min` and `max` of each.
    let aggregate: Aggregate = "min(day) as first, max(day) as last, max(starts) as latest"
        .parse()
        .unwrap();
    let rows = collection.aggregate(Filter::new(), &aggregate).unwrap();
    assert_eq!(
        rows[0].get("first"),
        Some(&Document::Date(date("2026-10-25")))
    );
    assert_eq!(
        rows[0].get("last"),
        Some(&Document::Date(date("2026-11-23")))
    );
    assert_eq!(rows[0].get("latest"), Some(&Document::Time(time("17:30"))));

    // The same after a reopen, and after an export imported into a new
    // file.
    let all: Vec<Shift> = collection.find(Filter::new().sort_asc("day")).unwrap();
    drop(collection);
    drop(db);
    let db = Database::open(&path).unwrap();
    assert!(db.check().unwrap().is_ok());
    let collection = db.collection::<Shift>("shifts");
    assert_eq!(collection.find(Filter::new().sort_asc("day")).unwrap(), all);
    assert_eq!(collection.find(november.clone()).unwrap().len(), 3);

    let mut export = Vec::new();
    db.export(&mut export).unwrap();
    let text = String::from_utf8(export).unwrap();
    assert!(text.starts_with("{\"$trunkdb_export\":4}\n"), "{text}");
    assert!(text.contains(r#""day":{"$date":"2026-10-25"}"#), "{text}");
    assert!(text.contains(r#""starts":{"$time":"06:00:00"}"#), "{text}");
    let copy = Database::open(dir.path().join("copy.trunkdb")).unwrap();
    copy.import(text.as_bytes()).unwrap();
    let copied = copy.collection::<Shift>("shifts");
    assert_eq!(copied.find(Filter::new().sort_asc("day")).unwrap(), all);
    assert!(matches!(
        copied.explain(&november).unwrap(),
        QueryPlan::Index { .. }
    ));
}

/// SPEC §173: an export of version 3 knew no `$time`; its `{"$time":
/// …}` objects import as the objects they were.
#[test]
fn an_older_export_keeps_its_time_objects() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("old.trunkdb")).unwrap();
    let text = "{\"$trunkdb_export\":3}\n{\"$collection\":\"c\"}\n{\"at\":{\"$time\":\"soon\"}}\n\
                {\"$end\":{\"collections\":1,\"documents\":1}}\n";
    db.import(text.as_bytes()).unwrap();
    let docs = db.collection::<Document>("c").find(Filter::new()).unwrap();
    let at = docs[0].get("at").unwrap();
    assert_eq!(at.get("$time"), Some(&Document::String("soon".into())));
}
