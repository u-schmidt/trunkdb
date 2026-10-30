//! A hint in the query (SPEC §132): `Filter::use_index` and
//! `Filter::no_index` choose the index, the rules choose the rest, and a
//! hint that can't be followed fails every read the same way.

use trunkdb::query::{Aggregate, Filter, QueryPlan, Update};
use trunkdb::{Collection, Database, DocId, Document, Error};

/// 200 tasks: `status` "done" for all but every 20th, which is "open";
/// `created` the number, `score` `n % 10`, `tag` "a" or "b". Indexes on
/// `status`, `created`, `score` and `(tag, created)`.
fn tasks(db: &Database) -> Collection<Document> {
    let tasks = db.collection::<Document>("tasks");
    for n in 0..200i64 {
        let status = if n % 20 == 0 { "open" } else { "done" };
        let tag = if n % 2 == 0 { "a" } else { "b" };
        let doc: Document = format!(
            r#"{{"status": "{status}", "created": {n}, "score": {}, "tag": "{tag}"}}"#,
            n % 10
        )
        .parse()
        .unwrap();
        tasks.insert(doc).unwrap();
    }
    tasks.ensure_index("status").unwrap();
    tasks.ensure_index("created").unwrap();
    tasks.ensure_index("score").unwrap();
    tasks.ensure_index(["tag", "created"]).unwrap();
    tasks
}

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("hints.trunkdb")).unwrap();
    (dir, db)
}

fn plan(tasks: &Collection<Document>, filter: &Filter) -> QueryPlan {
    tasks.explain(filter).unwrap()
}

/// The name of the index a plan reads, or `scan` / `by id`.
fn read(plan: &QueryPlan) -> String {
    match plan {
        QueryPlan::Scan => "scan".to_string(),
        QueryPlan::ById => "by id".to_string(),
        QueryPlan::Index { field, .. } => field.clone(),
        QueryPlan::IndexOrder { field, .. } => format!("{field} in order"),
        QueryPlan::IndexUnion { fields, .. } => fields.join(" + "),
        other => format!("{other:?}"),
    }
}

/// The ids `filter` finds, in its order, or sorted where it has none.
fn ids(tasks: &Collection<Document>, filter: Filter) -> Result<Vec<DocId>, Error> {
    let sorted = filter.to_string().contains("order by");
    let mut ids: Vec<DocId> = tasks
        .find(filter)?
        .iter()
        .map(|doc| match doc {
            Document::Object(fields) => match fields.get("_id") {
                Some(Document::Id(id)) => *id,
                other => panic!("no id: {other:?}"),
            },
            other => panic!("not an object: {other:?}"),
        })
        .collect();
    if !sorted {
        ids.sort();
    }
    Ok(ids)
}

fn is_hint_error<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
    match result {
        Err(e @ Error::Hint { .. }) => e.to_string(),
        other => panic!("expected a hint error, got {other:?}"),
    }
}

/// The skewed case of ROADMAP.md: an `Eq` on a value nearly every
/// document has beats, by the rules, a range that holds a few. The hint
/// reads the range instead, and finds the same.
#[test]
fn a_hint_reads_the_index_the_rules_pass_over() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let filter = Filter::new().eq("status", "done").gte("created", 190);
    assert_eq!(read(&plan(&tasks, &filter)), "status");
    let hinted = filter.clone().use_index("created");
    assert_eq!(read(&plan(&tasks, &hinted)), "created");

    let ruled = tasks.explain_run(filter.clone()).unwrap();
    let followed = tasks.explain_run(hinted.clone()).unwrap();
    assert!(!ruled.hinted);
    assert!(followed.hinted);
    assert_eq!(ruled.stats.keys_examined, 190);
    assert_eq!(followed.stats.keys_examined, 10);
    assert_eq!(ruled.stats.documents_returned, 10);
    assert_eq!(ids(&tasks, filter).unwrap(), ids(&tasks, hinted).unwrap());
}

