//! Synthetic data shaped like the time-series workload (SPEC.md §5.1), exercised through trunkdb's public API only (this is an
//! integration test, not a unit test — no access to crate internals).
//!
//! Covers the workload's query patterns, per §5.1:
//! - a time-range filter (`tst >= ? AND tst <= ?`) combined with sort + limit,
//!   with and without a secondary index on `tst`
//! - several optional filters ANDed together dynamically
//! - "most recent row" (`ORDER BY tst DESC LIMIT 1`)
//!
//! Deliberately NOT exercised here, per §5.1/§15 (explicitly out of v0
//! scope, not gaps to chase):
//! - the `zone_key` unique constraint on geofences — v0 has no constraint
//!   system at all.
//! - the `LEFT JOIN` + `GROUP BY` + `COUNT` aggregate — that becomes
//!   application-level code on top of `find`, not something `Filter`
//!   should grow support for.

use serde::{Deserialize, Serialize};
use trunkdb::query::{Condition, Filter, Op, QueryPlan, Sort, SortOrder};
use trunkdb::{Database, Document};

/// A location ping — high-frequency, append-only data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LocationPing {
    tid: String,
    tst: i64,
    lat: f64,
    lon: f64,
    batt: i64,
}

/// A geofence — a small, rarely-written reference table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Geofence {
    zone_key: String,
    name: String,
    lat: f64,
    lon: f64,
    radius: i64,
}

fn ping(tid: &str, tst: i64, lat: f64, lon: f64, batt: i64) -> LocationPing {
    LocationPing {
        tid: tid.to_string(),
        tst,
        lat,
        lon,
        batt,
    }
}

#[test]
fn time_range_filter_combined_with_sort_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("timeseries.trunkdb")).unwrap();
    let pings = db.collection::<LocationPing>("pings");

    for tst in [100, 200, 300, 400, 500] {
        pings.insert(ping("phone-1", tst, 52.5, 13.4, 90)).unwrap();
    }

    // tst >= 200 AND tst <= 400, ordered oldest-first.
    let filter = Filter {
        conditions: vec![
            Condition {
                field: "tst".to_string(),
                op: Op::Gte,
                value: Document::Int(200),
            },
            Condition {
                field: "tst".to_string(),
                op: Op::Lte,
                value: Document::Int(400),
            },
        ],
        sort: Some(Sort {
            field: "tst".to_string(),
            order: SortOrder::Asc,
        }),
        limit: None,
    };

    let found = pings.find(filter.clone()).unwrap();
    let tsts: Vec<i64> = found.iter().map(|p| p.tst).collect();
    assert_eq!(tsts, vec![200, 300, 400]);

    // The same query over a secondary index on `tst` (SPEC §28): only
    // the range 200..=400 is read, and the result is the same.
    assert!(pings.ensure_index("tst").unwrap());
    assert_eq!(
        pings.explain(&filter).unwrap(),
        QueryPlan::Index {
            field: "tst".to_string()
        }
    );
    assert_eq!(pings.find(filter).unwrap(), found);
}

#[test]
fn most_recent_ping_is_sort_desc_limit_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("timeseries.trunkdb")).unwrap();
    let pings = db.collection::<LocationPing>("pings");

    pings.insert(ping("phone-1", 100, 52.5, 13.4, 90)).unwrap();
    pings.insert(ping("phone-1", 300, 52.6, 13.5, 85)).unwrap();
    pings
        .insert(ping("phone-1", 200, 52.55, 13.45, 88))
        .unwrap();

    let filter = Filter {
        sort: Some(Sort {
            field: "tst".to_string(),
            order: SortOrder::Desc,
        }),
        limit: Some(1),
        ..Default::default()
    };

    let found = pings.find(filter).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].tst, 300);
}

#[test]
fn dynamically_built_optional_filters_are_anded_together() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("timeseries.trunkdb")).unwrap();
    let pings = db.collection::<LocationPing>("pings");

    pings.insert(ping("phone-1", 100, 52.5, 13.4, 90)).unwrap();
    pings.insert(ping("phone-1", 200, 52.5, 13.4, 15)).unwrap();
    pings.insert(ping("phone-2", 200, 52.5, 13.4, 15)).unwrap();

    // Mirrors how a real query endpoint builds `conditions` incrementally
    // from whichever optional query params were actually supplied.
    let tid_param: Option<&str> = Some("phone-1");
    let max_batt_param: Option<i64> = Some(50);

    let mut conditions = Vec::new();
    if let Some(tid) = tid_param {
        conditions.push(Condition {
            field: "tid".to_string(),
            op: Op::Eq,
            value: Document::String(tid.to_string()),
        });
    }
    if let Some(max_batt) = max_batt_param {
        conditions.push(Condition {
            field: "batt".to_string(),
            op: Op::Lte,
            value: Document::Int(max_batt),
        });
    }

    let found = pings
        .find(Filter {
            conditions,
            ..Default::default()
        })
        .unwrap();

    assert_eq!(found, vec![ping("phone-1", 200, 52.5, 13.4, 15)]);
}

#[test]
fn pings_and_geofences_coexist_as_independent_collections() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("timeseries.trunkdb")).unwrap();
    let pings = db.collection::<LocationPing>("pings");
    let geofences = db.collection::<Geofence>("geofences");

    pings.insert(ping("phone-1", 100, 52.5, 13.4, 90)).unwrap();
    geofences
        .insert(Geofence {
            zone_key: "home".to_string(),
            name: "Home".to_string(),
            lat: 52.5,
            lon: 13.4,
            radius: 100,
        })
        .unwrap();

    assert_eq!(pings.find(Filter::default()).unwrap().len(), 1);
    assert_eq!(geofences.find(Filter::default()).unwrap().len(), 1);
}
