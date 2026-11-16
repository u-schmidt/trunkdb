use super::*;

// --- OR, AND and NOT (SPEC §36) ---

#[test]
fn nested_filters_find_what_a_scan_finds_on_every_plan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let mut rng = XorShift(0x5151_7A7A_0F0F_3C3C);
    {
        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>("docs");
        docs.ensure_index("v").unwrap();
        docs.ensure_index("w").unwrap();
        for _ in 0..10 {
            let ops = (0..25)
                .map(|_| {
                    let doc = random_document(&mut rng);
                    WriteOp::Insert("docs".into(), db.id_gen().generate(), doc)
                })
                .collect();
            db.write_batch(ops).unwrap();
        }
        assert_nested_filters_agree_with_a_scan(&docs, &mut rng);
    }
    let db = Database::open(&path).unwrap();
    assert_nested_filters_agree_with_a_scan(&db.collection("docs"), &mut rng);
}

/// SPEC §100: where the entries decide, a document is read only to be
/// handed out — and where one entry can't, that document alone.
#[test]
fn where_entries_decide_only_what_is_handed_out_is_read() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("tasks");
    let ops = (0..2000)
        .map(|i| {
            let doc = object(vec![
                ("tenant", format!("t{}", i % 10).into()),
                ("status", ["Queued", "Done"][i as usize % 2].into()),
                ("created", Document::Int(i)),
                ("tries", Document::Int(i % 7)),
            ]);
            WriteOp::Insert("tasks".into(), db.id_gen().generate(), doc)
        })
        .collect();
    db.write_batch(ops).unwrap();
    docs.ensure_index("tenant").unwrap();
    docs.ensure_index(["status", "created"]).unwrap();
    let reads = |read: &dyn Fn() -> usize| {
        let mut n = 0;
        let read = records_read(|| n = read());
        (n, read)
    };
    let tenant = || Filter::new().eq("tenant", "t3");
    let found = |f: Filter| reads(&|| docs.find(f.clone()).unwrap().len());
    // The total behind a page: no document read.
    assert_eq!(reads(&|| docs.count(tenant()).unwrap()), (200, 0));
    let queued = Filter::new().eq("status", "Queued").gte("created", 100);
    assert_eq!(reads(&|| docs.count(queued.clone()).unwrap()), (950, 0));
    // Page 50 of 20, in index order and in `_id` order: 20 read.
    let page = |f: Filter| f.skip(49 * 20).limit(20);
    let by_created = page(Filter::new().eq("status", "Queued").sort_asc("created"));
    assert_eq!(found(by_created), (20, 20));
    assert_eq!(found(page(Filter::new().sort_asc("_id"))), (20, 20));
    // By `_id`, the tenant's range walked in id order (§103): 20 read.
    let newest = tenant().sort_desc("_id").limit(20);
    assert_eq!(found(newest), (20, 20));
    // Sorted by a key the index lacks: of the 400 in the range, the
    // entries reject t4's 200 unread, and the matches are read for
    // the key.
    let t3_of_two = Filter::new()
        .gte("tenant", "t3")
        .lte("tenant", "t4")
        .ne("tenant", "t4");
    assert_eq!(found(t3_of_two.sort_asc("tries").limit(5)), (5, 200));
    // A condition on a field the index lacks: every candidate read.
    let busy = tenant().gt("tries", 4);
    let many = (3..2000).step_by(10).filter(|i| i % 7 > 4).count();
    assert_eq!(found(busy.clone()), (many, 200));
    assert_eq!(reads(&|| docs.count(busy.clone()).unwrap()), (many, 200));
    // An unsorted cursor passes over the skipped unread.
    let streamed = || docs.cursor(tenant().skip(150)).unwrap().count();
    assert_eq!(reads(&streamed), (50, 50));

    // Values an entry can't tell exactly: their documents alone read.
    for (i, value) in [
        Document::from("t3".to_string() + &"x".repeat(2000)),
        Document::Int(1 << 60),
    ]
    .into_iter()
    .enumerate()
    {
        docs.insert(object(vec![
            ("tenant", value.clone()),
            ("tries", Document::Int(i as i64)),
        ]))
        .unwrap();
        let at_least = Filter::new().gte("tenant", value.clone());
        let expected = docs.find(at_least.clone()).unwrap().len();
        // The walk on `tenant` reads the inexact group whole, the
        // first here, though skipped (§34.2).
        let inexact = Filter::new()
            .gte("tenant", value)
            .sort_asc("tenant")
            .limit(5)
            .skip(1);
        assert_eq!(
            reads(&|| docs.count(at_least.clone()).unwrap()),
            (expected, 1)
        );
        assert_eq!(found(inexact).1, (expected - 1).min(5) + 1, "{i}");
    }
    assert_consistent(&db);
}

/// SPEC §101: runs of one value longer than a leaf, ties across leaf
/// boundaries, a cut string and a big number inside a run, emptied
/// leaves: `find`, `count` and a cursor give what checking every
/// document gives, at every skip, in both directions.
#[test]
fn whole_leaves_decide_as_the_documents_would() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index("g").unwrap();
    docs.ensure_index("n").unwrap();
    docs.ensure_index(["g", "n"]).unwrap();
    let mut rng = XorShift(0x0101_1EAF_0101_1EAF);
    let ops = (0..6000i64)
        .map(|i| {
            let g = match rng.below(400) {
                0 => Document::from("b".to_string() + &"x".repeat(3000)),
                1 => Document::Int((1 << 60) + i),
                k => ["a", "b", "c"][k % 3].into(),
            };
            // Ties of three, which a leaf may split.
            let doc = object(vec![("g", g), ("n", Document::Int(i / 3))]);
            WriteOp::Insert("docs".into(), db.id_gen().generate(), doc)
        })
        .collect();
    db.write_batch(ops).unwrap();
    // A stretch emptied, leaving empty leaves behind (§10.5).
    let stretch = Filter::new().gte("n", 700).lt("n", 1100);
    docs.delete_many(stretch).unwrap();
    let mut all = docs.find_with_ids(Filter::default()).unwrap();
    all.sort_by_key(|(id, _doc)| *id);
    let conditions = [
        Filter::new(),
        Filter::new().eq("g", "b"),
        Filter::new().ne("g", "b"),
        Filter::new().gte("g", "b"),
        Filter::new().lt("g", "c"),
        Filter::new().eq("g", "a").gte("n", 300).lt("n", 1500),
        Filter::new().gte("n", 500).lt("n", 1800),
        Filter::new().eq("g", "c").ne("n", 100),
    ];
    let sorts: [&[(&str, SortOrder)]; 6] = [
        &[],
        &[("n", SortOrder::Asc)],
        &[("n", SortOrder::Desc)],
        &[("_id", SortOrder::Asc)],
        &[("_id", SortOrder::Desc)],
        &[("g", SortOrder::Asc)],
    ];
    for _ in 0..300 {
        let mut f = conditions[rng.below(conditions.len())].clone();
        for (field, order) in sorts[rng.below(sorts.len())] {
            f = f.then_by(*field, *order);
        }
        let skip = match rng.below(3) {
            0 => rng.below(20),
            _ => rng.below(5000),
        };
        let f = f.skip(skip).limit([None, Some(1), Some(7)][rng.below(3)]);
        let expected: Vec<DocId> = f
            .apply_to(all.clone(), |(_, doc)| doc)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let plan = explained(&docs, &f);
        assert_eq!(docs.count(f.clone()).unwrap(), expected.len(), "{f:?}");
        let found: Vec<DocId> = docs
            .find_with_ids(f.clone())
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        if f.sort.is_empty() {
            assert_eq!(found.len(), expected.len(), "{f:?} via {plan:?}");
        } else {
            assert_eq!(found, expected, "{f:?} via {plan:?}");
            assert_eq!(typed_ids(&docs, &f), expected, "typed: {f:?}");
        }
        selection_agrees(&docs, &f, &mut rng);
        aggregate_agrees(&docs, &f, &mut rng);
    }
    assert_consistent(&db);
}

/// SPEC §101.5: a skip passes over a leaf whole only where the group
/// its last entry is in is exact. Numbers past 2^60 share an encoding
/// 256 apart, so each such group spans several leaves, and is sorted
/// in memory by the true values — here against id order.
#[test]
fn a_skip_keeps_an_inexact_group_across_leaves_in_its_order() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index(["g", "n"]).unwrap();
    let big = |k: i64| Document::Int((1 << 60) + k);
    let ns = (0..50).map(Document::Int).chain((0..900).rev().map(big));
    let ops = ns
        .map(|n| {
            let doc = object(vec![("g", "b".into()), ("n", n)]);
            WriteOp::Insert("docs".into(), db.id_gen().generate(), doc)
        })
        .collect();
    db.write_batch(ops).unwrap();
    let all = docs.find_with_ids(Filter::default()).unwrap();
    for skip in (0..950).step_by(7) {
        let f = Filter::new().eq("g", "b").sort_asc("n").skip(skip).limit(3);
        let expected: Vec<DocId> = f
            .apply_to(all.clone(), |(_, doc)| doc)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let found: Vec<DocId> = docs
            .find_with_ids(f.clone())
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(found, expected, "skip {skip}");
    }
}

/// SPEC §101: a count and a deep page copy no key, and a run of one
/// value is decided once a leaf at most.
#[test]
fn a_walk_copies_and_decides_only_what_it_must() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("tasks");
    let ops = (0..5000)
        .map(|i| {
            let doc = object(vec![
                ("tenant", format!("t{}", i % 5).into()),
                ("status", ["Queued", "Done"][i as usize % 2].into()),
                ("created", Document::Int(i)),
            ]);
            WriteOp::Insert("tasks".into(), db.id_gen().generate(), doc)
        })
        .collect();
    db.write_batch(ops).unwrap();
    docs.ensure_index("tenant").unwrap();
    docs.ensure_index(["status", "created"]).unwrap();
    let counted = |run: &dyn Fn() -> usize| {
        let copied = crate::index::KEYS_COPIED.with(|n| n.replace(0));
        let decided = crate::query::DECISIONS.with(|n| n.replace(0));
        let found = run();
        let copied = crate::index::KEYS_COPIED.with(|n| n.replace(copied));
        let decided = crate::query::DECISIONS.with(|n| n.replace(decided));
        (found, copied, decided)
    };
    let entries = |f: Filter| {
        let run = docs.explain_run(f).unwrap();
        run.reads[0].entries as usize
    };
    let tenant = Filter::new().eq("tenant", "t3");
    let (n, copied, decided) = counted(&|| docs.count(tenant.clone()).unwrap());
    assert_eq!((n, copied), (1000, 0));
    // A leaf holds well over ten entries of this index.
    assert!(decided <= entries(tenant.clone()) / 10, "{decided}");
    assert_eq!(
        counted(&|| docs.count(Filter::new()).unwrap()),
        (5000, 0, 0)
    );
    let page = Filter::new()
        .eq("status", "Done")
        .sort_asc("created")
        .skip(1000)
        .limit(20);
    let (n, copied, _) = counted(&|| docs.find(page.clone()).unwrap().len());
    assert_eq!((n, copied), (20, 0));
    let by_id = Filter::new().sort_desc("_id").skip(3000).limit(20);
    let (n, copied, _) = counted(&|| docs.find(by_id.clone()).unwrap().len());
    assert_eq!((n, copied), (20, 0));
}

