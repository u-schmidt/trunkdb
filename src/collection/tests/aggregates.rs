use super::*;

// --- Aggregates (SPEC §107) ---

/// SPEC §107.5: random aggregates of random filters the entries can
/// serve — conditions on an index's fields, no sort or limit, columns
/// on its fields — give what the documents give, through a compound
/// index, a single-field one and a sparse one, with values of every
/// kind: `Int` and `Float`, cut strings, null and missing, arrays,
/// NaN. Most matches come from their entries.
/// A group's first document by id found late in the walk, as an
/// entry of the group the last match went to, after a match read
/// from its document: the row gives the first document's value, not
/// the one read before (`5` and `5.0` are one group, SPEC §107.5).
/// The index's second field orders the group's entries otherwise
/// than by id: the cut string first, its entry unable to answer.
#[test]
fn a_groups_first_document_found_late_gives_its_value() {
    use crate::query::Aggregate;
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index(["a", "b"]).unwrap();
    let cut = "a".repeat(600);
    for (a, b) in [
        (Document::Float(3.0), "z".to_string()),
        (Document::Int(3), cut),
        (Document::Int(3), "m".to_string()),
    ] {
        docs.insert(object(vec![("a", a), ("b", Document::String(b))]))
            .unwrap();
    }
    let aggregate = Aggregate::new().group_by("a").min("b").named("first_b");
    let rows = docs.aggregate(Filter::new(), &aggregate).unwrap();
    let run = docs.explain_aggregate(Filter::new(), &aggregate).unwrap();
    // The cut string's document, and the first document's, for its
    // number.
    assert_eq!(
        (run.stats.from_entries, run.stats.documents_examined),
        (2, 2),
        "{run:#?}"
    );
    let all = docs.find(Filter::new()).unwrap();
    assert_eq!(rows, aggregate.apply(all).unwrap());
    assert!(matches!(&rows[0], Document::Object(f) if f["a"] == Document::Float(3.0)));
}

/// A mean of large numbers, by scan and through an index: the same
/// finite mean either way, where both gave infinity (SPEC §131).
#[test]
fn a_mean_of_large_numbers_is_finite_however_it_is_read() {
    use crate::query::Aggregate;
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    for n in [1e308, 1e308, f64::MAX] {
        docs.insert(object(vec![("n", Document::Float(n))]))
            .unwrap();
    }
    let aggregate = Aggregate::new().avg("n").named("mean");
    let f = Filter::new().gte("n", Document::Float(0.0));
    let expected = (2.0 * (1e308 / 3.0)) + f64::MAX / 3.0;
    let mean = |rows: Vec<Document>| match &rows[..] {
        [Document::Object(row)] => row["mean"].clone(),
        other => panic!("{other:?}"),
    };
    let scanned = mean(docs.aggregate(f.clone(), &aggregate).unwrap());
    docs.ensure_index("n").unwrap();
    let run = docs.explain_aggregate(f.clone(), &aggregate).unwrap();
    // Through the index, its documents read: an entry gives `avg` a
    // number only below 2^53 (SPEC §107.5).
    assert!(
        matches!(run.plan, crate::query::QueryPlan::Index { .. }),
        "{run:#?}"
    );
    let indexed = mean(docs.aggregate(f.clone(), &aggregate).unwrap());
    assert_eq!(scanned, indexed);
    let Document::Float(mean) = scanned else {
        panic!("{scanned:?}");
    };
    assert!(mean.is_finite());
    assert!((mean - expected).abs() <= expected * f64::EPSILON, "{mean}");
}

