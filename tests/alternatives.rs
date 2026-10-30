//! The plans `explain_run` passed over (SPEC §133): what the filter,
//! hinted to each index, `_id` or a scan, would have read.

use trunkdb::query::{Aggregate, Alternative, EntryCount, Explained, Filter, QueryPlan};
use trunkdb::{Collection, Database, Document};

/// 200 tasks, as in `tests/hints.rs`: `status` "done" but every 20th
/// "open", `created` the number, `score` `n % 10`, `tag` "a" or "b";
/// indexes on `status`, `created`, `score` and `(tag, created)`.
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
    let db = Database::open(dir.path().join("alternatives.trunkdb")).unwrap();
    (dir, db)
}

/// A plan as a short name: the index read, `in order` for a walk.
fn named(plan: &QueryPlan) -> String {
    match plan {
        QueryPlan::Scan => "scan".to_string(),
        QueryPlan::ById => "by id".to_string(),
        QueryPlan::Index { field, .. } => field.clone(),
        QueryPlan::IndexOrder { field, .. } => format!("{field} in order"),
        QueryPlan::IndexUnion { fields, .. } => fields.join(" + "),
        other => format!("{other:?}"),
    }
}

/// The alternatives as `(plan, entries, fewer)`, for a whole comparison.
fn listed(run: &Explained) -> Vec<(String, EntryCount, bool)> {
    run.alternatives
        .iter()
        .map(|a| (named(&a.plan), a.entries, a.fewer))
        .collect()
}

fn find<'a>(run: &'a Explained, plan: &str) -> &'a Alternative {
    run.alternatives
        .iter()
        .find(|a| named(&a.plan) == plan)
        .unwrap_or_else(|| panic!("no {plan} among {:?}", listed(run)))
}

/// `filter` with the hint that gives `plan`, as `alternatives` made it.
fn hinted(filter: &Filter, plan: &QueryPlan) -> Filter {
    let index = match plan {
        QueryPlan::Scan => return filter.clone().no_index(),
        QueryPlan::ById => "_id".to_string(),
        QueryPlan::Index { field, .. } | QueryPlan::IndexOrder { field, .. } => field.clone(),
        QueryPlan::IndexUnion { fields, .. } => fields[0].clone(),
        other => panic!("{other:?}"),
    };
    let fields: Vec<String> = match index.strip_prefix('(') {
        Some(inner) => inner
            .trim_end_matches(')')
            .split(", ")
            .map(String::from)
            .collect(),
        None => vec![index],
    };
    filter.clone().use_index(fields)
}

/// Every alternative counted exactly reads exactly that when run with
/// its hint, and the run itself is none of them.
fn predictions_hold(tasks: &Collection<Document>, filter: &Filter) {
    let run = tasks.explain_run(filter.clone()).unwrap();
    for other in &run.alternatives {
        assert_ne!(other.plan, run.plan, "{filter}");
        let hinted = hinted(filter, &other.plan);
        let ran = tasks.explain_run(hinted.clone()).unwrap();
        assert_eq!(ran.plan, other.plan, "{hinted}");
        match other.entries {
            EntryCount::Exactly(n) => {
                assert_eq!(ran.stats.keys_examined, n, "{hinted}");
                assert_eq!(other.fewer, n < run.stats.keys_examined, "{hinted}");
            }
            EntryCount::MoreThan(n) => {
                assert!(ran.stats.keys_examined > n, "{hinted}");
                assert!(!other.fewer, "{hinted}");
            }
            _ => unreachable!(),
        }
        // Whether it reads documents: its read says whether its entries
        // decide.
        let decides = ran.reads.iter().all(|read| read.covering);
        assert_eq!(other.reads_documents, !decides, "{hinted}");
    }
}

/// The skewed case: the rules read 190 entries for `status`, the
/// `created` range would read 10. A scan reads 200, and no index has
/// nothing to bound and isn't listed.
#[test]
fn the_skewed_case_shows_the_range_that_reads_fewer() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let filter = Filter::new().eq("status", "done").gte("created", 190);
    let run = tasks.explain_run(filter.clone()).unwrap();
    assert_eq!(named(&run.plan), "status");
    assert_eq!(run.stats.keys_examined, 190);
    assert_eq!(
        listed(&run),
        [
            ("created".to_string(), EntryCount::Exactly(10), true),
            ("scan".to_string(), EntryCount::Exactly(200), false),
        ]
    );
    assert!(find(&run, "created").reads_documents);
    predictions_hold(&tasks, &filter);
}

/// The other way round, the user's question: hinted, the run reads 10,
/// and the rules' plan it passed over shows the whole 190, not "more
/// than 10".
#[test]
fn a_hinted_run_shows_how_much_the_rules_would_read() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let filter = Filter::new()
        .eq("status", "done")
        .gte("created", 190)
        .use_index("created");
    let run = tasks.explain_run(filter.clone()).unwrap();
    assert_eq!(run.stats.keys_examined, 10);
    let rules = find(&run, "status");
    assert_eq!(rules.entries, EntryCount::Exactly(190));
    assert!(!rules.fewer);
    assert!(run.alternatives.iter().all(|a| !a.fewer));
    predictions_hold(&tasks, &filter);
}