/// SPEC §103: a sort by `_id` after `Eq`s fixing an index — one
/// field, compound or multikey — gives what sorting every match in
/// memory gives, with values that share an encoding (cut strings,
/// numbers past 2^53, `Int` and `Float`), nulls and missing fields,
/// `_id` comparisons, a second sort key and skips of every size,
/// through every kind of write.
#[test]
fn a_walk_by_id_after_an_eq_gives_what_a_sort_in_memory_gives() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let db = Database::open(&path).unwrap();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index("t").unwrap();
    docs.ensure_index(["t", "u"]).unwrap();
    docs.ensure_index("tags[*]").unwrap();
    let long = |end: &str| Document::from("b".to_string() + &"x".repeat(3000) + end);
    let values = [
        "a".into(),
        "b".into(),
        long("1"),
        long("2"),
        Document::Int(1 << 60),
        Document::Int((1 << 60) + 1),
        Document::Int(5),
        Document::Float(5.0),
        Document::Null,
    ];
    let mut rng = XorShift(0x0103_1D1D_0103_1D1D);
    let random_doc = |rng: &mut XorShift| {
        let mut fields = Vec::new();
        if rng.below(8) != 0 {
            fields.push(("t", values[rng.below(values.len())].clone()));
        }
        if rng.below(5) != 0 {
            fields.push(("u", Document::Int(rng.below(3) as i64)));
        }
        if rng.below(3) != 0 {
            let tags = (0..rng.below(4))
                .map(|_| values[rng.below(values.len())].clone())
                .collect();
            fields.push(("tags", Document::Array(tags)));
        }
        fields.push(("c", Document::Int(rng.below(3) as i64)));
        object(fields)
    };
    let ops = (0..3000)
        .map(|_| WriteOp::Insert("docs".into(), db.id_gen().generate(), random_doc(&mut rng)))
        .collect();
    db.write_batch(ops).unwrap();
    let walked = std::cell::Cell::new(0);
    let check = |docs: &Collection<Document>, rng: &mut XorShift| {
        let mut all = docs.find_with_ids(Filter::default()).unwrap();
        all.sort_by_key(|(id, _doc)| *id);
        let ids: Vec<DocId> = all.iter().map(|(id, _)| *id).collect();
        for _ in 0..150 {
            let value = |rng: &mut XorShift| values[rng.below(values.len())].clone();
            let mut f = match rng.below(3) {
                0 => Filter::new().eq("t", value(rng)),
                1 => Filter::new()
                    .eq("t", value(rng))
                    .eq("u", rng.below(3) as i64),
                _ => Filter::new().eq("tags[*]", value(rng)),
            };
            if rng.below(3) == 0 {
                let id = Document::Id(ids[rng.below(ids.len())]);
                let ops = [Op::Gt, Op::Gte, Op::Lt, Op::Lte, Op::Ne];
                f = f.and(Condition::Compare {
                    field: "_id".into(),
                    op: ops[rng.below(ops.len())].clone(),
                    value: id,
                });
            }
            if rng.below(4) == 0 {
                f = f.eq("c", rng.below(3) as i64);
            }
            f = f.then_by("_id", [SortOrder::Asc, SortOrder::Desc][rng.below(2)]);
            if rng.below(4) == 0 {
                f = f.then_desc("u");
            }
            let skip = match rng.below(3) {
                0 => 0,
                1 => rng.below(10),
                _ => rng.below(400),
            };
            let f = f.skip(skip).limit([1, 7, 50][rng.below(3)]);
            let expected: Vec<DocId> = f
                .apply_to(all.clone(), |(_, doc)| doc)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let plan = explained(docs, &f);
            if matches!(&plan, QueryPlan::IndexOrder { field } if field != "_id") {
                walked.set(walked.get() + 1);
            }
            let found: Vec<DocId> = docs
                .find_with_ids(f.clone())
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            assert_eq!(found, expected, "{f:?} via {plan:?}");
            assert_eq!(typed_ids(docs, &f), expected, "typed: {f:?}");
            selection_agrees(docs, &f, rng);
            aggregate_agrees(docs, &f, rng);
        }
    };
    check(&docs, &mut rng);
    for round in 0..4 {
        let mut ids: Vec<DocId> = docs
            .find_with_ids(Filter::default())
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        match round {
            0 => {
                let ops = (0..500)
                    .map(|_| {
                        WriteOp::Insert("docs".into(), db.id_gen().generate(), random_doc(&mut rng))
                    })
                    .collect();
                db.write_batch(ops).unwrap();
            }
            1 => {
                let ops = (0..300)
                    .map(|_| ids[rng.below(ids.len())])
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .map(|id| WriteOp::Update("docs".into(), id, random_doc(&mut rng)))
                    .collect();
                db.write_batch(ops).unwrap();
            }
            2 => {
                let ops = (0..300)
                    .map(|_| ids[rng.below(ids.len())])
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .map(|id| WriteOp::Delete("docs".into(), id));
                db.write_batch(ops.collect()).unwrap();
            }
            _ => {
                let update = crate::query::Update::new().set("t", "a").set("u", 2);
                docs.update_fields(Filter::new().eq("c", 1), &update)
                    .unwrap();
            }
        }
        check(&docs, &mut rng);
    }
    assert!(walked.get() > 300, "{}", walked.get());
    assert_consistent(&db);
    drop((docs, db));
    let db = Database::open(&path).unwrap();
    check(&db.collection("docs"), &mut rng);
}

/// SPEC §103.4: "the newest 20 of a tenant" walks the tenant's range
/// from its end and reads 20 entries and 20 documents; a deep page
/// passes its skip leaf by leaf; paging by the last id starts after it.
#[test]
fn a_walk_by_id_after_an_eq_reads_only_its_page() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("tasks");
    let ops = (0..5000)
        .map(|i| {
            let doc = object(vec![
                ("tenant", format!("t{}", i % 5).into()),
                ("tries", Document::Int(i % 7)),
                ("note", Document::Int(i % 3)),
            ]);
            WriteOp::Insert("tasks".into(), db.id_gen().generate(), doc)
        })
        .collect();
    db.write_batch(ops).unwrap();
    docs.ensure_index("tenant").unwrap();
    docs.ensure_index(["tenant", "tries"]).unwrap();
    let tenant = || Filter::new().eq("tenant", "t3");
    let mut ids: Vec<DocId> = docs
        .find_with_ids(tenant())
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    ids.sort();
    let ids_of = |f: &Filter| -> Vec<DocId> {
        docs.find_with_ids(f.clone())
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    };

    let newest = tenant().sort_desc("_id").limit(20);
    let run = docs.explain_run(newest.clone()).unwrap();
    assert_eq!(
        run.plan,
        QueryPlan::IndexOrder {
            field: "tenant".into()
        }
    );
    let sort = SortWork {
        from_index: 1,
        in_memory: 0,
    };
    assert_eq!(run.sort, Some(sort));
    let read = &run.reads[0];
    assert_eq!(
        (read.index.as_str(), read.backward, read.covering),
        ("tenant", true, true)
    );
    assert_eq!(read.bounds, vec![Condition::eq("tenant", "t3")]);
    assert_eq!((read.entries, run.stats.documents_examined), (20, 20));
    let expected: Vec<DocId> = ids.iter().rev().take(20).copied().collect();
    assert_eq!(ids_of(&newest), expected);

    // Page 25 of 20, both ways, and on a compound index fixed whole:
    // the skip passes whole leaves, copying no key and deciding once
    // a leaf at most.
    let counted = |f: &Filter| {
        let copied = crate::index::KEYS_COPIED.with(|n| n.replace(0));
        let decided = crate::query::DECISIONS.with(|n| n.replace(0));
        let mut found = Vec::new();
        let read = records_read(|| found = ids_of(f));
        let copied = crate::index::KEYS_COPIED.with(|n| n.replace(copied));
        let decided = crate::query::DECISIONS.with(|n| n.replace(decided));
        (found, read, copied, decided)
    };
    let deep = tenant().sort_asc("_id").skip(480).limit(20);
    let (found, read, copied, decided) = counted(&deep);
    assert_eq!((found, read, copied), (ids[480..500].to_vec(), 20, 0));
    assert!(decided <= 10, "{decided}");
    let deep_back = tenant().sort_desc("_id").skip(480).limit(20);
    let back: Vec<DocId> = ids.iter().rev().skip(480).take(20).copied().collect();
    let (found, read, copied, decided) = counted(&deep_back);
    assert_eq!((found, read, copied), (back, 20, 0));
    assert!(decided <= 10, "{decided}");
    let fixed = tenant().eq("tries", 2).sort_desc("_id").skip(60).limit(20);
    assert_eq!(
        explained(&docs, &fixed),
        QueryPlan::IndexOrder {
            field: "(tenant, tries)".into()
        }
    );
    let (found, read, copied, _) = counted(&fixed);
    assert_eq!((found.len(), read, copied), (20, 20, 0));
    let run = docs.explain_run(deep).unwrap();
    assert!(run.stats.keys_examined >= 500 && run.stats.documents_examined == 20);

    // The 20 after the last one shown: from right after it.
    let after = tenant().gt("_id", ids[600]).sort_asc("_id").limit(20);
    let run = docs.explain_run(after.clone()).unwrap();
    assert_eq!(run.reads[0].bounds.len(), 2);
    assert_eq!(
        (run.reads[0].entries, run.stats.documents_examined),
        (20, 20)
    );
    assert_eq!(ids_of(&after), ids[601..621]);
    let before = tenant().lte("_id", ids[600]).sort_desc("_id").limit(3);
    assert_eq!(ids_of(&before), [ids[600], ids[599], ids[598]]);

    // A condition the index lacks: documents read until 20 match.
    let busy = tenant().eq("note", 1).sort_asc("_id").limit(20);
    let run = docs.explain_run(busy.clone()).unwrap();
    assert!(!run.reads[0].covering);
    assert!(
        run.stats.documents_examined < 1000,
        "{}",
        run.stats.documents_examined
    );
    assert_eq!(run.stats.documents_returned, 20);
}

