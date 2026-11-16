use super::*;

// --- find_one, count, cursor, upsert (SPEC §29) ---

fn name_is(name: &str) -> Filter {
    filter(vec![cond(
        "name",
        Op::Eq,
        Document::String(name.to_string()),
    )])
}

fn users_db() -> (tempfile::TempDir, Database, Collection<User>) {
    let (dir, db) = fresh_db();
    let users = db.collection::<User>("users");
    let mut batch = db.batch();
    for (name, age) in [("Ada", 36), ("Grace", 45), ("Alan", 41), ("Edsger", 72)] {
        batch.insert(&users, user(name, age)).unwrap();
    }
    batch.commit().unwrap();
    (dir, db, users)
}

#[test]
fn find_one_returns_a_match_or_none() {
    let (_dir, _db, users) = users_db();

    assert_eq!(
        users.find_one(name_is("Alan")).unwrap(),
        Some(user("Alan", 41))
    );
    assert_eq!(users.find_one(name_is("Barbara")).unwrap(), None);
    // With a sort: the first in that order.
    assert_eq!(
        users.find_one(by_age(SortOrder::Asc)).unwrap(),
        Some(user("Ada", 36))
    );
    assert_eq!(
        users.find_one(by_age(SortOrder::Desc)).unwrap(),
        Some(user("Edsger", 72))
    );

    let (id, found) = users.find_one_with_id(name_is("Grace")).unwrap().unwrap();
    assert_eq!(users.get(&id).unwrap(), Some(found));
}

#[test]
fn count_counts_matches_up_to_the_limit() {
    let (_dir, db, users) = users_db();

    assert_eq!(users.count(Filter::default()).unwrap(), 4);
    assert_eq!(users.count(age_filter(Op::Gt, 40)).unwrap(), 3);
    let limited = Filter {
        limit: Some(2),
        ..age_filter(Op::Gt, 40)
    };
    assert_eq!(users.count(limited).unwrap(), 2);
    // Through an index, with a candidate the recheck must drop.
    users.ensure_index("age").unwrap();
    assert_eq!(users.count(age_filter(Op::Gt, 41)).unwrap(), 2);
    assert_eq!(
        db.collection::<User>("nobody")
            .count(Filter::default())
            .unwrap(),
        0
    );
}

/// A cursor shows the moment it was created (SPEC §82). It holds no
/// lock — the writes below, on the same thread, would deadlock
/// otherwise — and sees none of them: not a delete of a document it
/// hasn't reached, not an update that makes one stop matching, not an
/// insert. Until §82 it saw each document as it was when it got there.
#[test]
fn a_cursor_shows_the_moment_it_was_created() {
    let (_dir, db, users) = users_db();
    let over_40 = age_filter(Op::Gt, 40);
    let ids: Vec<DocId> = users
        .find_with_ids(Filter::default())
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect(); // _id order: Ada, Grace, Alan, Edsger

    let mut cursor = users.cursor(over_40.clone()).unwrap();
    assert_eq!(cursor.next().unwrap().unwrap(), user("Grace", 45));

    assert!(users.delete(&ids[2]).unwrap()); // Alan: gone before he's read
    assert!(users.update(&ids[3], user("Edsger", 39)).unwrap()); // no longer matches
    assert!(users.update(&ids[0], user("Ada", 50)).unwrap()); // matches now
    users.insert(user("Barbara", 60)).unwrap(); // after the cursor was created
    // And the slot Alan was in taken by someone else, in the file.
    users.insert(user("Linus", 99)).unwrap();
    db.checkpoint().unwrap();

    assert_eq!(cursor.next().unwrap().unwrap(), user("Alan", 41));
    assert_eq!(cursor.next().unwrap().unwrap(), user("Edsger", 72));
    assert!(cursor.next().is_none());

    // A cursor made now has all of it.
    let now: Vec<User> = users
        .cursor(over_40)
        .unwrap()
        .collect::<crate::Result<_>>()
        .unwrap();
    let expected = [
        user("Ada", 50),
        user("Grace", 45),
        user("Barbara", 60),
        user("Linus", 99),
    ];
    assert_eq!(now, expected);

    let limited = Filter {
        limit: Some(1),
        ..Filter::default()
    };
    assert_eq!(users.cursor(limited).unwrap().count(), 1);
}

