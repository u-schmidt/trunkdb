use super::*;

// --- Secondary indexes (SPEC §28) ---

/// A document of about `pad` bytes, numbered `n`.
fn padded(n: i64, pad: usize) -> Document {
    object(vec![
        ("n", Document::Int(n)),
        ("pad", Document::String("x".repeat(pad))),
    ])
}

/// SPEC §75: the space deletes leave behind in older pages is refilled
/// by later inserts, so a collection deleting and inserting as much
/// stays the size it was.
#[test]
fn inserts_refill_the_space_deletes_leave_behind() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    // In batches of 100, each one commit: one flush, not one per document.
    // Numbers of one width as varints (§170), so every document is
    // the size of every other.
    let mut ids = Vec::new();
    for chunk in (1000..3000).collect::<Vec<i64>>().chunks(100) {
        let mut batch = db.batch();
        for &n in chunk {
            ids.push(batch.insert(&docs, padded(n, 400)).unwrap());
        }
        batch.commit().unwrap();
    }
    let mut batch = db.batch();
    for id in ids.iter().step_by(2) {
        batch.delete(&docs, id);
    }
    batch.commit().unwrap();
    let before = db.file_info().unwrap().pages;

    for chunk in (1000..2000).collect::<Vec<i64>>().chunks(100) {
        let mut batch = db.batch();
        for &n in chunk {
            batch.insert(&docs, padded(n, 400)).unwrap();
        }
        batch.commit().unwrap();
    }
    let after = db.file_info().unwrap().pages;
    // The new ids' index entries need new pages; the documents don't.
    let state = db.state();
    let new_data_pages = (before..after)
        .filter(|&page| {
            state.store.read_page(page).unwrap()[0] == crate::storage::PageType::Data as u8
        })
        .count();
    drop(state);
    assert_eq!(new_data_pages, 0, "of {} new pages", after - before);
    assert_consistent(&db);
}

/// SPEC §75: dropping a collection forgets its map, whose pages are
/// freed; a new collection of the same name starts without it.
#[test]
fn a_dropped_collection_takes_its_free_space_map_along() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    let ids: Vec<DocId> = (0..8)
        .map(|n| docs.insert(padded(n, 3000)).unwrap())
        .collect();
    docs.delete(&ids[0]).unwrap();
    assert!(db.drop_collection("docs").unwrap());

    docs.insert(padded(0, 3000)).unwrap();
    assert_consistent(&db);
}

/// SPEC §75: a failed batch leaves the map as it was, so it names no
/// page the batch allocated or freed and the rollback took back.
#[test]
fn a_failed_batch_leaves_the_free_space_map_as_it_was() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    let ids: Vec<DocId> = (0..8)
        .map(|n| docs.insert(padded(n, 3000)).unwrap())
        .collect();
    docs.delete(&ids[0]).unwrap();
    let map = |db: &Database| db.state().catalog().free_space("docs").unwrap().entries();
    let before = map(&db);
    assert!(!before.is_empty());

    let mut batch = db.batch();
    for id in &ids[1..6] {
        batch.delete(&docs, id); // empties and frees pages
    }
    for n in 0..6 {
        batch.insert(&docs, padded(n, 5000)).unwrap(); // allocates pages
    }
    batch.update(&docs, &DocId([7; 16]), padded(0, 1)).unwrap();
    assert!(matches!(batch.commit(), Err(crate::Error::NotFound { .. })));

    assert_eq!(map(&db), before);
    assert_consistent(&db);
}

#[test]
fn indexed_finds_match_full_scans_through_every_kind_of_write() {
    indexed_finds_match_full_scans(IndexOptions::default());
}

/// A sparse index (SPEC §44) lacks the null and missing `v`s, which
/// a random filter asks for now and then: it must be left out then.
#[test]
fn sparse_finds_match_full_scans_through_every_kind_of_write() {
    indexed_finds_match_full_scans(SPARSE);
}

