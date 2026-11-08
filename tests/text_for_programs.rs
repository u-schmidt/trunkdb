//! What a program that speaks JSON passes, read by the library (SPEC
//! §178): updates as operators, parameters as an object, a batch's ops
//! as lines.

use trunkdb::query::{Filter, Params, Template, Update};
use trunkdb::{BatchLineError, BatchLines, Database, DocId, Document, Error, OpKind, Upserted};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("text.trunkdb")).unwrap();
    (dir, db)
}

fn doc(text: &str) -> Document {
    text.parse().unwrap()
}

/// `update` made to `start`, as the only document of a fresh collection.
fn updated(db: &Database, name: &str, start: &str, update: &Update) -> Document {
    let items = db.collection::<Document>(name);
    let id = items.insert(doc(start)).unwrap();
    items.update_fields(Filter::new().id(id), update).unwrap();
    let mut found = items.get(&id).unwrap().unwrap();
    if let Document::Object(fields) = &mut found {
        fields.remove("_id");
    }
    found
}

/// Each operator as JSON does what the builder's method does.
#[test]
fn every_operator_reads_as_the_builder_writes_it() {
    let (_dir, db) = open();
    let start = r#"{"a": 1, "b": 2.5, "s": "x", "list": [1, 2, 3], "tags": ["red"], "old": 7, "gone": true, "low": 5, "high": 5}"#;
    let cases: Vec<(&str, Update)> = vec![
        (
            r#"{"$set": {"a": 10, "n.deep": {"$date": "2026-11-08"}}}"#,
            Update::new()
                .set("a", 10)
                .set("n.deep", "2026-11-08".parse::<trunkdb::Date>().unwrap()),
        ),
        (
            r#"{"$setOnInsert": {"made": true}}"#,
            Update::new().set_on_insert("made", true),
        ),
        (
            r#"{"$unset": {"gone": "", "s": 1}}"#,
            Update::new().unset("gone").unset("s"),
        ),
        (
            r#"{"$unset": ["gone", "s"]}"#,
            Update::new().unset("gone").unset("s"),
        ),
        (
            r#"{"$inc": {"a": 2, "b": -0.5}}"#,
            Update::new().inc("a", 2).inc("b", -0.5),
        ),
        (r#"{"$mul": {"a": 3}}"#, Update::new().mul("a", 3)),
        (
            r#"{"$min": {"low": 1}, "$max": {"high": 9}}"#,
            Update::new().min("low", 1).max("high", 9),
        ),
        (
            r#"{"$rename": {"old": "new"}}"#,
            Update::new().rename("old", "new"),
        ),
        (r#"{"$push": {"list": 4}}"#, Update::new().push("list", 4)),
        (
            r#"{"$push": {"list": {"$each": [4, 5]}}}"#,
            Update::new().push_all("list", [4, 5]),
        ),
        (
            r#"{"$push": {"list": {"x": 1}}}"#,
            Update::new().push("list", doc(r#"{"x": 1}"#)),
        ),
        (
            r#"{"$addToSet": {"tags": "red"}}"#,
            Update::new().add_to_set("tags", "red"),
        ),
        (
            r#"{"$addToSet": {"tags": {"$each": ["red", "blue", "blue"]}}}"#,
            Update::new()
                .add_to_set("tags", "red")
                .add_to_set("tags", "blue")
                .add_to_set("tags", "blue"),
        ),
        (r#"{"$pull": {"list": 2}}"#, Update::new().pull("list", 2)),
        (r#"{"$pop": {"list": 1}}"#, Update::new().pop_last("list")),
        (r#"{"$pop": {"list": -1}}"#, Update::new().pop_first("list")),
    ];
    for (n, (text, built)) in cases.iter().enumerate() {
        let read: Update = text.parse().unwrap_or_else(|e| panic!("{text}: {e}"));
        let from_doc = Update::try_from(doc(text)).unwrap();
        let want = updated(&db, &format!("built{n}"), start, built);
        assert_eq!(
            updated(&db, &format!("read{n}"), start, &read),
            want,
            "{text}"
        );
        assert_eq!(
            updated(&db, &format!("doc{n}"), start, &from_doc),
            want,
            "{text}"
        );
    }
}

/// What isn't an update says why, in the words `trunkdb update` prints.
#[test]
fn what_isnt_an_update_says_why() {
    let cases = [
        (r#"{"$set": "#, "the update isn't JSON"),
        ("[1]", "the update is an object of operators"),
        ("{}", "the update has no operators; one of $set"),
        (r#"{"$set": 1}"#, "$set takes an object of paths"),
        (r#"{"$set": {}}"#, "$set takes an object of paths"),
        (r#"{"$unset": [1]}"#, "$unset takes paths as strings"),
        (
            r#"{"$rename": {"a": 1}}"#,
            "$rename takes the new path as a string, for a",
        ),
        (
            r#"{"$push": {"a": {"$each": 1}}}"#,
            "$push: $each takes an array, for a",
        ),
        (r#"{"$pop": {"a": 2}}"#, "$pop takes 1 for the last element"),
        (r#"{"$where": {"a": 1}}"#, "$where: not an update operator"),
        (r#"{"status": "done"}"#, "`status` isn't an operator"),
    ];
    for (text, why) in cases {
        let e = text.parse::<Update>().unwrap_err().to_string();
        assert!(e.starts_with(why), "{text}: {e}");
    }
    assert!(Update::try_from(Document::Int(1)).is_err());
}

/// Values from an object, tags read as their kinds; each text of
/// several bound to its own.
#[test]
fn parameters_come_from_an_object() {
    let params: Params =
        r#"{"s": "open", "n": 2, "at": {"$date": "2026-11-08T12:00:00Z"}, "ids": [1, 2]}"#
            .parse()
            .unwrap();
    assert_eq!(params.names().collect::<Vec<_>>(), ["at", "ids", "n", "s"]);
    let template: Template<Filter> = "status = :s and due < :at limit :n".parse().unwrap();
    assert!(matches!(
        template.bind(&params),
        Err(trunkdb::query::BindError::Unused { .. })
    ));
    let mine = params.only(&template.params());
    assert_eq!(mine.names().collect::<Vec<_>>(), ["at", "n", "s"]);
    let filter = template.bind(&mine).unwrap();
    assert_eq!(
        filter.to_string(),
        r#"status = "open" and due < datetime("2026-11-08T12:00:00Z") limit 2"#
    );
    assert_eq!(params.only(&["nothing"]), Params::new());

    assert_eq!(
        Params::try_from(doc("{}")).unwrap(),
        Params::new(),
        "an empty object, no values"
    );
    for text in ["[1]", "1", r#""s""#, "{"] {
        let e = text.parse::<Params>().unwrap_err().to_string();
        assert!(e.starts_with("the parameters"), "{text}: {e}");
    }
}

const COUNTER: &str = r#"
{"op": "insert", "collection": "log", "document": {"page": "/home"}}

{"op": "upsert", "collection": "visits", "filter": "page = :page", "update": {"$inc": {"hits": 1}}}
{"op": "update", "collection": "visits", "filter": "hits > :many", "update": {"$set": {"popular": true}}}
{"op": "require", "collection": "log", "filter": "page = :page"}
{"op": "require_none", "collection": "log", "filter": "page = \"/nowhere\""}
{"op": "delete", "collection": "old", "filter": "", "all": true}
"#;

/// Every op, its line counted with the blank ones, its collection and
/// kind; one batch that commits, the ids of the insert and the upsert.
#[test]
fn lines_read_into_a_batch_that_commits() {
    let (_dir, db) = open();
    db.collection::<Document>("old")
        .insert(doc(r#"{"x": 1}"#))
        .unwrap();
    let params: Params = r#"{"page": "/home", "many": 0}"#.parse().unwrap();
    let ops = BatchLines::read(COUNTER, &params).unwrap();
    let seen: Vec<(usize, &str, OpKind)> = (ops.ops().iter())
        .map(|op| (op.line(), op.collection(), op.kind()))
        .collect();
    assert_eq!(
        seen,
        [
            (2, "log", OpKind::Insert),
            (4, "visits", OpKind::Upsert),
            (5, "visits", OpKind::Update),
            (6, "log", OpKind::Require),
            (7, "log", OpKind::RequireNone),
            (8, "old", OpKind::Delete),
        ]
    );
    assert_eq!((ops.len(), ops.is_empty()), (6, false));

    let (batch, ids) = ops.batch(&db).unwrap();
    assert!(ids[0].is_some() && ids[1..].iter().all(Option::is_none));
    let written = batch.commit().unwrap();
    assert_eq!(written.counts(), [1, 1, 1, 0, 0, 1]);
    let Some(Upserted::Inserted(page)) = written.upserted(1) else {
        panic!("{:?}", written.upserted(1));
    };
    let visits = db.collection::<Document>("visits");
    let made = visits.get(&page).unwrap().unwrap();
    assert_eq!(made.get("hits"), Some(&Document::Int(1)));
    assert_eq!(made.get("popular"), Some(&Document::Bool(true)));
    let log = db.collection::<Document>("log");
    assert!(log.get(&ids[0].unwrap()).unwrap().is_some());
    assert_eq!(
        db.collection::<Document>("old")
            .count(Filter::new())
            .unwrap(),
        0
    );

    // Read again, the same lines upsert the same page.
    let (batch, _) = BatchLines::read(COUNTER, &params)
        .unwrap()
        .batch(&db)
        .unwrap();
    let written = batch.commit().unwrap();
    assert_eq!(written.upserted(1), Some(Upserted::Updated(page)));
    assert_eq!(
        visits.get(&page).unwrap().unwrap().get("hits"),
        Some(&Document::Int(2))
    );
}

fn line_error(text: &str, params: &Params) -> BatchLineError {
    BatchLines::read(text, params).unwrap_err()
}

/// A mistake names its line, the first one in the text; a filter's says
/// where in the filter.
#[test]
fn mistakes_name_their_line() {
    let none = Params::new();
    let ok = r#"{"op": "delete", "collection": "t", "filter": "a = 1"}"#;
    let cases = [
        ("{", "not a JSON object"),
        ("[1]", "an op is a JSON object"),
        (r#"{"collection": "t"}"#, "no `op`"),
        (r#"{"op": 1, "collection": "t"}"#, "`op` is a string"),
        (r#"{"op": "insert"}"#, "insert: no `collection`"),
        (
            r#"{"op": "insert", "collection": ""}"#,
            "`collection` is a collection's name",
        ),
        (
            r#"{"op": "insert", "collection": "t"}"#,
            "insert: no `document`",
        ),
        (
            r#"{"op": "insert", "collection": "t", "document": 1}"#,
            "insert: `document` is a JSON object",
        ),
        (
            r#"{"op": "insert", "collection": "t", "document": {}, "filter": ""}"#,
            "insert: takes no `filter`",
        ),
        (
            r#"{"op": "delete", "collection": "t"}"#,
            "delete: no `filter`",
        ),
        (
            r#"{"op": "delete", "collection": "t", "filter": 1}"#,
            "delete: `filter` is a filter's text",
        ),
        (
            r#"{"op": "delete", "collection": "t", "filter": ""}"#,
            "delete: the filter is empty, which takes every document of t",
        ),
        (
            r#"{"op": "delete", "collection": "t", "filter": "", "all": 1}"#,
            "delete: `all` is true or false",
        ),
        (
            r#"{"op": "update", "collection": "t", "filter": "a = 1"}"#,
            "update: no `update`",
        ),
        (
            r#"{"op": "update", "collection": "t", "filter": "a = 1", "update": {"a": 1}}"#,
            "`a` isn't an operator",
        ),
        (
            r#"{"op": "upsert", "collection": "t", "filter": "a = 1"}"#,
            "upsert: no `update`",
        ),
        (r#"{"op": "drop", "collection": "t"}"#, "drop: not an op"),
        (
            r#"{"op": "delete", "collection": "t", "filter": "a = 1", "extra": 1}"#,
            "`extra`: not a field of an op line",
        ),
    ];
    for (bad, why) in cases {
        let text = format!("{ok}\n\n{bad}\n{bad}\n");
        match line_error(&text, &none) {
            BatchLineError::Line { line, message, .. } => {
                assert_eq!(line, 3, "{bad}");
                assert!(message.starts_with(why), "{bad}: {message}");
            }
            other => panic!("{bad}: {other:?}"),
        }
    }

    let text = format!(
        "{ok}\n{}",
        r#"{"op": "delete", "collection": "t", "filter": "a = = 1"}"#
    );
    match line_error(&text, &none) {
        BatchLineError::Filter {
            line, text, error, ..
        } => {
            assert_eq!((line, text.as_str(), error.offset()), (2, "a = = 1", 4));
        }
        other => panic!("{other:?}"),
    }
    let missing = r#"{"op": "delete", "collection": "t", "filter": "a = :v"}"#;
    match line_error(missing, &none) {
        e @ BatchLineError::Bind { .. } => {
            assert_eq!(e.line(), Some(1));
            assert_eq!(
                e.to_string(),
                "line 1: filter: no value for the parameter :v"
            );
        }
        other => panic!("{other:?}"),
    }
    let params: Params = r#"{"v": 1, "typo": 2, "also": 3}"#.parse().unwrap();
    match line_error(missing, &params) {
        e @ BatchLineError::Unused { .. } => {
            assert_eq!(e.line(), None);
            assert!(e.to_string().starts_with("a value for :also,"), "{e}");
        }
        other => panic!("{other:?}"),
    }
}

/// A value used by any line is used; lines share one.
#[test]
fn lines_share_the_values() {
    let text = r#"{"op": "require", "collection": "t", "filter": "a = :v"}
{"op": "delete", "collection": "t", "filter": "a = :v or b = :w"}"#;
    let params: Params = r#"{"v": 1, "w": 2}"#.parse().unwrap();
    let ops = BatchLines::read(text, &params).unwrap();
    assert_eq!(ops.len(), 2);
    assert!(BatchLines::read("", &params).is_err(), "no line used them");
    let empty = BatchLines::read("\n  \n", &Params::new()).unwrap();
    assert!(empty.is_empty());
}

/// An op the batch refuses names its line; a failed commit's line is
/// found, a precondition's at once, another's by dry runs.
#[test]
fn a_failure_finds_its_line() {
    let (_dir, db) = open();
    let users = db.collection::<Document>("users");
    users
        .ensure_index_with("email", trunkdb::IndexOptions::new().unique())
        .unwrap();
    users.insert(doc(r#"{"email": "a@x"}"#)).unwrap();
    let none = Params::new();

    let limited = "\n".to_string()
        + r#"{"op": "require", "collection": "users", "filter": "email = \"a@x\" limit 1"}"#;
    match BatchLines::read(&limited, &none).unwrap().batch(&db) {
        Err(e @ BatchLineError::Refused { .. }) => assert_eq!(e.line(), Some(2)),
        other => panic!("{:?}", other.map(|_| ())),
    }

    let text = r#"{"op": "insert", "collection": "users", "document": {"email": "b@x"}}
{"op": "require_none", "collection": "users", "filter": "email = \"c@x\""}
{"op": "insert", "collection": "users", "document": {"email": "a@x"}}
{"op": "insert", "collection": "users", "document": {"email": "d@x"}}
{"op": "require", "collection": "users", "filter": "email = \"zzz\""}"#;
    let ops = BatchLines::read(text, &none).unwrap();
    let duplicate = ops.batch(&db).unwrap().0.commit().unwrap_err();
    assert!(
        matches!(duplicate, Error::DuplicateValue { .. }),
        "{duplicate}"
    );
    assert_eq!(ops.failed_line(&db, &duplicate), Some(3));
    let e = ops.batch(&db).unwrap().0.dry_run().unwrap_err();
    assert_eq!(ops.failed_line(&db, &e), Some(3));

    let without_dup: String = text
        .lines()
        .filter(|l| !l.contains("a@x"))
        .collect::<Vec<_>>()
        .join("\n");
    let ops = BatchLines::read(&without_dup, &none).unwrap();
    let e = ops.batch(&db).unwrap().0.commit().unwrap_err();
    assert!(matches!(e, Error::PreconditionFailed { op: 3, .. }), "{e}");
    assert_eq!(ops.failed_line(&db, &e), Some(4));

    // Nothing of the batch fails now, as when another write came in
    // between: no line to blame.
    let fine = BatchLines::read(text.lines().next().unwrap(), &none).unwrap();
    assert_eq!(fine.failed_line(&db, &duplicate), None);
    assert_eq!(BatchLines::default().failed_line(&db, &duplicate), None);
    assert_eq!(users.count(Filter::new()).unwrap(), 1, "nothing written");
}

/// The id of each insert, and of each upsert from what was written, in
/// the order of their lines, as `trunkdb batch` prints them.
#[test]
fn ids_come_in_the_order_of_the_lines() {
    let (_dir, db) = open();
    let text = r#"{"op": "upsert", "collection": "c", "filter": "k = 1", "update": {"$inc": {"n": 1}}}
{"op": "insert", "collection": "c", "document": {"k": 2}}
{"op": "upsert", "collection": "c", "filter": "k = 1", "update": {"$inc": {"n": 1}}}"#;
    let ops = BatchLines::read(text, &Params::new()).unwrap();
    let (batch, ids) = ops.batch(&db).unwrap();
    let written = batch.commit().unwrap();
    let all: Vec<DocId> = (ids.iter().enumerate())
        .filter_map(|(at, id)| id.or_else(|| written.upserted(at).map(|done| done.id())))
        .collect();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0], all[2], "the second upsert found the first's");
    assert_eq!(written.upserted(0), Some(Upserted::Inserted(all[0])));
    assert_eq!(written.upserted(2), Some(Upserted::Updated(all[0])));
}
