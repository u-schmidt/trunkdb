use super::*;

// --- delete_many (SPEC §37) ---

#[test]
fn delete_many_deletes_what_find_would_return() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<User>("users");
    let mut batch = db.batch();
    for age in 0..100 {
        batch
            .insert(&users, user(&format!("user {age}"), age))
            .unwrap();
    }
    batch.commit().unwrap();
    users.ensure_index("age").unwrap();
    users
        .ensure_index_with("name", crate::IndexOptions::new().unique())
        .unwrap();
    let ages = || -> Vec<i64> {
        let mut ages: Vec<i64> = users
            .find(Filter::new())
            .unwrap()
            .iter()
            .map(|u| u.age)
            .collect();
        ages.sort();
        ages
    };

    assert_eq!(users.delete_many(Filter::new().lt("age", 18)).unwrap(), 18);
    assert_eq!(ages().first(), Some(&18));
    // Sort and limit count: the five youngest left.
    assert_eq!(
        users
            .delete_many(Filter::new().sort_asc("age").limit(5))
            .unwrap(),
        5
    );
    assert_eq!(ages().first(), Some(&23));
    let either = Filter::new().any_of([Condition::eq("name", "user 50"), Condition::eq("age", 60)]);
    assert_eq!(users.delete_many(either).unwrap(), 2);
    assert_eq!(users.delete_many(Filter::new().eq("age", 500)).unwrap(), 0);
    assert_eq!(ages().len(), 75);
    // Indexes lost their entries: a deleted unique value is free again,
    // and a query through the age index finds nothing deleted.
    users.insert(user("user 50", 50)).unwrap();
    assert_eq!(users.count(Filter::new().lt("age", 23)).unwrap(), 0);

    // Nothing to delete in a collection that isn't there, and it
    // isn't created.
    let nobody = db.collection::<User>("nobody");
    assert_eq!(nobody.delete_many(Filter::new()).unwrap(), 0);
    assert_eq!(db.collections().unwrap(), ["users"]);
}

/// Random nested filters, sometimes sorted and limited: each
/// `delete_many` removes exactly what `find` returned just before,
/// and the indexes still agree with a scan afterwards.
#[test]
fn delete_many_matches_find_through_random_filters() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index("v").unwrap();
    docs.ensure_index("w").unwrap();
    let mut rng = XorShift(0x7777_1234_ABCD_0001);
    let mut deleted_any = 0;
    for _ in 0..40 {
        let ops = (0..20)
            .map(|_| {
                WriteOp::Insert(
                    "docs".into(),
                    db.id_gen().generate(),
                    random_document(&mut rng),
                )
            })
            .collect();
        db.write_batch(ops).unwrap();

        let mut f = Filter::new().and(random_condition(&mut rng, 2));
        if rng.below(2) == 0 {
            f = f
                .sort_by("v", [SortOrder::Asc, SortOrder::Desc][rng.below(2)])
                .limit(rng.below(8));
        }
        let before = sorted_ids(docs.find_with_ids(Filter::new()).unwrap());
        let doomed = sorted_ids(docs.find_with_ids(f.clone()).unwrap());
        assert_eq!(docs.delete_many(f.clone()).unwrap(), doomed.len(), "{f:?}");
        let after = sorted_ids(docs.find_with_ids(Filter::new()).unwrap());
        let expected: Vec<DocId> = before
            .into_iter()
            .filter(|id| !doomed.contains(id))
            .collect();
        assert_eq!(after, expected, "{f:?}");
        deleted_any += doomed.len();
    }
    assert!(deleted_any > 100, "only {deleted_any} deleted");
    assert_index_agrees_with_scan(&docs, &mut rng, "v");
    assert_nested_filters_agree_with_a_scan(&docs, &mut rng);
}

// --- update_many (SPEC §38) ---

