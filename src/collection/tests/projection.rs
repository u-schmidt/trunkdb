use super::*;

// --- Projection (SPEC §106) ---

/// SPEC §106.2: what a selection hands out, through `find`, `cursor`,
/// `find_one`, a `View` and the typed path.
/// SPEC §143: `except` on every read: the rest in its stored order,
/// nested, alone and after a `select`, typed, from index entries where
/// a selection is answered there, live; counts and writes ignore it.
#[test]
fn an_except_hands_out_the_rest_on_every_read() {
    let (_dir, db) = fresh_db();
    let notes = db.collection::<Document>("notes");
    notes.ensure_index(["team", "title"]).unwrap();
    for json in [
        r#"{"title":"Plan","team":"core","body":"long","meta":{"raw":"x","by":"Ada"}}"#,
        r#"{"title":"Notes","team":"core","meta":"none"}"#,
        r#"{"title":"Web","team":"web","body":"text","meta":{"raw":"y"}}"#,
    ] {
        notes.insert(json.parse().unwrap()).unwrap();
    }
    let texts = |found: Vec<Document>| -> Vec<String> {
        (found.into_iter())
            .map(|doc| {
                let Document::Object(mut map) = doc else {
                    panic!("{doc}")
                };
                assert!(matches!(map.iter().next(), Some((key, Document::Id(_))) if key == "_id"));
                map["_id"] = Document::String("ID".into());
                Document::Object(map).to_string()
            })
            .collect()
    };
    let lean = || Filter::new().sort_asc("title").except(["body", "meta.raw"]);
    let expected = [
        r#"{"_id":"ID","title":"Notes","team":"core","meta":"none"}"#,
        r#"{"_id":"ID","title":"Plan","team":"core","meta":{"by":"Ada"}}"#,
        r#"{"_id":"ID","title":"Web","team":"web","meta":{}}"#,
    ];
    assert_eq!(texts(notes.find(lean()).unwrap()), expected);
    let streamed: Vec<Document> = notes.cursor(lean()).unwrap().map(Result::unwrap).collect();
    assert_eq!(texts(streamed), expected);
    let unsorted = || Filter::new().eq("team", "core").except(["body", "meta"]);
    let streamed: Vec<Document> = notes
        .cursor(unsorted())
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut streamed = texts(streamed);
    streamed.sort();
    assert_eq!(
        streamed,
        [
            r#"{"_id":"ID","title":"Notes","team":"core"}"#,
            r#"{"_id":"ID","title":"Plan","team":"core"}"#
        ]
    );
    let view = db.snapshot().unwrap().collection::<Document>("notes");
    assert_eq!(texts(view.find(lean()).unwrap()), expected);
    assert_eq!(
        texts(notes.find_one(lean()).unwrap().into_iter().collect()),
        expected[..1]
    );
    assert_eq!(
        texts(notes.find(lean().except(["meta", "title"])).unwrap())[0],
        r#"{"_id":"ID","team":"core"}"#
    );
    assert_eq!(
        texts(notes.find(lean().except([""])).unwrap()),
        [r#"{"_id":"ID"}"#; 3]
    );

    // After a select, and answered from entries where the select is.
    let pairs = || {
        Filter::new()
            .eq("team", "core")
            .sort_asc("title")
            .select(["team", "title"])
    };
    let run = notes.explain_run(pairs().except(["team"])).unwrap();
    assert_eq!(
        (run.stats.from_entries, run.stats.documents_examined),
        (2, 0)
    );
    assert_eq!(run.except, ["team"]);
    assert_eq!(
        texts(notes.find(pairs().except(["team"])).unwrap()),
        [
            r#"{"_id":"ID","title":"Notes"}"#,
            r#"{"_id":"ID","title":"Plan"}"#
        ]
    );
    let run = notes
        .explain_run(Filter::new().eq("team", "core").except(["body"]))
        .unwrap();
    assert_eq!(
        (run.stats.from_entries, run.stats.documents_examined),
        (0, 2),
        "no entry holds every field"
    );
    assert!(notes.explain_run(Filter::new()).unwrap().except.is_empty());

    // Typed: a struct without the large field reads past it; one
    // that needs it is told it's missing.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Note {
        title: String,
        body: Option<String>,
    }
    let typed = db.collection::<Note>("notes");
    let found = typed
        .find(Filter::new().eq("title", "Plan").except(["body"]))
        .unwrap();
    assert_eq!(
        found,
        [Note {
            title: "Plan".into(),
            body: None
        }]
    );
    let missing = typed.find(Filter::new().eq("title", "Plan").except(["title"]));
    assert!(
        matches!(missing, Err(crate::Error::Document(_))),
        "{missing:?}"
    );

    // Live: what's handed out is cut, at the start and after.
    let live = notes
        .watch(Filter::new().eq("team", "web").except(["body", "meta"]))
        .unwrap();
    notes
        .update_fields(
            Filter::new().eq("team", "web"),
            &Update::new().set("body", "more"),
        )
        .unwrap();
    let docs: Vec<String> = (0..2)
        .map(|_| match live.recv().unwrap().unwrap() {
            crate::Live::Entered { doc, .. } | crate::Live::Changed { doc, .. } => {
                texts(vec![doc]).remove(0)
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(docs, [r#"{"_id":"ID","title":"Web","team":"web"}"#; 2]);

    // Counts and writes see whole documents.
    assert_eq!(
        notes
            .count(Filter::new().except(["team"]).eq("team", "core"))
            .unwrap(),
        2
    );
    notes
        .update_many(Filter::new().eq("title", "Plan").except(["body"]), |doc| {
            assert_eq!(doc.get("body").and_then(Document::as_str), Some("long"));
        })
        .unwrap();
    assert_eq!(
        notes
            .delete_many(Filter::new().except(["title"]).eq("team", "web"))
            .unwrap(),
        1
    );
}

#[test]
fn a_selection_hands_out_what_it_names_on_every_read() {
    let (_dir, db) = fresh_db();
    let users = db.collection::<Document>("users");
    for json in [
        r#"{"name":"Ada","team":"core","address":{"zip":"N1","city":"London"},"n":null}"#,
        r#"{"name":"Bo","team":"core","address":"somewhere","tags":[{"x":1}]}"#,
        r#"{"team":"web","address":{"zip":"10115"}}"#,
    ] {
        users.insert(json.parse().unwrap()).unwrap();
    }
    // Each document as text, its id (checked to be one) as `"ID"`.
    let texts = |found: Vec<Document>| -> Vec<String> {
        (found.into_iter())
            .map(|doc| {
                let Document::Object(mut map) = doc else {
                    panic!("{doc}")
                };
                assert!(matches!(map.iter().next(), Some((key, Document::Id(_))) if key == "_id"));
                map["_id"] = Document::String("ID".into());
                Document::Object(map).to_string()
            })
            .collect()
    };
    let by_name = || Filter::new().sort_asc("name");
    // `_id` first, then the selection's order, nested as stored; a
    // missing field left out, a stored null kept; a path that stops
    // early selects nothing.
    assert_eq!(
        texts(
            users
                .find(by_name().select(["address.city", "name", "n"]))
                .unwrap()
        ),
        [
            r#"{"_id":"ID"}"#,
            r#"{"_id":"ID","address":{"city":"London"},"name":"Ada","n":null}"#,
            r#"{"_id":"ID","name":"Bo"}"#,
        ]
    );
    // A second `select` adds; `[*]` selects the array whole.
    let more = by_name().select(["name"]).select(["tags[*].x"]);
    assert_eq!(
        texts(users.find(more.clone()).unwrap()),
        [
            r#"{"_id":"ID"}"#,
            r#"{"_id":"ID","name":"Ada"}"#,
            r#"{"_id":"ID","name":"Bo","tags":[{"x":1}]}"#,
        ]
    );
    let streamed: Vec<Document> = users
        .cursor(more.clone())
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(texts(streamed), texts(users.find(more.clone()).unwrap()));
    let view = db.snapshot().unwrap().collection::<Document>("users");
    assert_eq!(
        texts(view.find(more.clone()).unwrap()),
        texts(users.find(more.clone()).unwrap())
    );
    assert_eq!(
        texts(users.find_one(more.clone()).unwrap().into_iter().collect()),
        [r#"{"_id":"ID"}"#]
    );
    // Unsorted, through a streaming cursor; `_id` alone.
    let core = || Filter::new().eq("team", "core");
    let streamed: Vec<Document> = (users.cursor(core().select([] as [&str; 0])).unwrap())
        .map(Result::unwrap)
        .collect();
    assert_eq!(texts(streamed), [r#"{"_id":"ID"}"#; 2]);
    // Conditions and sort keys see the whole document.
    assert_eq!(
        texts(
            users
                .find(core().sort_desc("name").select(["address"]))
                .unwrap()
        ),
        [
            r#"{"_id":"ID","address":"somewhere"}"#,
            r#"{"_id":"ID","address":{"zip":"N1","city":"London"}}"#,
        ]
    );

    // Typed: a field of `T` outside the selection is missing to it.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Summary {
        name: Option<String>,
        team: Option<String>,
    }
    let typed = db.collection::<Summary>("users");
    let summary = |name: &str| Summary {
        name: Some(name.to_string()),
        team: None,
    };
    let named = || by_name().gt("name", "A").select(["name"]);
    assert_eq!(
        typed.find(named()).unwrap(),
        [summary("Ada"), summary("Bo")]
    );
    let streamed: Vec<Summary> = typed.cursor(named()).unwrap().map(Result::unwrap).collect();
    assert_eq!(streamed, [summary("Ada"), summary("Bo")]);
    #[derive(Debug, Serialize, Deserialize)]
    struct Strict {
        name: String,
        team: String,
    }
    let strict = db.collection::<Strict>("users");
    assert_eq!(strict.find(named().select(["team"])).unwrap().len(), 2);
    let err = strict.find(named()).unwrap_err();
    assert!(matches!(err, crate::Error::Document(_)), "{err}");
    assert!(err.to_string().contains("missing field `team`"), "{err}");
}

/// SPEC §106.5: a match is handed out from its index entry where
/// every selected value is held as stored — a bool, a string that
/// fits the key, an id, a date-time — and read from its document
/// otherwise: a number, which the key holds as a float, a null,
/// which a missing field is keyed as too, a cut string.
#[test]
fn results_from_entries_hand_out_values_as_stored() {
    let (_dir, db) = fresh_db();
    let long = "x".repeat(2000);
    let kinds: [(&str, Document, bool); 10] = [
        ("bool", Document::Bool(true), true),
        ("string", Document::String("abc".into()), true),
        ("cut", Document::String(long.clone()), false),
        ("int", Document::Int(5), false),
        ("float", Document::Float(5.0), false),
        ("negative_zero", Document::Float(-0.0), false),
        ("huge", Document::Int(1 << 60), false),
        ("id", Document::Id(DocId([9; 16])), true),
        (
            "date",
            Document::DateTime(crate::DateTime::from_unix(1_700_000_000, 5).unwrap()),
            true,
        ),
        ("null", Document::Null, false),
    ];
    for (kind, value, answered) in kinds {
        let docs = db.collection::<Document>(kind);
        docs.ensure_index("v").unwrap();
        for w in 0..3 {
            docs.insert(object(vec![("v", value.clone()), ("w", Document::Int(w))]))
                .unwrap();
        }
        docs.insert(object(vec![("w", Document::Int(9))])).unwrap(); // `v` missing
        let filters = [
            Filter::new().eq("v", value.clone()),
            Filter::new()
                .eq("v", value.clone())
                .sort_asc("_id")
                .limit(10),
            Filter::new().sort_asc("v").limit(10),
            Filter::new().eq("v", value.clone()).skip(1),
        ];
        for f in filters {
            let s = f.clone().select(["v"]);
            let selection = crate::select::Selection::new(["v"]);
            let expected: Vec<String> = (docs.find(f.clone()).unwrap().into_iter())
                .map(|doc| selection.project(doc).to_string())
                .collect();
            let found: Vec<String> = docs
                .find(s.clone())
                .unwrap()
                .iter()
                .map(Document::to_string)
                .collect();
            assert_eq!(found, expected, "{kind}: {s:?}");
            let streamed: Vec<String> = (docs.cursor(s.clone()).unwrap())
                .map(|doc| doc.unwrap().to_string())
                .collect();
            assert_eq!(streamed, expected, "{kind}: cursor {s:?}");
            // Each result with `v` came from its entry, or none did.
            let with_v = expected
                .iter()
                .filter(|doc| doc.contains(r#""v":"#))
                .count() as u64;
            let run = docs.explain_run(s.clone()).unwrap();
            let from_entries = if answered { with_v } else { 0 };
            assert_eq!(
                run.stats.from_entries, from_entries,
                "{kind}: {s:?}: {run:?}"
            );
        }
    }
    // A missing field stays missing, a null null: neither from a key.
    let docs = db.collection::<Document>("null");
    let found: Vec<String> = (docs.find(
        Filter::new()
            .eq("v", Document::Null)
            .sort_asc("w")
            .select(["v"]),
    ))
    .unwrap()
    .iter()
    .map(|doc| {
        doc.to_string()
            .split_once(',')
            .map_or("{}".into(), |(_, rest)| rest.to_string())
    })
    .collect();
    assert_eq!(
        found,
        [r#""v":null}"#; 3]
            .map(String::from)
            .into_iter()
            .chain(["{}".into()])
            .collect::<Vec<_>>()
    );
}

/// SPEC §106.5: the page a list shows, from a compound index's
/// entries alone: `(status, created)` answers "the newest queued,
/// their status and creation", and `_id` alone a whole scan, without
/// a document read.
#[test]
fn a_page_of_a_compound_index_is_answered_without_a_document() {
    let (_dir, db) = fresh_db();
    let tasks = db.collection::<Document>("tasks");
    tasks.ensure_index(["status", "created"]).unwrap();
    let mut batch = db.batch();
    for n in 0..500i64 {
        let created = crate::DateTime::from_unix(1_700_000_000 + (n * 7919) % 500, 0).unwrap();
        let task = object(vec![
            (
                "status",
                Document::String(["Queued", "Done"][n as usize % 2].into()),
            ),
            ("created", Document::DateTime(created)),
            ("tries", Document::Int(n % 7)),
        ]);
        batch.insert(&tasks, task).unwrap();
    }
    batch.commit().unwrap();
    let newest = || {
        Filter::new()
            .eq("status", "Queued")
            .sort_desc("created")
            .skip(40)
            .limit(20)
    };
    let page = newest().select(["status", "created"]);
    let run = tasks.explain_run(page.clone()).unwrap();
    assert_eq!(run.stats.documents_returned, 20);
    assert_eq!(
        (run.stats.from_entries, run.stats.documents_examined),
        (20, 0),
        "{run:?}"
    );
    let selection = crate::select::Selection::new(["status", "created"]);
    let expected: Vec<String> = (tasks.find(newest()).unwrap().into_iter())
        .map(|doc| selection.project(doc).to_string())
        .collect();
    let found: Vec<String> = tasks
        .find(page.clone())
        .unwrap()
        .iter()
        .map(Document::to_string)
        .collect();
    assert_eq!(found, expected);
    // A number selected too: read from the documents.
    let run = tasks.explain_run(page.select(["tries"])).unwrap();
    assert_eq!(
        (run.stats.from_entries, run.stats.documents_examined),
        (0, 20),
        "{run:?}"
    );
    // Every id, from the primary index alone.
    let ids = Filter::new().select([] as [&str; 0]);
    let run = tasks.explain_run(ids.clone()).unwrap();
    assert_eq!(
        (run.stats.from_entries, run.stats.documents_examined),
        (500, 0),
        "{run:?}"
    );
    assert_eq!(tasks.find(ids).unwrap().len(), 500);
}

/// SPEC §106.3: writes work on whole documents, whatever is selected,
/// and `count` counts what it counts without a selection.
#[test]
fn writes_ignore_the_selection() {
    let (_dir, db) = fresh_db();
    let docs = db.collection::<Document>("docs");
    docs.ensure_index("a").unwrap();
    for n in 0..10i64 {
        docs.insert(object(vec![
            ("a", Document::Int(n % 2)),
            ("b", Document::Int(n)),
        ]))
        .unwrap();
    }
    let odd = || Filter::new().eq("a", 1).select(["a"]);
    assert_eq!(docs.count(odd()).unwrap(), 5);
    // `update_many` is given the whole document, and keeps it.
    let changed = docs
        .update_many(odd(), |doc| {
            let Document::Object(map) = doc else { panic!() };
            assert!(map.contains_key("b"), "given the whole document");
            map.insert("c".into(), Document::Bool(true));
        })
        .unwrap();
    assert_eq!(changed, 5);
    let whole = docs.find(Filter::new().eq("a", 1)).unwrap();
    assert!(
        whole.iter().all(|doc| doc.to_string().contains(r#""b":"#)),
        "{whole:?}"
    );
    let update = crate::update::Update::new().inc("b", 100);
    assert_eq!(docs.update_fields(odd(), &update).unwrap(), 5);
    assert_eq!(docs.count(Filter::new().gt("b", 100)).unwrap(), 5);
    assert!(matches!(
        docs.upsert(
            Filter::new().eq("b", 0).select(["a"]),
            object(vec![("b", Document::Int(0))])
        ),
        Ok(Upserted::Updated(_))
    ));
    assert_eq!(docs.delete_many(odd().sort_asc("b").limit(2)).unwrap(), 2);
    assert_eq!(docs.count(Filter::new()).unwrap(), 8);
}
