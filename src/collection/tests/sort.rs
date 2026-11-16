use super::*;

// --- Sorting through an index (SPEC §34) ---

/// A sorted filter, mostly limited, sometimes with conditions: a range
/// on `v`, an `Eq` on `w` (which beats reading in order), or ones no
/// index answers. Mostly sorted by `v`; sometimes by `w`, whose three
/// values tie a lot — with a range on `v` and no limit, candidates
/// come in `v`'s order, and only the id order of ties makes the
/// result match.
///
/// Sometimes sorted by `_id` (SPEC §96), first or after `v`, and
/// compared with one of `ids` or an id no document has: one or
/// two comparisons, `ne` or one with a number, none of which an
/// `Eq` makes a lookup by id.
fn random_sorted_filter(rng: &mut XorShift, ids: &[DocId]) -> Filter {
    let ops = [Op::Eq, Op::Lt, Op::Lte, Op::Gt, Op::Gte];
    let some_id = |rng: &mut XorShift| match rng.below(4) {
        0 => Document::Id(DocId(
            rng.next().to_be_bytes().repeat(2).try_into().unwrap(),
        )),
        _ => Document::Id(ids[rng.below(ids.len())]),
    };
    let ranges = [Op::Lt, Op::Lte, Op::Gt, Op::Gte, Op::Ne];
    let mut conditions = match rng.below(8) {
        0 => vec![cond("v", ops[rng.below(5)].clone(), random_value(rng))],
        1 => vec![cond("w", Op::Eq, Document::Int(rng.below(3) as i64))],
        2 => vec![cond("v", Op::Ne, random_value(rng))],
        3 => vec![cond("pad", Op::Eq, Document::String(String::new()))],
        4 => (0..1 + rng.below(2))
            .map(|_| cond("_id", ranges[rng.below(5)].clone(), some_id(rng)))
            .collect(),
        5 => vec![cond("_id", ranges[rng.below(5)].clone(), Document::Int(3))],
        _ => vec![],
    };
    if rng.below(6) == 0 {
        conditions.push(cond("_id", ranges[rng.below(4)].clone(), some_id(rng)));
    }
    let order = [SortOrder::Asc, SortOrder::Desc][rng.below(2)];
    let field = ["v", "v", "w", "_id", "_id"][rng.below(5)].to_string();
    let limits = [
        None,
        Some(0),
        Some(1),
        Some(2),
        Some(5),
        Some(20),
        Some(60),
        Some(1000),
    ];
    let skips = [0, 0, 0, 1, 3, 25, 1000];
    let mut sort = vec![crate::query::Sort { field, order }];
    if rng.below(4) == 0 {
        // `_id` after `w`, which ties a lot, or keys after `_id`,
        // which never ties.
        let field = match sort[0].field.as_str() {
            "_id" => "w",
            _ => "_id",
        };
        sort.push(crate::query::Sort {
            field: field.to_string(),
            order: [SortOrder::Asc, SortOrder::Desc][rng.below(2)],
        });
    }
    Filter {
        conditions,
        sort,
        skip: skips[rng.below(skips.len())],
        limit: limits[rng.below(limits.len())],
        select: None,
        except: Vec::new(),
        hint: None,
        by_key: None,
    }
}

/// Every sorted, limited find must return exactly what sorting a full
/// scan in memory returns — same documents, same order, ties in id
/// order — whichever plan it runs.
fn assert_sorted_finds_agree_with_memory(docs: &Collection<Document>, rng: &mut XorShift) {
    assert_consistent(docs.db());
    let mut all = docs.find_with_ids(Filter::default()).unwrap();
    all.sort_by_key(|(id, _doc)| *id);
    let ids: Vec<DocId> = all.iter().map(|(id, _)| *id).collect();
    let mut plans = std::collections::HashSet::new();
    for _ in 0..500 {
        let f = random_sorted_filter(rng, &ids);
        let expected: Vec<DocId> = f
            .apply_to(all.clone(), |(_, doc)| doc)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let plan = explained(docs, &f);
        let found: Vec<DocId> = docs
            .find_with_ids(f.clone())
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(found, expected, "{f:?} via {plan:?}");
        // Typed (SPEC §99): matched on fields, deserialized last.
        assert_eq!(typed_ids(docs, &f), expected, "{f:?} via {plan:?}");
        selection_agrees(docs, &f, rng);
        aggregate_agrees(docs, &f, rng);
        plans.insert(format!("{plan:?}"));
    }
    for plan in [
        r#"IndexOrder { field: "v" }"#,
        r#"IndexOrder { field: "w" }"#,
        r#"IndexOrder { field: "_id" }"#,
        r#"Index { field: "v" }"#,
        r#"Index { field: "w" }"#,
        "Scan",
    ] {
        assert!(plans.contains(plan), "{plan} never ran: {plans:?}");
    }
}

/// A sort in memory with a limit holds about twice `skip + limit`
/// matches at most, however many match (SPEC §176); without a limit,
/// all of them.
#[test]
fn a_sort_with_a_limit_holds_few_matches() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    let mut batch = db.batch();
    for n in 0..2000i64 {
        let doc: Document = format!(r#"{{"v": {}}}"#, (n * 7919) % 2000)
            .parse()
            .unwrap();
        batch.insert(&docs, doc).unwrap();
    }
    batch.commit().unwrap();
    let held = |filter: Filter| {
        let state = docs.read().unwrap();
        let at = state.snapshot();
        let trace = Trace::default();
        let found = find_in(at.catalog, &at.store, "docs", &filter, true, None, &trace).unwrap();
        (found.len(), trace.held_most())
    };
    assert_eq!(held(Filter::new().sort_asc("v").limit(10)), (10, 21));
    assert_eq!(
        held(Filter::new().sort_desc("v").skip(5).limit(10)),
        (10, 31)
    );
    assert_eq!(held(Filter::new().sort_asc("v").limit(0)), (0, 0));
    assert_eq!(held(Filter::new().sort_asc("v")), (2000, 2000));
}