#[test]
fn update_many_changes_what_find_would_return() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Task {
        name: String,
        status: String,
        rank: i64,
    }
    let (_dir, db) = fresh_db();
    let tasks = db.collection::<Task>("tasks");
    let mut batch = db.batch();
    for rank in 0..60 {
        let status = if rank < 40 { "Queued" } else { "Done" }.to_string();
        let name = format!("task {rank}");
        batch.insert(&tasks, Task { name, status, rank }).unwrap();
    }
    batch.commit().unwrap();
    tasks.ensure_index("status").unwrap();
    tasks
        .ensure_index_with("name", crate::IndexOptions::new().unique())
        .unwrap();
    let count = |status: &str| tasks.count(Filter::new().eq("status", status)).unwrap();

    // Sort and limit count, and `change` sees the matches in order.
    let mut seen = Vec::new();
    let first_five = Filter::new()
        .eq("status", "Queued")
        .sort_asc("rank")
        .limit(5);
    let n = tasks
        .update_many(first_five, |task| {
            seen.push(task.rank);
            task.status = "Running".to_string();
        })
        .unwrap();
    assert_eq!((n, seen), (5, vec![0, 1, 2, 3, 4]));
    // The index moved with them.
    let running = Filter::new().eq("status", "Running");
    assert_eq!(
        tasks.explain(&running).unwrap(),
        QueryPlan::Index {
            field: "status".to_string()
        }
    );
    assert_eq!((count("Running"), count("Queued")), (5, 35));

    // Matches left as they were aren't written, nor counted.
    let done = Filter::new().eq("status", "Done");
    assert_eq!(
        tasks
            .update_many(done, |task| task.status = "Done".to_string())
            .unwrap(),
        0
    );

    // One change a unique index refuses rolls back all of them.
    let queued = || Filter::new().eq("status", "Queued");
    let all_same = tasks.update_many(queued(), |task| {
        task.name = "same".to_string();
        task.status = "Renamed".to_string();
    });
    assert!(
        matches!(all_same, Err(crate::Error::DuplicateValue { .. })),
        "{all_same:?}"
    );
    assert_eq!((count("Queued"), count("Renamed")), (35, 0));

    // An `_id` stays what it was, whatever `change` does to it.
    let docs = db.collection::<Document>("tasks");
    let (id, _) = tasks
        .find_one_with_id(Filter::new().eq("rank", 7))
        .unwrap()
        .unwrap();
    let n = docs
        .update_many(Filter::new().eq("rank", 7), |doc| {
            if let Document::Object(fields) = doc {
                fields.insert("_id".to_string(), Document::Id(DocId([0; 16])));
                fields.insert("rank".to_string(), Document::Int(700));
            }
        })
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(tasks.get(&id).unwrap().map(|task| task.rank), Some(700));

    // A match that doesn't convert to `T` fails the whole batch.
    let odd = [
        ("name", Document::String("odd".into())),
        ("status", Document::String("Queued".into())),
    ];
    docs.insert(object(odd.to_vec())).unwrap(); // no `rank`
    let bumped = tasks.update_many(queued(), |task| task.rank += 1000);
    assert!(
        matches!(bumped, Err(crate::Error::Document(_))),
        "{bumped:?}"
    );
    assert_eq!(tasks.count(Filter::new().gte("rank", 1000)).unwrap(), 0);
}