fn indexed_finds_match_full_scans(options: IndexOptions) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let mut rng = XorShift(0x2545_F491_4F6C_DD1D);
    {
        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>("docs");
        let mut ids = Vec::new();
        // Batches of 25 ops keep the test fast (each commit syncs to
        // disk twice); every op still goes through `apply_write_op`.
        let mut insert = |rng: &mut XorShift, count| {
            for _ in 0..count / 25 {
                let ops = (0..25)
                    .map(|_| {
                        let id = db.id_gen().generate();
                        ids.push(id);
                        WriteOp::Insert("docs".into(), id, random_document(rng))
                    })
                    .collect();
                db.write_batch(ops).unwrap();
            }
        };
        // Some documents before the index exists: it's built from them.
        insert(&mut rng, 150);
        assert!(docs.ensure_index_with("v", options).unwrap());
        // ...and some after: inserts maintain it.
        insert(&mut rng, 150);
        for _ in 0..6 {
            let ops = (0..25)
                .map(|_| {
                    let id = ids[rng.below(ids.len())];
                    let new = match docs.get(&id).unwrap() {
                        // Half the time only the padding changes, so
                        // `v`'s key stays and only a move touches it.
                        Some(Document::Object(mut fields)) if rng.below(2) == 0 => {
                            let pad = "z".repeat([0, 4500, 9000][rng.below(3)]);
                            fields.insert("pad".into(), Document::String(pad));
                            Document::Object(fields)
                        }
                        _ => random_document(&mut rng),
                    };
                    WriteOp::Update("docs".into(), id, new)
                })
                .collect();
            db.write_batch(ops).unwrap();
        }
        let deleted = (0..60).map(|_| ids.swap_remove(rng.below(ids.len())));
        let ops = deleted.map(|id| WriteOp::Delete("docs".into(), id));
        db.write_batch(ops.collect()).unwrap();
        let v_eq_1 = filter(vec![cond("v", Op::Eq, Document::Int(1))]);
        assert_eq!(
            docs.explain(&v_eq_1).unwrap(),
            QueryPlan::Index {
                field: "v".to_string()
            }
        );
        assert_index_agrees_with_scan(&docs, &mut rng, "v");
    }

    let db = Database::open(&path).unwrap();
    let docs = db.collection::<Document>("docs");
    assert_eq!(docs.index_names().unwrap(), vec!["v".to_string()]);
    assert_index_agrees_with_scan(&docs, &mut rng, "v");
}

/// A document with arrays for `[*]` paths (SPEC §42): `tags` holds
/// up to 4 random values (repeats and nested arrays included), or
/// is missing, empty or a scalar; `items` holds objects with an `n`
/// (or without one), and now and then something that isn't an
/// object.
fn random_array_document(rng: &mut XorShift) -> Document {
    let mut fields = vec![("w", Document::Int(rng.below(3) as i64))];
    match rng.below(8) {
        0 => {}
        1 => fields.push(("tags", random_value(rng))),
        _ => {
            let tags = (0..rng.below(5)).map(|_| random_value(rng)).collect();
            fields.push(("tags", Document::Array(tags)));
        }
    }
    let items = (0..rng.below(4))
        .map(|_| match rng.below(6) {
            0 => object(vec![]),
            1 => random_value(rng),
            _ => object(vec![("n", random_value(rng))]),
        })
        .collect();
    fields.push(("items", Document::Array(items)));
    let pad = [0, 0, 1500, 4500, 9000][rng.below(5)];
    fields.push(("pad", Document::String("y".repeat(pad))));
    object(fields)
}

/// A condition about one element, for `elem_match`: on `n` of an
/// `items` element, or on a `tags` element itself (`""`) — ANDs,
/// ORs and NOTs of comparisons, any of which may be null.
fn random_element_condition(rng: &mut XorShift, path: &str) -> Condition {
    let ops = [Op::Eq, Op::Ne, Op::Lt, Op::Lte, Op::Gt, Op::Gte];
    let one = |rng: &mut XorShift| cond(path, ops[rng.below(6)].clone(), random_value(rng));
    match rng.below(6) {
        0 | 1 => one(rng),
        2 | 3 => one(rng) & one(rng),
        4 => one(rng) | one(rng),
        _ => !one(rng),
    }
}