/// SPEC §167: a sort by `_id` after an OR or an `in` of `Eq`s, each
/// fixing its index whole — one field, compound or multikey, one
/// index or several — merges the ranges by id and gives what sorting
/// every match in memory gives: with values that share an encoding,
/// a document in several ranges, the same value twice, `_id`
/// comparisons, a second sort key and skips of every size, through
/// every kind of write.
#[test]
fn an_or_of_eqs_merged_by_id_gives_what_a_sort_in_memory_gives() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let db = Database::open(&path).unwrap();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index("t").unwrap();
    docs.ensure_index(["t", "u"]).unwrap();
    docs.ensure_index("tags[*]").unwrap();
    let long = |end: &str| Document::from("b".to_string() + &"x".repeat(3000) + end);
    let values = [
        "a".into(),
        "b".into(),
        long("1"),
        long("2"),
        Document::Int(1 << 60),
        Document::Int((1 << 60) + 1),
        Document::Int(5),
        Document::Float(5.0),
        Document::Null,
    ];
    let mut rng = XorShift(0x0167_3E3E_0167_3E3E);
    let random_doc = |rng: &mut XorShift| {
        let mut fields = Vec::new();
        if rng.below(8) != 0 {
            fields.push(("t", values[rng.below(values.len())].clone()));
        }
        if rng.below(5) != 0 {
            fields.push(("u", Document::Int(rng.below(3) as i64)));
        }
        if rng.below(3) != 0 {
            let tags = (0..rng.below(4))
                .map(|_| values[rng.below(values.len())].clone())
                .collect();
            fields.push(("tags", Document::Array(tags)));
        }
        fields.push(("c", Document::Int(rng.below(3) as i64)));
        object(fields)
    };
    let ops = (0..3000)
        .map(|_| WriteOp::Insert("docs".into(), db.id_gen().generate(), random_doc(&mut rng)))
        .collect();
    db.write_batch(ops).unwrap();
    let merged = std::cell::Cell::new(0);
    let check = |docs: &Collection<Document>, rng: &mut XorShift| {
        let mut all = docs.find_with_ids(Filter::default()).unwrap();
        all.sort_by_key(|(id, _doc)| *id);
        let ids: Vec<DocId> = all.iter().map(|(id, _)| *id).collect();
        for _ in 0..150 {
            let value = |rng: &mut XorShift| values[rng.below(values.len())].clone();
            let branch = |rng: &mut XorShift| match rng.below(3) {
                0 => Condition::eq("t", value(rng)),
                1 => Condition::eq("t", value(rng)) & Condition::eq("u", rng.below(3) as i64),
                _ => Condition::eq("tags[*]", value(rng)),
            };
            let mut f = match rng.below(3) {
                0 => Filter::new().is_in("t", (0..2 + rng.below(3)).map(|_| value(rng))),
                1 => Filter::new().is_in("tags[*]", (0..2 + rng.below(3)).map(|_| value(rng))),
                _ => Filter::new().any_of((0..2 + rng.below(3)).map(|_| branch(rng))),
            };
            if rng.below(3) == 0 {
                let id = Document::Id(ids[rng.below(ids.len())]);
                let ops = [Op::Gt, Op::Gte, Op::Lt, Op::Lte, Op::Ne];
                f = f.and(Condition::Compare {
                    field: "_id".into(),
                    op: ops[rng.below(ops.len())].clone(),
                    value: id,
                });
                if rng.below(3) == 0 {
                    let other = Document::Id(ids[rng.below(ids.len())]);
                    f = f.and(Condition::Compare {
                        field: "_id".into(),
                        op: ops[rng.below(4)].clone(),
                        value: other,
                    });
                }
            }
            if rng.below(4) == 0 {
                f = f.eq("c", rng.below(3) as i64);
            }
            f = f.then_by("_id", [SortOrder::Asc, SortOrder::Desc][rng.below(2)]);
            if rng.below(4) == 0 {
                f = f.then_desc("u");
            }
            let skip = match rng.below(3) {
                0 => 0,
                1 => rng.below(10),
                _ => rng.below(400),
            };
            let f = f.skip(skip).limit([1, 7, 50][rng.below(3)]);
            let expected: Vec<DocId> = f
                .apply_to(all.clone(), |(_, doc)| doc)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let plan = explained(docs, &f);
            if matches!(&plan, QueryPlan::IndexUnion { in_order: true, .. }) {
                merged.set(merged.get() + 1);
            }
            let found: Vec<DocId> = docs
                .find_with_ids(f.clone())
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            assert_eq!(found, expected, "{f:?} via {plan:?}");
            assert_eq!(typed_ids(docs, &f), expected, "typed: {f:?}");
            selection_agrees(docs, &f, rng);
            aggregate_agrees(docs, &f, rng);
        }
    };
    check(&docs, &mut rng);
    for round in 0..4 {
        let mut ids: Vec<DocId> = docs
            .find_with_ids(Filter::default())
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        match round {
            0 => {
                let ops = (0..500)
                    .map(|_| {
                        WriteOp::Insert("docs".into(), db.id_gen().generate(), random_doc(&mut rng))
                    })
                    .collect();
                db.write_batch(ops).unwrap();
            }
            1 => {
                let ops = (0..300)
                    .map(|_| ids[rng.below(ids.len())])
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .map(|id| WriteOp::Update("docs".into(), id, random_doc(&mut rng)))
                    .collect();
                db.write_batch(ops).unwrap();
            }
            2 => {
                let ops = (0..300)
                    .map(|_| ids[rng.below(ids.len())])
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .map(|id| WriteOp::Delete("docs".into(), id));
                db.write_batch(ops.collect()).unwrap();
            }
            _ => {
                let update = crate::query::Update::new().set("t", "a").set("u", 2);
                docs.update_fields(Filter::new().eq("c", 1), &update)
                    .unwrap();
            }
        }
        check(&docs, &mut rng);
    }
    assert!(merged.get() > 500, "{}", merged.get());
    assert_consistent(&db);
    drop((docs, db));
    let db = Database::open(&path).unwrap();
    check(&db.collection("docs"), &mut rng);
}

/// SPEC §167: "the newest 20 of two tenants" merges the two ranges
/// by id from their ends and reads 20 entries and two more, and 20
/// documents; a deep page reads only its own documents; paging by
/// the last id starts each range after it; ranges of two indexes
/// merge too, without deciding; a compound index fixed in part, or
/// ids beside the ranges, still sort in memory.
#[test]
fn an_or_of_eqs_merged_by_id_reads_only_its_page() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("tasks");
    let ops = (0..5000)
        .map(|i| {
            let doc = object(vec![
                ("tenant", format!("t{}", i % 5).into()),
                ("tries", Document::Int(i % 7)),
                ("status", ["open", "done"][i as usize % 2].into()),
            ]);
            WriteOp::Insert("tasks".into(), db.id_gen().generate(), doc)
        })
        .collect();
    db.write_batch(ops).unwrap();
    docs.ensure_index("tenant").unwrap();
    docs.ensure_index("status").unwrap();
    docs.ensure_index(["tenant", "tries"]).unwrap();
    let ids_of = |f: &Filter| -> Vec<DocId> {
        docs.find_with_ids(f.clone())
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    };
    let two = || Filter::new().is_in("tenant", ["t1", "t3"]);
    let mut ids = ids_of(&two());
    ids.sort();
    assert_eq!(ids.len(), 2000);
    let tenant = |name: &str| vec![Condition::eq("tenant", name)];

    let newest = two().sort_desc("_id").limit(20);
    let run = docs.explain_run(newest.clone()).unwrap();
    assert_eq!(
        run.plan,
        QueryPlan::IndexUnion {
            fields: vec!["tenant".into(), "tenant".into()],
            in_order: true,
        }
    );
    let sort = SortWork {
        from_index: 1,
        in_memory: 0,
    };
    assert_eq!(run.sort, Some(sort));
    let reads: Vec<_> = (run.reads.iter())
        .map(|r| (r.index.as_str(), r.bounds.clone(), r.backward, r.covering))
        .collect();
    assert_eq!(
        reads,
        [
            ("tenant", tenant("t1"), true, true),
            ("tenant", tenant("t3"), true, true),
        ]
    );
    // Each range's next entry is read before the one handed out
    // before it: 20, and one more in each, at most.
    assert!(run.stats.keys_examined <= 22, "{}", run.stats.keys_examined);
    assert_eq!(run.stats.documents_examined, 20);
    let expected: Vec<DocId> = ids.iter().rev().take(20).copied().collect();
    assert_eq!(ids_of(&newest), expected);
    // A second sort key: ids are unique, so the walk gives it too.
    let two_keys = two().sort_desc("_id").then_asc("tries").limit(20);
    let sort = SortWork {
        from_index: 2,
        in_memory: 0,
    };
    assert_eq!(docs.explain_run(two_keys).unwrap().sort, Some(sort));

    // Page 25 of 20: the skip is decided on entries, only the page's
    // documents are read.
    let deep = two().sort_asc("_id").skip(480).limit(20);
    let mut found = Vec::new();
    let decided = crate::query::DECISIONS.with(|n| n.replace(0));
    let read = records_read(|| found = ids_of(&deep));
    let decided = crate::query::DECISIONS.with(|n| n.replace(decided));
    assert_eq!((found, read), (ids[480..500].to_vec(), 20));
    // One decision a range, each taken for the range's other entries.
    assert!(decided <= 4, "{decided}");
    let run = docs.explain_run(deep).unwrap();
    assert!(
        run.stats.keys_examined <= 502,
        "{}",
        run.stats.keys_examined
    );

    // The 20 after the last one shown: each range from right after it.
    let after = two().gt("_id", ids[600]).sort_asc("_id").limit(20);
    let run = docs.explain_run(after.clone()).unwrap();
    for read in &run.reads {
        assert_eq!(read.bounds[1], Condition::gt("_id", ids[600]));
    }
    assert!(run.stats.keys_examined <= 22, "{}", run.stats.keys_examined);
    assert_eq!(run.stats.documents_examined, 20);
    assert_eq!(ids_of(&after), ids[601..621]);
    let before = two().lte("_id", ids[600]).sort_desc("_id").limit(3);
    assert_eq!(ids_of(&before), [ids[600], ids[599], ids[598]]);

    // Two indexes: merged, each document once, read to be checked.
    let either = Filter::new()
        .any_of([
            Condition::eq("tenant", "t1"),
            Condition::eq("status", "open"),
        ])
        .sort_asc("_id")
        .limit(20);
    let run = docs.explain_run(either.clone()).unwrap();
    assert_eq!(
        run.plan,
        QueryPlan::IndexUnion {
            fields: vec!["tenant".into(), "status".into()],
            in_order: true,
        }
    );
    assert!(run.reads.iter().all(|read| !read.covering));
    assert_eq!(run.stats.documents_examined, 20);
    let mut all = docs.find_with_ids(Filter::default()).unwrap();
    all.sort_by_key(|(id, _)| *id);
    let expected: Vec<DocId> = (either.apply_to(all, |(_, doc)| doc).into_iter())
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ids_of(&either), expected);

    // A compound index fixed whole merges; fixed in part, its keys
    // are in the order of `tries`, and the matches sort in memory.
    let fixed = Filter::new()
        .any_of([
            Condition::eq("tenant", "t1") & Condition::eq("tries", 2),
            Condition::eq("tenant", "t3") & Condition::eq("tries", 4),
        ])
        .sort_desc("_id")
        .limit(5);
    assert!(matches!(
        explained(&docs, &fixed),
        QueryPlan::IndexUnion { in_order: true, ref fields } if fields[0] == "(tenant, tries)"
    ));
    let in_part = two()
        .use_index(["tenant", "tries"])
        .sort_asc("_id")
        .limit(20);
    let run = docs.explain_run(in_part.clone()).unwrap();
    assert!(matches!(
        run.plan,
        QueryPlan::IndexUnion {
            in_order: false,
            ..
        }
    ));
    assert_eq!(run.stats.keys_examined, 2000);
    assert_eq!(ids_of(&in_part), ids[..20]);

    // Without a limit, or sorted by another field first: no merge.
    let unlimited = two().sort_asc("_id");
    assert!(matches!(
        explained(&docs, &unlimited),
        QueryPlan::IndexUnion {
            in_order: false,
            ..
        }
    ));
    assert_eq!(ids_of(&unlimited), ids);
    let by_tries = two().sort_asc("tries").then_asc("_id").limit(5);
    assert!(matches!(
        explained(&docs, &by_tries),
        QueryPlan::IndexUnion {
            in_order: false,
            ..
        }
    ));
    let mut all = docs.find_with_ids(Filter::default()).unwrap();
    all.sort_by_key(|(id, _)| *id);
    let expected: Vec<DocId> = (by_tries.apply_to(all, |(_, doc)| doc).into_iter())
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ids_of(&by_tries), expected);

    // Ids beside the ranges (SPEC §164): looked up, and sorted.
    let with_id = Filter::new()
        .any_of([Condition::id(ids[7]), Condition::eq("tenant", "t1")])
        .sort_asc("_id")
        .limit(5);
    assert!(matches!(
        explained(&docs, &with_id),
        QueryPlan::IndexUnion { in_order: false, ref fields } if fields[0] == "_id"
    ));

    // Hinted to walk the primary index, the merge is the alternative
    // that pays, counted as the run would read it (SPEC §133).
    let hinted = two().use_index("_id").sort_desc("_id").limit(20);
    let run = docs.explain_run(hinted).unwrap();
    let merge = (run.alternatives.iter())
        .find(|a| matches!(a.plan, QueryPlan::IndexUnion { in_order: true, .. }))
        .expect("the merge among the alternatives");
    let n = docs.explain_run(newest).unwrap().stats.keys_examined;
    assert_eq!(merge.entries, EntryCount::Exactly(n));
    assert!(merge.fewer && !merge.reads_documents);
}