/// The second case: a walk in sort order with a limit, beside a range
/// that holds few. Hinted to the range's index, the range is read and
/// sorted in memory.
#[test]
fn a_hint_reads_a_range_instead_of_walking_in_sort_order() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let filter = Filter::new().eq("score", 3).sort_asc("created").limit(5);
    assert_eq!(read(&plan(&tasks, &filter)), "score");
    // An `Eq` on another indexed field beats the walk (§34.2); a range
    // doesn't.
    let ranged = Filter::new().gte("score", 9).sort_desc("created").limit(5);
    assert_eq!(read(&plan(&tasks, &ranged)), "created in order");
    let hinted = ranged.clone().use_index("score");
    assert_eq!(read(&plan(&tasks, &hinted)), "score");
    let run = tasks.explain_run(hinted.clone()).unwrap();
    assert_eq!(run.sort.unwrap().in_memory, 1);
    assert_eq!(ids(&tasks, ranged).unwrap(), ids(&tasks, hinted).unwrap());

    // And the other way: hinted to the sort field's index, the walk.
    let walked = filter.clone().use_index("created");
    assert_eq!(read(&plan(&tasks, &walked)), "created in order");
    assert_eq!(ids(&tasks, filter).unwrap(), ids(&tasks, walked).unwrap());
}

/// `no_index` scans, a lookup by id too; `use_index("_id")` is the
/// primary index, by id or in `_id` order, and nothing else.
#[test]
fn no_index_scans_and_id_is_the_primary_index() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let some = ids(&tasks, Filter::new().eq("created", 7)).unwrap()[0];
    let by_id = Filter::new().id(some);
    assert_eq!(read(&plan(&tasks, &by_id)), "by id");
    assert_eq!(read(&plan(&tasks, &by_id.clone().no_index())), "scan");
    assert_eq!(ids(&tasks, by_id.clone().no_index()).unwrap(), vec![some]);
    assert_eq!(
        read(&plan(&tasks, &by_id.clone().use_index("_id"))),
        "by id"
    );

    let ranged = Filter::new().eq("status", "open");
    assert_eq!(read(&plan(&tasks, &ranged.clone().no_index())), "scan");
    assert_eq!(
        ids(&tasks, ranged.clone()).unwrap(),
        ids(&tasks, ranged.clone().no_index()).unwrap()
    );
    // `no_index` never fails, even with nothing to bound.
    assert_eq!(tasks.count(Filter::new().no_index()).unwrap(), 200);

    let newest = Filter::new().eq("status", "open").sort_desc("_id").limit(3);
    // An `Eq` fixing the whole index: walked, in id order (§103).
    assert_eq!(read(&plan(&tasks, &newest)), "status in order");
    let walked = newest.clone().use_index("_id");
    assert_eq!(read(&plan(&tasks, &walked)), "_id in order");
    assert_eq!(ids(&tasks, newest).unwrap(), ids(&tasks, walked).unwrap());

    // Hinted to a secondary index, a sort by `_id` doesn't walk the
    // primary one: the range, sorted in memory.
    let by_score = Filter::new().gte("score", 8).sort_asc("_id").limit(3);
    assert_eq!(read(&plan(&tasks, &by_score)), "_id in order");
    assert_eq!(
        read(&plan(&tasks, &by_score.clone().use_index("score"))),
        "score"
    );
    assert_eq!(
        ids(&tasks, by_score.clone()).unwrap(),
        ids(&tasks, by_score.use_index("score")).unwrap()
    );
}

/// A compound index is hinted by its fields, as `ensure_index` takes
/// them.
#[test]
fn a_compound_index_is_hinted_by_its_fields() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let filter = Filter::new()
        .eq("status", "done")
        .eq("tag", "a")
        .lt("created", 10);
    assert_eq!(read(&plan(&tasks, &filter)), "(tag, created)");
    let named = filter.clone().use_index(["tag", "created"]);
    assert_eq!(read(&plan(&tasks, &named)), "(tag, created)");
    assert_eq!(
        tasks
            .explain_run(named.clone())
            .unwrap()
            .stats
            .keys_examined,
        6
    );
    let other = filter.clone().use_index("status");
    assert_eq!(read(&plan(&tasks, &other)), "status");
    assert_eq!(
        tasks
            .explain_run(other.clone())
            .unwrap()
            .stats
            .keys_examined,
        190
    );
    assert_eq!(
        ids(&tasks, filter.clone()).unwrap(),
        ids(&tasks, named).unwrap()
    );
    assert_eq!(ids(&tasks, filter).unwrap(), ids(&tasks, other).unwrap());
    // Its fields in another order are another index, which there isn't.
    is_hint_error(tasks.explain(&Filter::new().eq("tag", "a").use_index(["created", "tag"])));
}