/// Random `elem_match` filters must find what checking every
/// document finds (SPEC §46), bounded by the multikey indexes on
/// `items[*].n` and `tags[*]` where their condition allows — which
/// must happen.
fn assert_elem_match_agrees_with_scan(docs: &Collection<Document>, rng: &mut XorShift) {
    let all = docs.find_with_ids(Filter::default()).unwrap();
    let mut plans = std::collections::HashSet::new();
    for _ in 0..300 {
        let mut f = match rng.below(2) {
            0 => Filter::new().elem_match("items", random_element_condition(rng, "n")),
            _ => Filter::new().elem_match("tags", random_element_condition(rng, "")),
        };
        if rng.below(3) == 0 {
            f = f.eq("w", rng.below(3) as i64);
        }
        let expected = sorted_ids(f.apply_to(all.clone(), |(_, doc)| doc));
        let plan = explained(docs, &f);
        let found = sorted_ids(docs.find_with_ids(f.clone()).unwrap());
        assert_eq!(found, expected, "{f:?} via {plan:?}");
        plans.insert(format!("{plan:?}"));
    }
    for plan in [
        r#"Index { field: "items[*].n" }"#,
        r#"Index { field: "tags[*]" }"#,
        "Scan",
    ] {
        assert!(plans.contains(plan), "{plan} never ran: {plans:?}");
    }
}

/// `indexed_finds_match_full_scans_through_every_kind_of_write` for
/// multikey indexes (SPEC §42.2): a document has as many entries as
/// elements, and every write must keep exactly those.
#[test]
fn multikey_finds_match_full_scans_through_every_kind_of_write() {
    multikey_finds_match_full_scans(IndexOptions::default());
}

/// Sparse, a multikey index leaves out the null elements (SPEC §44).
#[test]
fn sparse_multikey_finds_match_full_scans_through_every_kind_of_write() {
    multikey_finds_match_full_scans(SPARSE);
}