/// Random nested filters, sometimes sorted and limited, and a change
/// that gives `v` a random new value, or leaves the document alone:
/// the count and every document afterwards must match a model, and
/// the indexes must still agree with a scan.
#[test]
fn update_many_matches_a_model_through_random_filters() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index("v").unwrap();
    docs.ensure_index("w").unwrap();
    let mut rng = XorShift(0x0DDB_A11C_0FFE_E123);
    // Compared as text: NaN isn't equal to itself.
    let text = |all: Vec<(DocId, Document)>| {
        let mut all: Vec<_> = all
            .into_iter()
            .map(|(id, doc)| (id, format!("{doc:?}")))
            .collect();
        all.sort();
        all
    };
    let mut changed_any = 0;
    for _ in 0..30 {
        let ops = (0..15)
            .map(|_| {
                WriteOp::Insert(
                    "docs".into(),
                    db.id_gen().generate(),
                    random_document(&mut rng),
                )
            })
            .collect();
        db.write_batch(ops).unwrap();

        let mut f = Filter::new().and(random_condition(&mut rng, 2));
        if rng.below(2) == 0 {
            f = f
                .sort_by("v", [SortOrder::Asc, SortOrder::Desc][rng.below(2)])
                .limit(rng.below(8));
        }
        let mut model = docs.find_with_ids(Filter::new()).unwrap();
        let matched = docs.find_with_ids(f.clone()).unwrap();
        // What the change will do to each match, decided up front.
        let new_values: Vec<Option<Document>> = matched
            .iter()
            .map(|_| (rng.below(4) != 0).then(|| random_value(&mut rng)))
            .collect();
        let mut expected_changes = 0;
        for ((id, old), new_v) in matched.iter().zip(&new_values) {
            let Some(new_v) = new_v else { continue };
            let mut new = old.clone();
            if let Document::Object(fields) = &mut new {
                fields.insert("v".to_string(), new_v.clone());
            }
            if encode_document(&new) != encode_document(old) {
                expected_changes += 1;
                let slot = model.iter_mut().find(|(other, _)| other == id).unwrap();
                slot.1 = new;
            }
        }

        let mut next = new_values.into_iter();
        let n = docs
            .update_many(f.clone(), |doc| {
                if let (Some(new_v), Document::Object(fields)) = (next.next().unwrap(), doc) {
                    fields.insert("v".to_string(), new_v);
                }
            })
            .unwrap();
        assert_eq!(n, expected_changes, "{f:?}");
        assert_eq!(
            text(docs.find_with_ids(Filter::new()).unwrap()),
            text(model),
            "{f:?}"
        );
        changed_any += n;
    }
    assert!(changed_any > 50, "only {changed_any} changed");
    assert_index_agrees_with_scan(&docs, &mut rng, "v");
    assert_nested_filters_agree_with_a_scan(&docs, &mut rng);
}

// --- Update operators (SPEC §68) ---

/// `update_fields` on typed tasks: what `find` would return changes,
/// sort and limit included; unchanged matches aren't counted; the
/// indexes follow; a change `T` can't hold, an operator that can't be
/// made, and an update refused as written each change nothing.
#[test]
fn update_fields_changes_what_find_would_return() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Task {
        #[serde(rename = "_id")]
        id: Option<DocId>,
        status: String,
        rank: i64,
        tries: i64,
        error: Option<String>,
    }
    let (_dir, db) = fresh_db();
    let tasks = db.collection::<Task>("tasks");
    let mut batch = db.batch();
    for rank in 0..30 {
        let status = if rank < 20 { "Queued" } else { "Done" }.to_string();
        let error = (rank % 2 == 0).then(|| "timeout".to_string());
        let task = Task {
            id: None,
            status,
            rank,
            tries: 0,
            error,
        };
        batch.insert(&tasks, task).unwrap();
    }
    batch.commit().unwrap();
    tasks.ensure_index("status").unwrap();
    tasks.ensure_index("tries").unwrap();
    let count = |f: Filter| tasks.count(f).unwrap();

    let start = Update::new()
        .set("status", "Running")
        .inc("tries", 1)
        .unset("error");
    let first_five = Filter::new()
        .eq("status", "Queued")
        .sort_asc("rank")
        .limit(5);
    assert_eq!(tasks.update_fields(first_five, &start).unwrap(), 5);
    let running = tasks
        .find(Filter::new().eq("status", "Running").sort_asc("rank"))
        .unwrap();
    let ranks: Vec<i64> = running.iter().map(|t| t.rank).collect();
    assert_eq!(ranks, [0, 1, 2, 3, 4]);
    assert!(running.iter().all(|t| t.tries == 1 && t.error.is_none()));
    assert_eq!(
        count(Filter::new().eq("tries", 1)),
        5,
        "the index on tries follows"
    );
    assert_eq!(count(Filter::new().eq("status", "Queued")), 15);

    // Matches left as they were aren't written or counted.
    let same = Update::new().set("status", "Done");
    assert_eq!(
        tasks
            .update_fields(Filter::new().gte("rank", 20), &same)
            .unwrap(),
        0
    );

    // A change `T` can't hold fails the batch: none of it lands.
    let wrong_type = Update::new().set("tries", "many");
    let Err(crate::Error::Document(_)) = tasks.update_fields(Filter::new(), &wrong_type) else {
        panic!("a string in an i64 field must fail");
    };
    // So does an operator that can't be made. Untyped, so nothing but
    // the operator can fail: ranks 0 to 4 have no `error` since the
    // `unset`, and `inc` starts it; rank 5's is null, and fails.
    let untyped = db.collection::<Document>("tasks");
    let bad = Update::new().inc("tries", 1).inc("error", 1);
    let by_rank = Filter::new().sort_asc("rank");
    let Err(crate::Error::Update { id, message, .. }) = untyped.update_fields(by_rank, &bad) else {
        panic!("an inc of null must fail");
    };
    let rank_5 = tasks
        .find_one(Filter::new().eq("rank", 5))
        .unwrap()
        .unwrap();
    assert_eq!(Some(id), rank_5.id);
    assert!(
        message.contains("\"error\"") && message.contains("null"),
        "{message}"
    );
    assert_eq!(count(Filter::new().eq("tries", 1)), 5, "rolled back");
    assert_eq!(count(Filter::new().eq("tries", 0)), 25, "rolled back");
    assert_eq!(count(Filter::new().exists("error")), 25, "rolled back");

    // Refused as written, before anything is read.
    for refused in [Update::new().set("_id", 1), Update::new().unset("tags[*]")] {
        let Err(crate::Error::Io(e)) = tasks.update_fields(Filter::new(), &refused) else {
            panic!("{refused:?} must be refused");
        };
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
    }
    let nothing = db.collection::<Document>("nothing");
    assert!(
        nothing
            .update_fields(Filter::new(), &Update::new().inc("n", "x"))
            .is_err()
    );
}