/// SPEC §104: a count over a range of different values decides on
/// the entries' bytes, filling no document, and counts the leaves
/// inside the range whole; a deep page by such a range passes them;
/// a compound key's value no key encodes decides unread.
#[test]
fn a_range_is_decided_on_bytes_and_its_inner_leaves_whole() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("tasks");
    let ops = (0..5000)
        .map(|i| {
            let doc = object(vec![
                ("status", ["Queued", "Done"][i as usize % 2].into()),
                ("created", Document::Int(i)),
            ]);
            WriteOp::Insert("tasks".into(), db.id_gen().generate(), doc)
        })
        .collect();
    db.write_batch(ops).unwrap();
    docs.ensure_index("created").unwrap();
    docs.ensure_index(["status", "created"]).unwrap();
    let counted = |run: &dyn Fn() -> usize| {
        let decided = crate::query::DECISIONS.with(|n| n.replace(0));
        let filled = crate::query::FILLS.with(|n| n.replace(0));
        let copied = crate::index::KEYS_COPIED.with(|n| n.replace(0));
        let mut found = 0;
        let read = records_read(|| found = run());
        let decided = crate::query::DECISIONS.with(|n| n.replace(decided));
        let filled = crate::query::FILLS.with(|n| n.replace(filled));
        let copied = crate::index::KEYS_COPIED.with(|n| n.replace(copied));
        (found, read, decided, filled, copied)
    };
    // 2,000 different values, in leaves of a few hundred entries.
    let range = Filter::new().gte("created", 1000).lt("created", 3000);
    let entries = docs.explain_run(range.clone()).unwrap().reads[0].entries as usize;
    let (n, read, decided, filled, _) = counted(&|| docs.count(range.clone()).unwrap());
    assert_eq!((n, read, filled), (2000, 0, 0));
    assert!(decided < entries / 2, "{decided} of {entries}");
    // Fixed by `status`, ranged on `created`, on the compound index.
    let done = range.clone().eq("status", "Done");
    let (n, read, decided, filled, _) = counted(&|| docs.count(done.clone()).unwrap());
    assert_eq!((n, read, filled), (1000, 0, 0));
    assert!(decided < 500, "{decided}");
    // `ne` accepts two intervals: decided entry by entry, on bytes.
    let gap = range.clone().ne("created", 2000);
    let (n, _, decided, filled, _) = counted(&|| docs.count(gap.clone()).unwrap());
    assert_eq!((n, filled), (1999, 0));
    assert!(decided >= 2000, "{decided}");
    // Page 50 of 20 by the range: the skip passes whole leaves.
    let page = Filter::new()
        .gte("created", 1000)
        .sort_asc("created")
        .skip(980)
        .limit(20);
    let (n, read, _, filled, copied) = counted(&|| docs.find(page.clone()).unwrap().len());
    assert_eq!((n, read, filled, copied), (20, 20, 0, 0));
    // A value no key encodes, in a compound key: decided, unread.
    docs.insert(object(vec![
        ("status", "Done".into()),
        ("created", Document::Array(vec![Document::Int(1)])),
    ]))
    .unwrap();
    let some = Filter::new().eq("status", "Done").ne("created", 7);
    let (n, read, _, filled, _) = counted(&|| docs.count(some.clone()).unwrap());
    // 2,500 done, one more, less created 7.
    assert_eq!((n, read, filled), (2500, 0, 0));
    // A leaf of the walk's range whose ends both fail, with matches
    // between: the range holds the bounds' own encodings (§28.3), so
    // a leaf from a run of 10s to a run of 12s lies inside it.
    let edges = db.collection::<Document>("edges");
    edges.ensure_index("v").unwrap();
    let ops = (0..2000)
        .map(|i| Document::Int(if i < 1000 { 10 } else { 12 }))
        .chain((0..5).map(|_| Document::Int(11)))
        .map(|v| {
            WriteOp::Insert(
                "edges".into(),
                db.id_gen().generate(),
                object(vec![("v", v)]),
            )
        })
        .collect();
    db.write_batch(ops).unwrap();
    let between = Filter::new().gt("v", 10).lt("v", 12);
    assert_eq!(edges.count(between.clone()).unwrap(), 5);
    assert_eq!(edges.find(between).unwrap().len(), 5);
    // `contains` on a string needs the text.
    let named = Filter::new().eq("status", "Done").contains("status", "ON");
    let (n, _, _, filled, _) = counted(&|| docs.count(named.clone()).unwrap());
    assert_eq!(n, 2501);
    assert!(filled > 0);
}

/// Conditions mostly on what the indexes of
/// `entries_decide_as_the_documents_would` hold — `a`, `b`, `o.p`
/// and `_id` — now and then on `c`, which none holds.
fn covered_condition(rng: &mut XorShift, depth: usize, ids: &[DocId]) -> Condition {
    if depth == 0 || rng.below(3) == 0 {
        #[cfg(feature = "regex")]
        if rng.below(8) == 0 {
            let field = ["b", "b", "o.p", "c"][rng.below(4)];
            return Condition::regex(field, random_regex(rng));
        }
        let ops = [
            Op::Eq,
            Op::Ne,
            Op::Lt,
            Op::Lte,
            Op::Gt,
            Op::Gte,
            Op::Contains,
            Op::StartsWith,
        ];
        let op = ops[rng.below(ops.len())].clone();
        return match rng.below(9) {
            0..=2 => cond("b", op, random_value(rng)),
            3 => cond("a", op, Document::Int(rng.below(4) as i64 - 1)),
            4 | 5 => cond("o.p", op, random_value(rng)),
            6 | 7 => cond("_id", op, Document::Id(ids[rng.below(ids.len())])),
            _ => cond("c", op, Document::Int(rng.below(3) as i64)),
        };
    }
    let children = (0..rng.below(4))
        .map(|_| covered_condition(rng, depth - 1, ids))
        .collect::<Vec<_>>();
    match rng.below(5) {
        0 => Condition::All(children),
        1 => !covered_condition(rng, depth - 1, ids),
        _ => Condition::Any(children),
    }
}