fn multikey_finds_match_full_scans(options: IndexOptions) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let mut rng = XorShift(0x6A09_E667_F3BC_C908);
    let paths = ["tags[*]", "items[*].n"];
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
                        WriteOp::Insert("docs".into(), id, random_array_document(rng))
                    })
                    .collect();
                db.write_batch(ops).unwrap();
            }
        };
        insert(&mut rng, 150);
        for path in paths {
            assert!(docs.ensure_index_with(path, options.clone()).unwrap());
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
                        _ => random_array_document(&mut rng),
                    };
                    WriteOp::Update("docs".into(), id, new)
                })
                .collect();
            db.write_batch(ops).unwrap();
        }
        let deleted = (0..60).map(|_| ids.swap_remove(rng.below(ids.len())));
        let ops = deleted.map(|id| WriteOp::Delete("docs".into(), id));
        db.write_batch(ops.collect()).unwrap();
        for path in paths {
            let by_value = filter(vec![cond(path, Op::Eq, Document::Int(1))]);
            let field = path.to_string();
            assert_eq!(docs.explain(&by_value).unwrap(), QueryPlan::Index { field });
            assert_index_agrees_with_scan(&docs, &mut rng, path);
        }
        assert_elem_match_agrees_with_scan(&docs, &mut rng);
    }

    let db = Database::open(&path).unwrap();
    let docs = db.collection::<Document>("docs");
    assert_eq!(docs.index_names().unwrap(), paths);
    for path in paths {
        assert_index_agrees_with_scan(&docs, &mut rng, path);
    }
    assert_elem_match_agrees_with_scan(&docs, &mut rng);
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Post {
    title: String,
    tags: Vec<String>,
    comments: Vec<Comment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Comment {
    author: String,
    likes: i64,
}

fn post(title: &str, tags: &[&str], comments: &[(&str, i64)]) -> Post {
    Post {
        title: title.into(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        comments: comments
            .iter()
            .map(|(author, likes)| Comment {
                author: author.to_string(),
                likes: *likes,
            })
            .collect(),
    }
}

/// The typed use: conditions on elements, the same answers with and
/// without indexes, each post once even when several of its elements
/// match, and every kind of write keeping the indexes right.
#[test]
fn posts_are_found_by_any_tag_or_comment() {
    let (dir, db) = fresh_db();
    let posts = db.collection::<Post>("posts");
    posts
        .insert(post("a", &["rust", "db"], &[("ann", 3), ("bob", 12)]))
        .unwrap();
    posts
        .insert(post("b", &["rust", "rust", "web"], &[("ann", 40)]))
        .unwrap();
    posts.insert(post("c", &[], &[])).unwrap();
    posts.insert(post("d", &["draft"], &[("cy", 1)])).unwrap();

    let titles = |f: Filter| {
        let mut titles: Vec<String> = posts
            .find(f.sort_asc("title"))
            .unwrap()
            .into_iter()
            .map(|p| p.title)
            .collect();
        titles.dedup();
        titles
    };
    let queries = || {
        [
            (Filter::new().eq("tags[*]", "rust"), vec!["a", "b"]),
            (Filter::new().ne("tags[*]", "rust"), vec!["c", "d"]),
            (Filter::new().gt("comments[*].likes", 10), vec!["a", "b"]),
            (
                Filter::new().eq("comments[*].author", "ann"),
                vec!["a", "b"],
            ),
            (Filter::new().contains("tags[*]", "RA"), vec!["d"]),
            (
                Filter::new()
                    .eq("tags[*]", "rust")
                    .lt("comments[*].likes", 5),
                vec!["a"],
            ),
        ]
    };
    for (f, expected) in queries() {
        assert_eq!(titles(f.clone()), expected, "{f:?}");
    }
    posts.ensure_index("tags[*]").unwrap();
    posts.ensure_index("comments[*].likes").unwrap();
    for (f, expected) in queries() {
        assert_eq!(
            titles(f.clone()),
            expected,
            "{f:?} via {:?}",
            posts.explain(&f)
        );
    }
    let rust = Filter::new().eq("tags[*]", "rust");
    assert_eq!(
        posts.explain(&rust).unwrap(),
        QueryPlan::Index {
            field: "tags[*]".into()
        }
    );
    // "b" has "rust" twice and matches once, in every kind of read.
    assert_eq!(posts.find(rust.clone()).unwrap().len(), 2);
    assert_eq!(posts.count(rust.clone()).unwrap(), 2);
    assert_eq!(posts.cursor(rust.clone()).unwrap().count(), 2);

    // Writes: a tag removed stops matching, one added starts.
    let changed = posts
        .update_many(Filter::new().eq("title", "a"), |p| {
            p.tags = vec!["db".into(), "new".into()];
        })
        .unwrap();
    assert_eq!(changed, 1);
    assert_eq!(titles(rust.clone()), vec!["b"]);
    assert_eq!(titles(Filter::new().eq("tags[*]", "new")), vec!["a"]);
    assert_eq!(
        posts
            .delete_many(Filter::new().eq("tags[*]", "web"))
            .unwrap(),
        1
    );
    assert_eq!(titles(rust), Vec::<String>::new());
    assert_consistent(&db);

    // Export and import keep the multikey indexes as they are.
    let mut export = Vec::new();
    db.export(&mut export).unwrap();
    let copy = Database::open(dir.path().join("copy.trunkdb")).unwrap();
    copy.import(export.as_slice()).unwrap();
    let copied = copy.collection::<Post>("posts");
    assert_eq!(
        copied.index_names().unwrap(),
        ["tags[*]", "comments[*].likes"]
    );
    let new = Filter::new().eq("tags[*]", "new");
    assert_eq!(
        copied.find(new.clone()).unwrap(),
        posts.find(new.clone()).unwrap()
    );
    assert_eq!(
        copied.explain(&new).unwrap(),
        QueryPlan::Index {
            field: "tags[*]".into()
        }
    );
    assert_consistent(&copy);
}

/// Unique across documents, not within one (SPEC §42.2): no two
/// users share an alias, and one user may list an alias twice.
#[test]
fn a_unique_multikey_index_refuses_an_element_another_document_has() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");
    let aliases = |names: &[&str]| {
        let names = names.iter().map(|n| Document::from(*n)).collect();
        object(vec![("aliases", Document::Array(names))])
    };
    users
        .ensure_index_with("aliases[*]", crate::IndexOptions::new().unique())
        .unwrap();
    let ada = users.insert(aliases(&["ada", "al"])).unwrap();
    users.insert(aliases(&["bo", "bo"])).unwrap();
    users.insert(aliases(&[])).unwrap();
    users.insert(aliases(&[])).unwrap();
    match users.insert(aliases(&["cy", "al"])) {
        Err(crate::Error::DuplicateValue {
            existing, field, ..
        }) => {
            assert_eq!((existing, field.as_str()), (ada, "aliases[*]"));
        }
        other => panic!("expected DuplicateValue, got {other:?}"),
    }
    // Ada gives "al" up; then someone else may have it.
    assert!(users.update(&ada, aliases(&["ada"])).unwrap());
    users.insert(aliases(&["cy", "al"])).unwrap();
    // Ada keeps what she has: her own elements don't collide.
    assert!(users.update(&ada, aliases(&["ada", "ada2"])).unwrap());
    assert_consistent(&db);

    // Built over existing documents that share an element: refused.
    let other = db.collection::<Document>("other");
    other.insert(aliases(&["x", "y"])).unwrap();
    other.insert(aliases(&["y"])).unwrap();
    assert!(matches!(
        other.ensure_index_with("aliases[*]", crate::IndexOptions::new().unique()),
        Err(crate::Error::DuplicateValue { .. })
    ));
    assert_eq!(other.index_names().unwrap(), Vec::<String>::new());
}

/// Two long strings that differ only past the key budget share a key
/// (SPEC §28.1). In one document's elements they're one entry — and
/// still both checked: a document listing `…a` and `…b` can't take
/// the `…b` another one has, whichever comes first.
#[test]
fn a_unique_multikey_index_checks_every_element_under_a_shared_key() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");
    let long = |end: &str| Document::String("x".repeat(1200) + end);
    let aliases = |names: Vec<Document>| object(vec![("aliases", Document::Array(names))]);
    users
        .ensure_index_with("aliases[*]", crate::IndexOptions::new().unique())
        .unwrap();
    users.insert(aliases(vec![long("b")])).unwrap();
    for both in [vec![long("a"), long("b")], vec![long("b"), long("a")]] {
        assert!(matches!(
            users.insert(aliases(both)),
            Err(crate::Error::DuplicateValue { .. })
        ));
    }
    users.insert(aliases(vec![long("a"), long("c")])).unwrap();
    assert_consistent(&db);
}

