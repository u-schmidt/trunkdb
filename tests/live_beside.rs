//! Live queries and aggregates of a read-only open beside a writer (SPEC
//! §195): read again as the writer commits, what changed reported as a
//! commit's would be. The writer here is in the same process, opened
//! apart, as one in another process is.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use trunkdb::query::{Aggregate, Condition, Filter, Update};
use trunkdb::{Collection, Database, DocId, Document, Live, LiveQuery, LiveRow, OpenOptions};

/// Long enough for any look to come, short of a test that hangs.
const WAIT: Duration = Duration::from_secs(10);

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("live.trunkdb")).unwrap();
    (dir, db)
}

/// A read-only open beside `writer`, looking every 5 ms.
fn beside(dir: &Path) -> Database {
    let options = OpenOptions::default()
        .read_only(true)
        .watch_every(Duration::from_millis(5));
    Database::open_with(dir.join("live.trunkdb"), options).unwrap()
}

fn doc(text: &str) -> Document {
    text.parse().unwrap()
}

fn vans(db: &Database) -> Collection<Document> {
    db.collection::<Document>("vans")
}

fn van(name: &str, zone: &str, speed: i64) -> Document {
    doc(&format!(
        r#"{{"name": "{name}", "zone": "{zone}", "speed": {speed}}}"#
    ))
}

fn in_depot() -> Filter {
    Filter::new().eq("zone", "depot")
}

/// An event as `(kind, id, the document's name)`: `None` for a document
/// gone.
type Short = (&'static str, DocId, Option<String>);

fn short(live: Live<Document>) -> Short {
    let name = |doc: &Document| doc.get("name").and_then(Document::as_str).map(String::from);
    match &live {
        Live::Entered { id, doc, .. } => ("entered", *id, name(doc)),
        Live::Left { id, doc, .. } => ("left", *id, doc.as_ref().and_then(name)),
        Live::Changed { id, doc, .. } => ("changed", *id, name(doc)),
        _ => unreachable!(),
    }
}

/// The next event, waiting for the look that reports it.
fn next(live: &LiveQuery<Document>) -> Short {
    short(live.recv_timeout(WAIT).expect("an event in time").unwrap())
}

/// Nothing more comes, through several looks.
fn quiet(live: &LiveQuery<Document>) {
    assert_eq!(
        live.recv_timeout(Duration::from_millis(100))
            .map(|e| short(e.unwrap())),
        None
    );
    assert!(!live.has_ended());
}

fn ev(kind: &'static str, id: DocId, name: Option<&str>) -> Short {
    (kind, id, name.map(String::from))
}

#[test]
fn a_live_query_beside_a_writer_reports_what_it_commits() {
    let (dir, writer) = open();
    let vans_w = vans(&writer);
    let a = vans_w.insert(van("a", "depot", 0)).unwrap();
    vans_w.insert(van("b", "road", 0)).unwrap();

    let reader = beside(dir.path());
    let live = vans(&reader).watch(in_depot()).unwrap();
    assert_eq!(next(&live), ev("entered", a, Some("a")));
    quiet(&live);

    let c = vans_w.insert(van("c", "depot", 0)).unwrap();
    assert_eq!(next(&live), ev("entered", c, Some("c")));
    vans_w.update(&a, van("a", "road", 0)).unwrap();
    assert_eq!(next(&live), ev("left", a, Some("a")), "as it is now");
    vans_w.update(&c, van("c2", "depot", 0)).unwrap();
    assert_eq!(next(&live), ev("changed", c, Some("c2")));
    vans_w.delete(&c).unwrap();
    assert_eq!(next(&live), ev("left", c, None), "gone");
    // A document that matches neither before nor after is none of its
    // business; nor is a commit to another collection.
    vans_w.update(&a, van("a", "yard", 0)).unwrap();
    writer
        .collection::<Document>("other")
        .insert(doc("{}"))
        .unwrap();
    quiet(&live);
}

#[test]
fn several_commits_between_two_looks_come_as_one() {
    let (dir, writer) = open();
    let vans_w = vans(&writer);
    let reader = Database::open_with(
        dir.path().join("live.trunkdb"),
        OpenOptions::default()
            .read_only(true)
            .watch_every(Duration::from_millis(300)),
    )
    .unwrap();
    let live = vans(&reader).watch(in_depot()).unwrap();
    let started = Instant::now();
    let a = vans_w.insert(van("a", "depot", 0)).unwrap();
    vans_w.update(&a, van("a2", "depot", 0)).unwrap();
    let b = vans_w.insert(van("b", "depot", 0)).unwrap();
    vans_w.delete(&b).unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "the writes took too long to tell"
    );
    // In with what the last commit left; `b` came and went unseen.
    assert_eq!(next(&live), ev("entered", a, Some("a2")));
    quiet(&live);
}