/// The entries are handed on as the walk reaches them, never gathered
/// first (SPEC §176): a reader that stops after three has read three,
/// of a scan, of an index range and of an OR's ranges.
#[test]
fn entries_stream_and_stop_where_the_reader_stops() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index("v").unwrap();
    let mut batch = db.batch();
    for n in 0..2000i64 {
        let doc: Document = format!(r#"{{"v": {}}}"#, n % 100).parse().unwrap();
        batch.insert(&docs, doc).unwrap();
    }
    batch.commit().unwrap();
    for (filter, all) in [
        (Filter::new(), 2000),
        (Filter::new().gte("v", 50), 1000),
        ("v < 10 or v >= 90".parse::<Filter>().unwrap(), 400),
        // Overlapping branches: 40 to 59 in both, each document once.
        ("v < 60 or v >= 40".parse::<Filter>().unwrap(), 2000),
    ] {
        let state = docs.read().unwrap();
        let at = state.snapshot();
        let (trace, mut taken) = (Trace::default(), 0);
        let stopped = each_entry(at.catalog, &at.store, "docs", &filter, &trace, |_| {
            taken += 1;
            match taken {
                3 => Err(std::io::Error::other("enough")),
                _ => Ok(()),
            }
        });
        assert!(stopped.is_err(), "{filter:?}");
        assert_eq!(trace.entries_read(), 3, "{filter:?}: read no further");
        let mut taken = Vec::new();
        each_entry(
            at.catalog,
            &at.store,
            "docs",
            &filter,
            &Trace::default(),
            |(key, _)| {
                taken.push(key::doc_id(&key));
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        let once: std::collections::HashSet<_> = taken.iter().collect();
        assert_eq!(once.len(), taken.len(), "{filter:?}: each document once");
        // A range may hold more than its conditions match, checked after.
        assert!(taken.len() >= all, "{filter:?}: {} of {all}", taken.len());
    }
}

#[test]
fn sorting_through_an_index_matches_sorting_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let mut rng = XorShift(0x0BAD_5EED_1234_5678);
    {
        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>("docs");
        docs.ensure_index("v").unwrap();
        docs.ensure_index("w").unwrap();
        let mut ids = Vec::new();
        for _ in 0..12 {
            let ops = (0..25)
                .map(|_| {
                    let id = db.id_gen().generate();
                    ids.push(id);
                    WriteOp::Insert("docs".into(), id, random_document(&mut rng))
                })
                .collect();
            db.write_batch(ops).unwrap();
        }
        let updates = (0..40)
            .map(|_| {
                let id = ids[rng.below(ids.len())];
                WriteOp::Update("docs".into(), id, random_document(&mut rng))
            })
            .collect();
        db.write_batch(updates).unwrap();
        let deleted = (0..30).map(|_| ids.swap_remove(rng.below(ids.len())));
        let ops = deleted.map(|id| WriteOp::Delete("docs".into(), id));
        db.write_batch(ops.collect()).unwrap();
        assert_sorted_finds_agree_with_memory(&docs, &mut rng);
    }
    let db = Database::open(&path).unwrap();
    assert_sorted_finds_agree_with_memory(&db.collection("docs"), &mut rng);
}

/// 1,000 documents `{n, group}`, `n` from 0 and `group` its last
/// digit, both indexed — and their ids, in id order.
fn thousand_by_id(db: &Database) -> (Collection<Document>, Vec<DocId>) {
    let docs = db.collection::<Document>("docs");
    docs.ensure_index("n").unwrap();
    docs.ensure_index("group").unwrap();
    let mut ids = Vec::new();
    for batch in 0..10 {
        let ops = (0..100)
            .map(|i| {
                let n = Document::Int(batch * 100 + i);
                let group = Document::Int(i % 10);
                let id = db.id_gen().generate();
                ids.push(id);
                WriteOp::Insert("docs".into(), id, object(vec![("n", n), ("group", group)]))
            })
            .collect();
        db.write_batch(ops).unwrap();
    }
    ids.sort();
    (docs, ids)
}

/// SPEC §96: a sort by `_id` with a limit walks the primary index,
/// either way, from where its `_id` comparisons with an id start it,
/// and reads only what it hands out and skips — and, where the filter
/// names nothing but `_id`, only what it hands out (SPEC §100).
#[test]
fn a_sort_by_id_walks_the_primary_index() {
    let (_dir, db) = fresh_db();
    let (docs, ids) = thousand_by_id(&db);
    // `(entries, read)`: entries walked, documents read. Fewer read
    // than walked: the walk covered the filter.
    let walk = |f: Filter, bounds: Vec<Condition>, backward: bool, (entries, read): (u64, u64)| {
        let run = docs.explain_run(f.clone()).unwrap();
        assert_eq!(
            run.plan,
            QueryPlan::IndexOrder {
                field: "_id".into()
            },
            "{f:?}"
        );
        let index = "_id".to_string();
        let covering = f.conditions.iter().all(|c| match c {
            Condition::Compare { field, .. } => field == "_id",
            _ => false,
        });
        assert!(covering || entries == read);
        let expected = crate::explain::IndexRead {
            index,
            bounds,
            backward,
            entries,
            covering,
        };
        assert_eq!(run.reads, [expected], "{f:?}");
        assert_eq!(run.stats.documents_examined, read, "{f:?}");
        let keys = f.sort.len();
        let sort = SortWork {
            from_index: keys,
            in_memory: 0,
        };
        assert_eq!(run.sort, Some(sort));
        let found = docs.find_with_ids(f).unwrap();
        found.into_iter().map(|(id, _)| id).collect::<Vec<_>>()
    };
    let first = Filter::new().sort_asc("_id").limit(20);
    assert_eq!(walk(first, vec![], false, (20, 20)), ids[..20]);
    let last = Filter::new().sort_desc("_id").limit(20);
    let mut newest = ids[980..].to_vec();
    newest.reverse();
    assert_eq!(walk(last, vec![], true, (20, 20)), newest);
    // Keyset paging: the 10 after the 500th.
    let after = Condition::gt("_id", ids[500]);
    let next = Filter::new().sort_asc("_id").limit(10).and(after.clone());
    assert_eq!(walk(next, vec![after], false, (10, 10)), ids[501..511]);
    // Up to and with the 500th, backwards, and a stretch between two.
    let upto = Condition::lte("_id", ids[500]);
    let back = Filter::new().sort_desc("_id").limit(3).and(upto.clone());
    assert_eq!(
        walk(back, vec![upto], true, (3, 3)),
        [ids[500], ids[499], ids[498]]
    );
    let (from, before) = (Condition::gte("_id", ids[7]), Condition::lt("_id", ids[9]));
    let between = Filter::new()
        .sort_asc("_id")
        .limit(5)
        .and(from.clone())
        .and(before.clone());
    assert_eq!(walk(between, vec![from, before], false, (2, 2)), ids[7..9]);
    // Page 3 of 50: the 100 skipped are walked, not read (SPEC §100).
    let page = Filter::new().sort_asc("_id").skip(100).limit(50);
    assert_eq!(walk(page, vec![], false, (150, 50)), ids[100..150]);
    // Keys after `_id` have nothing to order: no two ids tie.
    let more = Filter::new().sort_asc("_id").then_desc("n").limit(4);
    assert_eq!(walk(more, vec![], false, (4, 4)), ids[..4]);
    // Other conditions are checked, up to the third match.
    let all = docs.find(Filter::new().sort_asc("_id")).unwrap();
    let high = Filter::new().gt("n", 994);
    let third = (all.iter().enumerate())
        .filter(|(_, doc)| high.matches(doc))
        .nth(2)
        .unwrap()
        .0;
    let range = Filter::new().sort_asc("_id").limit(3).gt("n", 994);
    let found = walk(range, vec![], false, (third as u64 + 1, third as u64 + 1));
    assert_eq!(found.len(), 3);
    // Comparing `_id` with a number, or `ne`, doesn't narrow; the
    // entries reject what doesn't match, unread.
    let number = Filter::new().sort_asc("_id").limit(3).gt("_id", 5);
    assert!(walk(number, vec![], false, (1000, 0)).is_empty());
    let ne = Filter::new().sort_asc("_id").limit(3).ne("_id", ids[1]);
    assert_eq!(walk(ne, vec![], false, (4, 3)), [ids[0], ids[2], ids[3]]);
    // A collection that doesn't exist walks nothing.
    let none = db.collection::<Document>("none");
    let sorted = Filter::new().sort_desc("_id").limit(3);
    assert!(none.find(sorted).unwrap().is_empty());
}

/// SPEC §99.3: a read decodes whole only the documents it hands out —
/// after the filter, a sort in memory, `skip` and `limit` — and `count`
/// and `delete_many` none. Untyped, candidates from an index range are
/// decoded whole once and checked on that (§99.2).
#[test]
fn only_what_is_handed_out_is_decoded_whole() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    let typed = db.collection::<IdOnly>("docs");
    let ops = (0..1000)
        .map(|i| {
            let doc = object(vec![
                ("n", Document::Int(i)),
                ("pad", "x".repeat(50).into()),
            ]);
            WriteOp::Insert("docs".into(), db.id_gen().generate(), doc)
        })
        .collect();
    db.write_batch(ops).unwrap();
    let decoded = |read: &dyn Fn() -> usize| {
        crate::data::DECODED.with(|n| n.set(0));
        let handed_out = read();
        (handed_out, crate::data::DECODED.with(|n| n.get()))
    };
    // Sorted in memory by an unindexed field: 1,000 checked on `n`.
    let top = || Filter::new().sort_desc("n").limit(20);
    assert_eq!(decoded(&|| docs.find(top()).unwrap().len()), (20, 20));
    assert_eq!(decoded(&|| typed.find(top()).unwrap().len()), (20, 20));
    let streamed = || typed.cursor(Filter::new().gt("n", 900)).unwrap().count();
    assert_eq!(decoded(&streamed), (99, 99));
    // Page 3 of 50: 50 read and decoded; the 100 skipped decided by
    // their entries (SPEC §100).
    let page = || Filter::new().sort_asc("_id").skip(100).limit(50);
    assert_eq!(decoded(&|| docs.find(page()).unwrap().len()), (50, 50));
    assert_eq!(records_read(|| drop(typed.find(page()).unwrap())), 50);
    let over = || Filter::new().gt("n", 499);
    assert_eq!(decoded(&|| docs.count(over()).unwrap()), (500, 0));
    assert_eq!(
        decoded(&|| docs.delete_many(Filter::new().lt("n", 10)).unwrap()),
        (10, 0)
    );
    // From an index range, untyped, a filter its entries don't decide:
    // each candidate once, whole.
    docs.ensure_index("n").unwrap();
    let high = || Filter::new().gte("n", 990).ne("n", 995).ne("pad", "");
    assert_eq!(decoded(&|| docs.find(high()).unwrap().len()), (9, 10));
    assert_eq!(decoded(&|| typed.find(high()).unwrap().len()), (9, 9));
    // One they decide: only the matches (SPEC §100).
    let decided = || Filter::new().gte("n", 990).ne("n", 995);
    assert_eq!(decoded(&|| docs.find(decided()).unwrap().len()), (9, 9));
}