#[test]
fn index_paths_take_brackets_only_as_a_whole_step_suffix() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    for bad in [
        "tags[*", "tags[1]", "ta[*]gs", "[*]", "a.[*]", "_id[*]", "tags[]",
    ] {
        assert!(docs.ensure_index(bad).is_err(), "{bad}");
    }
    for good in ["tags[*]", "items[*].n", "grid[*][*]", "a.b[*].c"] {
        assert!(docs.ensure_index(good).unwrap(), "{good}");
    }
}

/// Sorting by a path with `[*]` has nothing to sort by — every
/// document reads as null there, in id order — and never reads a
/// document twice, with or without its index (SPEC §42.3).
#[test]
fn sorting_by_elements_reads_each_document_once() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    for tags in [vec![3, 1], vec![2], vec![5, 4, 5]] {
        let tags = tags.into_iter().map(Document::from).collect();
        docs.insert(object(vec![("tags", Document::Array(tags))]))
            .unwrap();
    }
    docs.ensure_index("tags[*]").unwrap();
    let sorted = Filter::new().sort_desc("tags[*]").limit(10);
    assert_ne!(
        docs.explain(&sorted).unwrap(),
        QueryPlan::IndexOrder {
            field: "tags[*]".into()
        }
    );
    let found = docs.find_with_ids(sorted).unwrap();
    let ids: Vec<DocId> = found.iter().map(|(id, _)| *id).collect();
    let mut in_order = ids.clone();
    in_order.sort();
    assert_eq!(ids, in_order);
}

#[test]
fn ensure_and_drop_index_are_idempotent_and_drop_frees_the_tree() {
    let (dir, db) = fresh_db();
    let users = db.collection::<User>("users");
    let mut batch = db.batch();
    for age in 0..1000 {
        let name = format!("user {age}");
        batch.insert(&users, User { name, age }).unwrap();
    }
    batch.commit().unwrap();
    let file_len = || {
        std::fs::metadata(dir.path().join("test.trunkdb"))
            .unwrap()
            .len()
    };
    let adults = age_filter(Op::Gte, 18);

    assert!(users.ensure_index("age").unwrap());
    assert!(!users.ensure_index("age").unwrap());
    assert!(users.ensure_index("name").unwrap());
    assert_eq!(users.index_names().unwrap(), ["age", "name"]);
    assert_eq!(
        users.explain(&adults).unwrap(),
        QueryPlan::Index {
            field: "age".to_string()
        }
    );
    let with_indexes = file_len();

    assert!(users.drop_index("age").unwrap());
    assert!(!users.drop_index("age").unwrap());
    assert_eq!(users.index_names().unwrap(), ["name"]);
    assert_eq!(users.explain(&adults).unwrap(), QueryPlan::Scan);
    assert_eq!(users.find(adults.clone()).unwrap().len(), 982);

    // The dropped index's pages are reused, not appended after.
    assert!(users.ensure_index("age").unwrap());
    assert_eq!(file_len(), with_indexes);
    assert_eq!(users.find(adults).unwrap().len(), 982);
}