#[test]
fn aggregates_from_entries_agree_with_the_documents() {
    use crate::query::Aggregate;
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index(["s", "t"]).unwrap();
    docs.ensure_index(["s", "n"]).unwrap();
    docs.ensure_index("n").unwrap();
    docs.ensure_index_with("t", IndexOptions::new().sparse())
        .unwrap();
    let mut rng = XorShift(0x0107_E27E);
    let long = "z".repeat(600);
    let value = |rng: &mut XorShift| -> Option<Document> {
        Some(match rng.below(12) {
            0 => return None,
            1 => Document::Null,
            2 => Document::Int(rng.below(4) as i64),
            3 => Document::Float(rng.below(4) as f64),
            4 => Document::Float(-0.0),
            5 => Document::Float(f64::NAN),
            6 => Document::Int(1 << 60),
            7 => Document::String(format!("{long}{}", rng.below(3))),
            8 => Document::Array(vec![Document::Int(1)]),
            9 => Document::Bool(rng.below(2) == 0),
            _ => Document::String(["a", "b", "c"][rng.below(3)].into()),
        })
    };
    let mut batch = db.batch();
    for _ in 0..600 {
        let mut fields = Vec::new();
        for name in ["s", "t", "n"] {
            if let Some(v) = value(&mut rng) {
                fields.push((name, v));
            }
        }
        batch.insert(&docs, object(fields)).unwrap();
    }
    batch.commit().unwrap();
    let (mut from_entries, mut matched) = (0, 0);
    let fields = ["s", "t", "n"];
    for _ in 0..400 {
        let field = fields[rng.below(3)];
        let f = match rng.below(4) {
            0 => Filter::new(),
            1 => Filter::new().eq(field, value(&mut rng).unwrap_or(Document::Null)),
            2 => Filter::new().gte(field, Document::String("b".into())),
            _ => Filter::new()
                .lt(field, Document::Int(2))
                .ne(field, Document::Int(0)),
        };
        let mut aggregate = Aggregate::new();
        let groups = rng.below(3);
        for g in fields.iter().skip(rng.below(3)).take(groups) {
            aggregate = aggregate.group_by(*g);
        }
        for c in 0..1 + rng.below(3) {
            let path = fields[rng.below(3)];
            let name = format!("c{c}");
            aggregate = match rng.below(5) {
                0 => aggregate.count(),
                1 => aggregate.min(path),
                2 => aggregate.max(path),
                3 => aggregate.count_distinct(path),
                _ => aggregate.avg(path),
            }
            .named(name);
        }
        let expected: Vec<String> = (aggregate.apply(docs.find(f.clone()).unwrap()).unwrap())
            .iter()
            .map(Document::to_string)
            .collect();
        let found: Vec<String> = (docs.aggregate(f.clone(), &aggregate).unwrap())
            .iter()
            .map(Document::to_string)
            .collect();
        assert_eq!(found, expected, "{f:?} {aggregate:?}");
        let run = docs.explain_aggregate(f.clone(), &aggregate).unwrap();
        from_entries += run.stats.from_entries;
        matched += run.stats.documents_matched;
    }
    // A third of the values are ones a key can't hand out.
    assert!(
        from_entries * 4 > matched,
        "{from_entries} of {matched} from entries"
    );
}