/// SPEC §96.2: the walk by `_id` gives way as a walk on a secondary
/// index does (§34.2) — to ids named, to an `Eq` or an OR on indexed
/// fields, and without a limit — but not to a range. An `Eq` fixing
/// its index whole is walked by id itself (§103).
#[test]
fn finding_by_id_or_by_value_beats_the_walk_by_id() {
    let (_dir, db) = fresh_db();
    let (docs, ids) = thousand_by_id(&db);
    let by_id = |f: Filter| docs.explain(&f.sort_asc("_id").limit(5)).unwrap();
    let field = |name: &str| name.to_string();
    assert_eq!(by_id(Filter::new().eq("_id", ids[3])), QueryPlan::ById);
    // Two ids looked up, not a walk the OR can't narrow.
    let two = Filter::new().any_of([Condition::id(ids[3]), Condition::id(ids[900])]);
    let run = docs.explain_run(two.sort_asc("_id").limit(5)).unwrap();
    assert_eq!(run.plan, QueryPlan::ById);
    assert_eq!(run.stats.documents_examined, 2);
    let sort = SortWork {
        from_index: 0,
        in_memory: 1,
    };
    assert_eq!(run.sort, Some(sort));
    let group = Filter::new().eq("group", 3);
    assert_eq!(
        by_id(group),
        QueryPlan::IndexOrder {
            field: field("group")
        }
    );
    // Each branch's range in id order, merged (SPEC §167).
    let either = [Condition::eq("group", 1), Condition::eq("n", 2)];
    let fields = vec![field("group"), field("n")];
    assert_eq!(
        by_id(Filter::new().any_of(either)),
        QueryPlan::IndexUnion {
            fields,
            in_order: true
        }
    );
    let range = Filter::new().gt("n", 10);
    assert_eq!(
        by_id(range),
        QueryPlan::IndexOrder {
            field: field("_id")
        }
    );
    let unlimited = Filter::new().sort_asc("_id");
    assert_eq!(docs.explain(&unlimited).unwrap(), QueryPlan::Scan);
}