#[test]
fn an_index_can_be_declared_before_the_collection_exists() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<User>("users");

    assert!(users.ensure_index("name").unwrap());
    users
        .insert(User {
            name: "Ada".to_string(),
            age: 36,
        })
        .unwrap();

    let found = users
        .find(filter(vec![cond(
            "name",
            Op::Eq,
            Document::String("Ada".to_string()),
        )]))
        .unwrap();
    assert_eq!(found.len(), 1);
}

#[test]
fn id_malformed_and_overlong_paths_cannot_be_indexed() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");

    let invalid = ["_id", "_id.x", "", ".", "a.", ".a", "a..b"].map(String::from);
    for field in invalid.into_iter().chain(["f".repeat(256)]) {
        let Err(crate::Error::Io(err)) = docs.ensure_index(&field) else {
            panic!("{field:?} must be rejected");
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
    assert!(docs.index_names().unwrap().is_empty());
}

/// A document with a value at `a.b.c`, or at a spot that path must
/// not reach: one level short, inside an array, under a key with a
/// dot in it — or no `a` at all.
fn random_nested_document(rng: &mut XorShift) -> Document {
    let v = random_value(rng);
    let a = match rng.below(8) {
        0..=3 => object(vec![("b", object(vec![("c", v), ("d", Document::Int(1))]))]),
        4 => object(vec![("b", v)]),
        5 => object(vec![("b.c", v)]),
        6 => Document::Array(vec![object(vec![("b", object(vec![("c", v)]))])]),
        _ => v,
    };
    let mut fields = vec![("w", Document::Int(rng.below(3) as i64))];
    match rng.below(8) {
        0 => fields.push(("a.b.c", random_value(rng))),
        1 => {}
        _ => fields.push(("a", a)),
    }
    let pad = [0, 0, 1500, 9000][rng.below(4)];
    fields.push(("pad", Document::String("y".repeat(pad))));
    object(fields)
}

#[test]
fn nested_path_indexes_match_full_scans_through_every_kind_of_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    {
        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>("docs");
        let mut ids = Vec::new();
        let mut insert = |rng: &mut XorShift| {
            let ops = (0..25)
                .map(|_| {
                    let id = db.id_gen().generate();
                    ids.push(id);
                    WriteOp::Insert("docs".into(), id, random_nested_document(rng))
                })
                .collect();
            db.write_batch(ops).unwrap();
        };
        (0..4).for_each(|_| insert(&mut rng));
        assert!(docs.ensure_index("a.b.c").unwrap());
        (0..4).for_each(|_| insert(&mut rng));
        // Updates move values into and out of the path's reach.
        for _ in 0..4 {
            let ops = (0..25)
                .map(|_| {
                    let id = ids[rng.below(ids.len())];
                    WriteOp::Update("docs".into(), id, random_nested_document(&mut rng))
                })
                .collect();
            db.write_batch(ops).unwrap();
        }
        let deleted = (0..30).map(|_| ids.swap_remove(rng.below(ids.len())));
        let ops = deleted.map(|id| WriteOp::Delete("docs".into(), id));
        db.write_batch(ops.collect()).unwrap();

        let reachable = docs
            .find(Filter::default())
            .unwrap()
            .iter()
            .filter(|doc| crate::query::field_value(doc, "a.b.c").is_some())
            .count();
        assert!(reachable > 50, "only {reachable} documents have the path");
        let eq_1 = filter(vec![cond("a.b.c", Op::Eq, Document::Int(1))]);
        assert_eq!(
            docs.explain(&eq_1).unwrap(),
            QueryPlan::Index {
                field: "a.b.c".to_string()
            }
        );
        assert_index_agrees_with_scan(&docs, &mut rng, "a.b.c");
    }

    let db = Database::open(&path).unwrap();
    let docs = db.collection::<Document>("docs");
    assert_eq!(docs.index_names().unwrap(), ["a.b.c"]);
    assert_index_agrees_with_scan(&docs, &mut rng, "a.b.c");
}