/// An event with its places: `(kind, id, from, at)`.
fn apply(list: &mut Vec<DocId>, live: &Live<Document>) {
    match live {
        Live::Entered { id, at, .. } => list.insert(at.expect("sorted"), *id),
        Live::Left { id, from, .. } => {
            let from = from.expect("sorted");
            assert_eq!(list.get(from), Some(id), "taken out where it isn't");
            list.remove(from);
        }
        Live::Changed { id, from, at, .. } => {
            let from = from.expect("sorted");
            assert_eq!(list.get(from), Some(id), "moved from where it isn't");
            list.remove(from);
            list.insert(at.expect("sorted"), *id);
        }
        _ => unreachable!(),
    }
}

/// A sorted list kept from the places is `find`'s, after each commit's
/// look, through random writes and a checkpoint now and then.
#[test]
fn a_sorted_list_beside_a_writer_is_finds_after_every_look() {
    let (dir, writer) = open();
    let vans_w = vans(&writer);
    let mut ids: Vec<DocId> = (0..20)
        .map(|i| {
            vans_w
                .insert(van(&format!("v{i}"), "road", i * 7 % 13))
                .unwrap()
        })
        .collect();
    let reader = beside(dir.path());
    let fastest = Filter::new().sort_desc("speed").limit(5);
    let live = vans(&reader).watch(fastest.clone()).unwrap();
    let ids_of = |docs: Vec<Document>| -> Vec<DocId> {
        (docs.iter())
            .map(|d| d.get("_id").and_then(Document::as_id).unwrap())
            .collect()
    };
    let mut list = Vec::new();
    let mut state = 12345u64;
    let mut random = move |n: u64| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) % n
    };
    for step in 0..60 {
        match random(4) {
            0 => ids.push(
                vans_w
                    .insert(van("new", "road", random(30) as i64))
                    .unwrap(),
            ),
            1 if ids.len() > 6 => {
                let id = ids.swap_remove(random(ids.len() as u64) as usize);
                vans_w.delete(&id).unwrap();
            }
            _ => {
                let id = ids[random(ids.len() as u64) as usize];
                let update = Update::new().set("speed", random(30) as i64);
                vans_w.update_fields(Filter::new().id(id), &update).unwrap();
            }
        }
        if step % 15 == 14 {
            writer.checkpoint().unwrap();
        }
        let want = ids_of(vans_w.find(fastest.clone()).unwrap());
        let deadline = Instant::now() + WAIT;
        while list != want {
            let left = deadline.saturating_duration_since(Instant::now());
            let event = live.recv_timeout(left).expect("the list in time").unwrap();
            apply(&mut list, &event);
        }
    }
    quiet(&live);
}

