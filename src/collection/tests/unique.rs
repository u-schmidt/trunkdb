use super::*;

// --- Unique indexes (SPEC §33) ---

#[test]
fn a_unique_index_refuses_equal_values_and_rolls_back_the_whole_batch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.trunkdb");
    let email = |s: &str| object(vec![("email", Document::String(s.to_string()))]);
    let (ada, bob) = {
        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>("docs");
        assert!(
            docs.ensure_index_with("email", crate::IndexOptions::new().unique())
                .unwrap()
        );
        assert!(
            !docs
                .ensure_index_with("email", crate::IndexOptions::new().unique())
                .unwrap()
        );
        assert_eq!(docs.unique_indexes().unwrap(), ["email"]);

        let ada = docs.insert(email("ada@x")).unwrap();
        let (id, existing) = duplicate(docs.insert(email("ada@x")));
        assert_eq!(existing, ada);
        assert_ne!(id, ada);
        // Equal as `Eq` sees it: case matters.
        let bob = docs.insert(email("ADA@x")).unwrap();
        assert_eq!(docs.count(Filter::default()).unwrap(), 2);

        // The first insert of a failing batch is rolled back too.
        let insert = |doc| WriteOp::Insert("docs".into(), db.id_gen().generate(), doc);
        duplicate(db.write_batch(vec![insert(email("cy@x")), insert(email("cy@x"))]));
        assert_eq!(docs.count(Filter::default()).unwrap(), 2);

        // An update onto a taken value fails and changes nothing...
        duplicate(docs.update(&bob, email("ada@x")));
        assert_eq!(docs.get(&bob).unwrap(), Some(with_id(email("ADA@x"), bob)));
        // ...keeping one's own value is fine, even when the document
        // grows and moves (SPEC §20.3)...
        let mut grown = email("ada@x");
        if let Document::Object(fields) = &mut grown {
            fields.insert("pad".into(), Document::String("p".repeat(9000)));
        }
        docs.update(&ada, grown).unwrap();
        // ...and a value is free again once its document is gone.
        docs.delete(&bob).unwrap();
        let bob = docs.insert(email("ADA@x")).unwrap();

        // Any number of documents may lack the field or hold null.
        for doc in [object(vec![]), object(vec![]), email("x")] {
            docs.insert(doc).unwrap();
        }
        for _ in 0..2 {
            docs.insert(object(vec![("email", Document::Null)]))
                .unwrap();
        }
        (ada, bob)
    };

    let db = Database::open(&path).unwrap();
    let docs = db.collection::<Document>("docs");
    assert_eq!(docs.unique_indexes().unwrap(), ["email"]);
    assert_eq!(duplicate(docs.insert(email("ada@x"))).1, ada);
    assert_eq!(duplicate(docs.insert(email("ADA@x"))).1, bob);
}

#[test]
fn unique_means_equal_values_not_equal_keys() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index_with("k", crate::IndexOptions::new().unique())
        .unwrap();
    let k = |value: Document| object(vec![("k", value)]);
    let long = |tail: &str| Document::String("x".repeat(1200) + tail);
    let big = 1i64 << 53;

    docs.insert(k(Document::Int(1))).unwrap();
    duplicate(docs.insert(k(Document::Float(1.0))));
    docs.insert(k(Document::Float(0.0))).unwrap();
    duplicate(docs.insert(k(Document::Float(-0.0))));
    // Same key (§28.1), different values: both allowed.
    docs.insert(k(Document::Int(big))).unwrap();
    docs.insert(k(Document::Int(big + 1))).unwrap();
    docs.insert(k(long("a"))).unwrap();
    docs.insert(k(long("b"))).unwrap();
    duplicate(docs.insert(k(long("a"))));
    // Values no condition can match equal aren't checked.
    for _ in 0..2 {
        docs.insert(k(Document::Float(f64::NAN))).unwrap();
        docs.insert(k(Document::Array(vec![Document::Int(1)])))
            .unwrap();
    }
}