/// The typed path: a nested struct is a nested object, so its fields
/// are reachable by path — for filters, indexes and sort alike.
#[test]
fn typed_nested_fields_are_filtered_indexed_and_sorted_by_path() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Address {
        city: String,
        zip: i64,
    }
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Person {
        name: String,
        address: Address,
    }
    let person = |name: &str, city: &str, zip| Person {
        name: name.to_string(),
        address: Address {
            city: city.to_string(),
            zip,
        },
    };

    let (_dir, db) = fresh_db();
    let people = db.collection::<Person>("people");
    people.insert(person("Ada", "Berlin", 10115)).unwrap();
    people.insert(person("Bob", "Hamburg", 20095)).unwrap();
    people.insert(person("Cy", "Berlin", 10245)).unwrap();
    assert!(people.ensure_index("address.city").unwrap());

    let in_berlin = Filter {
        sort: vec![crate::query::Sort {
            field: "address.zip".to_string(),
            order: SortOrder::Desc,
        }],
        ..filter(vec![cond(
            "address.city",
            Op::Eq,
            Document::String("Berlin".to_string()),
        )])
    };
    assert_eq!(
        people.explain(&in_berlin).unwrap(),
        QueryPlan::Index {
            field: "address.city".to_string()
        }
    );
    assert_eq!(
        people.find(in_berlin.clone()).unwrap(),
        [
            person("Cy", "Berlin", 10245),
            person("Ada", "Berlin", 10115)
        ]
    );

    // Moving to another city moves the index entry too.
    let (cy, _) = people.find_with_ids(in_berlin.clone()).unwrap().remove(0);
    people.update(&cy, person("Cy", "Hamburg", 20095)).unwrap();
    assert_eq!(
        people.find(in_berlin).unwrap(),
        [person("Ada", "Berlin", 10115)]
    );
}

/// `None` on the typed path, whether stored as null, left out by serde,
/// or never written because the field is newer than the document:
/// `== null` finds all three, through the index (SPEC §32).
#[test]
fn null_and_missing_fields_are_found_alike_through_the_index() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Member {
        name: String,
        nick: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        team: Option<String>,
    }
    let member = |name: &str, nick: Option<&str>, team: Option<&str>| Member {
        name: name.to_string(),
        nick: nick.map(String::from),
        team: team.map(String::from),
    };

    let (_dir, db) = fresh_db();
    // Written before `Member` had `nick`: no such key at all.
    db.collection::<User>("members")
        .insert(user("Old", 99))
        .unwrap();
    let members = db.collection::<Member>("members");
    members.insert(member("Ada", None, None)).unwrap();
    members
        .insert(member("Bob", Some("B"), Some("red")))
        .unwrap();
    members.insert(member("Cy", Some("C"), None)).unwrap();
    assert!(members.ensure_index("nick").unwrap());
    assert!(members.ensure_index("team").unwrap());

    let names = |field: &str, op: Op| {
        let f = filter(vec![cond(field, op, Document::Null)]);
        let mut names: Vec<String> = db
            .collection::<Document>("members")
            .find(f)
            .unwrap()
            .into_iter()
            .map(|doc| match crate::query::field_value(&doc, "name") {
                Some(Document::String(name)) => name.clone(),
                other => panic!("no name: {other:?}"),
            })
            .collect();
        names.sort();
        names
    };
    let nick_is_null = filter(vec![cond("nick", Op::Eq, Document::Null)]);
    assert_eq!(
        members.explain(&nick_is_null).unwrap(),
        QueryPlan::Index {
            field: "nick".to_string()
        }
    );
    assert_eq!(names("nick", Op::Eq), ["Ada", "Old"]);
    assert_eq!(names("nick", Op::Ne), ["Bob", "Cy"]);
    // `team` is never stored as null, only left out.
    assert_eq!(names("team", Op::Eq), ["Ada", "Cy", "Old"]);
    assert_eq!(names("team", Op::Ne), ["Bob"]);

    // Setting and clearing a field moves its index entry.
    let (bob, _) = members
        .find_with_ids(filter(vec![cond(
            "team",
            Op::Eq,
            Document::String("red".to_string()),
        )]))
        .unwrap()
        .remove(0);
    members.update(&bob, member("Bob", None, None)).unwrap();
    assert_eq!(names("nick", Op::Eq), ["Ada", "Bob", "Old"]);
    assert_eq!(names("team", Op::Eq), ["Ada", "Bob", "Cy", "Old"]);
}