/// Random filters and random updates on the indexed `v` and `w`, some
/// of which can't be made to every match: the count, every document
/// and the indexes afterwards must match a model that applies the
/// update to each match on its own, and a failed batch changes
/// nothing.
#[test]
fn update_fields_matches_a_model_through_random_updates() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index("v").unwrap();
    docs.ensure_index("w").unwrap();
    docs.ensure_index("o.x").unwrap();
    docs.ensure_index("t[*]").unwrap();
    let mut rng = XorShift(0x85EB_CA77_C2B2_AE63);
    let text = |all: Vec<(DocId, Document)>| {
        let mut all: Vec<_> = all
            .into_iter()
            .map(|(id, doc)| (id, format!("{doc:?}")))
            .collect();
        all.sort();
        all
    };
    let (mut changed_any, mut failed_any) = (0, 0);
    for _ in 0..50 {
        let ops = (0..10)
            .map(|_| {
                WriteOp::Insert(
                    "docs".into(),
                    db.id_gen().generate(),
                    random_document(&mut rng),
                )
            })
            .collect();
        db.write_batch(ops).unwrap();

        let mut update = Update::new();
        for _ in 0..1 + rng.below(3) {
            let path = ["v", "w", "o.x"][rng.below(3)];
            let other = ["v", "t", "o.y"][rng.below(3)];
            let number = [Document::Int(1), Document::Float(0.5)][rng.below(2)].clone();
            let element = Document::Int(rng.below(4) as i64);
            update = match rng.below(13) {
                0 => update.set(path, random_value(&mut rng)),
                1 => update.unset(path),
                2 => update.inc(path, number),
                3 => update.min(path, number),
                4 => update.max(path, number),
                5 if path != other => update.rename(path, other),
                6 => update.push("t", element),
                7 => update.add_to_set("t", element),
                8 => update.mul(
                    path,
                    [Document::Int(2), Document::Float(-0.5)][rng.below(2)].clone(),
                ),
                9 => update.push_all("t", (0..rng.below(3)).map(|i| i as i64)),
                10 => update.pop_first("t"),
                11 => update.pop_last("t"),
                _ => update.pull("t", element),
            };
        }
        let mut f = Filter::new().and(random_condition(&mut rng, 2));
        if rng.below(2) == 0 {
            f = f.sort_by("w", SortOrder::Asc).limit(rng.below(8));
        }

        let before = docs.find_with_ids(Filter::new()).unwrap();
        let mut model = before.clone();
        let mut expected = Ok(0);
        for (id, old) in docs.find_with_ids(f.clone()).unwrap() {
            let mut new = old.clone();
            if update.apply(&mut new).is_err() {
                expected = Err(());
                break;
            }
            if encode_document(&new) != encode_document(&old) {
                expected = expected.map(|n| n + 1);
                model.iter_mut().find(|(other, _)| *other == id).unwrap().1 = new;
            }
        }

        let result = docs.update_fields(f.clone(), &update);
        match expected {
            Ok(n) => {
                assert_eq!(result.unwrap(), n, "{f:?} {update:?}");
                assert_eq!(
                    text(docs.find_with_ids(Filter::new()).unwrap()),
                    text(model)
                );
                changed_any += n;
            }
            Err(()) => {
                assert!(
                    matches!(result, Err(crate::Error::Update { .. })),
                    "{result:?}"
                );
                assert_eq!(
                    text(docs.find_with_ids(Filter::new()).unwrap()),
                    text(before)
                );
                failed_any += 1;
            }
        }
    }
    assert!(
        changed_any > 30 && failed_any > 3,
        "{changed_any} changed, {failed_any} failed"
    );
    for path in ["v", "w", "o.x", "t[*]"] {
        assert_index_agrees_with_scan(&docs, &mut rng, path);
    }
}