/// A document for compound indexes (SPEC §43): `a` has few values (or
/// is missing, null, or an array), `b` anything `random_value` makes — arrays,
/// NaN, huge numbers, long strings included — `c` a small number.
fn random_compound_document(rng: &mut XorShift) -> Document {
    let mut fields = Vec::new();
    match rng.below(10) {
        0 => {}
        1 => fields.push(("a", Document::Null)),
        // Sorts after everything (§34.1); a compound index holds it.
        2 => fields.push(("a", Document::Array(vec![Document::Int(1)]))),
        _ => fields.push(("a", Document::Int(rng.below(3) as i64))),
    }
    if rng.below(8) != 0 {
        fields.push(("b", random_value(rng)));
    }
    fields.push(("c", Document::Int(rng.below(3) as i64)));
    let pad = [0, 0, 1500, 4500][rng.below(4)];
    fields.push(("pad", Document::String("y".repeat(pad))));
    object(fields)
}

fn random_compound_filter(rng: &mut XorShift) -> Filter {
    let ops = [Op::Eq, Op::Lt, Op::Lte, Op::Gt, Op::Gte];
    let mut conditions = Vec::new();
    for _ in 0..rng.below(4) {
        conditions.push(match rng.below(11) {
            0 | 1 => cond("a", Op::Eq, Document::Int(rng.below(4) as i64)),
            2 => cond(
                "a",
                ops[rng.below(5)].clone(),
                Document::Int(rng.below(4) as i64),
            ),
            3 => cond("b", ops[rng.below(5)].clone(), random_value(rng)),
            4 => cond("c", Op::Eq, Document::Int(rng.below(3) as i64)),
            5 => cond("a", Op::Eq, Document::Null),
            6 => cond(["a", "b"][rng.below(2)], Op::Ne, Document::Null),
            7 => cond("b", Op::Eq, Document::Int(rng.below(11) as i64 - 5)),
            // Shape, not value (SPEC §45): never bounds an index.
            8 => match rng.below(2) {
                0 => Condition::exists(["a", "b"][rng.below(2)]),
                _ => Condition::missing(["a", "b"][rng.below(2)]),
            },
            9 => Condition::size(
                ["a", "b"][rng.below(2)],
                [Op::Eq, Op::Ne, Op::Gt][rng.below(3)].clone(),
                rng.below(2),
            ),
            _ => cond("b", Op::Ne, random_value(rng)),
        });
    }
    // Up to three sort keys (SPEC §47), each field once, each way;
    // mostly one direction for all, so an index can serve several.
    let mut fields = vec!["a", "b", "c"];
    let order = [SortOrder::Asc, SortOrder::Desc][rng.below(2)];
    let sort = (0..rng.below(4))
        .map(|_| crate::query::Sort {
            field: fields.remove(rng.below(fields.len())).to_string(),
            order: match rng.below(4) {
                0 => [SortOrder::Asc, SortOrder::Desc][rng.below(2)],
                _ => order,
            },
        })
        .collect();
    let limits = [None, Some(0), Some(1), Some(3), Some(10), Some(1000)];
    let skips = [0, 0, 0, 1, 4, 30];
    Filter {
        conditions,
        sort,
        skip: skips[rng.below(skips.len())],
        limit: limits[rng.below(limits.len())],
        select: None,
        except: Vec::new(),
        hint: None,
        by_key: None,
    }
}

/// Every random filter on compound indexes `(a, b)` and `(a, b, c)`
/// must find what sorting a scan in memory finds — same documents,
/// and for a sorted filter the same order — through inserts, updates
/// that move documents, deletes and a reopen; the compound plans must
/// actually run.
#[test]
fn compound_finds_match_full_scans_through_every_kind_of_write() {
    let plain = IndexOptions::default();
    compound_finds_match_full_scans(
        &[(&["a", "b"], plain.clone()), (&["a", "b", "c"], plain)],
        &[
            r#"Index { field: "(a, b)" }"#,
            r#"IndexOrder { field: "(a, b)" }"#,
            r#"IndexOrder { field: "(a, b, c)" }"#,
        ],
    );
}

/// Sparse, a compound index leaves out a document null in all of its
/// fields, and a one-field index every null (SPEC §44).
#[test]
fn sparse_compound_finds_match_full_scans_through_every_kind_of_write() {
    compound_finds_match_full_scans(
        &[
            (&["a", "b"], SPARSE),
            (&["a", "b", "c"], SPARSE),
            (&["b"], SPARSE),
        ],
        &[
            r#"Index { field: "(a, b)" }"#,
            r#"IndexOrder { field: "(a, b)" }"#,
            r#"IndexOrder { field: "(a, b, c)" }"#,
            r#"Index { field: "b" }"#,
            r#"IndexOrder { field: "b" }"#,
            "Scan",
        ],
    );
}

fn compound_finds_match_full_scans(indexes: &[(&[&str], IndexOptions)], expected_plans: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let mut rng = XorShift(0xBB67_AE85_84CA_A73B);
    let check = |docs: &Collection<Document>, rng: &mut XorShift| {
        assert_consistent(docs.db());
        let mut all = docs.find_with_ids(Filter::default()).unwrap();
        all.sort_by_key(|(id, _)| *id);
        let mut plans = std::collections::HashSet::new();
        for _ in 0..400 {
            let f = random_compound_filter(rng);
            let ids = |found: Vec<(DocId, Document)>| {
                let mut ids: Vec<DocId> = found.into_iter().map(|(id, _)| id).collect();
                if f.sort.is_empty() {
                    ids.sort();
                }
                ids
            };
            let expected = ids(f.apply_to(all.clone(), |(_, doc)| doc));
            let plan = explained(docs, &f);
            let found = ids(docs.find_with_ids(f.clone()).unwrap());
            // Typed (SPEC §99), the same documents the same way.
            let typed = typed_ids(docs, &f).into_iter();
            assert_eq!(ids(typed.map(|id| (id, Document::Null)).collect()), found);
            selection_agrees(docs, &f, rng);
            aggregate_agrees(docs, &f, rng);
            if f.sort.is_empty() && (f.limit.is_some() || f.skip > 0) {
                // Which documents a skip or limit without a sort keeps
                // isn't promised: any that match, as many as they allow.
                let unlimited = Filter {
                    skip: 0,
                    limit: None,
                    ..f.clone()
                };
                let matching = ids(unlimited.apply_to(all.clone(), |(_, doc)| doc));
                assert_eq!(found.len(), expected.len(), "{f:?} via {plan:?}");
                assert!(found.iter().all(|id| matching.contains(id)), "{f:?}");
            } else {
                assert_eq!(found, expected, "{f:?} via {plan:?}");
            }
            plans.insert(format!("{plan:?}"));
        }
        for plan in expected_plans {
            assert!(plans.contains(*plan), "{plan} never ran: {plans:?}");
        }
    };
    {
        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>("docs");
        let mut ids = Vec::new();
        let mut insert = |rng: &mut XorShift, count| {
            for _ in 0..count / 25 {
                let ops = (0..25)
                    .map(|_| {
                        let id = db.id_gen().generate();
                        ids.push(id);
                        WriteOp::Insert("docs".into(), id, random_compound_document(rng))
                    })
                    .collect();
                db.write_batch(ops).unwrap();
            }
        };
        insert(&mut rng, 150);
        for (fields, options) in indexes {
            assert!(docs.ensure_index_with(*fields, options.clone()).unwrap());
        }
        insert(&mut rng, 150);
        for _ in 0..6 {
            let ops = (0..25)
                .map(|_| {
                    let id = ids[rng.below(ids.len())];
                    let new = match docs.get(&id).unwrap() {
                        Some(Document::Object(mut fields)) if rng.below(2) == 0 => {
                            let pad = "z".repeat([0, 4500, 9000][rng.below(3)]);
                            fields.insert("pad".into(), Document::String(pad));
                            Document::Object(fields)
                        }
                        _ => random_compound_document(&mut rng),
                    };
                    WriteOp::Update("docs".into(), id, new)
                })
                .collect();
            db.write_batch(ops).unwrap();
        }
        let deleted = (0..60).map(|_| ids.swap_remove(rng.below(ids.len())));
        let ops = deleted.map(|id| WriteOp::Delete("docs".into(), id));
        db.write_batch(ops.collect()).unwrap();
        check(&docs, &mut rng);
    }
    let db = Database::open(&path).unwrap();
    let docs = db.collection::<Document>("docs");
    let names: Vec<String> = indexes
        .iter()
        .map(|(fields, _)| match fields {
            [field] => field.to_string(),
            fields => format!("({})", fields.join(", ")),
        })
        .collect();
    assert_eq!(docs.index_names().unwrap(), names);
    check(&docs, &mut rng);
}