/// A walk in sort order is walked only as far as the run read: here 42
/// entries and more to find 5 matches, more than the 20 the `score` range reads.
/// With a limit of one, it finds its match in the first entry, and
/// reads the next to see its group end.
#[test]
fn a_walk_is_walked_as_far_as_the_run_read() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let ranged = Filter::new().gte("score", 9).sort_desc("created");
    let by_range = ranged.clone().limit(5).use_index("score");
    let run = tasks.explain_run(by_range.clone()).unwrap();
    assert_eq!(run.stats.keys_examined, 20);
    let walk = find(&run, "created in order");
    assert_eq!(walk.entries, EntryCount::MoreThan(20));
    assert!(!walk.fewer);
    predictions_hold(&tasks, &by_range);

    let one = ranged.clone().limit(1).use_index("score");
    let run = tasks.explain_run(one.clone()).unwrap();
    let walk = find(&run, "created in order");
    assert_eq!(walk.entries, EntryCount::Exactly(2));
    assert!(walk.fewer);
    predictions_hold(&tasks, &one);

    // The walk the rules chose, with the range it passed over.
    let walked = tasks.explain_run(ranged.clone().limit(5)).unwrap();
    assert_eq!(named(&walked.plan), "created in order");
    assert_eq!(walked.stats.keys_examined, 42);
    assert_eq!(find(&walked, "score").entries, EntryCount::Exactly(20));
    assert!(find(&walked, "score").fewer);
    predictions_hold(&tasks, &ranged.limit(5));
}

/// Lookups by id count one each; a scan of them reads everything. A
/// covering read says its entries decide.
#[test]
fn ids_unions_and_covering_reads_are_counted_too() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let ids: Vec<_> = tasks
        .find(Filter::new().lt("created", 3))
        .unwrap()
        .into_iter()
        .map(|doc| match doc {
            Document::Object(fields) => fields.get("_id").cloned().unwrap(),
            other => panic!("{other:?}"),
        })
        .collect();
    let by_id = Filter::new().is_in("_id", ids);
    let run = tasks.explain_run(by_id.clone().no_index()).unwrap();
    assert_eq!(find(&run, "by id").entries, EntryCount::Exactly(3));
    assert!(find(&run, "by id").fewer);
    predictions_hold(&tasks, &by_id.no_index());

    let union = Filter::new().any_of(["score = 1".parse().unwrap(), "score = 2".parse().unwrap()]);
    let run = tasks.explain_run(union.clone().no_index()).unwrap();
    let both = find(&run, "score + score");
    assert_eq!(both.entries, EntryCount::Exactly(40));
    assert!(!both.reads_documents);
    predictions_hold(&tasks, &union.no_index());

    // A union of two indexes no hint gives (§132.3): listed for a
    // hinted run as the rules' plan, reading a document per entry.
    let across =
        Filter::new().any_of(["score = 1".parse().unwrap(), "created < 5".parse().unwrap()]);
    let ruled = tasks.explain_run(across.clone()).unwrap();
    assert_eq!(named(&ruled.plan), "score + created");
    let run = tasks.explain_run(across.no_index()).unwrap();
    let union = find(&run, "score + created");
    assert_eq!(
        union.entries,
        EntryCount::Exactly(ruled.stats.keys_examined)
    );
    assert!(union.reads_documents);
    assert!(union.fewer);

    // `_id` in order: no other hint gives it, and the rules walk
    // `status` (§103).
    let newest = Filter::new().eq("status", "open").sort_desc("_id").limit(2);
    let run = tasks.explain_run(newest.clone()).unwrap();
    assert_eq!(named(&run.plan), "status in order");
    assert!(matches!(
        find(&run, "_id in order").entries,
        EntryCount::MoreThan(_)
    ));

    for filter in [
        Filter::new().eq("tag", "a").lt("created", 20),
        Filter::new().eq("tag", "b").sort_asc("created").limit(3),
        Filter::new().eq("status", "open").sort_desc("_id").limit(2),
        Filter::new()
            .gte("created", 100)
            .lt("score", 3)
            .skip(5)
            .limit(5),
        Filter::new().lt("created", 50).sort_asc("score").limit(4),
        Filter::new().eq("score", 5).select(["score"]),
    ] {
        predictions_hold(&tasks, &filter);
    }
}

/// The run's own numbers stay its own; aggregates list nothing; and a
/// filter only a scan can read has nothing to list.
#[test]
fn the_run_stays_its_own() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let filter = Filter::new().eq("status", "done").gte("created", 190);
    let run = tasks.explain_run(filter.clone()).unwrap();
    assert_eq!(run.reads.len(), 1);
    assert_eq!(run.stats.documents_examined, 190);
    let summed = tasks
        .explain_aggregate(filter, &Aggregate::new().count().named("n"))
        .unwrap();
    assert!(summed.alternatives.is_empty());
    let none = tasks.explain_run(Filter::new().ne("tag", "a")).unwrap();
    assert!(none.alternatives.is_empty(), "{:?}", listed(&none));
    let missing = db.collection::<Document>("missing");
    assert!(
        missing
            .explain_run(Filter::new().eq("a", 1))
            .unwrap()
            .alternatives
            .is_empty()
    );
}