/// SPEC §142 on typed workers: jobs queued with `push_all`, taken
/// with `pop_first` and dropped with `pop_last`, found through a
/// multikey index on them; a rate multiplied, through an index on it.
#[test]
fn update_fields_queues_and_multiplies() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Worker {
        #[serde(rename = "_id")]
        id: Option<DocId>,
        jobs: Vec<String>,
        rate: f64,
    }
    let (_dir, db) = fresh_db();
    let workers = db.collection::<Worker>("workers");
    workers.ensure_index("jobs[*]").unwrap();
    workers.ensure_index("rate").unwrap();
    let ids: Vec<DocId> = (0..4)
        .map(|n| {
            let worker = Worker {
                id: None,
                jobs: Vec::new(),
                rate: n as f64,
            };
            workers.insert(worker).unwrap()
        })
        .collect();
    let jobs = |worker: DocId| workers.get(&worker).unwrap().unwrap().jobs;
    let holding = |job: &str| workers.count(Filter::new().eq("jobs[*]", job)).unwrap();

    let queued = Update::new().push_all("jobs", ["a", "b", "c"]);
    assert_eq!(workers.update_fields(Filter::new(), &queued).unwrap(), 4);
    assert_eq!(jobs(ids[0]), ["a", "b", "c"]);
    let take = Update::new().pop_first("jobs");
    assert_eq!(
        workers
            .update_fields(Filter::new().id(ids[0]), &take)
            .unwrap(),
        1
    );
    assert_eq!(jobs(ids[0]), ["b", "c"]);
    assert_eq!((holding("a"), holding("b")), (3, 4), "the index follows");
    let drop_last = Update::new().pop_last("jobs").pop_last("jobs");
    assert_eq!(
        workers
            .update_fields(Filter::new().gte("rate", 2), &drop_last)
            .unwrap(),
        2
    );
    assert_eq!(jobs(ids[3]), ["a"]);
    assert_eq!((holding("b"), holding("c")), (2, 2));
    let emptied = Update::new()
        .pop_first("jobs")
        .pop_first("jobs")
        .pop_first("jobs");
    assert_eq!(workers.update_fields(Filter::new(), &emptied).unwrap(), 4);
    assert_eq!(
        workers.update_fields(Filter::new(), &emptied).unwrap(),
        0,
        "popping an empty array changes nothing"
    );
    assert_eq!(holding("a") + holding("b") + holding("c"), 0);

    let doubled = Update::new().mul("rate", 2);
    assert_eq!(
        workers
            .update_fields(Filter::new().gt("rate", 0), &doubled)
            .unwrap(),
        3
    );
    let rates: Vec<f64> = workers
        .find(Filter::new().sort_asc("rate"))
        .unwrap()
        .iter()
        .map(|w| w.rate)
        .collect();
    assert_eq!(rates, [0.0, 2.0, 4.0, 6.0]);
    assert_eq!(
        workers.count(Filter::new().gte("rate", 4)).unwrap(),
        2,
        "the index on rate follows"
    );

    // A `pop_first` on a string fails the batch, nothing written.
    let untyped = db.collection::<Document>("workers");
    untyped
        .update_fields(Filter::new().id(ids[1]), &Update::new().set("jobs", "x"))
        .unwrap();
    let failed = untyped.update_fields(
        Filter::new(),
        &Update::new().mul("rate", 3).pop_first("jobs"),
    );
    assert!(
        matches!(failed, Err(crate::Error::Update { .. })),
        "{failed:?}"
    );
    assert_eq!(workers.count(Filter::new().eq("rate", 6)).unwrap(), 1);
}