/// SPEC §146: `first` and `last` read their documents, where an index
/// holds every path; `count_distinct` is served by the entries, a long
/// string cut in its key read from its document, so two that differ
/// only past the cut count twice.
#[test]
fn distinct_counts_and_ends_read_their_documents() {
    use crate::query::Aggregate;
    let (_dir, db) = fresh_db();
    let notes = db.collection::<Document>("notes");
    notes.ensure_index(["g", "v", "at"]).unwrap();
    let long = "z".repeat(600);
    let mut batch = db.batch();
    for n in 0..40i64 {
        batch
            .insert(
                &notes,
                object(vec![
                    ("g", Document::String(["a", "b"][n as usize % 2].into())),
                    ("v", Document::String(format!("{long}{}", n % 5))),
                    ("at", Document::Int(n % 7)),
                ]),
            )
            .unwrap();
    }
    batch.commit().unwrap();
    let counts = Aggregate::new().group_by("g").count().named("n");
    let r = notes.explain_aggregate(Filter::new(), &counts).unwrap();
    assert!(r.stats.from_entries > 0, "the index serves a count");
    // `at` is read only for `by`: the last by id, n = 38 in group
    // "a", isn't the last by `at`, 6, which n = 6, 20 and 34 share.
    let a = Aggregate::new()
        .group_by("g")
        .count_distinct("v")
        .named("d")
        .last("v")
        .named("last")
        .last("v")
        .by("at")
        .named("latest");
    let rows = notes.aggregate(Filter::new(), &a).unwrap();
    assert_eq!(rows, a.apply(notes.find(Filter::new()).unwrap()).unwrap());
    let ends = |row: &Document, column: &str| {
        let v = row.get(column).and_then(Document::as_str).unwrap();
        (v.len(), v.chars().last().unwrap())
    };
    assert_eq!(rows[0].get("d"), Some(&Document::Int(5)));
    assert_eq!(rows[1].get("d"), Some(&Document::Int(5)));
    assert_eq!(ends(&rows[0], "last"), (601, '3'));
    assert_eq!(ends(&rows[0], "latest"), (601, '4'));
    let r = notes.explain_aggregate(Filter::new(), &a).unwrap();
    assert_eq!(r.stats.from_entries, 0);
    // `first` and `last` alone read their documents; `count_distinct`
    // is from the entries where they hold the value whole, and every
    // `v` is cut there, so read from its document.
    for (a, entries) in [
        (Aggregate::new().group_by("g").first("at").named("f"), false),
        (Aggregate::new().group_by("g").last("at").named("l"), false),
        (Aggregate::new().count_distinct("at").named("d"), true),
        (
            Aggregate::new()
                .group_by("g")
                .count_distinct("v")
                .named("d"),
            false,
        ),
    ] {
        let rows = notes.aggregate(Filter::new(), &a).unwrap();
        assert_eq!(rows, a.apply(notes.find(Filter::new()).unwrap()).unwrap());
        let r = notes.explain_aggregate(Filter::new(), &a).unwrap();
        assert_eq!(r.stats.from_entries > 0, entries, "{a:?}");
    }
}