/// Past `ALTERNATIVE_LEAVES`, an alternative's ranges are counted
/// no further (SPEC §133.3): `MoreThan` what they counted, a union's
/// ranges together. Below it, exactly.
#[test]
fn an_alternative_is_counted_through_so_many_leaves() {
    use crate::query::EntryCount;
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    let mut batch = db.batch();
    for n in 0..3000i64 {
        let doc: Document = format!(r#"{{"a": {}, "b": {n}}}"#, n % 2).parse().unwrap();
        batch.insert(&docs, doc).unwrap();
    }
    batch.commit().unwrap();
    docs.ensure_index("a").unwrap();
    docs.ensure_index("b").unwrap();
    let entries = |run: &Explained, field: &str| {
        let other = run.alternatives.iter().find(|a| {
            matches!(&a.plan, QueryPlan::Index { field: f, .. } if f == field)
                || matches!(&a.plan, QueryPlan::IndexUnion { fields, .. } if fields[0] == field)
        });
        other
            .unwrap_or_else(|| panic!("{field}: {:?}", run.alternatives))
            .entries
    };
    // 1,500 entries of `a = 0` over more than four leaves.
    let run = docs
        .explain_run(Filter::new().eq("a", 0).use_index("b").gte("b", 0))
        .unwrap();
    let EntryCount::MoreThan(counted) = entries(&run, "a") else {
        panic!("{:?}", run.alternatives);
    };
    assert!(counted > 0 && counted < 1500, "{counted}");
    // A few, in one leaf: exactly.
    let run = docs
        .explain_run(Filter::new().eq("a", 0).lt("b", 10).use_index("a"))
        .unwrap();
    assert_eq!(entries(&run, "b"), EntryCount::Exactly(11)); // 10 too, which the check leaves out (§28.3)
    // A union: each branch alone fits, the two together don't.
    let either = Filter::new()
        .any_of([
            Condition::compare("b", Op::Lt, 400),
            Condition::compare("b", Op::Gte, 2600),
        ])
        .no_index();
    for alone in [Filter::new().lt("b", 400), Filter::new().gte("b", 2600)] {
        let run = docs.explain_run(alone.no_index()).unwrap();
        assert!(matches!(entries(&run, "b"), EntryCount::Exactly(400..=401)));
    }
    let run = docs.explain_run(either).unwrap();
    let both = entries(&run, "b");
    assert!(
        matches!(both, EntryCount::MoreThan(n) if n >= 400),
        "{both:?}"
    );
}

/// A walk passed over stops once it has read what the run read
/// (SPEC §133.4): its documents too, never the rest of the walk.
#[test]
fn a_walk_passed_over_reads_no_more_than_the_run() {
    use crate::query::EntryCount;
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    let mut batch = db.batch();
    for n in 0..3000i64 {
        let doc: Document = format!(r#"{{"b": {n}, "c": {}}}"#, n % 100)
            .parse()
            .unwrap();
        batch.insert(&docs, doc).unwrap();
    }
    batch.commit().unwrap();
    docs.ensure_index("b").unwrap();
    docs.ensure_index("c").unwrap();
    // The rules read the 30 of `c = 99`; walking `b` down would read
    // some 400 to find 5.
    let filter = Filter::new().eq("c", 99).sort_desc("b").limit(5);
    let mut run = None;
    let read = records_read(|| run = Some(docs.explain_run(filter.clone()).unwrap()));
    let run = run.unwrap();
    assert_eq!(run.stats.keys_examined, 30);
    let walk = run
        .alternatives
        .iter()
        .find(|a| matches!(&a.plan, QueryPlan::IndexOrder { field, .. } if field == "b"))
        .unwrap();
    assert_eq!(walk.entries, EntryCount::MoreThan(30));
    assert!(read <= 2 * 30 + 2, "{read}");
}

/// The query compound indexes are for (SPEC §43): the oldest 20
/// queued tasks — found by status and read in order of creation at
/// once, 20 documents read of 3000.
#[test]
fn the_oldest_queued_tasks_are_read_through_status_and_created() {
    let (_dir, db) = fresh_db();
    let tasks = db.collection::<Task>("tasks");
    let mut batch = db.batch();
    for n in 0..3000i64 {
        let status = ["Queued", "Running", "Done"][(n * 7 % 3) as usize];
        let task = Task {
            status: status.into(),
            // Not in insertion order.
            created: (n * 7919) % 3000,
            tenant: format!("t{}", n % 5),
        };
        batch.insert(&tasks, task).unwrap();
    }
    batch.commit().unwrap();
    let oldest_queued = || {
        Filter::new()
            .eq("status", "Queued")
            .sort_asc("created")
            .limit(20)
    };
    let expected = oldest_queued().apply(
        tasks
            .find(Filter::new())
            .unwrap()
            .into_iter()
            .map(|t| crate::serde_bridge::to_document(&t).unwrap()),
    );

    tasks.ensure_index("status").unwrap();
    tasks.ensure_index("created").unwrap();
    let before = records_read(|| {
        tasks.find(oldest_queued()).unwrap();
    });
    assert!(before > 900, "{before}");

    tasks.ensure_index(["status", "created"]).unwrap();
    assert_eq!(
        tasks.explain(&oldest_queued()).unwrap(),
        QueryPlan::IndexOrder {
            field: "(status, created)".into()
        }
    );
    let mut found = Vec::new();
    let reads = records_read(|| found = tasks.find(oldest_queued()).unwrap());
    assert_eq!(reads, 20);
    let found: Vec<Document> = found
        .iter()
        .map(|t| crate::serde_bridge::to_document(t).unwrap())
        .collect();
    assert_eq!(found, expected);

    // Newest first, and a range on `created` within the status.
    let newest = Filter::new()
        .eq("status", "Done")
        .lt("created", 100)
        .sort_desc("created")
        .limit(5);
    let created: Vec<i64> = tasks
        .find(newest)
        .unwrap()
        .iter()
        .map(|t| t.created)
        .collect();
    let mut all_done: Vec<i64> = tasks
        .find(Filter::new().eq("status", "Done"))
        .unwrap()
        .iter()
        .map(|t| t.created)
        .filter(|c| *c < 100)
        .collect();
    all_done.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(created, all_done[..5]);
    assert_consistent(&db);
}

/// Values no one-field index holds (arrays here) sort after all the
/// others, found by a scan after the index (SPEC §34.2) — and among
/// themselves by the next sort key (SPEC §47.3), not in id order:
/// the scan reads them all before taking the limit.
#[test]
fn unordered_values_after_the_index_are_sorted_by_the_next_key() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index("v").unwrap();
    for w in 0..15i64 {
        let v = match w < 5 {
            true => Document::Int(w),
            false => Document::Array(vec![Document::Int(w)]),
        };
        docs.insert(object(vec![("v", v), ("w", Document::Int(w))]))
            .unwrap();
    }
    let f = Filter::new().sort_asc("v").then_desc("w").limit(8);
    assert_eq!(
        docs.explain(&f).unwrap(),
        QueryPlan::IndexOrder { field: "v".into() }
    );
    let w = |found: Vec<Document>| -> Vec<Document> {
        found
            .iter()
            .map(|d| crate::query::value_or_null(d, "w").clone())
            .collect()
    };
    let all = docs.find(Filter::new()).unwrap();
    let expected = w(f.apply(all));
    assert_eq!(expected, [0, 1, 2, 3, 4, 14, 13, 12].map(Document::Int));
    assert_eq!(w(docs.find(f).unwrap()), expected);
}

/// Sorting by several fields (SPEC §47) through `(status, created)`:
/// both keys in one direction are read in order, 20 documents read
/// for 20 results; with the second key the other way, the index gives
/// the status order and each status is sorted in memory; the index on
/// `created` alone serves a sort by `created, tenant` — ties by
/// tenant, in memory. Always what sorting everything finds.
#[test]
fn tasks_sort_by_status_then_by_created() {
    let (_dir, db) = fresh_db();
    let tasks = db.collection::<Task>("tasks");
    let mut batch = db.batch();
    for n in 0..3000i64 {
        let task = Task {
            status: ["Queued", "Running", "Done"][(n * 7 % 3) as usize].into(),
            // Each value 5 times, so `created` has ties.
            created: (n * 7919) % 600,
            tenant: format!("t{}", n * 13 % 5),
        };
        batch.insert(&tasks, task).unwrap();
    }
    batch.commit().unwrap();
    tasks.ensure_index(["status", "created"]).unwrap();
    tasks.ensure_index("created").unwrap();
    let all: Vec<Document> = tasks
        .find(Filter::new())
        .unwrap()
        .iter()
        .map(|t| crate::serde_bridge::to_document(t).unwrap())
        .collect();
    let check = |f: Filter, plan: &str, max_reads: usize| {
        assert_eq!(
            tasks.explain(&f).unwrap(),
            QueryPlan::IndexOrder { field: plan.into() },
            "{f:?}"
        );
        let mut found = Vec::new();
        let reads = records_read(|| found = tasks.find(f.clone()).unwrap());
        assert!(reads <= max_reads, "{f:?}: {reads} reads");
        let found: Vec<Document> = found
            .iter()
            .map(|t| crate::serde_bridge::to_document(t).unwrap())
            .collect();
        assert_eq!(found, f.apply(all.clone()), "{f:?}");
    };
    let by_both = Filter::new()
        .sort_asc("status")
        .then_asc("created")
        .limit(20);
    check(by_both, "(status, created)", 20);
    let by_both_desc = Filter::new()
        .sort_desc("status")
        .then_desc("created")
        .limit(20);
    check(by_both_desc, "(status, created)", 20);
    // "Done" first, newest first: the 1000 done tasks are sorted.
    let newest_per_status = Filter::new()
        .sort_asc("status")
        .then_desc("created")
        .limit(20);
    check(newest_per_status, "(status, created)", 1000);
    // Five tasks per `created`: each five sorted by tenant.
    let by_created_tenant = Filter::new()
        .sort_asc("created")
        .then_desc("tenant")
        .limit(12);
    check(by_created_tenant, "created", 15);
}

/// Unique in all its fields at once (SPEC §43.4): an email may repeat
/// across tenants, not within one; a null in any field exempts, as in
/// SQL.
#[test]
fn a_unique_compound_index_refuses_only_the_same_values_in_every_field() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");
    let user =
        |tenant: Document, email: &str| object(vec![("tenant", tenant), ("email", email.into())]);
    users
        .ensure_index_with(["tenant", "email"], crate::IndexOptions::new().unique())
        .unwrap();
    let ada = users.insert(user("t1".into(), "ada@x")).unwrap();
    users.insert(user("t2".into(), "ada@x")).unwrap();
    users.insert(user("t1".into(), "bob@x")).unwrap();
    match users.insert(user("t1".into(), "ada@x")) {
        Err(crate::Error::DuplicateValue {
            existing, field, ..
        }) => {
            assert_eq!((existing, field.as_str()), (ada, "(tenant, email)"));
        }
        other => panic!("expected DuplicateValue, got {other:?}"),
    }
    // Equal as `Eq` sees it: `1` and `1.0`.
    users.insert(user(1.into(), "cy@x")).unwrap();
    assert!(users.insert(user(Document::Float(1.0), "cy@x")).is_err());
    // A null, or a missing field, in any of them: exempt.
    for _ in 0..2 {
        users.insert(user(Document::Null, "ada@x")).unwrap();
        users
            .insert(object(vec![("email", "ada@x".into())]))
            .unwrap();
    }
    // Ada moves to t3; then t1's ada@x is free again.
    assert!(users.update(&ada, user("t3".into(), "ada@x")).unwrap());
    users.insert(user("t1".into(), "ada@x")).unwrap();
    assert_consistent(&db);

    // Built over documents that share both values: refused.
    let other = db.collection::<Document>("other");
    other.insert(user("t".into(), "e")).unwrap();
    other.insert(user("t".into(), "e")).unwrap();
    assert!(matches!(
        other.ensure_index_with(["tenant", "email"], crate::IndexOptions::new().unique()),
        Err(crate::Error::DuplicateValue { .. })
    ));
    assert_eq!(other.index_names().unwrap(), Vec::<String>::new());
}