/// An open cursor keeps its moment as a snapshot does (SPEC §82): the
/// database can't be compacted until it's dropped. A sorted cursor has
/// read everything when it's made, and keeps nothing.
#[test]
fn an_open_cursor_refuses_compaction() {
    let (_dir, db, users) = users_db();
    let ids: Vec<DocId> = (0..300)
        .map(|i| users.insert(user(&"x".repeat(i % 900), 1)).unwrap())
        .collect();
    for id in &ids {
        assert!(users.delete(id).unwrap());
    }

    let mut cursor = users.cursor(Filter::new()).unwrap();
    assert!(cursor.next().is_some());
    assert!(matches!(db.compact(), Err(crate::Error::SnapshotOpen)));
    assert_eq!(cursor.count(), 3, "and reads on");

    let sorted = users.cursor(by_age(SortOrder::Desc)).unwrap();
    let compacted = db.compact().unwrap();
    assert!(compacted.pages_after < compacted.pages_before);
    assert_eq!(sorted.count(), 4);
}

#[test]
fn a_sorted_cursor_yields_in_order() {
    let (_dir, _db, users) = users_db();
    let ages: Vec<i64> = users
        .cursor(by_age(SortOrder::Desc))
        .unwrap()
        .map(|result| result.unwrap().age)
        .collect();
    assert_eq!(ages, [72, 45, 41, 36]);
}

#[test]
fn upsert_inserts_updates_or_refuses() {
    let (_dir, _db, users) = users_db();

    let alan = users.find_one_with_id(name_is("Alan")).unwrap().unwrap().0;
    let updated = users.upsert(name_is("Alan"), user("Alan", 42)).unwrap();
    assert_eq!(updated, Upserted::Updated(alan));
    assert_eq!(users.get(&alan).unwrap(), Some(user("Alan", 42)));

    let inserted = users
        .upsert(name_is("Barbara"), user("Barbara", 80))
        .unwrap();
    let Upserted::Inserted(barbara) = inserted else {
        panic!("expected an insert, got {inserted:?}");
    };
    assert_eq!(users.get(&barbara).unwrap(), Some(user("Barbara", 80)));

    let err = users
        .upsert(age_filter(Op::Gt, 40), user("X", 1))
        .unwrap_err();
    assert!(
        matches!(err, crate::Error::MultipleMatches { count: 4, .. }),
        "{err}"
    );
    assert_eq!(
        users.count(Filter::default()).unwrap(),
        5,
        "nothing written"
    );
}

/// Lookup and write are one atomic step: threads racing to upsert the
/// same keys leave exactly one document per key.
#[test]
fn concurrent_upserts_of_one_key_insert_it_once() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<User>("users");
    users.ensure_index("name").unwrap();

    std::thread::scope(|scope| {
        for thread in 0..4 {
            let users = users.clone();
            scope.spawn(move || {
                for key in 0..5 {
                    let name = format!("user {key}");
                    users.upsert(name_is(&name), user(&name, thread)).unwrap();
                }
            });
        }
    });

    assert_eq!(users.count(Filter::default()).unwrap(), 5);
    for key in 0..5 {
        assert_eq!(users.count(name_is(&format!("user {key}"))).unwrap(), 1);
    }
}