/// SPEC §147: an unwind reads its documents, a multikey index on the
/// array there, and counts elements, not documents: neither from
/// entries nor by the count of every match.
#[test]
fn an_unwind_reads_documents_and_counts_elements() {
    use crate::query::Aggregate;
    let (_dir, db) = fresh_db();
    let tasks = db.collection::<Document>("tasks");
    tasks.ensure_index("tags[*]").unwrap();
    // And one on the array whole. Its entries hand an array over to
    // its document, which is unwound; but not a `null`, which an
    // unwind leaves out.
    tasks.ensure_index("tags").unwrap();
    for tags in [r#"["a","b"]"#, r#"["a"]"#, "[]", r#""b""#, "null"] {
        tasks
            .insert(format!(r#"{{"tags": {tags}}}"#).parse().unwrap())
            .unwrap();
    }
    for (a, expected) in [
        (
            Aggregate::new().unwind("tags").count().named("n"),
            r#"[{"n":4}]"#,
        ),
        (
            Aggregate::new()
                .unwind("tags")
                .group_by("tags")
                .count()
                .named("n"),
            r#"[{"tags":"a","n":2},{"tags":"b","n":2}]"#,
        ),
    ] {
        for f in [Filter::new(), Filter::new().eq("tags[*]", "a")] {
            let rows = tasks.aggregate(f.clone(), &a).unwrap();
            assert_eq!(rows, a.apply(tasks.find(f.clone()).unwrap()).unwrap());
            let run = tasks.explain_aggregate(f, &a).unwrap();
            assert_eq!(run.stats.from_entries, 0);
        }
        let text = Document::Array(tasks.aggregate(Filter::new(), &a).unwrap()).to_string();
        assert_eq!(text, expected);
    }
    // No arrays at all: an index on the field holds every document,
    // and would count the `null` an unwind leaves out.
    let plain = db.collection::<Document>("plain");
    plain.ensure_index("tags").unwrap();
    for tags in [r#""a""#, r#""b""#, "null"] {
        plain
            .insert(format!(r#"{{"tags": {tags}}}"#).parse().unwrap())
            .unwrap();
    }
    for a in [
        Aggregate::new().unwind("tags").count().named("n"),
        Aggregate::new()
            .unwind("tags")
            .group_by("tags")
            .count()
            .named("n"),
    ] {
        let rows = plain.aggregate(Filter::new(), &a).unwrap();
        assert_eq!(rows, a.apply(plain.find(Filter::new()).unwrap()).unwrap());
        let n: i64 = (rows.iter())
            .map(|row| row.get("n").and_then(Document::as_i64).unwrap())
            .sum();
        assert_eq!(n, 2, "{a:?}");
    }
}

/// SPEC §126.4: a column's condition decided from the entries, and
/// what reads documents.
#[test]
fn a_condition_on_a_column_reads_documents_only_where_entries_cannot_tell() {
    use crate::query::Aggregate;
    let (_dir, db) = fresh_db();
    let tasks = db.collection::<Document>("tasks");
    tasks.ensure_index(["status", "created"]).unwrap();
    tasks.ensure_index(["tenant", "tries"]).unwrap();
    tasks.ensure_index(["tenant", "status"]).unwrap();
    let start = 1_700_000_000;
    // Groups of (tenant, status) bigger than a packed leaf holds.
    const N: i64 = 30_000;
    let mut batch = db.batch();
    for n in 0..N {
        let created = crate::DateTime::from_unix(start + n, 0).unwrap();
        batch
            .insert(
                &tasks,
                object(vec![
                    (
                        "status",
                        Document::String(["Queued", "Running", "Done"][n as usize % 3].into()),
                    ),
                    ("created", Document::DateTime(created)),
                    ("tenant", Document::String(format!("t{}", n % 10))),
                    ("tries", Document::Int(n % 7)),
                ]),
            )
            .unwrap();
    }
    batch.commit().unwrap();
    let run = |f: Filter, a: &Aggregate| {
        LEAVES_COUNTED.with(|c| c.set(0));
        let run = tasks.explain_aggregate(f.clone(), a).unwrap();
        let expected = a.apply(tasks.find(f.clone()).unwrap()).unwrap();
        assert_eq!(tasks.aggregate(f, a).unwrap(), expected);
        (run, LEAVES_COUNTED.with(|c| c.get()))
    };
    // Per tenant, all counted and the mean of those tried four times
    // or more: `(tenant, tries)` holds both, no document read.
    let long = Aggregate::new()
        .group_by("tenant")
        .count()
        .named("tasks")
        .avg("tries")
        .filter(Condition::gte("tries", 4))
        .named("mean_long");
    let (r, _) = run(Filter::new(), &long);
    assert_eq!(
        r.plan,
        QueryPlan::IndexOrder {
            field: "(tenant, tries)".into()
        }
    );
    assert_eq!(
        (r.stats.documents_examined, r.stats.from_entries),
        (0, N as u64),
        "{r:?}"
    );
    // Per tenant, all and the running ones: `(tenant, status)`, its
    // leaves of one status decided whole.
    let running = Aggregate::new()
        .group_by("tenant")
        .count()
        .named("tasks")
        .count()
        .filter(Condition::eq("status", "Running"))
        .named("running");
    let (r, leaves) = run(Filter::new(), &running);
    assert_eq!(
        r.plan,
        QueryPlan::IndexOrder {
            field: "(tenant, status)".into()
        }
    );
    assert_eq!(r.stats.documents_examined, 0, "{r:?}");
    assert!(leaves > 5, "{leaves} leaves counted whole");
    // Per status, all and those since a date: from the entries, but
    // entry by entry, as `decide_all` can't decide a leaf whose ends
    // differ in `created` (SPEC §126.4).
    let since = crate::DateTime::from_unix(start + N * 2 / 3, 0).unwrap();
    let recent = Aggregate::new()
        .group_by("status")
        .count()
        .named("tasks")
        .count()
        .filter(Condition::gte("created", Document::DateTime(since)))
        .named("recent");
    let (r, leaves) = run(Filter::new(), &recent);
    assert_eq!(
        r.plan,
        QueryPlan::IndexOrder {
            field: "(status, created)".into()
        }
    );
    assert_eq!(
        (r.stats.documents_examined, r.stats.from_entries, leaves),
        (0, N as u64, 0),
        "{r:?}"
    );
    // A condition on a field no index holds beside the others: the
    // documents.
    let done = Aggregate::new()
        .group_by("tenant")
        .avg("tries")
        .filter(Condition::eq("status", "Done"))
        .named("mean_done");
    let (r, _) = run(Filter::new(), &done);
    assert_eq!(r.plan, QueryPlan::Scan);
    assert_eq!(r.stats.documents_examined, N as u64, "{r:?}");
    // A condition on a string cut in its key: those entries' documents
    // only.
    let notes = db.collection::<Document>("notes");
    notes.ensure_index(["tenant", "note"]).unwrap();
    let long_note = |end: &str| Document::String("x".repeat(1200) + end);
    for n in 0..20 {
        let note = match n % 4 {
            0 => long_note("a"),
            _ => Document::String(format!("short {n}")),
        };
        notes
            .insert(object(vec![
                ("tenant", Document::String(format!("t{}", n % 2))),
                ("note", note),
            ]))
            .unwrap();
    }
    let a = Aggregate::new()
        .group_by("tenant")
        .count()
        .named("all")
        .count()
        .filter(Condition::eq("note", long_note("a")))
        .named("long_a");
    let r = notes.explain_aggregate(Filter::new(), &a).unwrap();
    let expected = a.apply(notes.find(Filter::new()).unwrap()).unwrap();
    assert_eq!(notes.aggregate(Filter::new(), &a).unwrap(), expected);
    assert_eq!(
        (r.stats.documents_examined, r.stats.from_entries),
        (5, 15),
        "{r:?}"
    );
}

/// SPEC §107.5: what the entries answer, and what reads documents.
#[test]
fn an_aggregate_reads_documents_only_where_entries_cannot_tell() {
    use crate::query::Aggregate;
    let (_dir, db) = fresh_db();
    let tasks = db.collection::<Document>("tasks");
    tasks.ensure_index(["status", "created"]).unwrap();
    tasks.ensure_index(["tenant", "tries"]).unwrap();
    tasks.ensure_index("tries").unwrap();
    let mut batch = db.batch();
    for n in 0..3000i64 {
        let created = crate::DateTime::from_unix(1_700_000_000 + n, 0).unwrap();
        batch
            .insert(
                &tasks,
                object(vec![
                    (
                        "status",
                        Document::String(["Queued", "Running", "Done"][n as usize % 3].into()),
                    ),
                    ("created", Document::DateTime(created)),
                    ("tenant", Document::String(format!("t{}", n % 10))),
                    ("tries", Document::Int(n % 7)),
                ]),
            )
            .unwrap();
    }
    batch.commit().unwrap();
    let run = |f: Filter, a: &Aggregate| {
        let run = tasks.explain_aggregate(f.clone(), a).unwrap();
        let expected = a.apply(tasks.find(f.clone()).unwrap()).unwrap();
        assert_eq!(tasks.aggregate(f, a).unwrap(), expected);
        run
    };
    // Per status: three runs of the compound index, no document, its
    // leaves counted whole.
    let per_status = Aggregate::new().group_by("status").count().named("n");
    LEAVES_COUNTED.with(|c| c.set(0));
    let r = run(Filter::new(), &per_status);
    let leaves = LEAVES_COUNTED.with(|c| c.get());
    assert!(leaves > 5, "{leaves} leaves counted whole");
    assert_eq!(
        r.plan,
        QueryPlan::IndexOrder {
            field: "(status, created)".into()
        }
    );
    assert_eq!(
        (r.stats.documents_examined, r.stats.from_entries),
        (0, 3000),
        "{r:?}"
    );
    assert_eq!(r.stats.groups, 3);
    // The newest per status: a date-time, from the entries.
    let newest = per_status.clone().max("created").named("newest");
    let r = run(Filter::new(), &newest);
    assert_eq!(r.stats.documents_examined, 0, "{r:?}");
    // Per tenant, average and fewest tries: numbers. The average from
    // the entries; the fewest read from their one document each.
    let tries = Aggregate::new()
        .group_by("tenant")
        .avg("tries")
        .named("avg")
        .min("tries")
        .named("least");
    let r = run(Filter::new().gte("tenant", "t5"), &tries);
    assert_eq!(
        r.plan,
        QueryPlan::Index {
            field: "(tenant, tries)".into()
        }
    );
    assert_eq!(
        (r.stats.documents_examined, r.stats.groups),
        (5, 5),
        "{r:?}"
    );
    // Grouped by a number: one document per group, for `5` or `5.0`.
    let by_tries = Aggregate::new().group_by("tries").count().named("n");
    let r = run(Filter::new(), &by_tries);
    assert_eq!(
        (r.stats.documents_examined, r.stats.groups),
        (7, 7),
        "{r:?}"
    );
    assert_eq!(
        r.plan,
        QueryPlan::IndexOrder {
            field: "tries".into()
        }
    );
    // `sum` reads every document.
    let sum = Aggregate::new()
        .group_by("tenant")
        .sum("tries")
        .named("total");
    let r = run(Filter::new().eq("tenant", "t1"), &sum);
    assert_eq!(
        (r.stats.documents_examined, r.stats.from_entries),
        (300, 0),
        "{r:?}"
    );
    // An index `find` would take for `tenant == x` doesn't hold
    // `tries`: the compound one that does is read instead.
    tasks.ensure_index("tenant").unwrap();
    let average = Aggregate::new().avg("tries").named("avg");
    let r = run(Filter::new().eq("tenant", "t1"), &average);
    assert_eq!(
        r.plan,
        QueryPlan::Index {
            field: "(tenant, tries)".into()
        }
    );
    assert_eq!(
        (r.stats.documents_examined, r.stats.from_entries),
        (0, 300),
        "{r:?}"
    );
    tasks.drop_index("tenant").unwrap();
    // A single-field index without every document isn't walked: an
    // array of tries isn't in it. The compound one holds it, as a
    // value of its own, read from its document.
    tasks
        .insert(object(vec![(
            "tries",
            Document::Array(vec![Document::Int(1)]),
        )]))
        .unwrap();
    let r = run(Filter::new(), &by_tries);
    assert_eq!(
        r.plan,
        QueryPlan::IndexOrder {
            field: "(tenant, tries)".into()
        }
    );
    assert_eq!(
        (r.stats.documents_examined, r.stats.groups),
        (8, 8),
        "{r:?}"
    );
    tasks.drop_index(["tenant", "tries"]).unwrap();
    let r = run(Filter::new(), &by_tries);
    assert_eq!(r.plan, QueryPlan::Scan, "{r:?}");
    assert_eq!(r.stats.groups, 8);
    // A sorted, limited filter: aggregated from its documents.
    let newest_20 = Filter::new()
        .eq("status", "Done")
        .sort_desc("created")
        .limit(20);
    let r = run(newest_20, &per_status);
    assert_eq!(r.stats.groups, 1);
    assert_eq!(
        run(
            Filter::new().eq("status", "Done").limit(20),
            &Aggregate::new().count().named("n")
        )
        .stats
        .documents_returned,
        1
    );
}