/// SPEC §100: where index entries decide, `find`, `count` and a
/// cursor, typed and untyped, give what checking every document gives
/// — on one-field, nested, compound and sparse indexes, with `_id` in
/// the conditions and the sort, with values that don't decide (cut
/// strings, numbers past 2^53, and arrays, objects and NaN in a
/// compound key), and skips of every size; and `delete_many` and
/// `update_fields` change exactly what `find` returns.
#[test]
fn entries_decide_as_the_documents_would() {
    for sparse in [false, true] {
        let (_dir, db) = fresh_db();
        let docs = db.collection::<Document>("docs");
        let options = match sparse {
            true => IndexOptions::new().sparse(),
            false => IndexOptions::new(),
        };
        docs.ensure_index_with("b", options.clone()).unwrap();
        docs.ensure_index_with(["a", "b"], options.clone()).unwrap();
        docs.ensure_index_with("o.p", options).unwrap();
        let mut rng = XorShift(0x0100_C0DE_0100_C0DE ^ sparse as u64);
        let ops = (0..300)
            .map(|_| {
                let mut fields = Vec::new();
                match rng.below(5) {
                    0 => {}
                    1 => fields.push(("a", Document::Null)),
                    _ => fields.push(("a", Document::Int(rng.below(3) as i64))),
                }
                if rng.below(6) != 0 {
                    fields.push(("b", random_value(&mut rng)));
                }
                match rng.below(4) {
                    0 => {}
                    1 => fields.push(("o", Document::Int(1))),
                    _ => fields.push(("o", object(vec![("p", random_value(&mut rng))]))),
                }
                fields.push(("c", Document::Int(rng.below(3) as i64)));
                WriteOp::Insert("docs".into(), db.id_gen().generate(), object(fields))
            })
            .collect();
        db.write_batch(ops).unwrap();
        let mut all = docs.find_with_ids(Filter::default()).unwrap();
        all.sort_by_key(|(id, _doc)| *id);
        let all = all;
        let ids: Vec<DocId> = all.iter().map(|(id, _)| *id).collect();
        let sorts: [&[(&str, SortOrder)]; 7] = [
            &[],
            &[("b", SortOrder::Asc)],
            &[("b", SortOrder::Desc), ("_id", SortOrder::Desc)],
            &[("a", SortOrder::Asc), ("b", SortOrder::Asc)],
            &[("_id", SortOrder::Desc)],
            &[("o.p", SortOrder::Asc)],
            &[("c", SortOrder::Asc), ("b", SortOrder::Desc)],
        ];
        let random_filter = |rng: &mut XorShift| {
            let mut f = Filter::new();
            for _ in 0..rng.below(3) {
                f = f.and(covered_condition(rng, 2, &ids));
            }
            if rng.below(10) == 0 {
                f = f.exists("b"); // never decided by an entry
            }
            for (field, order) in sorts[rng.below(sorts.len())] {
                f = f.then_by(*field, *order);
            }
            f.skip([0, 0, 1, 5, 40, 1000][rng.below(6)])
                .limit([None, Some(0), Some(1), Some(5), Some(30)][rng.below(5)])
        };
        let (mut covered, mut skipped_unread, mut answered) = (0, 0, 0);
        let mut aggregated = 0;
        for _ in 0..600 {
            let f = random_filter(&mut rng);
            let unlimited = Filter {
                skip: 0,
                limit: None,
                ..f.clone()
            };
            let matching: std::collections::HashSet<DocId> = unlimited
                .apply_to(all.clone(), |(_, doc)| doc)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let expected: Vec<DocId> = f
                .apply_to(all.clone(), |(_, doc)| doc)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let plan = explained(&docs, &f);
            let run = docs.explain_run(f.clone()).unwrap();
            if run.reads.iter().any(|read| read.covering) {
                covered += 1;
                if f.skip > 0 && run.stats.documents_examined <= run.stats.documents_returned {
                    skipped_unread += 1;
                }
            }
            let found: Vec<DocId> = docs
                .find_with_ids(f.clone())
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let typed = typed_ids(&docs, &f);
            answered += selection_agrees(&docs, &f, &mut rng);
            aggregated += aggregate_agrees(&docs, &f, &mut rng);
            let streamed: Vec<DocId> = docs
                .cursor(f.clone())
                .unwrap()
                .map(|doc| match doc.unwrap() {
                    Document::Object(fields) => match fields["_id"] {
                        Document::Id(id) => id,
                        _ => panic!("no id"),
                    },
                    doc => panic!("not an object: {doc}"),
                })
                .collect();
            let count = docs.count(f.clone()).unwrap();
            assert_eq!(count, expected.len(), "{f:?} via {plan:?}");
            if f.sort.is_empty() {
                // Which matches a skip passes over is unspecified
                // without a sort (§72.1): as many, and all matches.
                for got in [&found, &typed, &streamed] {
                    assert_eq!(got.len(), expected.len(), "{f:?} via {plan:?}");
                    assert!(got.iter().all(|id| matching.contains(id)), "{f:?}");
                    let unique: std::collections::HashSet<_> = got.iter().collect();
                    assert_eq!(unique.len(), got.len(), "{f:?}");
                }
            } else {
                assert_eq!(found, expected, "{f:?} via {plan:?}");
                assert_eq!(typed, expected, "typed: {f:?} via {plan:?}");
                assert_eq!(streamed, expected, "cursor: {f:?} via {plan:?}");
            }
        }
        assert!(covered > 150, "{covered} covered");
        assert!(skipped_unread > 20, "{skipped_unread} skipped unread");
        // Results answered from their entries (SPEC §106.5).
        assert!(answered > 100, "{answered} answered from entries");
        // Mostly sorted or limited: aggregated from documents.
        let _ = aggregated;

        // Writes change what `find` returns, no more and no less.
        for _ in 0..40 {
            let f = random_filter(&mut rng);
            let before: std::collections::HashSet<DocId> = docs
                .find_with_ids(Filter::new())
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let doomed: Vec<DocId> = docs
                .find_with_ids(f.clone())
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            if f.sort.is_empty() && (f.skip > 0 || f.limit.is_some()) {
                continue; // which ones is unspecified
            }
            match rng.below(2) {
                0 => {
                    assert_eq!(docs.delete_many(f.clone()).unwrap(), doomed.len(), "{f:?}");
                    let after: std::collections::HashSet<DocId> = docs
                        .find_with_ids(Filter::new())
                        .unwrap()
                        .into_iter()
                        .map(|(id, _)| id)
                        .collect();
                    let mut gone: Vec<DocId> = before.difference(&after).copied().collect();
                    gone.sort();
                    let mut doomed = doomed;
                    doomed.sort();
                    assert_eq!(gone, doomed, "{f:?}");
                }
                _ => {
                    let marked = Update::new().set("mark", Document::Int(1));
                    docs.update_fields(Filter::new(), &Update::new().unset("mark"))
                        .unwrap();
                    let changed = docs.update_fields(f.clone(), &marked).unwrap();
                    assert_eq!(changed, doomed.len(), "{f:?}");
                    let mut marked: Vec<DocId> = docs
                        .find_with_ids(Filter::new().eq("mark", 1))
                        .unwrap()
                        .into_iter()
                        .map(|(id, _)| id)
                        .collect();
                    marked.sort();
                    let mut doomed = doomed;
                    doomed.sort();
                    assert_eq!(marked, doomed, "{f:?}");
                }
            }
        }
        assert_consistent(&db);
    }
}

/// A reference to another document, a `DocId` field, has an index
/// key (SPEC §62): a filter on it reads its range of the index, a
/// sort on it walks the index in the order the ids were made, a
/// compound index serves both, a unique index refuses a second
/// document with the same one, and an array of them is multikey.
#[test]
fn an_index_on_a_reference_reads_just_its_documents() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Car {
        owner: DocId,
        seats: i64,
    }
    let (_dir, db) = fresh_db();
    let owners = (0..10).map(|_| db.id_gen().generate()).collect::<Vec<_>>();
    let cars = db.collection::<Car>("cars");
    let mut batch = db.batch();
    for seats in 0..300 {
        let owner = owners[seats as usize % 10];
        batch.insert(&cars, Car { owner, seats }).unwrap();
    }
    batch.commit().unwrap();
    cars.ensure_index("owner").unwrap();

    let owned = Filter::new().eq("owner", owners[3]);
    let field = "owner".to_string();
    assert_eq!(cars.explain(&owned).unwrap(), QueryPlan::Index { field });
    let mut found = Vec::new();
    let reads = records_read(|| found = cars.find(owned.clone()).unwrap());
    assert_eq!((found.len(), reads), (30, 30));
    assert!(found.iter().all(|car| car.owner == owners[3]));

    // Made before the third owner: the first two. The range includes
    // the bound itself, as every range does (`key::range_for`), and
    // the check leaves its 30 out — on the entries, which hold the
    // ids whole, so no car is read (SPEC §100).
    let earlier = Filter::new().lt("owner", owners[2]);
    let reads = records_read(|| assert_eq!(cars.count(earlier.clone()).unwrap(), 60));
    assert_eq!(reads, 0);

    let newest = Filter::new().sort_desc("owner").limit(5);
    let field = "owner".to_string();
    assert_eq!(
        cars.explain(&newest).unwrap(),
        QueryPlan::IndexOrder { field }
    );
    let reads = records_read(|| found = cars.find(newest.clone()).unwrap());
    assert_eq!(reads, 5);
    assert!(found.iter().all(|car| car.owner == owners[9]));

    cars.ensure_index(vec!["owner", "seats"]).unwrap();
    let most_seats = Filter::new()
        .eq("owner", owners[3])
        .sort_desc("seats")
        .limit(3);
    let field = "(owner, seats)".to_string();
    assert_eq!(
        cars.explain(&most_seats).unwrap(),
        QueryPlan::IndexOrder { field }
    );
    let reads = records_read(|| found = cars.find(most_seats.clone()).unwrap());
    let seats = found.iter().map(|car| car.seats).collect::<Vec<_>>();
    assert_eq!((seats, reads), (vec![293, 283, 273], 3));

    let plates = db.collection::<Document>("plates");
    plates
        .ensure_index_with("car", IndexOptions::new().unique())
        .unwrap();
    let plate = |car: DocId| object(vec![("car", Document::Id(car))]);
    plates.insert(plate(owners[0])).unwrap();
    plates.insert(plate(owners[1])).unwrap();
    let twice = plates.insert(plate(owners[0]));
    assert!(
        matches!(twice, Err(crate::Error::DuplicateValue { .. })),
        "{twice:?}"
    );

    let trips = db.collection::<Document>("trips");
    trips.ensure_index("drivers[*]").unwrap();
    for pair in owners.windows(2) {
        let drivers = pair.iter().map(|&id| Document::Id(id)).collect();
        trips
            .insert(object(vec![("drivers", Document::Array(drivers))]))
            .unwrap();
    }
    let drove = Filter::new().eq("drivers[*]", owners[4]);
    let field = "drivers[*]".to_string();
    assert_eq!(trips.explain(&drove).unwrap(), QueryPlan::Index { field });
    assert_eq!(trips.count(drove).unwrap(), 2);
    assert_consistent(&db);
}

/// Makes `collection`'s index on `fields` what a build before format
/// 10 made of it (SPEC §62): an id is left out of a one-field index,
/// and an "other" in a compound one — as if it were a value no index
/// orders. Binary too, which had no key before format 13 either
/// (SPEC §166). Creates the index if it isn't there.
fn key_ids_the_old_way(db: &Database, collection: &str, fields: &[&str], options: IndexOptions) {
    let unkeyed = |value: &Document| matches!(value, Document::Id(_) | Document::Binary(_));
    index_the_old_way(db, collection, fields, options, unkeyed);
}

/// `key_ids_the_old_way` for a build of format 10 to 12 (SPEC §166):
/// ids keyed, binary left out as a value no index orders.
fn key_binary_the_old_way(db: &Database, collection: &str, fields: &[&str], options: IndexOptions) {
    let unkeyed = |value: &Document| matches!(value, Document::Binary(_));
    index_the_old_way(db, collection, fields, options, unkeyed);
}

/// `collection`'s index on `fields` filled as if every `unkeyed`
/// value were NaN, which no index orders either.
fn index_the_old_way(
    db: &Database,
    collection: &str,
    fields: &[&str],
    options: IndexOptions,
    unkeyed: fn(&Document) -> bool,
) {
    fn without_ids(doc: Document, unkeyed: fn(&Document) -> bool) -> Document {
        match doc {
            doc if unkeyed(&doc) => Document::Float(f64::NAN),
            Document::Object(map) => Document::Object(
                map.into_iter()
                    .map(|(k, v)| (k, without_ids(v, unkeyed)))
                    .collect(),
            ),
            Document::Array(items) => Document::Array(
                items
                    .into_iter()
                    .map(|item| without_ids(item, unkeyed))
                    .collect(),
            ),
            other => other,
        }
    }
    let fields = fields.iter().map(|f| f.to_string()).collect::<Vec<_>>();
    db.transact(|catalog, store| {
        let existing = catalog
            .indexes(collection)
            .iter()
            .find(|i| i.fields == fields);
        let index = match existing.cloned() {
            Some(index) => {
                BTreeIndex::new(index.root).clear(store)?;
                index
            }
            None => catalog.create_index(store, collection, &fields, options)?,
        };
        let meta = *catalog.get(collection).unwrap();
        let mut tree = BTreeIndex::new(index.root);
        for (_key, loc) in BTreeIndex::new(meta.index_root).scan(store)? {
            let (id, doc) = data::get_record(store, catalog.names(), loc)?;
            for key in index_keys(&without_ids(doc, unkeyed), &index, id) {
                tree.insert(store, &key, loc)?;
            }
        }
        Ok(())
    })
    .unwrap();
}