#[test]
fn select_cuts_what_is_handed_out_beside_a_writer() {
    let (dir, writer) = open();
    let reader = beside(dir.path());
    let live = vans(&reader).watch(in_depot().select(["name"])).unwrap();
    let a = vans(&writer).insert(van("a", "depot", 3)).unwrap();
    let Live::Entered { id, doc, .. } = live.recv_timeout(WAIT).unwrap().unwrap() else {
        panic!("entered");
    };
    assert_eq!(id, a);
    assert_eq!(doc, doc_with(a, r#"{"name": "a"}"#));
    // A field it doesn't hand out, written: a write all the same.
    vans(&writer)
        .update_fields(Filter::new().id(a), &Update::new().set("speed", 4))
        .unwrap();
    assert_eq!(next(&live), ev("changed", a, Some("a")));
}

fn doc_with(id: DocId, text: &str) -> Document {
    let Document::Object(mut fields) = doc(text) else {
        unreachable!()
    };
    fields.insert("_id".into(), Document::Id(id));
    Document::Object(fields)
}

#[test]
fn a_dropped_collection_beside_a_writer_leaves_every_match() {
    let (dir, writer) = open();
    let a = vans(&writer).insert(van("a", "depot", 0)).unwrap();
    let reader = beside(dir.path());
    let live = vans(&reader).watch(in_depot()).unwrap();
    assert_eq!(next(&live), ev("entered", a, Some("a")));
    writer.drop_collection("vans").unwrap();
    assert_eq!(next(&live), ev("left", a, None));
    let b = vans(&writer).insert(van("b", "depot", 0)).unwrap();
    assert_eq!(next(&live), ev("entered", b, Some("b")));
}

#[test]
fn a_live_aggregate_beside_a_writer_reports_its_rows() {
    let (dir, writer) = open();
    let vans_w = vans(&writer);
    vans_w.insert(van("a", "depot", 0)).unwrap();
    let reader = beside(dir.path());
    let per_zone = Aggregate::new().group_by("zone").count().named("n");
    let live = vans(&reader)
        .watch_aggregate(Filter::new(), &per_zone)
        .unwrap();
    let row = |event: Option<trunkdb::Result<LiveRow>>| match event.expect("a row in time").unwrap()
    {
        LiveRow::Entered { row, .. } => ("entered", row),
        LiveRow::Changed { row, .. } => ("changed", row),
        LiveRow::Left { group, .. } => ("left", group),
        _ => unreachable!(),
    };
    assert_eq!(
        row(live.try_recv()),
        ("entered", doc(r#"{"zone": "depot", "n": 1}"#))
    );
    let b = vans_w.insert(van("b", "depot", 0)).unwrap();
    assert_eq!(
        row(live.recv_timeout(WAIT)),
        ("changed", doc(r#"{"zone": "depot", "n": 2}"#))
    );
    vans_w.update(&b, van("b", "road", 0)).unwrap();
    let mut two = [row(live.recv_timeout(WAIT)), row(live.recv_timeout(WAIT))];
    two.sort_by_key(|(kind, _)| *kind);
    assert_eq!(
        two,
        [
            ("changed", doc(r#"{"zone": "depot", "n": 1}"#)),
            ("entered", doc(r#"{"zone": "road", "n": 1}"#)),
        ]
    );
}

/// A look that finds no commit since the last reads no documents: a
/// condition of the application's own counts how many it is given.
#[test]
fn a_look_without_a_commit_reads_nothing() {
    let (dir, writer) = open();
    vans(&writer).insert(van("a", "depot", 0)).unwrap();
    let reader = beside(dir.path());
    let read = Arc::new(AtomicUsize::new(0));
    let counted = read.clone();
    let filter = Filter::new().and(Condition::custom(["zone"], move |_| {
        counted.fetch_add(1, Ordering::Relaxed);
        true
    }));
    let live = vans(&reader).watch(filter).unwrap();
    assert!(matches!(
        live.recv_timeout(WAIT),
        Some(Ok(Live::Entered { .. }))
    ));
    std::thread::sleep(Duration::from_millis(50));
    let before = read.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        read.load(Ordering::Relaxed),
        before,
        "twenty looks, no commit"
    );
    // A checkpoint is no commit either: the next look reads again, and
    // reports nothing.
    writer.checkpoint().unwrap();
    quiet(&live);
    vans(&writer).insert(van("b", "road", 0)).unwrap();
    assert!(matches!(
        live.recv_timeout(WAIT),
        Some(Ok(Live::Entered { .. }))
    ));
    assert!(
        read.load(Ordering::Relaxed) > before,
        "read again after the commit"
    );
}

#[test]
fn a_live_query_beside_a_writer_ends_with_its_database() {
    let (dir, writer) = open();
    let reader = beside(dir.path());
    let live = vans(&reader).watch(in_depot()).unwrap();
    let a = vans(&writer).insert(van("a", "depot", 0)).unwrap();
    assert_eq!(next(&live), ev("entered", a, Some("a")));
    drop(reader);
    assert!(live.recv_timeout(WAIT).is_none());
    assert!(live.has_ended());
}

#[test]
fn a_panic_in_a_condition_beside_a_writer_ends_that_query_alone() {
    let (dir, writer) = open();
    let reader = beside(dir.path());
    let filter = Filter::new().and(Condition::custom(["speed"], |doc| {
        assert!(
            doc.get("speed").and_then(Document::as_i64) != Some(99),
            "no 99"
        );
        true
    }));
    let panicking = vans(&reader).watch(filter).unwrap();
    let live = vans(&reader).watch(in_depot()).unwrap();
    vans(&writer).insert(van("a", "depot", 99)).unwrap();
    assert!(matches!(
        panicking.recv_timeout(WAIT),
        Some(Err(trunkdb::Error::ClosurePanicked { .. }))
    ));
    assert!(matches!(
        live.recv_timeout(WAIT),
        Some(Ok(Live::Entered { .. }))
    ));
    assert!(panicking.recv_timeout(WAIT).is_none());
}

/// The thread that looks ends at a look that finds no live query left;
/// a live query after that starts it again.
#[test]
fn a_live_query_after_the_last_one_ended_is_looked_at_again() {
    let (dir, writer) = open();
    let reader = beside(dir.path());
    let first = vans(&reader).watch(in_depot()).unwrap();
    drop(first);
    // Its end is found as the next look reports to it, and the look
    // after finds none left.
    vans(&writer).insert(van("a", "depot", 0)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let second = vans(&reader).watch(in_depot()).unwrap();
    assert!(matches!(next(&second), ("entered", _, _)));
    let b = vans(&writer).insert(van("b", "depot", 0)).unwrap();
    assert_eq!(next(&second), ev("entered", b, Some("b")));
}

/// After a checkpoint the reader builds its pages anew, its commits
/// counted from the file again: a state as far along as the last look's
/// is a new one all the same, and read.
#[test]
fn a_commit_after_a_checkpoint_is_read_though_counted_as_before() {
    let (dir, writer) = open();
    writer.checkpoint().unwrap();
    let reader = Database::open_with(
        dir.path().join("live.trunkdb"),
        OpenOptions::default()
            .read_only(true)
            .watch_every(Duration::from_millis(300)),
    )
    .unwrap();
    let live = vans(&reader).watch(in_depot()).unwrap();
    let a = vans(&writer).insert(van("a", "depot", 0)).unwrap();
    assert_eq!(next(&live), ev("entered", a, Some("a")));
    // Before the next look: the file gets `a`, the WAL `b` alone, one
    // batch past the file as `a` was.
    writer.checkpoint().unwrap();
    let b = vans(&writer).insert(van("b", "depot", 0)).unwrap();
    assert_eq!(next(&live), ev("entered", b, Some("b")));
}