/// SPEC §73 on typed devices: tags as a set, the latest time seen, a
/// best score, and a renamed field, found through their indexes.
#[test]
fn update_fields_changes_arrays_and_keeps_bounds() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Device {
        #[serde(rename = "_id")]
        id: Option<DocId>,
        tags: Vec<String>,
        last_seen: Option<crate::DateTime>,
        best: i64,
        label: Option<String>,
    }
    let (_dir, db) = fresh_db();
    let devices = db.collection::<Device>("devices");
    devices.ensure_index("tags[*]").unwrap();
    devices.ensure_index("last_seen").unwrap();
    let at = |secs| crate::DateTime::from_unix(secs, 0).unwrap();
    for n in 0..10 {
        let device = Device {
            id: None,
            tags: vec!["new".to_string()],
            last_seen: (n % 2 == 0).then(|| at(1_000 + n)),
            best: n,
            label: None,
        };
        devices.insert(device).unwrap();
    }

    let seen = Update::new()
        .add_to_set("tags", "online")
        .pull("tags", "new")
        .max("last_seen", at(1_005))
        .max("best", 5);
    assert_eq!(devices.update_fields(Filter::new(), &seen).unwrap(), 10);
    assert_eq!(
        devices.update_fields(Filter::new(), &seen).unwrap(),
        0,
        "a set, a bound"
    );
    let all = devices.find(Filter::new().sort_asc("best")).unwrap();
    assert!(all.iter().all(|d| d.tags == ["online"]));
    let seen_at: Vec<i64> = all
        .iter()
        .map(|d| d.last_seen.unwrap().unix_seconds())
        .collect();
    assert_eq!(
        seen_at,
        [1005, 1005, 1005, 1005, 1005, 1005, 1006, 1005, 1008, 1005]
    );
    let best: Vec<i64> = all.iter().map(|d| d.best).collect();
    assert_eq!(best, [5, 5, 5, 5, 5, 5, 6, 7, 8, 9]);
    assert_eq!(
        devices.count(Filter::new().eq("tags[*]", "new")).unwrap(),
        0
    );
    assert_eq!(
        devices
            .count(Filter::new().eq("tags[*]", "online"))
            .unwrap(),
        10
    );
    let later = Filter::new().gt("last_seen", at(1_005));
    assert_eq!(
        devices.count(later).unwrap(),
        2,
        "the index on last_seen follows"
    );

    // A rename, untyped, into a field the struct has.
    let untyped = db.collection::<Document>("devices");
    let named = Update::new().set("name", "sensor").rename("name", "label");
    assert_eq!(
        untyped
            .update_fields(Filter::new().gte("best", 8), &named)
            .unwrap(),
        2
    );
    let labeled = devices.find(Filter::new().eq("label", "sensor")).unwrap();
    assert_eq!(labeled.iter().map(|d| d.best).collect::<Vec<_>>(), [8, 9]);

    // A push into a field `T` holds as a string fails the batch.
    let wrong = Update::new().push("label", "x");
    let Err(crate::Error::Update { message, .. }) =
        devices.update_fields(Filter::new().gte("best", 8), &wrong)
    else {
        panic!("a push onto a string must fail");
    };
    assert!(
        message.contains("`push` needs an array there, not a string"),
        "{message}"
    );
    assert_consistent(&db);
}
