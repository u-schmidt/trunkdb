use super::*;

// --- Date-times (SPEC §69) ---

/// A struct with `SystemTime` fields end to end: stored as
/// date-times, found by a range through their index, sorted, changed
/// by `update_fields`, and the same to the nanosecond after a reopen
/// and after an export and import.
#[test]
fn system_time_fields_are_indexed_ranged_sorted_and_exported() {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Scan {
        #[serde(rename = "_id")]
        id: Option<DocId>,
        device: String,
        at: SystemTime,
        done: Option<SystemTime>,
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let db = Database::open(&path).unwrap();
    let scans = db.collection::<Scan>("scans");
    scans.ensure_index("at").unwrap();
    // Whole 100 ns, as every platform's `SystemTime` holds them.
    let start = UNIX_EPOCH + Duration::new(1_790_000_000, 999_999_900);
    let at = |i: u64| start + Duration::from_nanos(i * 1_000_000_100);
    let mut batch = db.batch();
    for i in 0..200 {
        let device = format!("d{}", i % 3);
        let scan = Scan {
            id: None,
            device,
            at: at(i),
            done: None,
        };
        batch.insert(&scans, scan).unwrap();
    }
    batch.commit().unwrap();

    let window = Filter::new().gte("at", at(50)).lt("at", at(60));
    assert_eq!(
        scans.explain(&window).unwrap(),
        QueryPlan::Index { field: "at".into() }
    );
    let found = scans.find(window.sort_desc("at")).unwrap();
    let times: Vec<SystemTime> = found.iter().map(|s| s.at).collect();
    assert_eq!(times, (50..60).rev().map(at).collect::<Vec<_>>());
    let newest = scans
        .find_one(Filter::new().sort_desc("at"))
        .unwrap()
        .unwrap();
    assert_eq!(newest.at, at(199));
    assert_eq!(scans.count(Filter::new().lt("at", start)).unwrap(), 0);
    assert_eq!(
        scans
            .count(Filter::new().eq("at", "2026-09-21T14:13:20Z"))
            .unwrap(),
        0
    );

    let finished = SystemTime::now();
    let first_ten = Filter::new().lt("at", at(10));
    let changed = scans
        .update_fields(first_ten.clone(), &Update::new().set("done", finished))
        .unwrap();
    assert_eq!(changed, 10);
    assert_eq!(scans.count(Filter::new().eq("done", finished)).unwrap(), 10);

    let everything = |db: &Database| {
        let mut all = db.collection::<Scan>("scans").find(Filter::new()).unwrap();
        all.sort_by_key(|s| s.id);
        all
    };
    let before = everything(&db);
    drop((scans, db));
    let db = Database::open(&path).unwrap();
    assert_eq!(everything(&db), before, "after a reopen");
    assert!(db.check().unwrap().is_ok());

    let mut export = Vec::new();
    db.export(&mut export).unwrap();
    let text = String::from_utf8(export.clone()).unwrap();
    assert!(
        text.contains(r#""at":{"$date":"2026-09-21T14:13:20.9999999Z"}"#),
        "{text}"
    );
    let copy = Database::open(dir.path().join("copy.trunkdb")).unwrap();
    copy.import(&export[..]).unwrap();
    assert_eq!(everything(&copy), before, "after an export and import");
    let window = Filter::new().gte("at", at(50)).lt("at", at(60));
    assert_eq!(copy.collection::<Scan>("scans").count(window).unwrap(), 10);
}
