//! An upsert in a batch (SPEC §175): `Batch::upsert_fields`, at its
//! place, under the writer's lock, and what `Written` says it did.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use trunkdb::query::{Filter, Update};
use trunkdb::{Collection, Database, DocId, Document, Error, Upserted};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("batch.trunkdb")).unwrap();
    (dir, db)
}

fn doc(text: &str) -> Document {
    text.parse().unwrap()
}

fn filter(text: &str) -> Filter {
    text.parse().unwrap()
}

fn int(doc: &Document, name: &str) -> i64 {
    match doc.get(name) {
        Some(Document::Int(n)) => *n,
        other => panic!("{name}: {other:?}"),
    }
}

fn id_of(doc: &Document) -> DocId {
    match doc.get("_id") {
        Some(Document::Id(id)) => *id,
        other => panic!("_id: {other:?}"),
    }
}

/// Each document's `(k, v)` by id.
fn stored(items: &Collection<Document>) -> BTreeMap<DocId, (i64, i64)> {
    (items.find(Filter::new()).unwrap().iter())
        .map(|d| (id_of(d), (int(d, "k"), int(d, "v"))))
        .collect()
}

/// The roadmap's case: a page's count and the site's, each made by the
/// first visit and added to by the later ones, in one commit.
#[test]
fn counters_made_by_the_first_write_and_added_to_after() {
    let (_dir, db) = open();
    let visits = db.collection::<Document>("visits");
    let totals = db.collection::<Document>("totals");
    visits.ensure_index("page").unwrap();
    let visit = |page: &str| {
        let mut batch = db.batch();
        batch
            .upsert_fields(
                &visits,
                Filter::new().eq("page", page),
                &Update::new().inc("hits", 1).set_on_insert("first", true),
            )
            .unwrap();
        batch
            .upsert_fields(
                &totals,
                filter(r#"site = "main""#),
                &Update::new().inc("hits", 1),
            )
            .unwrap();
        batch.commit().unwrap()
    };

    let first = visit("/home");
    assert_eq!(first.counts(), [1, 1]);
    let Some(Upserted::Inserted(home)) = first.upserted(0) else {
        panic!("{:?}", first.upserted(0));
    };
    assert!(matches!(first.upserted(1), Some(Upserted::Inserted(_))));
    let again = visit("/home");
    assert_eq!(again.counts(), [1, 1]);
    assert_eq!(again.upserted(0), Some(Upserted::Updated(home)));
    assert_eq!(
        again.upserted(1),
        first.upserted(1).map(|u| Upserted::Updated(u.id()))
    );
    visit("/about");

    let home_doc = visits.get(&home).unwrap().unwrap();
    assert_eq!(int(&home_doc, "hits"), 2);
    assert_eq!(home_doc.get("page"), Some(&Document::from("/home")));
    assert_eq!(home_doc.get("first"), Some(&Document::Bool(true)));
    assert_eq!(visits.count(Filter::new()).unwrap(), 2);
    let total = totals.find_one(Filter::new()).unwrap().unwrap();
    assert_eq!(int(&total, "hits"), 3);
    assert!(db.check().unwrap().is_ok());
}

/// At its place: an insert before it is what it finds; two upserts of
/// one key make one document; an op after it sees what it wrote.
#[test]
fn an_upsert_sees_the_ops_before_it() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    let mut batch = db.batch();
    let a = batch.insert(&items, doc(r#"{"k": 1, "v": 0}"#)).unwrap();
    let inc = Update::new().inc("v", 1);
    batch.upsert_fields(&items, filter("k = 1"), &inc).unwrap();
    batch.upsert_fields(&items, filter("k = 2"), &inc).unwrap();
    batch.upsert_fields(&items, filter("k = 2"), &inc).unwrap();
    batch.require(&items, filter("k = 2 and v = 2")).unwrap();
    batch.delete(&items, &a);
    batch.upsert_fields(&items, filter("k = 1"), &inc).unwrap();
    let written = batch.commit().unwrap();
    assert_eq!(written.counts(), [1, 1, 1, 1, 0, 1, 1]);
    assert_eq!(written.upserted(1), Some(Upserted::Updated(a)));
    let Some(Upserted::Inserted(b)) = written.upserted(2) else {
        panic!("{:?}", written.upserted(2));
    };
    assert_eq!(written.upserted(3), Some(Upserted::Updated(b)));
    let Some(Upserted::Inserted(c)) = written.upserted(6) else {
        panic!("a deleted match is gone: {:?}", written.upserted(6));
    };
    assert_ne!(c, a);
    for other in [0, 4, 5, 7, 100] {
        assert_eq!(written.upserted(other), None, "op {other}");
    }
    let expected: BTreeMap<DocId, (i64, i64)> = [(b, (2, 2)), (c, (1, 1))].into();
    assert_eq!(stored(&items), expected);
}

/// A match the operators leave as it was isn't written, and counts 0;
/// it is still the document found.
#[test]
fn a_match_left_as_it_was_counts_nothing() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    let id = items.insert(doc(r#"{"k": 1, "v": 5}"#)).unwrap();
    let changes = db.subscribe().unwrap();
    let mut batch = db.batch();
    batch
        .upsert_fields(&items, filter("k = 1"), &Update::new().set("v", 5))
        .unwrap();
    let written = batch.commit().unwrap();
    assert_eq!(written.counts(), [0]);
    assert_eq!(written.upserted(0), Some(Upserted::Updated(id)));
    assert!(
        changes.try_recv().is_none(),
        "nothing written, nothing told"
    );
}

/// The filter's `_id` names the new document, and its other `eq`s,
/// nested too, are its fields.
#[test]
fn the_new_document_is_made_of_the_filters_eqs() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    let named = DocId([7; 16]);
    let mut batch = db.batch();
    batch
        .upsert_fields(
            &items,
            Filter::new().id(named).eq("owner.name", "ada").gt("v", 3),
            &Update::new().set("v", 4),
        )
        .unwrap();
    let written = batch.commit().unwrap();
    assert_eq!(written.upserted(0), Some(Upserted::Inserted(named)));
    let made = items.get(&named).unwrap().unwrap();
    assert_eq!(made.get("owner"), Some(&doc(r#"{"name": "ada"}"#)));
    assert_eq!(int(&made, "v"), 4);
}

/// Several matches, an operator that can't be made on the match or on
/// the insert, and a typed document that doesn't convert: each fails the
/// whole batch, the ops before it not landing.
#[test]
fn a_failing_upsert_rolls_back_the_batch() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    items.insert(doc(r#"{"k": 1, "v": 0}"#)).unwrap();
    items.insert(doc(r#"{"k": 1, "v": 1}"#)).unwrap();
    items.insert(doc(r#"{"k": 2, "v": "text"}"#)).unwrap();
    let before = items.find(Filter::new()).unwrap();
    let log = db.collection::<Document>("log");

    let fails = |upsert: &dyn Fn(&mut trunkdb::Batch)| {
        let mut batch = db.batch();
        batch.insert(&log, doc(r#"{"x": 1}"#)).unwrap();
        upsert(&mut batch);
        let rehearsed = batch.dry_run().unwrap_err().to_string();
        let error = batch.commit().unwrap_err();
        assert_eq!(
            rehearsed,
            error.to_string(),
            "a dry run fails as commit does"
        );
        assert_eq!(log.count(Filter::new()).unwrap(), 0);
        assert_eq!(items.find(Filter::new()).unwrap(), before);
        error
    };
    let inc = Update::new().inc("v", 1);
    let error = fails(&|b| b.upsert_fields(&items, filter("k = 1"), &inc).unwrap());
    assert!(
        matches!(error, Error::MultipleMatches { count: 2, .. }),
        "{error:?}"
    );
    let error = fails(&|b| b.upsert_fields(&items, filter("k = 2"), &inc).unwrap());
    assert!(
        matches!(error, Error::Update { .. }),
        "on the match: {error:?}"
    );
    let error = fails(&|b| {
        let on_text = Update::new().set_on_insert("v", "text").inc("v", 1);
        b.upsert_fields(&items, filter("k = 3"), &on_text).unwrap()
    });
    assert!(
        matches!(error, Error::Update { .. }),
        "on the insert: {error:?}"
    );

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Item {
        k: i64,
        v: i64,
    }
    let typed = db.collection::<Item>("items");
    let error = fails(&|b| {
        b.upsert_fields(&typed, filter("k = 4"), &Update::new().set("w", 1))
            .unwrap()
    });
    assert!(!matches!(error, Error::Update { .. }), "no v: {error:?}");
    // The untyped handle on the same collection takes it.
    let mut batch = db.batch();
    batch
        .upsert_fields(&items, filter("k = 4"), &Update::new().set("w", 1))
        .unwrap();
    assert_eq!(batch.commit().unwrap().counts(), [1]);
}

/// What can be told from the op alone is refused when it's added, and
/// the batch is left as it was.
#[test]
fn refused_when_added() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    let inc = Update::new().inc("v", 1);
    for (filter_text, says) in [
        ("k = 1 order by v", "a sort"),
        ("k = 1 offset 1", "a skip"),
        ("k = 1 limit 1", "a limit"),
        ("k = 1 and k = 2", "two values"),
        ("a = 1 and a.b = 2", "a.b"),
    ] {
        let mut batch = db.batch();
        let error = batch
            .upsert_fields(&items, filter(filter_text), &inc)
            .unwrap_err();
        let Error::Io(io) = &error else {
            panic!("{filter_text}: {error:?}");
        };
        assert_eq!(io.kind(), ErrorKind::InvalidInput, "{filter_text}");
        assert!(error.to_string().contains(says), "{filter_text}: {error}");
        assert!(batch.is_empty());
    }
    let mut batch = db.batch();
    let selecting = filter("k = 1").select(["v"]);
    assert!(batch.upsert_fields(&items, selecting, &inc).is_err());
    let error = batch
        .upsert_fields(&items, filter("k = 1"), &Update::new().set("_id", 1))
        .unwrap_err();
    assert!(error.to_string().contains("_id"), "{error}");
    assert!(batch.is_empty());
}

/// A dry run says what `commit` will, the same new id too, and writes
/// nothing.
#[test]
fn a_dry_run_says_what_commit_would() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    let found = items.insert(doc(r#"{"k": 1, "v": 0}"#)).unwrap();
    let mut batch = db.batch();
    let inc = Update::new().inc("v", 1);
    batch.upsert_fields(&items, filter("k = 1"), &inc).unwrap();
    batch.upsert_fields(&items, filter("k = 2"), &inc).unwrap();
    let rehearsed = batch.dry_run().unwrap();
    assert_eq!(rehearsed.upserted(0), Some(Upserted::Updated(found)));
    assert!(matches!(rehearsed.upserted(1), Some(Upserted::Inserted(_))));
    assert_eq!(items.count(Filter::new()).unwrap(), 1);
    assert_eq!(int(&items.get(&found).unwrap().unwrap(), "v"), 0);
    assert_eq!(batch.commit().unwrap(), rehearsed);
    assert_eq!(items.count(Filter::new()).unwrap(), 2);
}

struct Random(u64);

impl Random {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Who a document of the model is: an id known when the op was added,
/// or the one upsert op `n` of the batch inserted, known at `commit`.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Who {
    Known(DocId),
    UpsertedBy(usize),
}

/// What the model says an upsert did, to whom.
#[derive(Clone, Copy, Debug)]
enum Did {
    Inserted,
    Updated(Who),
}

/// Random batches of inserts, upserts that raise `v`, upserts that
/// leave it as it was, and deletes, with and without an index on `k`,
/// against a model applying each op in order: the counts, what each
/// upsert did and to which document, the dry run, a failure on several
/// matches, and the state after each batch agree.
#[test]
fn random_batches_match_a_model() {
    for setup in 0..2 {
        let (_dir, db) = open();
        let items = db.collection::<Document>("items");
        if setup == 1 {
            items.ensure_index("k").unwrap();
        }
        let mut random = Random(0x2545_f491_4f6c_dd1d + setup);
        let mut model: BTreeMap<DocId, (i64, i64)> = BTreeMap::new();
        let (mut held, mut refused, mut inserted, mut updated) = (0, 0, 0, 0);
        for round in 0..300 {
            let mut state: Vec<(Who, i64, i64)> = (model.iter())
                .map(|(id, (k, v))| (Who::Known(*id), *k, *v))
                .collect();
            let mut batch = db.batch();
            let mut counts = Vec::new();
            let mut did: Vec<Option<Did>> = Vec::new();
            let mut several = false;
            for at in 0..1 + random.below(6) as usize {
                let a = random.below(4) as i64;
                match random.below(5) {
                    0 => {
                        let id = batch
                            .insert(&items, doc(&format!(r#"{{"k": {a}, "v": 0}}"#)))
                            .unwrap();
                        state.push((Who::Known(id), a, 0));
                        counts.push(1);
                        did.push(None);
                    }
                    1 | 2 => {
                        let leave = random.below(3) == 0;
                        let update = match leave {
                            true => Update::new().set("v", 0),
                            false => Update::new().inc("v", 1),
                        };
                        batch
                            .upsert_fields(&items, filter(&format!("k = {a}")), &update)
                            .unwrap();
                        let found: Vec<usize> =
                            (0..state.len()).filter(|&i| state[i].1 == a).collect();
                        match found[..] {
                            [] => {
                                state.push((Who::UpsertedBy(at), a, if leave { 0 } else { 1 }));
                                counts.push(1);
                                did.push(Some(Did::Inserted));
                            }
                            [i] => {
                                let new = if leave { 0 } else { state[i].2 + 1 };
                                counts.push(usize::from(new != state[i].2));
                                state[i].2 = new;
                                did.push(Some(Did::Updated(state[i].0)));
                            }
                            _ => {
                                several = true;
                                break;
                            }
                        }
                    }
                    _ => {
                        batch.delete_many(&items, filter(&format!("k = {a}")));
                        let before = state.len();
                        state.retain(|(_, k, _)| *k != a);
                        counts.push(before - state.len());
                        did.push(None);
                    }
                }
            }
            let rehearsed = batch.dry_run();
            assert_eq!(stored(&items), model, "a dry run writes nothing");
            let outcome = batch.commit();
            if several {
                for result in [outcome, rehearsed] {
                    let error = result.unwrap_err();
                    assert!(
                        matches!(error, Error::MultipleMatches { .. }),
                        "setup {setup}, round {round}: {error:?}"
                    );
                }
                assert_eq!(stored(&items), model, "setup {setup}, round {round}");
                refused += 1;
                continue;
            }
            let written = outcome.unwrap();
            let context = format!("setup {setup}, round {round}");
            assert_eq!(written.counts(), counts, "{context}");
            assert_eq!(rehearsed.unwrap(), written, "{context}");
            let resolve = |who: Who| match who {
                Who::Known(id) => id,
                Who::UpsertedBy(op) => written.upserted(op).unwrap().id(),
            };
            for (at, expected) in did.iter().enumerate() {
                match (expected, written.upserted(at)) {
                    (None, None) => {}
                    (Some(Did::Inserted), Some(Upserted::Inserted(_))) => inserted += 1,
                    (Some(Did::Updated(who)), Some(Upserted::Updated(id))) => {
                        assert_eq!(resolve(*who), id, "{context}, op {at}");
                        updated += 1;
                    }
                    (expected, got) => panic!("{context}, op {at}: {expected:?}, {got:?}"),
                }
            }
            let expected: BTreeMap<DocId, (i64, i64)> = (state.iter())
                .map(|(who, k, v)| (resolve(*who), (*k, *v)))
                .collect();
            assert_eq!(expected.len(), state.len(), "{context}: ids apart");
            assert_eq!(stored(&items), expected, "{context}");
            model = expected;
            held += 1;
        }
        assert!(
            held > 100 && refused > 20 && inserted > 50 && updated > 100,
            "setup {setup}: {held} held, {refused} refused, {inserted} inserted, {updated} updated"
        );
        assert!(db.check().unwrap().is_ok());
    }
}

/// Threads counting visits to the same pages, each visit a batch that
/// upserts the page's count and logs it: one document per page, its
/// count the visits logged.
#[test]
fn concurrent_upserts_make_one_document_per_key() {
    let (_dir, db) = open();
    db.collection::<Document>("visits")
        .ensure_index("page")
        .unwrap();
    let threads: Vec<_> = (0..4)
        .map(|worker| {
            let db = db.clone();
            std::thread::spawn(move || {
                let (visits, log) = (
                    db.collection::<Document>("visits"),
                    db.collection::<Document>("log"),
                );
                for n in 0..50 {
                    let page = (worker + n) % 3;
                    let mut batch = db.batch();
                    batch
                        .upsert_fields(
                            &visits,
                            filter(&format!("page = {page}")),
                            &Update::new().inc("hits", 1),
                        )
                        .unwrap();
                    batch
                        .insert(&log, doc(&format!(r#"{{"page": {page}}}"#)))
                        .unwrap();
                    assert_eq!(batch.commit().unwrap().counts(), [1, 1]);
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let (visits, log) = (
        db.collection::<Document>("visits"),
        db.collection::<Document>("log"),
    );
    assert_eq!(visits.count(Filter::new()).unwrap(), 3);
    for page in 0..3 {
        let counted = visits
            .find_one(filter(&format!("page = {page}")))
            .unwrap()
            .unwrap();
        let logged = log.count(filter(&format!("page = {page}"))).unwrap();
        assert_eq!(int(&counted, "hits") as usize, logged, "page {page}");
    }
}