/// Strings longer than a compound key's share for them (501 bytes of
/// two fields) are cut in the keys; conditions on them must be cut the
/// same way to find them — `Eq`, ranges, and the sorted read.
#[test]
fn long_strings_in_a_compound_index_are_found_by_every_condition() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    let long = |end: &str| Document::String("x".repeat(800) + end);
    for (a, end) in [(1, "a"), (1, "b"), (1, "c"), (2, "b")] {
        docs.insert(object(vec![("a", a.into()), ("b", long(end))]))
            .unwrap();
    }
    docs.ensure_index(["a", "b"]).unwrap();
    let ends = |f: Filter| -> Vec<String> {
        docs.find(f)
            .unwrap()
            .iter()
            .map(|doc| match crate::query::field_value(doc, "b") {
                Some(Document::String(b)) => b[800..].to_string(),
                other => panic!("{other:?}"),
            })
            .collect()
    };
    let a1 = || Filter::new().eq("a", 1);
    assert_eq!(ends(a1().eq("b", long("b"))), ["b"]);
    assert_eq!(ends(a1().gt("b", long("a")).sort_asc("b")), ["b", "c"]);
    assert_eq!(ends(a1().gte("b", long("b")).sort_asc("b")), ["b", "c"]);
    assert_eq!(ends(a1().lt("b", long("c")).sort_asc("b")), ["a", "b"]);
    assert_eq!(ends(a1().lte("b", long("b")).sort_desc("b")), ["b", "a"]);
    let newest = a1().sort_desc("b").limit(2);
    assert_eq!(
        docs.explain(&newest).unwrap(),
        QueryPlan::IndexOrder {
            field: "(a, b)".into()
        }
    );
    assert_eq!(ends(newest), ["c", "b"]);
    assert_eq!(
        docs.explain(&a1().gt("b", long("a"))).unwrap(),
        QueryPlan::Index {
            field: "(a, b)".into()
        }
    );
}