/// A hint the index can't follow fails, and reads and writes nothing,
/// on every read: never a quiet fall back to the rules.
#[test]
fn a_hint_that_cant_be_followed_fails_every_read() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let unknown = Filter::new().eq("status", "open").use_index("nope");
    let message = is_hint_error(tasks.explain(&unknown));
    assert!(message.contains("no such index"), "{message}");
    assert!(message.contains("nope"), "{message}");

    // An index there is, with no condition it bounds and no sort.
    let unbounded = Filter::new().eq("status", "open").use_index("created");
    let message = is_hint_error(tasks.explain(&unbounded));
    assert!(message.contains("no condition"), "{message}");
    // A sort on it without a limit is no walk either (§34.2).
    is_hint_error(tasks.explain(&unbounded.clone().sort_asc("created")));
    is_hint_error(tasks.explain(&Filter::new().eq("status", "open").use_index("_id")));

    for filter in [unknown, unbounded] {
        is_hint_error(tasks.find(filter.clone()));
        is_hint_error(tasks.find_one(filter.clone()));
        is_hint_error(tasks.count(filter.clone()));
        is_hint_error(tasks.cursor(filter.clone()).map(|_| ()));
        is_hint_error(tasks.cursor(filter.clone().sort_asc("score")).map(|_| ()));
        is_hint_error(tasks.explain_run(filter.clone()));
        let counted = Aggregate::new().group_by("tag").count().named("n");
        is_hint_error(tasks.aggregate(filter.clone(), &counted));
        is_hint_error(tasks.explain_aggregate(filter.clone(), &counted));
        let summed = Aggregate::new().sum("total").named("score");
        is_hint_error(tasks.aggregate(filter.clone(), &summed));
        is_hint_error(tasks.aggregate(filter.clone(), &Aggregate::new().count().named("n")));
        is_hint_error(tasks.delete_many(filter.clone()));
        is_hint_error(tasks.update_many(filter.clone(), |doc| *doc = Document::Null));
        is_hint_error(tasks.update_fields(filter.clone(), &Update::new().set("x", 1)));
        let new: Document = r#"{"status": "open"}"#.parse().unwrap();
        is_hint_error(tasks.upsert(filter.clone(), new));
        let snapshot = db.snapshot().unwrap();
        let view = snapshot.collection::<Document>("tasks");
        is_hint_error(view.find(filter.clone()));
        is_hint_error(view.count(filter.clone()));
        is_hint_error(view.explain(&filter));
    }
    // Nothing written: 200 still, none changed.
    assert_eq!(tasks.count(Filter::new()).unwrap(), 200);
    assert_eq!(tasks.count(Filter::new().exists("x")).unwrap(), 0);
    assert_eq!(tasks.count(Filter::new().eq("status", "open")).unwrap(), 10);
}

/// A collection that doesn't exist has no index to hint.
#[test]
fn a_hint_on_a_collection_that_doesnt_exist_fails() {
    let (_dir, db) = open();
    let none = db.collection::<Document>("none");
    is_hint_error(none.find(Filter::new().eq("a", 1).use_index("a")));
    assert_eq!(none.find(Filter::new().eq("a", 1).no_index()).unwrap(), []);
}

/// Aggregates follow the hint too, from index entries where the hinted
/// index holds what they read.
#[test]
fn an_aggregate_follows_the_hint() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let filter = Filter::new().eq("status", "done").gte("created", 100);
    let counted = Aggregate::new().count().named("n");
    let ruled = tasks.aggregate(filter.clone(), &counted).unwrap();
    for hinted in [
        filter.clone().use_index("created"),
        filter.clone().no_index(),
    ] {
        assert_eq!(tasks.aggregate(hinted.clone(), &counted).unwrap(), ruled);
        assert_eq!(tasks.count(hinted).unwrap(), 95);
    }
    let by_tag = Aggregate::new().group_by("tag").count().named("n");
    let walked = Filter::new().eq("tag", "a");
    let explained = tasks.explain_aggregate(walked.clone(), &by_tag).unwrap();
    assert_eq!(read(&explained.plan), "(tag, created)");
    let scanned = tasks
        .explain_aggregate(walked.clone().no_index(), &by_tag)
        .unwrap();
    assert_eq!(read(&scanned.plan), "scan");
    assert!(scanned.hinted);
    assert_eq!(
        tasks.aggregate(walked.clone(), &by_tag).unwrap(),
        tasks.aggregate(walked.no_index(), &by_tag).unwrap()
    );
    // Without conditions an aggregate may walk a whole index, but a
    // hint to one with no condition to bound fails, as `explain` says.
    let whole = Filter::new().use_index(["tag", "created"]);
    is_hint_error(tasks.aggregate(whole.clone(), &by_tag));
    is_hint_error(tasks.explain_aggregate(whole, &by_tag));
    let everything = tasks.explain_aggregate(Filter::new(), &by_tag).unwrap();
    assert_eq!(read(&everything.plan), "(tag, created) in order");
    let scanned = tasks
        .explain_aggregate(Filter::new().no_index(), &by_tag)
        .unwrap();
    assert_eq!(read(&scanned.plan), "scan");
    assert_eq!(
        tasks.aggregate(Filter::new().no_index(), &by_tag).unwrap(),
        tasks.aggregate(Filter::new(), &by_tag).unwrap()
    );
}