/// A file from before format 10 (SPEC §62) has indexes without keys
/// for ids. Opening it rebuilds those, and only those, in place, and
/// stamps the file 10. A file without secondary indexes needs nothing
/// and stays as it is, until the first write stamps it.
/// A read-only open keys the ids an old file's indexes lack as an
/// open that writes does, but in memory (SPEC §88): reads find them,
/// the file stays as it was, and is keyed again at the next open.
#[test]
fn a_read_only_open_keys_an_older_files_ids_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let owners;
    {
        let db = Database::open(&path).unwrap();
        owners = (0..3).map(|_| db.id_gen().generate()).collect::<Vec<_>>();
        let cars = db.collection::<Document>("cars");
        let mut batch = db.batch();
        for seats in 0..300 {
            let owner = Document::Id(owners[seats as usize % 3]);
            let car = object(vec![("owner", owner), ("seats", Document::Int(seats))]);
            batch.insert(&cars, car).unwrap();
        }
        batch.commit().unwrap();
        key_ids_the_old_way(&db, "cars", &["owner"], IndexOptions::new());
    }
    crate::storage::rewrite_format_version(&path, 9);
    let before = std::fs::read(&path).unwrap();

    let read_only = crate::OpenOptions::default().read_only(true);
    for _ in 0..2 {
        let mut opened = None;
        let reads = records_read(|| {
            opened = Some(Database::open_with(&path, read_only.clone()).unwrap());
        });
        assert!(reads >= 300, "keyed at every read-only open");
        let db = opened.unwrap();
        assert!(db.check().unwrap().is_ok());
        let cars = db.collection::<Document>("cars");
        let owned = Filter::new().eq("owner", owners[1]);
        let field = "owner".to_string();
        assert_eq!(cars.explain(&owned).unwrap(), QueryPlan::Index { field });
        assert_eq!(cars.count(owned).unwrap(), 100);
        drop((cars, db));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn opening_an_older_file_keys_the_ids_its_indexes_lack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let owners;
    {
        let db = Database::open(&path).unwrap();
        owners = (0..3).map(|_| db.id_gen().generate()).collect::<Vec<_>>();
        let cars = db.collection::<Document>("cars");
        // Enough that each index takes several pages, all of which a
        // rebuild must free or reuse.
        let mut batch = db.batch();
        for seats in 0..900 {
            let owner = Document::Id(owners[seats as usize % 3]);
            let car = object(vec![("owner", owner), ("seats", Document::Int(seats))]);
            batch.insert(&cars, car).unwrap();
        }
        batch
            .insert(&cars, object(vec![("seats", Document::Int(1000))]))
            .unwrap();
        batch.commit().unwrap();
        cars.ensure_index("seats").unwrap();
        key_ids_the_old_way(&db, "cars", &["owner"], IndexOptions::new());
        key_ids_the_old_way(&db, "cars", &["owner", "seats"], IndexOptions::new());
        let owned = Filter::new().eq("owner", owners[1]);
        assert_eq!(
            cars.count(owned).unwrap(),
            0,
            "an old index, read by this build"
        );
        assert!(!db.check().unwrap().is_ok());

        let state = db.read().unwrap();
        let at = state.snapshot();
        let (documents, stale) = stale_indexes(at.catalog, &at.store, "cars", true).unwrap();
        let stale = stale.iter().map(|index| index.name()).collect::<Vec<_>>();
        assert_eq!(
            (documents.len(), stale),
            (901, vec!["owner".to_string(), "(owner, seats)".to_string()])
        );
    }
    crate::storage::rewrite_format_version(&path, 9);

    let mut opened = None;
    let reads = records_read(|| opened = Some(Database::open(&path).unwrap()));
    assert!(reads >= 901, "every document checked, and read to rebuild");
    let db = opened.unwrap();
    assert_eq!(db.file_info().unwrap().format_version, 17);
    assert!(db.check().unwrap().is_ok());
    let cars = db.collection::<Document>("cars");
    assert_eq!(
        cars.index_names().unwrap(),
        ["seats", "owner", "(owner, seats)"]
    );
    let owned = Filter::new().eq("owner", owners[1]);
    let field = "owner".to_string();
    assert_eq!(cars.explain(&owned).unwrap(), QueryPlan::Index { field });
    assert_eq!(cars.count(owned).unwrap(), 300);
    let first = Filter::new()
        .eq("owner", owners[2])
        .sort_asc("seats")
        .limit(2);
    let field = "(owner, seats)".to_string();
    assert_eq!(
        cars.explain(&first).unwrap(),
        QueryPlan::IndexOrder { field }
    );
    let seats = cars
        .find(first)
        .unwrap()
        .into_iter()
        .map(|car| crate::query::value_or_null(&car, "seats").clone());
    assert_eq!(
        seats.collect::<Vec<_>>(),
        [Document::Int(2), Document::Int(5)]
    );
    drop((cars, db));
    let reads = records_read(|| drop(Database::open(&path).unwrap()));
    assert_eq!(reads, 0, "checked once: the file says 13 now");

    // Indexes, none holding an id: nothing to rebuild, but checked,
    // so the file is stamped, and not checked again.
    let unkeyed = dir.path().join("unkeyed.trunkdb");
    let db = Database::open(&unkeyed).unwrap();
    let notes = db.collection::<Document>("notes");
    notes.ensure_index("n").unwrap();
    notes.insert(object(vec![("n", Document::Int(1))])).unwrap();
    drop((notes, db));
    crate::storage::rewrite_format_version(&unkeyed, 9);
    let db = Database::open(&unkeyed).unwrap();
    assert_eq!(db.file_info().unwrap().format_version, 17);
    drop(db);
    let reads = records_read(|| drop(Database::open(&unkeyed).unwrap()));
    assert_eq!(reads, 0);

    // No secondary index: nothing for §62 to check. §95 checks every
    // document, so the open stamps the file all the same.
    let plain = dir.path().join("plain.trunkdb");
    let db = Database::open(&plain).unwrap();
    db.collection::<Document>("notes")
        .insert(object(vec![("n", Document::Int(1))]))
        .unwrap();
    drop(db);
    crate::storage::rewrite_format_version(&plain, 9);
    let db = Database::open(&plain).unwrap();
    assert_eq!(db.file_info().unwrap().format_version, 17);
}

/// Stores `doc` in `collection` past the check that it is an object,
/// as a build before SPEC §95 could.
fn write_unchecked(db: &Database, collection: &str, doc: Document) -> DocId {
    let id = db.id_gen().generate();
    let op = WriteOp::Insert(collection.to_string(), id, doc);
    db.transact(|catalog, store| apply_unchecked(catalog, store, &op))
        .unwrap();
    id
}

fn read_only(path: &std::path::Path) -> crate::Result<Database> {
    Database::open_with(path, crate::OpenOptions::default().read_only(true))
}

/// The format version the file on disk says.
fn format_on_disk(path: &std::path::Path) -> u32 {
    let bytes = std::fs::read(path).unwrap();
    u32::from_le_bytes(bytes[28..32].try_into().unwrap())
}

/// SPEC §95: a file of a format before 12 that holds only objects is
/// one of 12. The open checks every document, and stamps it, so the
/// check runs once; a read-only open checks too, and stamps nothing.
#[test]
fn an_older_file_of_objects_is_stamped_at_its_first_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("objects.trunkdb");
    let db = Database::open(&path).unwrap();
    let docs = db.collection::<Document>("docs");
    for n in 0..300 {
        docs.insert(object(vec![("n", Document::Int(n))])).unwrap();
    }
    // A large one, in an overflow chain.
    docs.insert(object(vec![("big", Document::String("x".repeat(30_000)))]))
        .unwrap();
    db.collection::<Document>("empty")
        .ensure_index("a")
        .unwrap();
    drop((docs, db));
    crate::storage::rewrite_format_version(&path, 11);

    drop(read_only(&path).unwrap());
    assert_eq!(format_on_disk(&path), 11, "a read-only open writes nothing");
    let db = Database::open(&path).unwrap();
    assert_eq!(db.file_info().unwrap().format_version, 17);
    assert_eq!(
        db.collection::<Document>("docs")
            .count(Filter::new())
            .unwrap(),
        301
    );
    drop(db);
    assert_eq!(format_on_disk(&path), 17);

    // An empty file is stamped too.
    let empty = dir.path().join("empty.trunkdb");
    drop(Database::open(&empty).unwrap());
    crate::storage::rewrite_format_version(&empty, 11);
    drop(Database::open(&empty).unwrap());
    assert_eq!(format_on_disk(&empty), 17);
}

/// SPEC §95: a file of a format before 12 that holds a document that
/// isn't an object can't be opened: both opens name the first one
/// found, and the file stays as it was, byte for byte. `check`
/// reports one in a file that says 12.
#[test]
fn an_older_file_with_a_document_that_is_not_an_object_is_refused() {
    // The collections are checked by name: the one holding it is
    // checked first, or after one holding only objects.
    for (bad, value) in [
        ("aaa", Document::Int(5)),
        ("docs", Document::String("y".repeat(30_000))), // an overflow chain
        ("docs", Document::Array(vec![Document::Int(1)])),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.trunkdb");
        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>(bad);
        docs.insert(object(vec![("n", Document::Int(1))])).unwrap();
        db.collection::<Document>("bbb")
            .insert(object(vec![("n", Document::Int(2))]))
            .unwrap();
        let id = write_unchecked(&db, bad, value);
        docs.insert(object(vec![("n", Document::Int(3))])).unwrap();
        let report = db.check().unwrap();
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.contains("isn't an object")),
            "{report:#?}"
        );
        drop((docs, db));
        crate::storage::rewrite_format_version(&path, 11);
        let before = std::fs::read(&path).unwrap();

        for opened in [Database::open(&path), read_only(&path)] {
            let Err(crate::Error::Format(crate::FormatError::NotAnObject {
                collection,
                id: found,
            })) = opened
            else {
                panic!("expected NotAnObject, got {:?}", opened.err());
            };
            assert_eq!((collection.as_str(), found), (bad, id));
        }
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing changed");
    }
}

