//! Live queries (SPEC §138): `Collection::watch`, a filter kept as a
//! subscription, reporting what comes to match it, what no longer does
//! and what is written and still does; with a sort and a limit, a list
//! of the first matches in order (SPEC §145).

use serde::{Deserialize, Serialize};
use std::time::Duration;
use trunkdb::query::{Condition, Filter, Update};
use trunkdb::{Collection, Database, DocId, Document, Error, Live, LiveQuery, OpenOptions};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("live.trunkdb")).unwrap();
    (dir, db)
}

fn doc(text: &str) -> Document {
    text.parse().unwrap()
}

fn vans(db: &Database) -> Collection<Document> {
    db.collection::<Document>("vans")
}

fn van(name: &str, zone: &str) -> Document {
    doc(&format!(
        r#"{{"name": "{name}", "zone": "{zone}", "speed": 0}}"#
    ))
}

fn in_depot() -> Filter {
    Filter::new().eq("zone", "depot")
}

/// An event as `(kind, id, the document's zone)`: `None` for a document
/// gone.
type Short = (&'static str, DocId, Option<String>);

fn short(live: Live<Document>) -> Short {
    let zone = |doc: &Document| doc.get("zone").and_then(Document::as_str).map(String::from);
    match &live {
        Live::Entered { id, doc, .. } => ("entered", *id, zone(doc)),
        Live::Left { id, doc, .. } => ("left", *id, doc.as_ref().and_then(zone)),
        Live::Changed { id, doc, .. } => ("changed", *id, zone(doc)),
        _ => unreachable!(),
    }
}

/// Every event waiting now: a live query is checked as its commit
/// commits, so they are there once the write returns.
fn waiting(live: &LiveQuery<Document>) -> Vec<Short> {
    std::iter::from_fn(|| live.try_recv())
        .map(|event| short(event.unwrap()))
        .collect()
}

fn ev(kind: &'static str, id: DocId, zone: Option<&str>) -> Short {
    (kind, id, zone.map(String::from))
}

fn move_to(vans: &Collection<Document>, id: &DocId, zone: &str) {
    let name = vans.get(id).unwrap().unwrap();
    let name = name.get("name").and_then(Document::as_str).unwrap();
    vans.update(id, van(name, zone)).unwrap();
}

#[test]
fn what_matches_at_the_start_comes_first() {
    let (_dir, db) = open();
    let vans = vans(&db);
    let a = vans.insert(van("a", "depot")).unwrap();
    vans.insert(van("b", "road")).unwrap();
    let c = vans.insert(van("c", "depot")).unwrap();

    let live = vans.watch(in_depot()).unwrap();
    assert_eq!(
        waiting(&live),
        [
            ev("entered", a, Some("depot")),
            ev("entered", c, Some("depot"))
        ]
    );
    // A collection not there yet: nothing at the start, then its first
    // document.
    let later = db.collection::<Document>("later");
    let watching = later.watch(in_depot()).unwrap();
    assert_eq!(waiting(&watching), []);
    let d = later.insert(van("d", "depot")).unwrap();
    assert_eq!(waiting(&watching), [ev("entered", d, Some("depot"))]);
}

#[test]
fn each_write_reports_entered_left_or_changed() {
    let (_dir, db) = open();
    let vans = vans(&db);
    let live = vans.watch(in_depot()).unwrap();

    // Inserted outside, and inside.
    let a = vans.insert(van("a", "road")).unwrap();
    assert_eq!(waiting(&live), []);
    let b = vans.insert(van("b", "depot")).unwrap();
    assert_eq!(waiting(&live), [ev("entered", b, Some("depot"))]);
    // Written and still matching; written outside, never matching.
    vans.update_fields(
        Filter::new().eq("name", "b"),
        &Update::new().set("speed", 5),
    )
    .unwrap();
    assert_eq!(waiting(&live), [ev("changed", b, Some("depot"))]);
    move_to(&vans, &a, "yard");
    assert_eq!(waiting(&live), []);
    // In, and out again with the document as it is now.
    move_to(&vans, &a, "depot");
    assert_eq!(waiting(&live), [ev("entered", a, Some("depot"))]);
    move_to(&vans, &a, "road");
    assert_eq!(waiting(&live), [ev("left", a, Some("road"))]);
    // Deleted inside: left, nothing to hand out; deleted outside: nothing.
    vans.delete(&b).unwrap();
    assert_eq!(waiting(&live), [ev("left", b, None)]);
    vans.delete(&a).unwrap();
    assert_eq!(waiting(&live), []);

    // `update_many`, `upsert`, `delete_many`: each document its event.
    let c = vans.insert(van("c", "road")).unwrap();
    let d = vans.insert(van("d", "depot")).unwrap();
    assert_eq!(waiting(&live), [ev("entered", d, Some("depot"))]);
    vans.update_many(Filter::new(), |doc| {
        if let Document::Object(map) = doc {
            let zone = if map.get("zone") == Some(&Document::from("depot")) {
                "road"
            } else {
                "depot"
            };
            map.insert("zone".into(), Document::from(zone));
        }
    })
    .unwrap();
    let mut events = waiting(&live);
    events.sort();
    let mut expected = vec![ev("entered", c, Some("depot")), ev("left", d, Some("road"))];
    expected.sort();
    assert_eq!(events, expected);
    vans.upsert(Filter::new().eq("name", "e"), van("e", "depot"))
        .unwrap();
    let e = match &waiting(&live)[..] {
        [(kind, id, zone)] if *kind == "entered" && zone.as_deref() == Some("depot") => *id,
        other => panic!("{other:?}"),
    };
    vans.delete_many(Filter::new()).unwrap();
    let mut events = waiting(&live);
    events.sort();
    let mut expected = vec![ev("left", c, None), ev("left", e, None)];
    expected.sort();
    assert_eq!(events, expected);
}

#[test]
fn a_batch_is_checked_by_its_net_effect() {
    let (_dir, db) = open();
    let vans = vans(&db);
    let a = vans.insert(van("a", "road")).unwrap();
    let live = vans.watch(in_depot()).unwrap();

    // In and out in one batch: nothing, as nothing matches after it.
    let mut batch = db.batch();
    batch.update(&vans, &a, van("a", "depot")).unwrap();
    batch.update(&vans, &a, van("a", "road")).unwrap();
    let b = batch.insert(&vans, van("b", "depot")).unwrap();
    batch.delete(&vans, &b);
    batch.commit().unwrap();
    assert_eq!(waiting(&live), []);

    // Inserted and moved: one event, as the batch left it.
    let mut batch = db.batch();
    let c = batch.insert(&vans, van("c", "road")).unwrap();
    batch.update(&vans, &c, van("c", "depot")).unwrap();
    batch.commit().unwrap();
    assert_eq!(waiting(&live), [ev("entered", c, Some("depot"))]);
}

#[test]
fn no_transition_is_lost_to_a_late_reader() {
    // Read only after all of it: each commit was checked against the
    // state it left, not the state at the read.
    let (_dir, db) = open();
    let vans = vans(&db);
    let a = vans.insert(van("a", "road")).unwrap();
    let live = vans.watch(in_depot()).unwrap();
    move_to(&vans, &a, "depot");
    move_to(&vans, &a, "road");
    move_to(&vans, &a, "depot");
    vans.update_fields(Filter::new(), &Update::new().set("zone", "yard"))
        .unwrap();
    assert_eq!(
        waiting(&live),
        [
            ev("entered", a, Some("depot")),
            ev("left", a, Some("road")),
            ev("entered", a, Some("depot")),
            ev("left", a, Some("yard")),
        ]
    );
}

#[test]
fn other_collections_are_not_reported() {
    let (_dir, db) = open();
    let vans = vans(&db);
    let trucks = db.collection::<Document>("trucks");
    let live = vans.watch(in_depot()).unwrap();
    // Watched too: each commit is checked, and a truck goes to its own.
    let of_trucks = trucks.watch(in_depot()).unwrap();
    let t = trucks.insert(van("t", "depot")).unwrap();
    trucks.delete(&t).unwrap();
    assert!(db.drop_collection("trucks").unwrap());
    assert_eq!(waiting(&live), []);
    assert_eq!(
        waiting(&of_trucks),
        [ev("entered", t, Some("depot")), ev("left", t, None)]
    );
    // One batch across both: each its own.
    let mut batch = db.batch();
    let v = batch.insert(&vans, van("v", "depot")).unwrap();
    let u = batch.insert(&trucks, van("u", "depot")).unwrap();
    batch.commit().unwrap();
    assert_eq!(waiting(&live), [ev("entered", v, Some("depot"))]);
    assert_eq!(waiting(&of_trucks), [ev("entered", u, Some("depot"))]);
}

#[test]
fn select_cuts_what_is_handed_out() {
    let (_dir, db) = open();
    let vans = vans(&db);
    let a = vans.insert(van("a", "depot")).unwrap();
    let live = vans.watch(in_depot().select(["name"])).unwrap();
    let only_name = |live: Live<Document>| match live {
        Live::Entered { doc, .. } | Live::Changed { doc, .. } => doc,
        Live::Left { doc, .. } => doc.unwrap(),
        _ => unreachable!(),
    };
    // At the start, and after: `_id` and `name`, the zone that decided it
    // not handed out.
    for event in [live.try_recv(), {
        move_to(&vans, &a, "road");
        live.try_recv()
    }] {
        let doc = only_name(event.unwrap().unwrap());
        assert_eq!(doc.get("name").and_then(Document::as_str), Some("a"));
        assert_eq!(doc.get("_id").and_then(Document::as_id), Some(a));
        assert_eq!(doc.get("zone"), None);
        assert_eq!(doc.get("speed"), None);
    }
}

#[test]
fn a_skip_or_a_limit_without_a_sort_is_refused() {
    let (_dir, db) = open();
    let vans = vans(&db);
    for (filter, reason) in [
        (in_depot().sort_asc("name").skip(1), "a skip"),
        (in_depot().skip(1), "a skip"),
        (in_depot().limit(10), "a limit without a sort"),
    ] {
        match vans.watch(filter) {
            Err(Error::NotLive { reason: got, .. }) => assert_eq!(got, reason),
            other => panic!("not refused: {other:?}"),
        }
    }
    // A hint steers the first read only; one that can't be followed
    // fails the watch, as it fails a find.
    vans.ensure_index("zone").unwrap();
    let a = vans.insert(van("a", "depot")).unwrap();
    let live = vans.watch(in_depot().use_index("zone")).unwrap();
    assert_eq!(waiting(&live), [ev("entered", a, Some("depot"))]);
    assert!(matches!(
        vans.watch(in_depot().use_index("speed")),
        Err(Error::Hint { .. })
    ));
}

#[test]
fn a_fence_entered_and_left_through_a_custom_condition() {
    // The time-series workload (§5.1): a position kept in one document,
    // the fence the application's own geometry.
    let (_dir, db) = open();
    let positions = db.collection::<Document>("positions");
    let at = |lat: f64, lon: f64| {
        doc(&format!(
            r#"{{"device": "van-7", "pos": {{"lat": {lat}, "lon": {lon}}}}}"#
        ))
    };
    let id = positions.insert(at(0.0, 0.0)).unwrap();
    let fence = |lat: f64, lon: f64| (1.0..2.0).contains(&lat) && (1.0..2.0).contains(&lon);
    let inside = Filter::new()
        .eq("device", "van-7")
        .and(Condition::custom(["pos"], move |doc| {
            match (
                doc.get("pos.lat").and_then(Document::as_f64),
                doc.get("pos.lon").and_then(Document::as_f64),
            ) {
                (Some(lat), Some(lon)) => fence(lat, lon),
                _ => false,
            }
        }));
    let live = positions.watch(inside).unwrap();
    let mut seen = Vec::new();
    for (lat, lon) in [(0.5, 0.5), (1.5, 1.5), (1.6, 1.4), (2.5, 1.5), (1.2, 1.2)] {
        positions.update(&id, at(lat, lon)).unwrap();
        while let Some(event) = live.try_recv() {
            seen.push(match event.unwrap() {
                Live::Entered { .. } => "entered",
                Live::Left { .. } => "left",
                Live::Changed { .. } => "changed",
                _ => unreachable!(),
            });
        }
    }
    assert_eq!(seen, ["entered", "changed", "left", "entered"]);
}

#[test]
fn a_panic_in_a_custom_condition_ends_that_query_alone() {
    let (_dir, db) = open();
    let vans = vans(&db);
    let a = vans.insert(van("a", "depot")).unwrap();
    let fragile = in_depot().and(Condition::custom(["speed"], |doc| {
        if doc.get("speed").and_then(Document::as_i64) == Some(13) {
            panic!("unlucky speed");
        }
        true
    }));
    let doomed = vans.watch(fragile).unwrap();
    let steady = vans.watch(in_depot()).unwrap();
    let changes = db.subscribe().unwrap();
    assert_eq!(waiting(&doomed), [ev("entered", a, Some("depot"))]);
    assert_eq!(waiting(&steady), [ev("entered", a, Some("depot"))]);

    vans.update_fields(Filter::new(), &Update::new().set("speed", 13))
        .unwrap();
    // The query that panicked: the error, then the end.
    match doomed.try_recv() {
        Some(Err(Error::ClosurePanicked { message, .. })) => assert_eq!(message, "unlucky speed"),
        other => panic!("not ClosurePanicked: {other:?}"),
    }
    assert!(doomed.try_recv().is_none());
    vans.update_fields(Filter::new(), &Update::new().set("speed", 1))
        .unwrap();
    assert!(doomed.try_recv().is_none(), "ended");
    // The commit stands, the others go on, and so does the database.
    assert_eq!(
        waiting(&steady),
        [
            ev("changed", a, Some("depot")),
            ev("changed", a, Some("depot"))
        ]
    );
    assert_eq!(changes.try_iter_len(), 2);
    let stored = vans.get(&a).unwrap().unwrap();
    assert_eq!(stored.get("speed").and_then(Document::as_i64), Some(1));
    db.check().unwrap();

    // A panic in the first read fails the watch itself.
    vans.update_fields(Filter::new(), &Update::new().set("speed", 13))
        .unwrap();
    let fragile = Filter::new().and(Condition::custom(["speed"], |_| panic!("at the start")));
    assert!(matches!(
        vans.watch(fragile),
        Err(Error::ClosurePanicked { .. })
    ));
}

/// How many reports wait on `Changes`, taking them.
trait CountReports {
    fn try_iter_len(&self) -> usize;
}

impl CountReports for trunkdb::Changes {
    fn try_iter_len(&self) -> usize {
        std::iter::from_fn(|| self.try_recv()).count()
    }
}

#[test]
fn a_dropped_collection_leaves_its_matches() {
    let (_dir, db) = open();
    let vans = vans(&db);
    let a = vans.insert(van("a", "depot")).unwrap();
    let b = vans.insert(van("b", "depot")).unwrap();
    vans.insert(van("c", "road")).unwrap();
    let live = vans.watch(in_depot()).unwrap();
    waiting(&live);
    assert!(db.drop_collection("vans").unwrap());
    assert_eq!(waiting(&live), [ev("left", a, None), ev("left", b, None)]);
    // It goes on, for the collection made again: a document under a
    // dropped one's id is new to it.
    let d = vans.insert(van("d", "depot")).unwrap();
    assert_eq!(waiting(&live), [ev("entered", d, Some("depot"))]);
    let mut again = van("a", "depot");
    if let Document::Object(map) = &mut again {
        map.insert("_id".into(), Document::Id(a));
    }
    assert_eq!(vans.insert(again).unwrap(), a);
    assert_eq!(waiting(&live), [ev("entered", a, Some("depot"))]);
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Van {
    name: String,
    zone: String,
    speed: i64,
}

#[test]
fn a_typed_query_converts_as_it_receives() {
    let (_dir, db) = open();
    let typed = db.collection::<Van>("vans");
    let a = typed
        .insert(Van {
            name: "a".into(),
            zone: "depot".into(),
            speed: 0,
        })
        .unwrap();
    let live = typed.watch(in_depot()).unwrap();
    match live.try_recv() {
        Some(Ok(Live::Entered { id, doc, .. })) => {
            assert_eq!(id, a);
            assert_eq!(doc.name, "a");
        }
        other => panic!("{other:?}"),
    }
    // A document that doesn't convert: an `Err` for it, and the query
    // goes on.
    let untyped = vans(&db);
    let bad = untyped
        .insert(doc(r#"{"name": "b", "zone": "depot", "speed": "fast"}"#))
        .unwrap();
    assert!(matches!(live.try_recv(), Some(Err(Error::Document(_)))));
    untyped.delete(&bad).unwrap();
    assert!(matches!(
        live.try_recv(),
        Some(Ok(Live::Left { id, doc: None, .. })) if id == bad
    ));
    typed
        .update(
            &a,
            Van {
                name: "a".into(),
                zone: "road".into(),
                speed: 3,
            },
        )
        .unwrap();
    match live.try_recv() {
        Some(Ok(Live::Left {
            id, doc: Some(van), ..
        })) => {
            assert_eq!(id, a);
            assert_eq!(van.zone, "road");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_reader_may_write_on_its_own_thread() {
    // Each van that enters the depot is stopped by the reader: a write of
    // its own, from the thread that reads the query, while others write.
    let (_dir, db) = open();
    let vans = vans(&db);
    let live = vans.watch(in_depot()).unwrap();
    let reader = {
        let vans = vans.clone();
        std::thread::spawn(move || {
            let mut entered = 0;
            while entered < 20 {
                match live
                    .recv_timeout(Duration::from_secs(10))
                    .expect("an event")
                    .unwrap()
                {
                    Live::Entered { id, .. } => {
                        entered += 1;
                        vans.update_fields(
                            Filter::new().eq("_id", id),
                            &Update::new().set("speed", 0),
                        )
                        .unwrap();
                    }
                    Live::Changed { .. } => {}
                    other => panic!("{other:?}"),
                }
            }
        })
    };
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let vans = vans.clone();
            std::thread::spawn(move || {
                for n in 0..5 {
                    let mut moving = van(&format!("{w}-{n}"), "depot");
                    if let Document::Object(map) = &mut moving {
                        map.insert("speed".into(), Document::Int(9));
                    }
                    vans.insert(moving).unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    reader.join().unwrap();
    let stopped = vans.count(Filter::new().eq("speed", 0)).unwrap();
    assert_eq!(stopped, 20);
}

#[test]
fn iterating_ends_when_the_database_is_dropped() {
    let (dir, db) = open();
    let vans = vans(&db);
    vans.insert(van("a", "depot")).unwrap();
    let live = vans.watch(in_depot()).unwrap();
    vans.insert(van("b", "depot")).unwrap();
    drop(vans);
    drop(db);
    assert_eq!(live.count(), 2, "the events waiting, then the end");

    // Read-only: the state at the start, and nothing after, as nothing
    // commits.
    let db = Database::open_with(
        dir.path().join("live.trunkdb"),
        OpenOptions::default().read_only(true),
    )
    .unwrap();
    let live = db.collection::<Document>("vans").watch(in_depot()).unwrap();
    assert_eq!(waiting(&live).len(), 2);
}

// Sorted live queries (SPEC §145).

fn racer(name: &str, speed: i64) -> Document {
    doc(&format!(
        r#"{{"name": "{name}", "zone": "road", "speed": {speed}}}"#
    ))
}

fn set_speed(vans: &Collection<Document>, id: &DocId, speed: i64) {
    vans.update_fields(Filter::new().id(*id), &Update::new().set("speed", speed))
        .unwrap();
}

/// An event with its places: `(kind, id, from, at)`.
type Placed = (&'static str, DocId, Option<usize>, Option<usize>);

fn placed(live: &Live<Document>) -> Placed {
    match live {
        Live::Entered { id, at, .. } => ("entered", *id, None, *at),
        Live::Left { id, from, .. } => ("left", *id, *from, None),
        Live::Changed { id, from, at, .. } => ("changed", *id, *from, *at),
        _ => unreachable!(),
    }
}

fn placed_waiting(live: &LiveQuery<Document>) -> Vec<Placed> {
    std::iter::from_fn(|| live.try_recv())
        .map(|event| placed(&event.unwrap()))
        .collect()
}

/// A list kept as a screen keeps it: each event applied in turn, its
/// places checked against the list as the events before left it.
fn apply(list: &mut Vec<DocId>, live: &Live<Document>) {
    match live {
        Live::Entered { id, at, .. } => {
            let at = at.expect("sorted: a place");
            assert!(at <= list.len(), "put in past the end");
            list.insert(at, *id);
        }
        Live::Left { id, from, .. } => {
            let from = from.expect("sorted: a place");
            assert_eq!(list.get(from), Some(id), "taken out where it isn't");
            list.remove(from);
        }
        Live::Changed { id, from, at, .. } => {
            let (from, at) = (from.expect("a place"), at.expect("a place"));
            assert_eq!(list.get(from), Some(id), "moved from where it isn't");
            list.remove(from);
            assert!(at <= list.len(), "moved past the end");
            list.insert(at, *id);
        }
        _ => unreachable!(),
    }
}

fn ids(docs: Vec<Document>) -> Vec<DocId> {
    (docs.iter())
        .map(|doc| doc.get("_id").and_then(Document::as_id).unwrap())
        .collect()
}

#[test]
fn the_first_matches_come_first_in_order_with_their_places() {
    let (_dir, db) = open();
    let vans = vans(&db);
    let slow = vans.insert(racer("slow", 10)).unwrap();
    let fast = vans.insert(racer("fast", 30)).unwrap();
    let mid = vans.insert(racer("mid", 20)).unwrap();
    vans.insert(racer("slowest", 5)).unwrap();
    let fastest = Filter::new().sort_desc("speed").limit(3);
    let live = vans.watch(fastest).unwrap();
    assert_eq!(
        placed_waiting(&live),
        [
            ("entered", fast, None, Some(0)),
            ("entered", mid, None, Some(1)),
            ("entered", slow, None, Some(2)),
        ]
    );
}

#[test]
fn a_document_coming_in_pushes_the_last_out() {
    let (_dir, db) = open();
    let vans = vans(&db);
    let a = vans.insert(racer("a", 30)).unwrap();
    let b = vans.insert(racer("b", 20)).unwrap();
    let c = vans.insert(racer("c", 10)).unwrap();
    let live = vans
        .watch(Filter::new().sort_desc("speed").limit(3))
        .unwrap();
    placed_waiting(&live);

    let d = vans.insert(racer("d", 40)).unwrap();
    // The one pushed out still matches: it is handed out as it is.
    let events: Vec<Live<Document>> = std::iter::from_fn(|| live.try_recv())
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        events.iter().map(placed).collect::<Vec<_>>(),
        [("left", c, Some(2), None), ("entered", d, None, Some(0))]
    );
    assert!(matches!(&events[0], Live::Left { doc: Some(doc), .. }
        if doc.get("speed") == Some(&Document::Int(10))));

    // Written and still in, at another place: moved.
    set_speed(&vans, &b, 50);
    assert_eq!(placed_waiting(&live), [("changed", b, Some(2), Some(0))]);
    // Written in place: changed where it is.
    set_speed(&vans, &a, 35);
    assert_eq!(placed_waiting(&live), [("changed", a, Some(2), Some(2))]);
    // Behind the list, written: nothing.
    set_speed(&vans, &c, 11);
    assert_eq!(placed_waiting(&live), []);
}

#[test]
fn a_full_list_losing_one_reads_the_next_again() {
    let (_dir, db) = open();
    let vans = vans(&db);
    let a = vans.insert(racer("a", 30)).unwrap();
    let b = vans.insert(racer("b", 20)).unwrap();
    let c = vans.insert(racer("c", 10)).unwrap();
    let e = vans.insert(racer("e", 5)).unwrap();
    let live = vans
        .watch(Filter::new().sort_desc("speed").limit(3))
        .unwrap();
    placed_waiting(&live);

    // `a` falls behind `e`, which no commit wrote: it comes in.
    set_speed(&vans, &a, 1);
    assert_eq!(
        placed_waiting(&live),
        [("left", a, Some(0), None), ("entered", e, None, Some(2))]
    );
    // Deleted from the list: the next comes in, `a` again.
    vans.delete(&b).unwrap();
    assert_eq!(
        placed_waiting(&live),
        [("left", b, Some(0), None), ("entered", a, None, Some(2))]
    );
    // Fewer than the limit match: every match is in the list.
    vans.delete(&c).unwrap();
    assert_eq!(placed_waiting(&live), [("left", c, Some(0), None)]);
    vans.delete(&e).unwrap();
    assert_eq!(placed_waiting(&live), [("left", e, Some(0), None)]);
    let f = vans.insert(racer("f", 0)).unwrap();
    assert_eq!(placed_waiting(&live), [("entered", f, None, Some(1))]);
}

#[test]
fn equal_sort_values_keep_find_s_order_by_id() {
    let (_dir, db) = open();
    let vans = vans(&db);
    let a = vans.insert(racer("a", 10)).unwrap();
    let b = vans.insert(racer("b", 10)).unwrap();
    let live = vans
        .watch(Filter::new().sort_asc("speed").limit(2))
        .unwrap();
    assert_eq!(
        placed_waiting(&live),
        [("entered", a, None, Some(0)), ("entered", b, None, Some(1))]
    );
    // A third with the same speed comes after both, by its id.
    vans.insert(racer("c", 10)).unwrap();
    assert_eq!(placed_waiting(&live), []);
    // `b` written as it was stays where it was.
    vans.update(&b, racer("b", 10)).unwrap();
    assert_eq!(placed_waiting(&live), [("changed", b, Some(1), Some(1))]);
}

#[test]
fn a_sort_without_a_limit_keeps_every_match_in_order() {
    let (_dir, db) = open();
    let vans = vans(&db);
    let a = vans.insert(racer("a", 10)).unwrap();
    let live = vans.watch(Filter::new().sort_asc("name")).unwrap();
    assert_eq!(placed_waiting(&live), [("entered", a, None, Some(0))]);
    let c = vans.insert(racer("c", 10)).unwrap();
    assert_eq!(placed_waiting(&live), [("entered", c, None, Some(1))]);
    let b = vans.insert(racer("b", 10)).unwrap();
    assert_eq!(placed_waiting(&live), [("entered", b, None, Some(1))]);
    // Renamed past the others: moved to the end.
    vans.update_fields(Filter::new().id(a), &Update::new().set("name", "d"))
        .unwrap();
    assert_eq!(placed_waiting(&live), [("changed", a, Some(0), Some(2))]);
}

#[test]
fn a_dropped_collection_empties_the_list_from_its_front() {
    let (_dir, db) = open();
    let vans = vans(&db);
    let a = vans.insert(racer("a", 30)).unwrap();
    let b = vans.insert(racer("b", 20)).unwrap();
    let live = vans
        .watch(Filter::new().sort_desc("speed").limit(5))
        .unwrap();
    placed_waiting(&live);
    db.drop_collection("vans").unwrap();
    assert_eq!(
        placed_waiting(&live),
        [("left", a, Some(0), None), ("left", b, Some(0), None)]
    );
    let c = vans.insert(racer("c", 1)).unwrap();
    assert_eq!(placed_waiting(&live), [("entered", c, None, Some(0))]);
}

#[test]
fn select_cuts_a_sorted_query_but_not_its_order() {
    let (_dir, db) = open();
    let vans = vans(&db);
    let a = vans.insert(racer("a", 10)).unwrap();
    let b = vans.insert(racer("b", 20)).unwrap();
    let live = vans
        .watch(Filter::new().sort_desc("speed").limit(1).select(["name"]))
        .unwrap();
    let first = live.try_recv().unwrap().unwrap();
    assert_eq!(placed(&first), ("entered", b, None, Some(0)));
    let Live::Entered { doc, .. } = first else {
        unreachable!()
    };
    assert_eq!(doc.get("speed"), None);
    set_speed(&vans, &a, 30);
    assert_eq!(
        placed_waiting(&live),
        [("left", b, Some(0), None), ("entered", a, None, Some(0))]
    );
}

#[test]
fn a_panic_ends_a_sorted_query_alone() {
    let (_dir, db) = open();
    let vans = vans(&db);
    vans.insert(racer("a", 10)).unwrap();
    let touchy = Filter::new()
        .and(Condition::custom(["speed"], |doc| {
            assert!(doc.get("speed") != Some(&Document::Int(99)), "too fast");
            true
        }))
        .sort_desc("speed")
        .limit(2);
    let live = vans.watch(touchy).unwrap();
    let plain = vans
        .watch(Filter::new().sort_desc("speed").limit(2))
        .unwrap();
    placed_waiting(&live);
    placed_waiting(&plain);
    let b = vans.insert(racer("b", 99)).unwrap();
    assert!(matches!(
        live.try_recv(),
        Some(Err(Error::ClosurePanicked { .. }))
    ));
    assert!(live.try_recv().is_none());
    assert_eq!(placed_waiting(&plain), [("entered", b, None, Some(0))]);
}

#[test]
fn a_limit_of_nothing_reports_nothing() {
    let (_dir, db) = open();
    let vans = vans(&db);
    vans.insert(racer("a", 10)).unwrap();
    let live = vans
        .watch(Filter::new().sort_asc("speed").limit(0))
        .unwrap();
    vans.insert(racer("b", 1)).unwrap();
    assert_eq!(placed_waiting(&live), []);
}

/// Random writes and batches; after each commit, the list kept from the
/// events is what `find` gives with the same filter.
#[test]
fn the_kept_list_is_find_s_after_every_commit() {
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let filters = [
        Filter::new().sort_desc("speed").limit(3),
        Filter::new().sort_asc("speed").limit(1),
        Filter::new()
            .gte("speed", 3)
            .sort_asc("zone")
            .then_desc("speed")
            .limit(4),
        Filter::new().sort_asc("speed"),
        Filter::new().eq("zone", "a").sort_desc("_id").limit(2),
        Filter::new()
            .sort_asc("speed")
            .then_asc("_id")
            .limit(5)
            .select(["zone"]),
    ];
    for (round, filter) in filters.iter().enumerate() {
        let (_dir, db) = open();
        let vans = vans(&db);
        if round % 2 == 1 {
            vans.ensure_index("speed").unwrap();
        }
        let mut held: Vec<DocId> = Vec::new();
        let make = |next: &mut dyn FnMut(u64) -> u64| {
            let zone = ["a", "b", "c"][next(3) as usize];
            doc(&format!(r#"{{"zone": "{zone}", "speed": {}}}"#, next(8)))
        };
        for _ in 0..6 {
            held.push(vans.insert(make(&mut next)).unwrap());
        }
        let live = vans.watch(filter.clone()).unwrap();
        let mut list = Vec::new();
        let check = |list: &mut Vec<DocId>, live: &LiveQuery<Document>| {
            while let Some(event) = live.try_recv() {
                apply(list, &event.unwrap());
            }
            assert_eq!(*list, ids(vans.find(filter.clone()).unwrap()), "{filter}");
        };
        check(&mut list, &live);
        for _ in 0..100 {
            match next(5) {
                0 => held.push(vans.insert(make(&mut next)).unwrap()),
                1 if !held.is_empty() => {
                    let id = held[next(held.len() as u64) as usize];
                    set_speed(&vans, &id, next(8) as i64);
                }
                2 if !held.is_empty() => {
                    let id = held.swap_remove(next(held.len() as u64) as usize);
                    vans.delete(&id).unwrap();
                }
                3 => {
                    let mut batch = db.batch();
                    for _ in 0..1 + next(4) {
                        match next(3) {
                            0 => held.push(batch.insert(&vans, make(&mut next)).unwrap()),
                            1 if !held.is_empty() => {
                                let id = held[next(held.len() as u64) as usize];
                                batch.update(&vans, &id, make(&mut next)).unwrap();
                            }
                            _ if !held.is_empty() => {
                                let id = held.swap_remove(next(held.len() as u64) as usize);
                                batch.delete(&vans, &id);
                            }
                            _ => {}
                        }
                    }
                    batch.commit().unwrap();
                }
                _ => {
                    let zone = ["a", "b", "c"][next(3) as usize];
                    vans.update_fields(
                        Filter::new().eq("zone", zone),
                        &Update::new().inc("speed", 1),
                    )
                    .unwrap();
                }
            }
            check(&mut list, &live);
        }
    }
}