/// SPEC §144.2: `upsert_fields` inserts from the filter's `eq`s and
/// the operators, `set_on_insert` included, then updates the one
/// match; a match left as it was isn't written; the index follows;
/// what can't be made, or matches twice, writes nothing.
#[test]
fn upsert_fields_inserts_from_the_filter_then_updates() {
    let (_dir, db) = fresh_db();
    let visits = db.collection::<Document>("visits");
    visits.ensure_index("hits").unwrap();
    let doc = |json: &str| json.parse::<Document>().unwrap();
    let home = Filter::new().eq("page", "/home").eq("at.site", "a");
    let visit = Update::new().inc("hits", 1).set_on_insert("first", 7);

    let Upserted::Inserted(id) = visits.upsert_fields(home.clone(), &visit).unwrap() else {
        panic!("nothing matched yet");
    };
    assert_eq!(
        visits.get(&id).unwrap(),
        Some(with_id(
            doc(r#"{"page":"/home","at":{"site":"a"},"hits":1,"first":7}"#),
            id
        ))
    );
    let again = Update::new().inc("hits", 1).set_on_insert("first", 8);
    assert_eq!(
        visits.upsert_fields(home.clone(), &again).unwrap(),
        Upserted::Updated(id)
    );
    assert_eq!(
        visits.get(&id).unwrap(),
        Some(with_id(
            doc(r#"{"page":"/home","at":{"site":"a"},"hits":2,"first":7}"#),
            id
        )),
        "`set_on_insert` leaves a match as it is"
    );
    assert_eq!(
        visits.count(Filter::new().eq("hits", 2)).unwrap(),
        1,
        "the index follows"
    );

    let named = DocId([9; 16]);
    assert_eq!(
        visits
            .upsert_fields(Filter::new().id(named).eq("page", "/about"), &visit)
            .unwrap(),
        Upserted::Inserted(named),
        "an `eq` on `_id` gives the id"
    );

    let changes = db.subscribe().unwrap();
    let same = Update::new().set("hits", 2);
    assert_eq!(
        visits.upsert_fields(home.clone(), &same).unwrap(),
        Upserted::Updated(id)
    );
    assert!(
        changes.try_recv().is_none(),
        "a match left as it was isn't written"
    );

    let before = visits.find(Filter::new()).unwrap();
    let refused = |result: crate::Result<Upserted>| match result {
        Err(crate::Error::Io(e)) => {
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{e}")
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    };
    refused(visits.upsert_fields(Filter::new().eq("page", "a").eq("page", "b"), &visit));
    refused(visits.upsert_fields(Filter::new().eq("at", 1).eq("at.site", "a"), &visit));
    refused(visits.upsert_fields(home.clone(), &Update::new().set("_id", 1)));
    let err = visits
        .upsert_fields(Filter::new().exists("page"), &visit)
        .unwrap_err();
    assert!(
        matches!(err, crate::Error::MultipleMatches { count: 2, .. }),
        "{err}"
    );
    let err = visits
        .upsert_fields(home.clone(), &Update::new().push("page", 1))
        .unwrap_err();
    assert!(
        matches!(&err, crate::Error::Update { id: at, .. } if *at == id),
        "{err}"
    );
    let err = visits
        .upsert_fields(Filter::new().eq("page", 5), &Update::new().set("page.x", 1))
        .unwrap_err();
    assert!(
        matches!(err, crate::Error::Update { .. }),
        "an insert's operators fail too: {err}"
    );
    assert_eq!(
        visits.find(Filter::new()).unwrap(),
        before,
        "nothing written"
    );
    assert!(changes.try_recv().is_none());
}

/// SPEC §144.2: the typed document written must convert to `T`; and
/// the lookup and the write are one step, so racing threads count
/// every visit into one document.
#[test]
fn typed_upsert_fields_converts_and_counts_once() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Visit {
        page: String,
        hits: i64,
        first: i64,
    }
    let (_dir, db) = fresh_db();
    let visits = db.collection::<Visit>("visits");
    visits.ensure_index("page").unwrap();
    let visit = Update::new().inc("hits", 1).set_on_insert("first", 1);

    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for n in 0..25 {
                    let page = format!("/{}", n % 2);
                    visits
                        .upsert_fields(Filter::new().eq("page", page), &visit)
                        .unwrap();
                }
            });
        }
    });
    let mut found = visits.find(Filter::new()).unwrap();
    found.sort_by(|a, b| a.page.cmp(&b.page));
    let visit_of = |page: &str, hits| Visit {
        page: page.into(),
        hits,
        first: 1,
    };
    assert_eq!(found, [visit_of("/0", 52), visit_of("/1", 48)]);

    let err = visits
        .upsert_fields(
            Filter::new().eq("page", "/2"),
            &Update::new().inc("hits", 1),
        )
        .unwrap_err();
    assert!(
        matches!(err, crate::Error::Document(_)),
        "no `first`: {err}"
    );
    let err = visits
        .upsert_fields(
            Filter::new().eq("page", "/0"),
            &Update::new().set("hits", "many"),
        )
        .unwrap_err();
    assert!(matches!(err, crate::Error::Document(_)), "{err}");
    assert_eq!(visits.count(Filter::new()).unwrap(), 2);
    assert_eq!(visits.count(Filter::new().eq("hits", 52)).unwrap(), 1);
}