/// Ids never collided in a unique index before format 10 (SPEC §62):
/// if two documents share one, rebuilding it fails the open, and the
/// file is left as it was — an older build can still open it.
#[test]
fn an_older_unique_index_with_shared_ids_fails_the_open_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    {
        let db = Database::open(&path).unwrap();
        let car = Document::Id(db.id_gen().generate());
        let plates = db.collection::<Document>("plates");
        plates.insert(object(vec![("car", car.clone())])).unwrap();
        plates.insert(object(vec![("car", car)])).unwrap();
        key_ids_the_old_way(&db, "plates", &["car"], IndexOptions::new().unique());
    }
    crate::storage::rewrite_format_version(&path, 9);
    let before = std::fs::read(&path).unwrap();
    for _ in 0..2 {
        let failed = Database::open(&path).err();
        assert!(
            matches!(failed, Some(crate::Error::DuplicateValue { .. })),
            "{failed:?}"
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

/// A file name as a program keeping names exactly stores it (SPEC
/// §166): bytes, with zeros and bytes no UTF-8 has. `n / 256` is the
/// second byte, so 0..256 share the prefix `[0x2F, 0]`.
fn file_name(n: u16) -> Document {
    Document::Binary(vec![0x2F, (n >> 8) as u8, n as u8, 0xFF, 0])
}

/// The `[0x2F, n]` part of `file_name`s, decoded back to `n`.
fn file_numbers(found: &[Document]) -> Vec<u16> {
    found
        .iter()
        .map(|doc| match crate::query::value_or_null(doc, "name") {
            Document::Binary(bytes) => u16::from_be_bytes([bytes[1], bytes[2]]),
            other => panic!("{other:?}"),
        })
        .collect()
}

/// SPEC §166: an index on a binary field serves `=`, ranges, `starts
/// with` and a sort, reading only what it must; a compound index
/// serves one size's names in order; a unique index refuses the same
/// bytes twice. The file checks out.
#[test]
fn an_index_on_binary_reads_only_what_it_must() {
    let (_dir, db) = fresh_db();
    let files = db.collection::<Document>("files");
    let mut batch = db.batch();
    for n in 0..300 {
        let size = Document::Int(n as i64 % 7);
        batch
            .insert(&files, object(vec![("name", file_name(n)), ("size", size)]))
            .unwrap();
    }
    batch.commit().unwrap();
    files.ensure_index("name").unwrap();
    let mut found = Vec::new();
    let field = || "name".to_string();

    let one = Filter::new().eq("name", file_name(42));
    assert_eq!(
        files.explain(&one).unwrap(),
        QueryPlan::Index { field: field() }
    );
    let reads = records_read(|| found = files.find(one.clone()).unwrap());
    assert_eq!((file_numbers(&found), reads), (vec![42], 1));

    let range = Filter::new()
        .gte("name", file_name(10))
        .lt("name", file_name(20));
    let reads = records_read(|| found = files.find(range.clone()).unwrap());
    assert_eq!(file_numbers(&found), (10..20).collect::<Vec<_>>());
    assert!(reads <= 11, "its own and the bound's: {reads}");
    // Decided on the entries, which hold the names whole (SPEC §100),
    // and answered from them.
    let reads = records_read(|| assert_eq!(files.count(range.clone()).unwrap(), 10));
    assert_eq!(reads, 0);
    let names = range.clone().select(["name"]);
    let reads = records_read(|| found = files.find(names.clone()).unwrap());
    assert_eq!((file_numbers(&found), reads), ((10..20).collect(), 0));

    let prefix = Document::Binary(vec![0x2F, 1]);
    let under = Filter::new().and(Condition::compare("name", Op::StartsWith, prefix));
    assert_eq!(
        files.explain(&under).unwrap(),
        QueryPlan::Index { field: field() }
    );
    let reads = records_read(|| found = files.find(under.clone()).unwrap());
    assert_eq!((found.len(), reads), (44, 44));
    assert_eq!(files.count(under).unwrap(), 44);

    let last = Filter::new().sort_desc("name").limit(3);
    assert_eq!(
        files.explain(&last).unwrap(),
        QueryPlan::IndexOrder { field: field() }
    );
    let reads = records_read(|| found = files.find(last.clone()).unwrap());
    assert_eq!((file_numbers(&found), reads), (vec![299, 298, 297], 3));

    files.ensure_index(vec!["size", "name"]).unwrap();
    let smallest = Filter::new().eq("size", 3).sort_asc("name").limit(2);
    let field = "(size, name)".to_string();
    assert_eq!(
        files.explain(&smallest).unwrap(),
        QueryPlan::IndexOrder { field }
    );
    let reads = records_read(|| found = files.find(smallest.clone()).unwrap());
    assert_eq!((file_numbers(&found), reads), (vec![3, 10], 2));

    let names = db.collection::<Document>("names");
    names
        .ensure_index_with("name", IndexOptions::new().unique())
        .unwrap();
    names.insert(object(vec![("name", file_name(1))])).unwrap();
    names.insert(object(vec![("name", file_name(2))])).unwrap();
    let twice = names.insert(object(vec![("name", file_name(1))]));
    assert!(
        matches!(twice, Err(crate::Error::DuplicateValue { .. })),
        "{twice:?}"
    );
    assert!(db.check().unwrap().is_ok());
}

/// A file from before format 13 (SPEC §166) has indexes that leave
/// binary out, or hold it as "other". Opening it rebuilds those, and
/// only those, and stamps the file 13; the next open reads nothing.
#[test]
fn opening_a_format_12_file_keys_the_binary_its_indexes_lack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    {
        let db = Database::open(&path).unwrap();
        let files = db.collection::<Document>("files");
        let mut batch = db.batch();
        for n in 0..600 {
            let size = Document::Int(n as i64 % 7);
            batch
                .insert(&files, object(vec![("name", file_name(n)), ("size", size)]))
                .unwrap();
        }
        batch
            .insert(&files, object(vec![("size", Document::Int(9))]))
            .unwrap();
        batch.commit().unwrap();
        files.ensure_index("size").unwrap();
        key_binary_the_old_way(&db, "files", &["name"], IndexOptions::new());
        key_binary_the_old_way(&db, "files", &["size", "name"], IndexOptions::new());
        let one = Filter::new().eq("name", file_name(42));
        assert_eq!(
            files.count(one).unwrap(),
            0,
            "an old index, read by this build"
        );
        assert!(!db.check().unwrap().is_ok());

        let state = db.read().unwrap();
        let at = state.snapshot();
        let (documents, stale) = stale_indexes(at.catalog, &at.store, "files", false).unwrap();
        let stale = stale.iter().map(|index| index.name()).collect::<Vec<_>>();
        assert_eq!(
            (documents.len(), stale),
            (601, vec!["name".to_string(), "(size, name)".to_string()])
        );
    }
    crate::storage::rewrite_format_version(&path, 12);
    crate::storage::rewrite_releases(&path, None, None);

    // A read-only open keys them in memory: its reads find them, it
    // says what the file says, and the file stays as it was (SPEC
    // §166.3).
    let before = std::fs::read(&path).unwrap();
    let db = read_only(&path).unwrap();
    let one = Filter::new().eq("name", file_name(42));
    assert_eq!(db.collection::<Document>("files").count(one).unwrap(), 1);
    assert!(db.check().unwrap().is_ok());
    let info = db.file_info().unwrap();
    assert_eq!((info.format_version, info.written_by), (12, None));
    drop(db);
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let mut opened = None;
    let reads = records_read(|| opened = Some(Database::open(&path).unwrap()));
    assert!(reads >= 601, "every document checked, and read to rebuild");
    let db = opened.unwrap();
    assert_eq!(db.file_info().unwrap().format_version, 17);
    assert!(db.check().unwrap().is_ok());
    let files = db.collection::<Document>("files");
    assert_eq!(
        files.index_names().unwrap(),
        ["size", "name", "(size, name)"]
    );
    let one = Filter::new().eq("name", file_name(42));
    let field = "name".to_string();
    assert_eq!(files.explain(&one).unwrap(), QueryPlan::Index { field });
    assert_eq!(files.count(one).unwrap(), 1);
    let smallest = Filter::new().eq("size", 3).sort_asc("name").limit(2);
    assert_eq!(file_numbers(&files.find(smallest).unwrap()), [3, 10]);
    drop((files, db));
    let reads = records_read(|| drop(Database::open(&path).unwrap()));
    assert_eq!(reads, 0, "checked once: the file says 13 now");

    // Indexes without binary: nothing to key. A read-only open says
    // what the file says, as `trunkdb info` shows it (SPEC §166.3);
    // an open that writes stamps it.
    let plain = dir.path().join("plain.trunkdb");
    let db = Database::open(&plain).unwrap();
    let notes = db.collection::<Document>("notes");
    notes.ensure_index("n").unwrap();
    notes.insert(object(vec![("n", Document::Int(1))])).unwrap();
    drop((notes, db));
    crate::storage::rewrite_format_version(&plain, 12);
    let db = read_only(&plain).unwrap();
    assert_eq!(db.file_info().unwrap().format_version, 12);
    drop(db);
    assert_eq!(format_on_disk(&plain), 12);
    let db = Database::open(&plain).unwrap();
    assert_eq!(db.file_info().unwrap().format_version, 17);
}

/// Binary never collided in a unique index before format 13 (SPEC
/// §166): two documents with the same bytes fail the open, as shared
/// ids did (§62.4), and the file is left as it was.
#[test]
fn an_older_unique_index_with_shared_binary_fails_the_open_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    {
        let db = Database::open(&path).unwrap();
        let names = db.collection::<Document>("names");
        names.insert(object(vec![("name", file_name(1))])).unwrap();
        names.insert(object(vec![("name", file_name(1))])).unwrap();
        key_binary_the_old_way(&db, "names", &["name"], IndexOptions::new().unique());
    }
    crate::storage::rewrite_format_version(&path, 12);
    let before = std::fs::read(&path).unwrap();
    for _ in 0..2 {
        let failed = Database::open(&path).err();
        assert!(
            matches!(failed, Some(crate::Error::DuplicateValue { .. })),
            "{failed:?}"
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

/// `eq("_id", id)` looks the document up in the primary index, as
/// `get` does, instead of reading every document (SPEC §61): alone,
/// next to other conditions, as an OR of several ids, and ahead of an
/// index that could serve the sort.
#[test]
fn a_filter_on_the_id_reads_just_those_documents() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Car {
        #[serde(rename = "_id")]
        id: Option<DocId>,
        seats: i64,
    }
    let (_dir, db) = fresh_db();
    let cars = db.collection::<Car>("cars");
    let mut batch = db.batch();
    let ids = (0..300)
        .map(|seats| batch.insert(&cars, Car { id: None, seats }).unwrap())
        .collect::<Vec<_>>();
    batch.commit().unwrap();
    cars.ensure_index("seats").unwrap();
    let car = |i: usize| Car {
        id: Some(ids[i]),
        seats: i as i64,
    };

    let one = Filter::new().eq("_id", ids[100]);
    assert_eq!(cars.explain(&one).unwrap(), QueryPlan::ById);
    let mut found = Vec::new();
    let reads = records_read(|| found = cars.find(one.clone()).unwrap());
    assert_eq!((&found[..], reads), (&[car(100)][..], 1));
    assert_eq!(cars.find_one(one.clone()).unwrap(), Some(car(100)));
    assert_eq!(cars.count(one.clone()).unwrap(), 1);

    // The other conditions still count, and an unknown id is no match.
    let both = one.clone().eq("seats", 101);
    assert_eq!(cars.explain(&both).unwrap(), QueryPlan::ById);
    assert!(cars.find(both).unwrap().is_empty());
    let unknown = Filter::new().eq("_id", DocId([0xAB; 16]));
    let reads = records_read(|| assert!(cars.find(unknown).unwrap().is_empty()));
    assert_eq!(reads, 0);

    // Several ids, one of them twice: each document once, sorted
    // without walking the index on `seats`.
    let some = Filter::new()
        .any_of([
            Condition::eq("_id", ids[7]),
            Condition::eq("_id", ids[250]),
            Condition::eq("_id", ids[7]),
        ])
        .sort_desc("seats")
        .limit(10);
    assert_eq!(cars.explain(&some).unwrap(), QueryPlan::ById);
    let reads = records_read(|| found = cars.find(some.clone()).unwrap());
    assert_eq!((&found[..], reads), (&[car(250), car(7)][..], 2));
    let streamed = cars.cursor(some.clone()).unwrap();
    let streamed = streamed.map(|r| r.map(|car| car.seats));
    assert_eq!(
        streamed.collect::<crate::Result<Vec<_>>>().unwrap(),
        [250, 7]
    );

    // Writes find their documents the same way.
    let raised = cars.update_many(one.clone(), |car| car.seats += 1000);
    assert_eq!(raised.unwrap(), 1);
    assert_eq!(cars.get(&ids[100]).unwrap().unwrap().seats, 1100);
    assert_eq!(cars.delete_many(some).unwrap(), 2);
    assert_eq!(cars.count(Filter::new()).unwrap(), 298);

    // An id's string is not an id (SPEC §59.5): no lookup, no match.
    let text = Filter::new().eq("_id", ids[100].to_string());
    assert_eq!(cars.explain(&text).unwrap(), QueryPlan::Scan);
    assert!(cars.find(text).unwrap().is_empty());
}

/// `Filter::id` and `Condition::id` (SPEC §63) find a struct by the
/// id it carries, through the primary index, where `eq("id", ...)`,
/// the struct's own name for the field, finds nothing.
#[test]
fn a_filter_by_id_finds_the_struct_that_carries_it() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Task {
        #[serde(rename = "_id")]
        id: Option<DocId>,
        title: String,
    }
    let (_dir, db) = fresh_db();
    let tasks = db.collection::<Task>("tasks");
    let ids = ["a", "b", "c"].map(|title| {
        tasks
            .insert(Task {
                id: None,
                title: title.into(),
            })
            .unwrap()
    });

    // As a command line would get it: the id as text.
    let wanted: DocId = ids[1].to_string().parse().unwrap();
    let one = Filter::new().id(wanted);
    assert_eq!(tasks.explain(&one).unwrap(), QueryPlan::ById);
    let task = tasks.find_one(one).unwrap().unwrap();
    assert_eq!((task.id, &task.title[..]), (Some(ids[1]), "b"));
    assert!(
        tasks
            .find(Filter::new().eq("id", wanted))
            .unwrap()
            .is_empty()
    );

    let two = Filter::new().any_of([ids[2], ids[0]].map(Condition::id));
    assert_eq!(tasks.explain(&two).unwrap(), QueryPlan::ById);
    let titles = tasks.find(two.sort_asc("title")).unwrap();
    let titles = titles.iter().map(|t| &t.title[..]).collect::<Vec<_>>();
    assert_eq!(titles, ["a", "c"]);
}

/// The typed path end to end: `status` is one of two values, read as
/// a union of two ranges of its index.
#[test]
fn an_or_of_indexed_values_reads_just_those() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Run {
        status: String,
        seen: i64,
    }
    let (_dir, db) = fresh_db();
    let runs = db.collection::<Run>("runs");
    let mut batch = db.batch();
    for seen in 0..300 {
        let status = ["Queued", "Running", "Complete"][(seen % 3) as usize].to_string();
        batch.insert(&runs, Run { status, seen }).unwrap();
    }
    batch.commit().unwrap();
    runs.ensure_index("status").unwrap();

    let active = Filter::new()
        .any_of([
            Condition::eq("status", "Queued"),
            Condition::eq("status", "Running"),
        ])
        .and(!Condition::lt("seen", 30));
    assert_eq!(
        runs.explain(&active).unwrap(),
        QueryPlan::IndexUnion {
            fields: vec!["status".to_string(), "status".to_string()],
            in_order: false,
        }
    );
    let mut found = Vec::new();
    let reads = records_read(|| found = runs.find(active.clone()).unwrap());
    assert_eq!(reads, 200, "the two ranges, not the Complete ones");
    assert_eq!(found.len(), 180);
    assert!(found.iter().all(|r| r.status != "Complete" && r.seen >= 30));
    assert_eq!(runs.count(active).unwrap(), 180);
}

