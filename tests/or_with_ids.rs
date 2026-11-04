//! An OR of ids and indexed conditions (SPEC §164): "these ids, or what
//! changed since" reads the ids from the primary index and the rest from
//! an index range, each document once, where it read every document.

use trunkdb::query::{Aggregate, Condition, Filter, QueryPlan, Update};
use trunkdb::{Collection, Database, DocId, Document, IndexOptions};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("or_with_ids.trunkdb")).unwrap();
    (dir, db)
}

/// `n` documents and their ids in order of `n`: `n` and `updated` the
/// number, `tenant` "a" or "b", `title` "Fix …" for every 7th, `tags`
/// with "hot" for every 11th, `due` only on every 3rd, `note` on none
/// indexed. Indexes on `updated`, `(tenant, updated)`, `title`,
/// `tags[*]`, a sparse one on `due`, and on `tenant` if `with_tenant`.
fn make_docs(db: &Database, n: i64, with_tenant: bool) -> (Collection<Document>, Vec<DocId>) {
    let docs = db.collection::<Document>("docs");
    let mut ids = Vec::new();
    for chunk in (0..n).collect::<Vec<_>>().chunks(1000) {
        let mut batch = db.batch();
        for &i in chunk {
            let tenant = ["a", "b"][i as usize % 2];
            let title = if i % 7 == 0 { "Fix it" } else { "Other" };
            let hot = if i % 11 == 0 { r#", "hot""# } else { "" };
            let due = match i % 3 {
                0 => format!(r#", "due": {i}"#),
                _ => String::new(),
            };
            let doc: Document = format!(
                r#"{{"n": {i}, "updated": {i}, "tenant": "{tenant}", "title": "{title} {i}", "tags": ["x"{hot}], "note": {}{due}}}"#,
                i % 5
            )
            .parse()
            .unwrap();
            ids.push(batch.insert(&docs, doc).unwrap());
        }
        batch.commit().unwrap();
    }
    docs.ensure_index("updated").unwrap();
    docs.ensure_index(["tenant", "updated"]).unwrap();
    docs.ensure_index("title").unwrap();
    docs.ensure_index("tags[*]").unwrap();
    docs.ensure_index_with("due", IndexOptions::new().sparse())
        .unwrap();
    if with_tenant {
        docs.ensure_index("tenant").unwrap();
    }
    (docs, ids)
}

/// An OR of an `Eq` on `_id` per id.
fn of(ids: &[DocId]) -> Condition {
    Condition::any(ids.iter().map(|id| Condition::id(*id)))
}

/// Whether `plan` is a union of the indexes `fields`, in that order.
fn is_union(plan: &QueryPlan, fields: &[&str]) -> bool {
    matches!(plan, QueryPlan::IndexUnion { fields: got, .. } if got == fields)
}

/// Every document `filter` finds, as text, sorted.
fn found(docs: &Collection<Document>, filter: Filter) -> Vec<String> {
    let mut found: Vec<String> = (docs.find(filter).unwrap().iter())
        .map(|doc| doc.to_string())
        .collect();
    found.sort();
    found
}

/// What `filter` finds, the same as the filter hinted to no index finds:
/// how many.
fn same_as_scan(docs: &Collection<Document>, filter: &Filter) -> usize {
    let scanned = found(docs, filter.clone().no_index());
    assert_eq!(found(docs, filter.clone()), scanned, "{filter}");
    scanned.len()
}

/// §164.1: which ORs are read as a union of lookups and ranges, which
/// stay as they were, and what each finds.
#[test]
fn an_or_of_ids_and_an_indexed_branch_is_a_union() {
    let (_dir, db) = open();
    let (docs, ids) = make_docs(&db, 300, false);
    let gone = ids[5];
    docs.delete_many(Filter::new().id(gone)).unwrap();
    let missing: DocId = "0192f0c4-7d7a-7c43-9e0b-1f6d1b8f0a11".parse().unwrap();
    let asked = [ids[3], ids[290], ids[295], gone, missing];
    let mixed = |branch: Condition| Filter::new().any_of([of(&asked), branch]);
    let cases: [(Filter, &[&str]); 8] = [
        (mixed(Condition::gt("updated", 280)), &["_id", "updated"]),
        (
            Filter::new().any_of([Condition::id(ids[1]), Condition::lte("updated", 2)]),
            &["_id", "updated"],
        ),
        (
            mixed(Condition::eq("tenant", "a")),
            &["_id", "(tenant, updated)"],
        ),
        (
            mixed(Condition::starts_with("title", "Fix")),
            &["_id", "title"],
        ),
        (mixed(Condition::eq("tags[*]", "hot")), &["_id", "tags[*]"]),
        (mixed(Condition::lt("due", 30)), &["_id", "due"]),
        (
            mixed(Condition::any([
                Condition::gt("updated", 297),
                Condition::eq("tags[*]", "hot"),
            ])),
            &["_id", "updated", "tags[*]"],
        ),
        // A branch that names ids inside an AND is looked up too.
        (
            mixed(Condition::all([
                Condition::id(ids[7]),
                Condition::eq("note", 2),
            ])),
            &["_id"],
        ),
    ];
    for (filter, fields) in cases {
        let plan = docs.explain(&filter).unwrap();
        // Ids alone, every branch naming some, stay `ById` (§61).
        let want_by_id = fields == ["_id"];
        match want_by_id {
            true => assert_eq!(plan, QueryPlan::ById, "{filter}"),
            false => assert!(is_union(&plan, fields), "{filter}: {plan:?}"),
        }
        assert!(same_as_scan(&docs, &filter) > 0, "{filter}");
    }

    // The ids found, the range's too, each once: 3, 290 and 295 by id,
    // 281..=299 but 290 and 295 again by the range.
    let filter = mixed(Condition::gt("updated", 280));
    assert_eq!(same_as_scan(&docs, &filter), 1 + 19);
    let run = docs.explain_run(filter).unwrap();
    // Read once each: 3, 290 and 295 looked up, the range's 280 to 299
    // but 290 and 295 again (its start, 280, read and rejected, as any
    // `>` range reads it).
    assert_eq!(run.stats.documents_examined, 3 + 18, "{run:?}");
    assert_eq!(run.reads[0].index, "_id");
    assert_eq!(run.reads[0].entries, 5);
    assert_eq!(
        (run.reads[1].index.as_str(), run.reads[1].entries),
        ("updated", 20)
    );

    // A branch on a field without an index: a scan, as before.
    let unindexed = mixed(Condition::eq("note", 1));
    assert_eq!(docs.explain(&unindexed).unwrap(), QueryPlan::Scan);
    same_as_scan(&docs, &unindexed);
    // `!=` bounds nothing either.
    let not_equal = mixed(Condition::ne("updated", 1));
    assert_eq!(docs.explain(&not_equal).unwrap(), QueryPlan::Scan);
}

/// §164.1, §164.2: the AND around the OR. Without an index on its other
/// condition, the union, each looked-up document checked against it;
/// with one, that index, as for any OR beside an `Eq`.
#[test]
fn the_and_around_the_or_is_checked_or_read() {
    let (_dir, db) = open();
    let (docs, ids) = make_docs(&db, 200, false);
    // ids[3] is tenant "b", ids[4] tenant "a".
    let filter = Filter::new()
        .eq("note", 3)
        .any_of([of(&[ids[3], ids[4], ids[8]]), Condition::gt("updated", 190)]);
    assert!(is_union(
        &docs.explain(&filter).unwrap(),
        &["_id", "updated"]
    ));
    let n = same_as_scan(&docs, &filter);
    assert_eq!(n, 2 + 2); // 3 and 8 by id, 193 and 198 by the range

    let (_dir, db) = open();
    let (docs, ids) = make_docs(&db, 200, true);
    let filter = Filter::new()
        .eq("tenant", "a")
        .any_of([of(&[ids[3], ids[4]]), Condition::gt("updated", 190)]);
    assert!(
        matches!(docs.explain(&filter).unwrap(), QueryPlan::Index { field, .. } if field == "tenant" || field == "(tenant, updated)"),
        "{:?}",
        docs.explain(&filter).unwrap()
    );
    // 4 by id, and 192 to 198, tenant "a"'s, by the range.
    assert_eq!(same_as_scan(&docs, &filter), 1 + 4);
}

/// §164.3: with a sort and a limit, the union is read and sorted, where
/// the index in sort order, or the primary index, was walked.
#[test]
fn a_sort_with_a_limit_sorts_the_union() {
    let (_dir, db) = open();
    let (docs, ids) = make_docs(&db, 400, false);
    let or = || {
        Filter::new().any_of([
            of(&[ids[1], ids[2], ids[399]]),
            Condition::gt("updated", 380),
        ])
    };
    for filter in [
        or().sort_asc("updated").limit(5),
        or().sort_desc("updated").skip(2).limit(4),
        or().sort_asc("_id").limit(6),
        or().sort_desc("_id").limit(3),
    ] {
        assert!(
            is_union(&docs.explain(&filter).unwrap(), &["_id", "updated"]),
            "{filter}"
        );
        let read: Vec<String> = (docs.find(filter.clone()).unwrap().iter())
            .map(|doc| doc.to_string())
            .collect();
        let scanned: Vec<String> = (docs.find(filter.clone().no_index()).unwrap().iter())
            .map(|doc| doc.to_string())
            .collect();
        assert_eq!(read, scanned, "{filter}");
        let run = docs.explain_run(filter.clone()).unwrap();
        assert!(run.stats.documents_examined <= 3 + 19, "{filter}: {run:?}");
    }
}

/// §164.4, §164.5: every way of reading takes the union, and finds what
/// the scan finds; a hint gives the plan it gave before.
#[test]
fn every_read_takes_it_and_hints_keep_theirs() {
    let (_dir, db) = open();
    let (docs, ids) = make_docs(&db, 200, false);
    let filter = || {
        Filter::new().any_of([
            of(&[ids[0], ids[10], ids[11]]),
            Condition::gte("updated", 195),
        ])
    };
    assert_eq!(docs.count(filter()).unwrap(), 3 + 5);
    assert_eq!(docs.count(filter().no_index()).unwrap(), 3 + 5);

    let aggregate: Aggregate = "count() as n group by tenant".parse().unwrap();
    let rows = |filter: Filter| {
        (docs.aggregate(filter, &aggregate).unwrap().iter())
            .map(|row| row.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(rows(filter()), rows(filter().no_index()));
    let explained = docs.explain_aggregate(filter(), &aggregate).unwrap();
    assert_eq!(explained.stats.documents_examined, 8);

    // The plans passed over: a scan and the others, each counted.
    let run = docs.explain_run(filter()).unwrap();
    assert!(is_union(&run.plan, &["_id", "updated"]));
    assert!(run.alternatives.iter().any(|a| a.plan == QueryPlan::Scan));
    // Run as a scan, the union is one passed over: its lookups counted
    // among its entries, three ids and five of the range (§164.4).
    let run = docs.explain_run(filter().no_index()).unwrap();
    assert_eq!(run.plan, QueryPlan::Scan);
    let union = (run.alternatives.iter())
        .find(|a| is_union(&a.plan, &["_id", "updated"]))
        .unwrap_or_else(|| panic!("{run:?}"));
    assert_eq!(union.entries, trunkdb::query::EntryCount::Exactly(3 + 5));
    assert!(union.reads_documents);

    // Hinted, the plan it was before: no lookups beside the hinted
    // index, which can't bound the ids' branch, so the hint fails.
    let hinted = docs.explain(&filter().use_index("updated"));
    assert!(
        matches!(hinted, Err(trunkdb::Error::Hint { .. })),
        "{hinted:?}"
    );
    let by_id = docs.explain(&filter().use_index("_id"));
    assert!(
        matches!(by_id, Err(trunkdb::Error::Hint { .. })),
        "{by_id:?}"
    );

    // Writes and a batch's precondition read it the same way.
    let changed = docs
        .update_fields(filter(), &Update::new().set("seen", true))
        .unwrap();
    assert_eq!(changed, 8);
    assert_eq!(docs.count(Filter::new().eq("seen", true)).unwrap(), 8);
    let mut batch = db.batch();
    batch
        .require(
            &docs,
            Filter::new()
                .eq("seen", true)
                .any_of([of(&[ids[10]]), Condition::gt("updated", 900)]),
        )
        .unwrap();
    batch.commit().unwrap();
    assert_eq!(docs.delete_many(filter()).unwrap(), 8);
    assert_eq!(docs.count(Filter::new()).unwrap(), 192);
    assert_eq!(docs.count(filter()).unwrap(), 0);
}

/// §164 in numbers: of 100,000 documents, ten ids and the hundred
/// changed since, 110 read where every one was. Run with
/// `cargo test --release --test or_with_ids -- --ignored --nocapture`.
#[test]
#[ignore]
fn measured() {
    let (_dir, db) = open();
    let (docs, ids) = make_docs(&db, 100_000, false);
    let asked: Vec<DocId> = (0..10).map(|i| ids[i * 997]).collect();
    let filter = Filter::new().any_of([of(&asked), Condition::gte("updated", 99_900)]);
    let time = |filter: &Filter| {
        let start = std::time::Instant::now();
        let mut n = 0;
        for _ in 0..20 {
            n = docs.find(filter.clone()).unwrap().len();
        }
        (n, start.elapsed() / 20)
    };
    let (n, union) = time(&filter);
    let (m, scan) = time(&filter.clone().no_index());
    assert_eq!((n, m), (110, 110));
    let run = docs.explain_run(filter).unwrap();
    println!(
        "union {union:?}, {} examined; scan {scan:?}",
        run.stats.documents_examined
    );
}