#[test]
fn ensure_unique_index_over_duplicates_creates_nothing() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<User>("users");
    let first = users.insert(user("Ada", 36)).unwrap();
    users.insert(user("Bob", 41)).unwrap();
    let second = users.insert(user("Ada", 20)).unwrap();

    assert_eq!(
        duplicate(users.ensure_index_with("name", crate::IndexOptions::new().unique())),
        (second, first)
    );
    assert!(users.index_names().unwrap().is_empty());

    users.delete(&second).unwrap();
    assert!(
        users
            .ensure_index_with("name", crate::IndexOptions::new().unique())
            .unwrap()
    );

    // The other kind of index on the same field is refused, not
    // swapped: dropping it first is the caller's call.
    users.ensure_index("age").unwrap();
    for result in [
        users.ensure_index_with("age", crate::IndexOptions::new().unique()),
        users.ensure_index("name"),
    ] {
        let Err(crate::Error::Io(err)) = result else {
            panic!("expected a refusal, got {result:?}");
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
    assert_eq!(users.index_names().unwrap(), ["name", "age"]);
    assert_eq!(users.unique_indexes().unwrap(), ["name"]);
}

/// What sparse indexes are for (SPEC §44): a field few documents
/// have. One document in ten has a nick — the sparse index holds
/// those, the plain one every document; both find the same.
#[test]
fn a_sparse_index_holds_only_the_documents_with_a_value() {
    let (_dir, db) = fresh_db();
    let members = db.collection::<Member>("members");
    members.ensure_index_with("nick", SPARSE).unwrap();
    members.ensure_index_with(["nick", "team"], SPARSE).unwrap();
    members.ensure_index("team").unwrap();
    let ops = (0..1000)
        .map(|i| {
            let member = Member {
                name: format!("m{i}"),
                nick: (i % 10 == 0).then(|| format!("n{}", i % 30)),
                team: (i % 25 == 0).then(|| "red".to_string()),
            };
            let doc = crate::serde_bridge::to_document(&member).unwrap();
            WriteOp::Insert("members".into(), db.id_gen().generate(), doc)
        })
        .collect();
    db.write_batch(ops).unwrap();
    assert_eq!(entries(&db, "members", "nick"), 100);
    // Null in both only if neither is set: 100 + 40 - 20 with both.
    assert_eq!(entries(&db, "members", "(nick, team)"), 120);
    assert_eq!(entries(&db, "members", "team"), 1000);
    assert_eq!(members.sparse_indexes().unwrap(), ["nick", "(nick, team)"]);

    let by_nick = Filter::new().eq("nick", "n10");
    assert_eq!(
        members.explain(&by_nick).unwrap(),
        QueryPlan::Index {
            field: "nick".into()
        }
    );
    // The index's entries answer the count (SPEC §100).
    let reads = records_read(|| assert_eq!(members.count(by_nick).unwrap(), 33));
    assert_eq!(reads, 0);
    // Without a nick: not in the index, so a scan finds them.
    let without = Filter::new().is_null("nick");
    assert_eq!(members.explain(&without).unwrap(), QueryPlan::Scan);
    assert_eq!(members.count(without).unwrap(), 900);
    let first_nicks = Filter::new().is_not_null("nick").sort_asc("nick").limit(3);
    assert_eq!(
        members.explain(&first_nicks).unwrap(),
        QueryPlan::IndexOrder {
            field: "nick".into()
        }
    );
    let nicks: Vec<_> = members.find(first_nicks).unwrap();
    assert!(nicks.iter().all(|m| m.nick.as_deref() == Some("n0")));
    assert_consistent(&db);
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Profile {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    nick: Option<String>,
    team: Option<String>,
    tags: Vec<String>,
}

/// `exists` and `missing` on typed documents (SPEC §45.1): serde
/// writes `None` as null, so the field is there — unless the struct
/// skips it, or the document was written before the field existed.
/// Next to an indexed comparison, the index finds and they check.
#[test]
fn exists_tells_skipped_and_older_fields_from_stored_nulls() {
    let (_dir, db) = fresh_db();
    let profiles = db.collection::<Profile>("profiles");
    profiles.ensure_index("name").unwrap();
    let profile = |name: &str, nick: Option<&str>, team: Option<&str>, tags: &[&str]| Profile {
        name: name.into(),
        nick: nick.map(str::to_string),
        team: team.map(str::to_string),
        tags: tags.iter().map(|t| t.to_string()).collect(),
    };
    profiles
        .insert(profile("ada", Some("countess"), Some("red"), &["math"]))
        .unwrap();
    profiles.insert(profile("bob", None, None, &[])).unwrap();
    // Written before `team` and `tags` were added to the struct.
    let untyped = db.collection::<Document>("profiles");
    untyped
        .insert(object(vec![
            ("name", "cy".into()),
            ("nick", Document::Null),
        ]))
        .unwrap();

    let names = |f: Filter| -> Vec<String> {
        let mut names: Vec<String> = untyped
            .find(f)
            .unwrap()
            .into_iter()
            .map(|doc| match crate::query::value_or_null(&doc, "name") {
                Document::String(name) => name.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        names.sort();
        names
    };
    // `None` skipped: missing. A stored null: there.
    assert_eq!(names(Filter::new().missing("nick")), ["bob"]);
    assert_eq!(names(Filter::new().exists("nick")), ["ada", "cy"]);
    assert_eq!(names(Filter::new().is_null("nick")), ["bob", "cy"]);
    // `None` written as null: there. Older documents: missing.
    assert_eq!(names(Filter::new().exists("team")), ["ada", "bob"]);
    assert_eq!(names(Filter::new().missing("team")), ["cy"]);
    // Sizes: bob's empty list; cy has no list at all.
    assert_eq!(names(Filter::new().size("tags", Op::Eq, 0)), ["bob"]);
    assert_eq!(names(Filter::new().size("tags", Op::Ne, 0)), ["ada", "cy"]);
    assert_eq!(names(Filter::new().size("tags", Op::Gte, 1)), ["ada"]);

    let bob_without_nick = Filter::new().eq("name", "bob").missing("nick");
    assert_eq!(
        untyped.explain(&bob_without_nick).unwrap(),
        QueryPlan::Index {
            field: "name".into()
        }
    );
    assert_eq!(
        records_read(|| assert_eq!(names(bob_without_nick), ["bob"])),
        1
    );
    let found = profiles.find(Filter::new().missing("nick")).unwrap();
    assert_eq!(found, [profile("bob", None, None, &[])]);
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Line {
    sku: String,
    qty: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Order {
    number: i64,
    lines: Vec<Line>,
}

/// What `elem_match` is for (SPEC §46): orders with a line of 9 or
/// more of sku A1 — not an A1 line and some other line of 9. The
/// index on `lines[*].sku` finds the orders with an A1 line; only
/// those are read.
/// SPEC §56: an insert or update nested deeper than the limit is
/// refused like a name too long, and its batch rolls back whole.
#[test]
fn writes_nested_too_deep_are_refused() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    let nest =
        |levels: usize| (0..levels).fold(Document::Int(1), |inner, _| Document::Array(vec![inner]));
    let wrap = |inner: Document| Document::Object([("a".to_string(), inner)].into_iter().collect());
    // The document is the first level, its field's arrays the rest.
    let id = docs.insert(wrap(nest(Document::MAX_NESTING - 1))).unwrap();

    let refused = |e: crate::Error| match e {
        crate::Error::Io(e) => {
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
            assert!(e.to_string().contains("deeper than the limit of 64"), "{e}");
        }
        other => panic!("{other:?}"),
    };
    refused(docs.insert(wrap(nest(Document::MAX_NESTING))).unwrap_err());
    refused(
        docs.update(&id, wrap(nest(Document::MAX_NESTING)))
            .unwrap_err(),
    );
    let ops = [wrap(Document::Int(2)), wrap(nest(100))]
        .into_iter()
        .enumerate()
        .map(|(n, doc)| WriteOp::Insert("docs".into(), DocId([n as u8 + 1; 16]), doc))
        .collect();
    refused(db.write_batch(ops).unwrap_err());
    assert_eq!(
        docs.count(Filter::new()).unwrap(),
        1,
        "the batch rolled back whole"
    );
    let Some(Document::Object(stored)) = docs.get(&id).unwrap() else {
        panic!("the first insert is there");
    };
    assert_eq!(stored["a"], nest(Document::MAX_NESTING - 1));
}

/// Found by fuzzing (SPEC §55): a damaged compound key with fewer
/// values than the index has fields groups by the ones it has, and
/// a key too short for even its id by none.
#[test]
fn groups_of_keys_with_values_missing() {
    let spans = Spans {
        at: 1,
        end: 3,
        fields: 3,
        by_id: false,
    };
    let queued = Document::String("Queued".into());
    let one = key::compound(&[&queued], DocId([1; 16]));
    let group = spans.group(&one);
    assert_eq!(group, key::value_part(&one));
    assert!(spans.values(&one).is_empty());
    for short in [&[][..], &[1, 2, 3][..]] {
        assert_eq!(spans.group(short), &[] as &[u8]);
        assert!(spans.values(short).is_empty());
    }
}

#[test]
fn orders_with_a_large_line_of_one_sku() {
    let (_dir, db) = fresh_db();
    let orders = db.collection::<Order>("orders");
    orders.ensure_index("lines[*].sku").unwrap();
    let mut rng = XorShift(0x5851_F42D_4C95_7F2D);
    let ops = (0..2000)
        .map(|number| {
            let lines = (0..1 + rng.below(3))
                .map(|_| Line {
                    sku: format!("A{}", rng.below(20)),
                    qty: 1 + rng.below(10) as i64,
                })
                .collect();
            let order = Order { number, lines };
            let doc = crate::serde_bridge::to_document(&order).unwrap();
            WriteOp::Insert("orders".into(), db.id_gen().generate(), doc)
        })
        .collect();
    db.write_batch(ops).unwrap();

    let large_a1 = Filter::new().elem_match(
        "lines",
        Condition::eq("sku", "A1") & Condition::gte("qty", 9),
    );
    let loose = Filter::new()
        .eq("lines[*].sku", "A1")
        .gte("lines[*].qty", 9);
    let all = orders.find(Filter::new()).unwrap();
    let has = |order: &Order, both: bool| {
        let a1 = |line: &Line| line.sku == "A1";
        let large = |line: &Line| line.qty >= 9;
        match both {
            true => order.lines.iter().any(|l| a1(l) && large(l)),
            false => order.lines.iter().any(a1) && order.lines.iter().any(large),
        }
    };
    let numbers = |orders: Vec<Order>| {
        let mut numbers: Vec<i64> = orders.iter().map(|o| o.number).collect();
        numbers.sort();
        numbers
    };
    let expected = numbers(all.iter().filter(|o| has(o, true)).cloned().collect());
    let expected_loose = numbers(all.iter().filter(|o| has(o, false)).cloned().collect());
    assert!(expected.len() < expected_loose.len(), "{expected:?}");
    assert!(!expected.is_empty());
    assert_eq!(
        orders.explain(&large_a1).unwrap(),
        QueryPlan::Index {
            field: "lines[*].sku".into()
        }
    );
    let with_a1 = orders
        .count(Filter::new().eq("lines[*].sku", "A1"))
        .unwrap();
    let reads = records_read(|| assert_eq!(numbers(orders.find(large_a1).unwrap()), expected));
    assert_eq!(reads, with_a1);
    assert!(reads < 2000 / 5, "{reads}");
    assert_eq!(numbers(orders.find(loose).unwrap()), expected_loose);
}

/// Asking for an index that exists with other options is refused,
/// whichever option differs; the same options again are a no-op.
#[test]
fn an_index_with_other_options_is_refused() {
    let (_dir, db) = fresh_db();
    let members = db.collection::<Member>("members");
    let both = IndexOptions {
        unique: true,
        sparse: true,
    };
    assert!(members.ensure_index_with("nick", SPARSE).unwrap());
    assert!(!members.ensure_index_with("nick", SPARSE).unwrap());
    assert!(members.ensure_index_with("name", both.clone()).unwrap());
    for result in [
        members.ensure_index("nick"),
        members.ensure_index_with("nick", crate::IndexOptions::new().unique()),
        members.ensure_index_with("nick", both),
        members.ensure_index_with("name", crate::IndexOptions::new().unique()),
    ] {
        let Err(crate::Error::Io(err)) = result else {
            panic!("expected a refusal, got {result:?}");
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("drop it first"), "{err}");
    }
    let Err(err) = members.ensure_index("nick") else {
        unreachable!()
    };
    assert!(
        err.to_string()
            .contains("already has a sparse index on \"nick\", not a plain"),
        "{err}"
    );
    assert_eq!(members.unique_indexes().unwrap(), ["name"]);
    assert_eq!(members.sparse_indexes().unwrap(), ["nick", "name"]);

    members
        .insert(Member {
            name: "Ada".into(),
            nick: None,
            team: None,
        })
        .unwrap();
    members
        .insert(Member {
            name: "Bob".into(),
            nick: None,
            team: None,
        })
        .unwrap();
    let taken = members.insert(Member {
        name: "Ada".into(),
        nick: None,
        team: None,
    });
    assert!(
        matches!(taken, Err(crate::Error::DuplicateValue { .. })),
        "{taken:?}"
    );
}

/// Random single-op batches against a unique index on `v`: each must
/// fail exactly when a scan finds another document with an equal,
/// non-null `v` — and a failed one must change nothing.
#[test]
fn a_unique_index_refuses_exactly_what_a_scan_finds_taken() {
    a_unique_index_refuses_what_a_scan_finds_taken(false);
}

/// Unique and sparse (SPEC §44): nulls were exempt anyway, now they
/// have no entries either.
#[test]
fn a_unique_sparse_index_refuses_exactly_what_a_scan_finds_taken() {
    a_unique_index_refuses_what_a_scan_finds_taken(true);
}

fn a_unique_index_refuses_what_a_scan_finds_taken(sparse: bool) {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    let options = IndexOptions {
        unique: true,
        sparse,
    };
    docs.ensure_index_with("v", options).unwrap();
    let mut rng = XorShift(0x1656_67B1_9E37_79F9);
    let mut ids: Vec<DocId> = Vec::new();
    let (mut accepted, mut refused) = (0, 0);

    // Single-op batches, so each outcome can be checked — each one
    // syncs to disk twice, which is what keeps the count low.
    for _ in 0..300 {
        let before = docs.find_with_ids(Filter::default()).unwrap();
        let doc = random_document(&mut rng);
        let (op, id) = match rng.below(4) {
            0 if !ids.is_empty() => {
                let id = ids[rng.below(ids.len())];
                (WriteOp::Delete("docs".into(), id), None)
            }
            1 | 2 if !ids.is_empty() => {
                let id = ids[rng.below(ids.len())];
                (WriteOp::Update("docs".into(), id, doc.clone()), Some(id))
            }
            _ => {
                let id = db.id_gen().generate();
                (WriteOp::Insert("docs".into(), id, doc.clone()), Some(id))
            }
        };
        let value = crate::query::value_or_null(&doc, "v");
        let taken = id.is_some()
            && !matches!(value, Document::Null)
            && before.iter().any(|(other, stored)| {
                Some(*other) != id
                    && crate::query::equal(crate::query::value_or_null(stored, "v"), value)
            });

        match (db.write_batch(vec![op.clone()]), taken) {
            (Ok(()), false) => {
                accepted += 1;
                match op {
                    WriteOp::Insert(_, id, _) => ids.push(id),
                    WriteOp::Delete(_, id) => ids.retain(|other| *other != id),
                    WriteOp::Update(..) => {}
                }
            }
            (Err(crate::Error::DuplicateValue { .. }), true) => {
                refused += 1;
                let after = docs.find_with_ids(Filter::default()).unwrap();
                assert_eq!(sorted_ids(after), sorted_ids(before));
            }
            (result, taken) => panic!("{op:?}: {result:?}, but taken = {taken}"),
        }
    }
    assert!(accepted > 100 && refused > 15, "{accepted} / {refused}");
    assert_index_agrees_with_scan(&docs, &mut rng, "v");
}

/// A batch that fails after touching an index leaves no entry behind.
#[test]
fn a_failed_batch_rolls_back_its_index_entries() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<User>("users");
    users.ensure_index("age").unwrap();
    let ada = users
        .insert(User {
            name: "Ada".to_string(),
            age: 36,
        })
        .unwrap();

    let mut batch = db.batch();
    let grace = User {
        name: "Grace".to_string(),
        age: 45,
    };
    batch.insert(&users, grace).unwrap();
    batch.delete(&users, &DocId([0; 16])); // doesn't exist: fails the batch
    assert!(batch.commit().is_err());

    let found = users.find_with_ids(age_filter(Op::Gte, 0)).unwrap();
    assert_eq!(
        found.into_iter().map(|(id, _)| id).collect::<Vec<_>>(),
        [ada]
    );
}