#[test]
fn compound_indexes_are_checked_created_dropped_exported_and_imported() {
    let (dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    let too_many = ["a", "b", "c", "d", "e", "f", "g", "h", "i"];
    for bad in [
        &[][..],
        &too_many[..],
        &["a", "tags[*]"],
        &["a", "a"],
        &["a", "_id"],
        &["a", ""],
    ] {
        assert!(docs.ensure_index(bad).is_err(), "{bad:?}");
    }
    assert!(docs.ensure_index(&too_many[..8]).unwrap());
    assert!(docs.ensure_index(["a", "b"]).unwrap());
    assert!(!docs.ensure_index(["a", "b"]).unwrap());
    // Another order is another index.
    assert!(docs.ensure_index(["b", "a"]).unwrap());
    assert!(
        docs.ensure_index_with(["a", "c"], crate::IndexOptions::new().unique())
            .unwrap()
    );
    assert!(
        docs.ensure_index_with(["a", "b"], crate::IndexOptions::new().unique())
            .is_err(),
        "exists, not unique"
    );
    docs.insert(object(vec![
        ("a", 1.into()),
        ("b", 2.into()),
        ("c", 3.into()),
    ]))
    .unwrap();
    assert_eq!(
        docs.index_names().unwrap(),
        ["(a, b, c, d, e, f, g, h)", "(a, b)", "(b, a)", "(a, c)"]
    );
    assert_eq!(docs.unique_indexes().unwrap(), ["(a, c)"]);

    let mut export = Vec::new();
    db.export(&mut export).unwrap();
    let text = String::from_utf8(export.clone()).unwrap();
    assert!(text.contains(r#"["a","b"]"#), "{text}");
    assert!(
        text.contains(r#"{"fields":["a","c"],"unique":true}"#),
        "{text}"
    );
    let copy = Database::open(dir.path().join("copy.trunkdb")).unwrap();
    copy.import(export.as_slice()).unwrap();
    let copied = copy.collection::<Document>("docs");
    assert_eq!(copied.index_names().unwrap(), docs.index_names().unwrap());
    assert_eq!(copied.unique_indexes().unwrap(), ["(a, c)"]);
    assert_consistent(&copy);

    assert!(docs.drop_index(["b", "a"]).unwrap());
    assert!(!docs.drop_index(["b", "a"]).unwrap());
    assert!(!docs.drop_index("b").unwrap(), "no index on b alone");
    assert_eq!(docs.index_names().unwrap().len(), 3);
    assert_consistent(&db);
}

/// SPEC §72: `skip` passes over matches after the sort, before the
/// limit — in `find`, `count`, `find_one`, a cursor and `delete_many`;
/// through an index, the reading stops after skip plus limit.
#[test]
fn skip_and_limit_page_through_the_matches() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<User>("users");
    let mut batch = db.batch();
    for age in 0..1000 {
        batch
            .insert(&users, user(&format!("user {age}"), age))
            .unwrap();
    }
    batch.commit().unwrap();
    let page = |p: usize| by_age(SortOrder::Asc).skip(p * 20).limit(20);
    let ages = |found: &[User]| found.iter().map(|u| u.age).collect::<Vec<_>>();

    let mut found = Vec::new();
    let reads = records_read(|| found = users.find(page(2)).unwrap());
    assert_eq!((reads, ages(&found)), (1000, (40..60).collect()));

    // Walked as far as skip plus limit; the skipped decided by their
    // entries, unread (SPEC §100).
    users.ensure_index("age").unwrap();
    let reads = records_read(|| found = users.find(page(2)).unwrap());
    assert_eq!(
        (reads, ages(&found)),
        (20, (40..60).collect()),
        "limit read"
    );
    let desc = by_age(SortOrder::Desc).skip(995).limit(20);
    assert_eq!(ages(&users.find(desc).unwrap()), [4, 3, 2, 1, 0]);
    assert!(users.find(page(50)).unwrap().is_empty());
    let beyond = by_age(SortOrder::Asc).skip(usize::MAX).limit(20);
    assert!(users.find(beyond).unwrap().is_empty());
    let unlimited = by_age(SortOrder::Asc).skip(990).limit(None);
    assert_eq!(
        ages(&users.find(unlimited).unwrap()),
        (990..1000).collect::<Vec<_>>()
    );

    // `count` counts what `find` returns; without skip and limit, the total.
    assert_eq!(users.count(page(2)).unwrap(), 20);
    assert_eq!(users.count(page(49)).unwrap(), 20);
    assert_eq!(users.count(Filter::new().skip(990)).unwrap(), 10);
    assert_eq!(users.count(Filter::new().skip(2000)).unwrap(), 0);
    let under_ten = filter(vec![cond("age", Op::Lt, Document::Int(10))]);
    assert_eq!(users.count(under_ten.clone().skip(4).limit(5)).unwrap(), 5);
    assert_eq!(users.count(under_ten.clone().skip(8).limit(5)).unwrap(), 2);

    // `find_one` is the first after the skipped.
    let seventh = users.find_one(by_age(SortOrder::Asc).skip(7)).unwrap();
    assert_eq!(seventh.map(|u| u.age), Some(7));

    // An unsorted cursor streams, and passes over the first matches.
    let all: Vec<i64> = users
        .cursor(under_ten.clone())
        .unwrap()
        .map(|r| r.unwrap().age)
        .collect();
    let mut rest: Vec<i64> = Vec::new();
    let reads = records_read(|| {
        rest = users
            .cursor(under_ten.clone().skip(3).limit(4))
            .unwrap()
            .map(|r| r.unwrap().age)
            .collect();
    });
    assert_eq!((all.len(), &rest[..]), (10, &all[3..7]));
    // The entries decide: the three skipped aren't read (SPEC §100).
    assert_eq!(reads, 4);

    // `delete_many` deletes the page `find` returns.
    assert_eq!(
        users
            .delete_many(by_age(SortOrder::Desc).skip(1).limit(3))
            .unwrap(),
        3
    );
    let left = ages(&users.find(by_age(SortOrder::Desc).limit(3)).unwrap());
    assert_eq!(left, [999, 995, 994]);
    assert_consistent(&db);
}

#[test]
fn a_limit_stops_the_reading_when_an_index_gives_the_order() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<User>("users");
    let mut batch = db.batch();
    for age in 0..1000 {
        batch
            .insert(&users, user(&format!("user {age}"), age))
            .unwrap();
    }
    batch.commit().unwrap();
    let oldest = |limit| Filter {
        limit,
        ..by_age(SortOrder::Desc)
    };

    // Without an index every document is read, then sorted.
    let reads = records_read(|| assert_eq!(users.find(oldest(Some(5))).unwrap().len(), 5));
    assert_eq!(reads, 1000);

    users.ensure_index("age").unwrap();
    assert_eq!(
        users.explain(&oldest(Some(5))).unwrap(),
        QueryPlan::IndexOrder {
            field: "age".to_string()
        }
    );
    let mut found = Vec::new();
    let reads = records_read(|| found = users.find(oldest(Some(5))).unwrap());
    assert_eq!(reads, 5);
    let ages: Vec<i64> = found.iter().map(|u| u.age).collect();
    assert_eq!(ages, [999, 998, 997, 996, 995]);

    // `find_one` with a sort reads one.
    let mut first = None;
    let reads = records_read(|| first = users.find_one(by_age(SortOrder::Asc)).unwrap());
    assert_eq!((reads, first.map(|u| u.age)), (1, Some(0)));

    // A range on the sort field narrows the walk, and rules out the
    // values no index holds — so running out of matches doesn't
    // scan for them.
    let young = Filter {
        limit: Some(50),
        ..filter(vec![cond("age", Op::Lt, Document::Int(10))])
    };
    let young = Filter {
        sort: by_age(SortOrder::Asc).sort,
        ..young
    };
    // The range includes its bound, age 10 (§28.4), whose entry
    // rejects it unread (SPEC §100).
    let reads = records_read(|| assert_eq!(users.find(young).unwrap().len(), 10));
    assert_eq!(reads, 10);

    // Without such a range, running out means one scan for them —
    // here a document whose age is an array, sorted after the rest.
    let docs = db.collection::<Document>("users");
    docs.insert(object(vec![("age", Document::Array(vec![]))]))
        .unwrap();
    let all = docs
        .find(Filter {
            limit: Some(2000),
            ..by_age(SortOrder::Desc)
        })
        .unwrap();
    assert_eq!(all.len(), 1001);
    assert!(matches!(
        crate::query::field_value(all.last().unwrap(), "age"),
        Some(Document::Array(_))
    ));
    // Without a limit, nothing stops early: the plan is a scan.
    assert_eq!(users.explain(&oldest(None)).unwrap(), QueryPlan::Scan);
}

/// The builder end to end (SPEC §35): conditions, a sort, a limit and
/// an `Option` passed as it is — `None` finds what serde stored as
/// `None`.
#[test]
fn a_built_filter_finds_through_a_typed_collection() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Member {
        name: String,
        age: i64,
        nick: Option<String>,
    }
    let (_dir, db) = fresh_db();
    let members = db.collection::<Member>("members");
    for (name, age, nick) in [("Ada", 36, None), ("Bob", 41, Some("B")), ("Cy", 17, None)] {
        let nick = nick.map(String::from);
        members
            .insert(Member {
                name: name.into(),
                age,
                nick,
            })
            .unwrap();
    }
    members.ensure_index("age").unwrap();
    let names = |filter| -> Vec<String> {
        members
            .find(filter)
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect()
    };

    let wanted_nick: Option<&str> = None;
    assert_eq!(
        names(Filter::new().eq("nick", wanted_nick).sort_asc("name")),
        ["Ada", "Cy"]
    );
    assert_eq!(
        names(Filter::new().gte("age", 18).sort_desc("age").limit(1)),
        ["Bob"]
    );
    assert_eq!(
        names(Filter::new().contains("name", "a").is_null("nick")),
        ["Ada"]
    );
    let oldest_adult = Filter::new().gte("age", 18).sort_desc("age").limit(1);
    assert_eq!(
        members.explain(&oldest_adult).unwrap(),
        QueryPlan::IndexOrder {
            field: "age".to_string()
        }
    );
}
