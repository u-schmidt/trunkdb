//! A sort by a key of the application's own (SPEC §154):
//! `Filter::sort_by_key` in every read and write, against a model of
//! the order it promises; the fields its closure gets; a panic in it;
//! and its text, which doesn't read back.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use trunkdb::query::{Aggregate, Filter};
use trunkdb::{Collection, Database, DocId, Document, Error};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("key.trunkdb")).unwrap();
    (dir, db)
}

fn doc(text: &str) -> Document {
    text.parse().unwrap()
}

/// 300 places on a grid of 20 by 20, so many lie as far from a point
/// as others: `lat` and `lon` whole numbers, `kind` "a", "b" or "c",
/// `n` the number. Every 17th has no `lon`. An index on `lat`.
fn places(db: &Database) -> Collection<Document> {
    let places = db.collection::<Document>("places");
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    for n in 0..300 {
        let kind = ["a", "b", "c"][next(3) as usize];
        let (lat, lon) = (next(20), next(20));
        let text = match n % 17 {
            0 => format!(r#"{{"n": {n}, "kind": "{kind}", "lat": {lat}}}"#),
            _ => format!(r#"{{"n": {n}, "kind": "{kind}", "lat": {lat}, "lon": {lon}}}"#),
        };
        places.insert(doc(&text)).unwrap();
    }
    places.ensure_index("lat").unwrap();
    places
}

/// The distance from (7, 9); none without `lon`, NaN on `lat` 13,
/// which leaves those out too.
fn distance(doc: &Document) -> Option<f64> {
    let lat = doc.get("lat")?.as_f64()?;
    let lon = doc.get("lon")?.as_f64()?;
    if lat == 13.0 {
        return Some(f64::NAN);
    }
    Some((lat - 7.0).hypot(lon - 9.0))
}

fn id_of(doc: &Document) -> DocId {
    doc.get("_id").and_then(Document::as_id).unwrap()
}

fn ids(docs: &[Document]) -> Vec<DocId> {
    docs.iter().map(id_of).collect()
}

fn number(doc: &Document, field: &str) -> i64 {
    doc.get(field).and_then(Document::as_i64).unwrap()
}

/// What a filter is, for the model: its parts, as the test sets them.
#[derive(Debug, Clone, Copy)]
struct Shape {
    band: bool,
    kind_a: bool,
    desc: bool,
    then: Then,
    skip: usize,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
enum Then {
    Nothing,
    KindAsc,
    NDesc,
    /// A field an index holds: the index's order still mustn't serve.
    LatDesc,
}

impl Shape {
    fn filter(self) -> Filter {
        let mut filter = Filter::new();
        if self.band {
            filter = filter.gte("lat", 4).lte("lat", 11); // what the index reads
        }
        if self.kind_a {
            filter = filter.eq("kind", "a");
        }
        filter = match self.desc {
            false => filter.sort_by_key(["lat", "lon"], distance),
            true => filter.sort_by_key_desc(["lat", "lon"], distance),
        };
        filter = match self.then {
            Then::Nothing => filter,
            Then::KindAsc => filter.then_asc("kind"),
            Then::NDesc => filter.then_desc("n"),
            Then::LatDesc => filter.then_desc("lat"),
        };
        filter.skip(self.skip).limit(self.limit)
    }

    /// The order promised, worked out apart: every document, those the
    /// conditions and the key keep, by the key, then the field, then by
    /// id; then the skip and the limit.
    fn expected(self, all: &[Document]) -> Vec<DocId> {
        let mut kept: Vec<(f64, &Document)> = all
            .iter()
            .filter(|d| !self.band || (4..=11).contains(&number(d, "lat")))
            .filter(|d| !self.kind_a || d.get("kind").and_then(Document::as_str) == Some("a"))
            .filter_map(|d| distance(d).filter(|k| !k.is_nan()).map(|k| (k, d)))
            .collect();
        kept.sort_by(|(ka, a), (kb, b)| {
            let by_key = ka.partial_cmp(kb).unwrap();
            let by_key = if self.desc { by_key.reverse() } else { by_key };
            let then = match self.then {
                Then::Nothing => std::cmp::Ordering::Equal,
                Then::KindAsc => a
                    .get("kind")
                    .and_then(Document::as_str)
                    .cmp(&b.get("kind").and_then(Document::as_str)),
                Then::NDesc => number(b, "n").cmp(&number(a, "n")),
                Then::LatDesc => number(b, "lat").cmp(&number(a, "lat")),
            };
            by_key.then(then).then_with(|| id_of(a).cmp(&id_of(b)))
        });
        kept.into_iter()
            .skip(self.skip)
            .take(self.limit.unwrap_or(usize::MAX))
            .map(|(_, d)| id_of(d))
            .collect()
    }
}

fn shapes() -> Vec<Shape> {
    let mut shapes = Vec::new();
    for band in [false, true] {
        for kind_a in [false, true] {
            for desc in [false, true] {
                for then in [Then::Nothing, Then::KindAsc, Then::NDesc, Then::LatDesc] {
                    for skip in [0, 3] {
                        for limit in [None, Some(0), Some(1), Some(7), Some(40), Some(1000)] {
                            shapes.push(Shape {
                                band,
                                kind_a,
                                desc,
                                then,
                                skip,
                                limit,
                            });
                        }
                    }
                }
            }
        }
    }
    shapes
}

#[test]
fn every_read_orders_as_the_key_says() {
    let (_dir, db) = open();
    let places = places(&db);
    let all = places.find(Filter::new()).unwrap();
    for shape in shapes() {
        let expected = shape.expected(&all);
        let found = places.find(shape.filter()).unwrap();
        assert_eq!(ids(&found), expected, "{shape:?}");
        assert_eq!(
            places.count(shape.filter()).unwrap(),
            expected.len(),
            "{shape:?}"
        );
        let streamed: Vec<Document> = (places.cursor(shape.filter()).unwrap())
            .collect::<trunkdb::Result<_>>()
            .unwrap();
        assert_eq!(ids(&streamed), expected, "cursor {shape:?}");
        let first = places.find_one(shape.filter()).unwrap();
        assert_eq!(
            first.map(|d| id_of(&d)),
            expected.first().copied(),
            "{shape:?}"
        );
        let selected = places.find(shape.filter().select(["kind"])).unwrap();
        assert_eq!(ids(&selected), expected, "select {shape:?}");
        // In memory, as `find` orders them.
        assert_eq!(
            ids(&shape.filter().apply(all.clone())),
            expected,
            "apply {shape:?}"
        );
        // As of a snapshot, as now.
        let view = db.snapshot().unwrap();
        let viewed = view
            .collection::<Document>("places")
            .find(shape.filter())
            .unwrap();
        assert_eq!(ids(&viewed), expected, "snapshot {shape:?}");
    }
}

#[test]
fn the_nearest_ten_as_the_docs_show_it() {
    let (_dir, db) = open();
    let places = places(&db);
    let (lat, lon) = (7.0, 9.0);
    let nearest = Filter::new()
        .gte("lat", lat - 3.0)
        .lte("lat", lat + 3.0)
        .sort_by_key(["lat", "lon"], move |doc| {
            let a = doc.get("lat")?.as_f64()?;
            let b = doc.get("lon")?.as_f64()?;
            Some((a - lat).hypot(b - lon))
        })
        .limit(10);
    let found = places.find(nearest).unwrap();
    assert_eq!(found.len(), 10);
    let away: Vec<f64> = (found.iter())
        .map(|d| (number(d, "lat") as f64 - lat).hypot(number(d, "lon") as f64 - lon))
        .collect();
    assert!(away.windows(2).all(|w| w[0] <= w[1]), "{away:?}");
}

#[test]
fn writes_take_the_same_matches() {
    let (_dir, db) = open();
    let places = places(&db);
    let all = places.find(Filter::new()).unwrap();
    let shape = Shape {
        band: true,
        kind_a: false,
        desc: false,
        then: Then::Nothing,
        skip: 2,
        limit: Some(5),
    };
    let expected = shape.expected(&all);
    let deleted = places.delete_many(shape.filter()).unwrap();
    assert_eq!(deleted, expected.len());
    let left = ids(&places.find(Filter::new()).unwrap());
    assert!(expected.iter().all(|id| !left.contains(id)));
    assert_eq!(left.len(), all.len() - expected.len());

    let marked = places
        .update_fields(
            Filter::new()
                .sort_by_key_desc(["lat", "lon"], distance)
                .limit(3),
            &trunkdb::query::Update::new().set("far", true),
        )
        .unwrap();
    assert_eq!(marked, 3);
    let far = places.find(Filter::new().eq("far", true)).unwrap();
    let farthest = Shape {
        band: false,
        kind_a: false,
        desc: true,
        then: Then::Nothing,
        skip: 0,
        limit: Some(3),
    };
    let mut expected = farthest.expected(&places.find(Filter::new()).unwrap());
    expected.sort();
    let mut far = ids(&far);
    far.sort();
    assert_eq!(far, expected);
}

#[test]
fn an_aggregate_takes_the_matches_the_key_picks() {
    let (_dir, db) = open();
    let places = places(&db);
    let nearest = || {
        Filter::new()
            .sort_by_key(["lat", "lon"], distance)
            .limit(12)
    };
    let rows = places
        .aggregate(
            nearest(),
            &Aggregate::new().group_by("kind").count().named("count"),
        )
        .unwrap();
    let counted: i64 = rows.iter().map(|row| number(row, "count")).sum();
    assert_eq!(counted, 12);
    let by_kind = |kind: &str| {
        (places.find(nearest()).unwrap().iter())
            .filter(|d| d.get("kind").and_then(Document::as_str) == Some(kind))
            .count() as i64
    };
    for row in &rows {
        let kind = row.get("kind").and_then(Document::as_str).unwrap();
        assert_eq!(number(row, "count"), by_kind(kind), "{kind}");
    }
}

#[test]
fn the_closure_gets_the_fields_it_names_and_no_others() {
    let (_dir, db) = open();
    let places = places(&db);
    let other = Arc::new(AtomicBool::new(false));
    let seen = other.clone();
    let key = move |doc: &Document| {
        let fields: Vec<&str> = (doc.as_object().unwrap().keys())
            .map(String::as_str)
            .collect();
        if fields.iter().any(|f| !["lat", "lon"].contains(f)) {
            seen.store(true, Ordering::Relaxed);
        }
        distance(doc)
    };
    // Conditions and a field after the key name other fields; whole
    // documents are read where the band bounds an index.
    let filter = Filter::new()
        .gte("lat", 2)
        .eq("kind", "b")
        .sort_by_key(["lat", "lon"], key)
        .then_desc("n")
        .limit(5);
    assert_eq!(places.find(filter.clone()).unwrap().len(), 5);
    places.count(filter.clone()).unwrap();
    filter.apply(places.find(Filter::new()).unwrap());
    assert!(!other.load(Ordering::Relaxed), "a field it didn't name");
}

/// `_id` only where named, unlike a `select` (SPEC §154.1): on a scan
/// and on a read through an index alike.
#[test]
fn the_closure_gets_the_id_only_if_it_names_it() {
    let (_dir, db) = open();
    let places = places(&db);
    for (fields, wanted) in [
        (&["lat", "lon"][..], false),
        (&["_id", "lat", "lon"][..], true),
    ] {
        for filter in [Filter::new(), Filter::new().gte("lat", 2)] {
            let calls = Arc::new(AtomicUsize::new(0));
            let with_id = Arc::new(AtomicUsize::new(0));
            let (counted, seen) = (calls.clone(), with_id.clone());
            let key = move |doc: &Document| {
                counted.fetch_add(1, Ordering::Relaxed);
                if doc.get("_id").is_some() {
                    seen.fetch_add(1, Ordering::Relaxed);
                }
                distance(doc)
            };
            places
                .find(filter.sort_by_key(fields.to_vec(), key).limit(3))
                .unwrap();
            let expected = if wanted {
                calls.load(Ordering::Relaxed)
            } else {
                0
            };
            assert!(calls.load(Ordering::Relaxed) > 0);
            assert_eq!(with_id.load(Ordering::Relaxed), expected, "{fields:?}");
        }
    }
}

#[test]
fn every_match_is_keyed_once_and_the_rest_not_at_all() {
    let (_dir, db) = open();
    let places = places(&db);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let filter = Filter::new()
        .gte("lat", 4)
        .lte("lat", 11)
        .sort_by_key(["lat", "lon"], move |doc| {
            counted.fetch_add(1, Ordering::Relaxed);
            distance(doc)
        })
        .limit(3);
    places.find(filter).unwrap();
    let in_band = places
        .count(Filter::new().gte("lat", 4).lte("lat", 11))
        .unwrap();
    assert_eq!(
        calls.load(Ordering::Relaxed),
        in_band,
        "once for each match, none for the rest"
    );
}

fn panics_at(at: i64) -> Filter {
    Filter::new().sort_by_key(["n", "lat", "lon"], move |doc| {
        if doc.get("n").and_then(Document::as_i64) == Some(at) {
            panic!("no distance for {at}");
        }
        distance(doc)
    })
}

fn panicked<T: std::fmt::Debug>(result: trunkdb::Result<T>) -> String {
    match result {
        Err(Error::ClosurePanicked { message, .. }) => message,
        other => panic!("expected a caught panic, got {other:?}"),
    }
}

#[test]
fn a_panic_fails_the_call_and_the_database_goes_on() {
    let (_dir, db) = open();
    let places = places(&db);
    let before = places.count(Filter::new()).unwrap();
    assert_eq!(
        panicked(places.find(panics_at(40).limit(3))),
        "no distance for 40"
    );
    assert_eq!(panicked(places.count(panics_at(40))), "no distance for 40");
    assert_eq!(
        panicked(places.cursor(panics_at(40)).map(|_| ())),
        "no distance for 40"
    );
    assert_eq!(
        panicked(places.delete_many(panics_at(40).limit(5))),
        "no distance for 40"
    );
    assert_eq!(
        places.count(Filter::new()).unwrap(),
        before,
        "nothing deleted"
    );
    // The writer's lock wasn't poisoned.
    places
        .insert(doc(r#"{"n": 1000, "kind": "a", "lat": 1, "lon": 1}"#))
        .unwrap();
    assert_eq!(places.count(Filter::new()).unwrap(), before + 1);
    let held =
        std::panic::catch_unwind(|| panics_at(40).apply(places.find(Filter::new()).unwrap()));
    assert!(held.is_err(), "in memory the panic goes on");
}

#[test]
fn it_has_text_but_none_that_reads_back() {
    let filter = Filter::new()
        .gte("lat", 4)
        .sort_by_key_desc(["lat", "pos.lon"], distance)
        .then_asc("kind")
        .limit(3);
    let text = filter.to_string();
    assert_eq!(
        text,
        "lat >= 4 order by key(lat, pos.lon) desc, kind limit 3"
    );
    let refused = text.parse::<Filter>().unwrap_err().to_string();
    assert!(
        refused.contains("`key(…)` is a closure in code"),
        "{refused}"
    );
    // A field named `key` still sorts.
    assert!("order by key, n desc".parse::<Filter>().is_ok());
}

#[test]
fn a_later_sort_replaces_it_and_it_replaces_an_earlier_one() {
    let (_dir, db) = open();
    let places = places(&db);
    let by_n = places.find(Filter::new().sort_asc("n").limit(5)).unwrap();
    let replaced = Filter::new()
        .sort_by_key(["lat", "lon"], distance)
        .sort_asc("n")
        .limit(5);
    assert_eq!(ids(&places.find(replaced).unwrap()), ids(&by_n));
    let all = places.find(Filter::new()).unwrap();
    let keyed = Filter::new()
        .sort_desc("n")
        .sort_by_key(["lat", "lon"], distance)
        .limit(5);
    let shape = Shape {
        band: false,
        kind_a: false,
        desc: false,
        then: Then::Nothing,
        skip: 0,
        limit: Some(5),
    };
    assert_eq!(ids(&places.find(keyed).unwrap()), shape.expected(&all));
}

#[test]
fn explain_counts_it_as_a_sort_in_memory() {
    let (_dir, db) = open();
    let places = places(&db);
    let explained = places
        .explain_run(
            Filter::new()
                .gte("lat", 4)
                .sort_by_key(["lat", "lon"], distance)
                .then_asc("n")
                .limit(3),
        )
        .unwrap();
    let sort = explained.sort.expect("a sort");
    assert_eq!((sort.from_index, sort.in_memory), (0, 2));
}