/// Every hint finds what the rules find, or fails: a hint chooses how,
/// never which.
#[test]
fn every_hint_finds_what_the_rules_find() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let filters = [
        Filter::new().eq("status", "done").gte("created", 150),
        Filter::new().eq("status", "open").lt("score", 5),
        Filter::new().gte("score", 7).sort_desc("created").limit(7),
        Filter::new()
            .eq("tag", "b")
            .sort_asc("created")
            .limit(4)
            .skip(2),
        Filter::new()
            .eq("tag", "a")
            .eq("score", 4)
            .sort_desc("_id")
            .limit(3),
        Filter::new()
            .any_of(["score = 1".parse().unwrap(), "created < 5".parse().unwrap()])
            .sort_asc("_id")
            .limit(6),
        "status = \"open\" and created > 30 order by score desc, created limit 3"
            .parse()
            .unwrap(),
    ];
    let hints: [fn(Filter) -> Filter; 7] = [
        |f| f,
        |f| f.no_index(),
        |f| f.use_index("_id"),
        |f| f.use_index("status"),
        |f| f.use_index("created"),
        |f| f.use_index("score"),
        |f| f.use_index(["tag", "created"]),
    ];
    let mut followed = 0;
    for filter in filters {
        let found = ids(&tasks, filter.clone()).unwrap();
        assert!(!found.is_empty(), "{filter}");
        for hint in hints {
            let hinted = hint(filter.clone());
            match ids(&tasks, hinted.clone()) {
                Ok(ids) => {
                    assert_eq!(ids, found, "{hinted}");
                    assert_eq!(
                        tasks.count(hinted.clone()).unwrap(),
                        found.len(),
                        "{hinted}"
                    );
                    let streamed = tasks.cursor(hinted.clone()).unwrap().count();
                    assert_eq!(streamed, found.len(), "{hinted}");
                    followed += 1;
                }
                Err(Error::Hint { .. }) => {
                    is_hint_error(tasks.explain(&hinted));
                }
                Err(e) => panic!("{hinted}: {e}"),
            }
        }
    }
    assert!(followed >= 25, "{followed}");
}

#[test]
fn a_hint_replaces_an_earlier_one_and_goes_through_text() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let filter = Filter::new().eq("status", "done").gte("created", 190);
    let twice = filter.clone().no_index().use_index("created");
    assert_eq!(read(&plan(&tasks, &twice)), "created");
    let text: Filter = r#"status = "done" and created >= 190 use index created"#
        .parse()
        .unwrap();
    assert_eq!(read(&plan(&tasks, &text)), "created");
    let compound: Filter = r#"tag = "a" and created < 10 use index (tag, created)"#
        .parse()
        .unwrap();
    assert_eq!(read(&plan(&tasks, &compound)), "(tag, created)");
}

/// An OR of no branches, `false`, bounds every index, and names no id.
/// Hinted away from the lookup of no ids that finds it without a hint,
/// it reads nothing from the hinted index, or scans (found by the
/// `queries` fuzz target).
#[test]
fn false_under_every_hint_finds_nothing() {
    use trunkdb::query::Condition;
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let none = Filter::new().and(Condition::any([]));
    assert_eq!(read(&plan(&tasks, &none)), "by id");
    assert_eq!(read(&plan(&tasks, &none.clone().no_index())), "scan");
    assert_eq!(read(&plan(&tasks, &none.clone().use_index("score"))), "");
    for hinted in [
        none.clone(),
        none.clone().no_index(),
        none.clone().use_index("_id"),
        none.clone().use_index("score"),
        none.clone().use_index(["tag", "created"]),
    ] {
        assert_eq!(tasks.find(hinted.clone()).unwrap(), [], "{hinted}");
        assert_eq!(tasks.count(hinted.clone()).unwrap(), 0, "{hinted}");
        assert_eq!(tasks.cursor(hinted.clone()).unwrap().count(), 0, "{hinted}");
    }
    // A collection without indexes: unchanged by the hint's scan.
    let plain = db.collection::<Document>("plain");
    plain.insert("{\"a\": 1}".parse().unwrap()).unwrap();
    assert_eq!(read(&plan(&plain, &none)), "by id");
    assert_eq!(plain.find(none).unwrap(), []);
}