/// `is_in` (SPEC §115): one point of the index per value, the
/// documents by id on `_id`, nothing for no values; `NOT IN` its
/// negation, a scan.
#[test]
fn a_list_of_values_reads_one_point_each() {
    let field = |run: &Document, name: &str| match run {
        Document::Object(fields) => fields.get(name).cloned(),
        _ => None,
    };
    let (_dir, db) = fresh_db();
    let runs = db.collection::<Document>("runs");
    let mut batch = db.batch();
    let mut ids = Vec::new();
    for seen in 0..300 {
        let status = ["Queued", "Running", "Complete"][seen % 3];
        let run: Document = format!(r#"{{"status": "{status}", "seen": {seen}}}"#)
            .parse()
            .unwrap();
        ids.push(batch.insert(&runs, run).unwrap());
    }
    batch.commit().unwrap();
    runs.ensure_index("status").unwrap();

    let active = Filter::new().is_in("status", ["Queued", "Running", "Paused"]);
    assert_eq!(
        runs.explain(&active).unwrap(),
        QueryPlan::IndexUnion {
            fields: vec!["status".to_string(); 3],
            in_order: false,
        }
    );
    let mut found = Vec::new();
    let reads = records_read(|| found = runs.find(active.clone()).unwrap());
    assert_eq!(
        (reads, found.len()),
        (200, 200),
        "the Queued and Running entries only"
    );
    let stats = runs.explain_run(active).unwrap().stats;
    assert_eq!(stats.keys_examined, 200);

    let others = Filter::new().and(!Condition::is_in("status", ["Queued", "Running"]));
    assert_eq!(runs.explain(&others).unwrap(), QueryPlan::Scan);
    let others = runs.find(others).unwrap();
    assert_eq!(others.len(), 100);
    assert!(
        others
            .iter()
            .all(|r| field(r, "status") == Some(Document::from("Complete")))
    );

    let two = Filter::new()
        .is_in("_id", [ids[7], ids[3]])
        .sort_asc("seen");
    assert_eq!(runs.explain(&two).unwrap(), QueryPlan::ById);
    let seen: Vec<_> = runs
        .find(two)
        .unwrap()
        .iter()
        .map(|r| field(r, "seen"))
        .collect();
    assert_eq!(seen, [Some(Document::Int(3)), Some(Document::Int(7))]);

    let none = Filter::new().is_in("status", [] as [&str; 0]);
    assert_eq!(runs.count(none).unwrap(), 0);
}

/// An update to a value past where its key is cut keeps its key,
/// but not its value, and is checked (SPEC §119.1): in a one-field
/// unique index, past about 1000 bytes; in a compound one, sooner. A
/// value kept as it was isn't a new duplicate, `5` for `5.0` neither.
#[test]
fn an_update_past_the_key_cut_is_checked_for_duplicates() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index_with("a", crate::IndexOptions::new().unique())
        .unwrap();
    docs.ensure_index_with(["b", "c"], crate::IndexOptions::new().unique())
        .unwrap();
    let doc = |a: &str, b: &str, c: &str| -> Document {
        format!(r#"{{"a": {a}, "b": "{b}", "c": "{c}"}}"#)
            .parse()
            .unwrap()
    };
    let (long, mid) = ("y".repeat(1100), "y".repeat(990));
    let a = |end: u8| format!(r#""{long}{end}""#);
    docs.insert(doc(&a(0), &format!("{mid}0"), "x")).unwrap();
    let id = docs.insert(doc(&a(1), &format!("{mid}1"), "x")).unwrap();

    // `a` past its cut, into the first document's.
    let result = docs.update(&id, doc(&a(0), &format!("{mid}1"), "x"));
    assert!(
        matches!(&result, Err(crate::Error::DuplicateValue { field, .. }) if field == "a"),
        "{result:?}"
    );
    // `b` past its cut in a key of two fields, into the first one's.
    let result = docs.update(&id, doc(&a(1), &format!("{mid}0"), "x"));
    assert!(
        matches!(&result, Err(crate::Error::DuplicateValue { field, .. }) if field == "(b, c)"),
        "{result:?}"
    );
    assert!(db.check().unwrap().is_ok());

    // What it holds already passes, as does a number as its equal;
    // and a value past the cut that no other document holds, under
    // the key the document's own entry has — which the check passes
    // over, as it now holds the new value itself (SPEC §119.6).
    assert!(
        docs.update(&id, doc(&a(1), &format!("{mid}1"), "x"))
            .unwrap()
    );
    assert!(
        docs.update(&id, doc(&a(2), &format!("{mid}2"), "x"))
            .unwrap()
    );
    let n = docs.insert(doc("5", "n", "x")).unwrap();
    assert!(docs.update(&n, doc("5.0", "n", "x")).unwrap());
    assert!(db.check().unwrap().is_ok());
}

/// Found by the `queries` target, not by the tests before it (SPEC
/// §119.6): a compound index's values compared in every field, also
/// where the key can't tell them apart. And a stored id inserted
/// again is `DuplicateId`, alone or in a batch, though its values are
/// those of the document stored.
#[test]
fn a_unique_check_compares_every_field_of_a_compound_index() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index_with(["b", "c"], crate::IndexOptions::new().unique())
        .unwrap();
    let mid = "y".repeat(990);
    let doc = |c: u8| -> Document { format!(r#"{{"b": "x", "c": "{mid}{c}"}}"#).parse().unwrap() };
    // The same `b`, and a `c` that differs past its cut: one key, two
    // values, no duplicate.
    let id = docs.insert(doc(0)).unwrap();
    docs.insert(doc(1)).unwrap();
    assert!(db.check().unwrap().is_ok());

    let mut again = doc(0);
    if let Document::Object(map) = &mut again {
        map.insert("_id".into(), Document::Id(id));
    }
    let result = docs.insert(again.clone());
    assert!(
        matches!(result, Err(crate::Error::DuplicateId { .. })),
        "{result:?}"
    );
    // In a batch too, of a stored id, and of one an op before stored.
    let mut batch = db.batch();
    batch.insert(&docs, again.clone()).unwrap();
    let result = batch.commit();
    assert!(
        matches!(result, Err(crate::Error::DuplicateId { .. })),
        "{result:?}"
    );
    docs.delete(&id).unwrap();
    let mut batch = db.batch();
    batch.insert(&docs, again.clone()).unwrap();
    batch.insert(&docs, again).unwrap();
    let result = batch.commit();
    assert!(
        matches!(result, Err(crate::Error::DuplicateId { .. })),
        "{result:?}"
    );
}
