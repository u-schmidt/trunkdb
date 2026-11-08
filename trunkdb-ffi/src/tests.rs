//! Every function called as C calls it, through raw pointers (SPEC
//! §179): what it hands out, every error code it gives, the details'
//! JSON, and misuse that can be caught.

use super::*;
use serde_json::Value;
use std::ffi::{CStr, CString};
use std::ptr::{null, null_mut};

fn c(text: &str) -> CString {
    CString::new(text).unwrap()
}

/// A string handed out, read and given back.
fn take(text: *mut c_char) -> Option<String> {
    if text.is_null() {
        return None;
    }
    let read = unsafe { CStr::from_ptr(text) }
        .to_str()
        .unwrap()
        .to_string();
    unsafe { trunkdb_string_free(text) };
    Some(read)
}

fn json(text: *mut c_char) -> Value {
    serde_json::from_str(&take(text).expect("a string")).unwrap()
}

/// This thread's last error's details.
fn detail() -> Value {
    let text = unsafe { CStr::from_ptr(trunkdb_error_detail()) };
    serde_json::from_str(text.to_str().unwrap()).unwrap()
}

fn message() -> String {
    let text = unsafe { CStr::from_ptr(trunkdb_error_message()) };
    text.to_str().unwrap().to_string()
}

#[track_caller]
fn ok(code: i32) {
    assert_eq!(code, TRUNKDB_OK, "{}", message());
}

#[track_caller]
fn fails(code: i32, want: i32) -> Value {
    assert_eq!(code, want, "{}", message());
    let detail = detail();
    assert_eq!(detail["code"], want);
    assert_eq!(detail["message"], message());
    detail
}

/// A database in a directory of its own, closed when dropped.
struct Open {
    db: *mut trunkdb_db,
    dir: tempfile::TempDir,
}

impl Open {
    fn new() -> Open {
        Open::with(None)
    }

    fn with(options: Option<&str>) -> Open {
        let dir = tempfile::tempdir().unwrap();
        let path = c(dir.path().join("app.trunkdb").to_str().unwrap());
        let options = options.map(c);
        let mut db = null_mut();
        let given = options.as_ref().map_or(null(), |o| o.as_ptr());
        ok(unsafe { trunkdb_open(path.as_ptr(), given, &mut db) });
        assert!(!db.is_null());
        Open { db, dir }
    }

    fn path(&self, name: &str) -> CString {
        c(self.dir.path().join(name).to_str().unwrap())
    }

    fn insert(&self, collection: &str, doc: &str) -> String {
        let mut id = null_mut();
        ok(unsafe { trunkdb_insert(self.db, c(collection).as_ptr(), c(doc).as_ptr(), &mut id) });
        take(id).unwrap()
    }

    fn find(&self, collection: &str, filter: &str, params: Option<&str>) -> Value {
        let params = params.map(c);
        let mut out = null_mut();
        ok(unsafe {
            trunkdb_find(
                self.db,
                null(),
                c(collection).as_ptr(),
                c(filter).as_ptr(),
                params.as_ref().map_or(null(), |p| p.as_ptr()),
                &mut out,
            )
        });
        json(out)
    }

    fn count(&self, collection: &str, filter: &str) -> u64 {
        let mut n = 99;
        ok(unsafe {
            trunkdb_count(
                self.db,
                null(),
                c(collection).as_ptr(),
                c(filter).as_ptr(),
                null(),
                &mut n,
            )
        });
        n
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        ok(unsafe { trunkdb_close(self.db) });
    }
}

fn ids(found: &Value) -> Vec<String> {
    (found.as_array().unwrap().iter())
        .map(|doc| doc["_id"]["$id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn versions_say_what_was_built() {
    assert_eq!(trunkdb_abi_version(), TRUNKDB_ABI_VERSION);
    let version = unsafe { CStr::from_ptr(trunkdb_version()) }
        .to_str()
        .unwrap();
    assert_eq!(
        version,
        db::RELEASE.to_string(),
        "the crate's version is trunkdb's"
    );
}

/// Documents in, found, changed and deleted, each call's out parameter
/// what it says.
#[test]
fn documents_go_in_and_come_out_as_json() {
    let db = Open::new();
    let a = db.insert(
        "tasks",
        r#"{"title": "a", "status": "open", "due": {"$date": "2026-11-08"}}"#,
    );
    let b = db.insert("tasks", r#"{"title": "b", "status": "done"}"#);
    assert_ne!(a, b);

    let mut found = null_mut();
    ok(unsafe {
        trunkdb_get(
            db.db,
            null(),
            c("tasks").as_ptr(),
            c(&a).as_ptr(),
            &mut found,
        )
    });
    let doc = json(found);
    assert_eq!(doc["title"], "a");
    assert_eq!(doc["due"]["$date"], "2026-11-08", "a date stays a date");
    let none = "01a1bc72-c792-772e-a864-a359a7ffaeb6";
    ok(unsafe {
        trunkdb_get(
            db.db,
            null(),
            c("tasks").as_ptr(),
            c(none).as_ptr(),
            &mut found,
        )
    });
    assert!(found.is_null(), "none: null");

    assert_eq!(
        ids(&db.find("tasks", "status = :s", Some(r#"{"s": "open"}"#))),
        [a.as_str()]
    );
    assert_eq!(ids(&db.find("tasks", "", None)), [a.clone(), b.clone()]);
    assert_eq!(db.count("tasks", "status = \"done\""), 1);
    assert_eq!(
        db.count("nothing", ""),
        0,
        "a collection not there is empty"
    );

    let mut one = null_mut();
    let filter = c("order by title desc");
    ok(unsafe {
        trunkdb_find_one(
            db.db,
            null(),
            c("tasks").as_ptr(),
            filter.as_ptr(),
            null(),
            &mut one,
        )
    });
    assert_eq!(json(one)["title"], "b");
    ok(unsafe {
        trunkdb_find_one(
            db.db,
            null(),
            c("tasks").as_ptr(),
            c("title = 1").as_ptr(),
            null(),
            &mut one,
        )
    });
    assert!(one.is_null());

    let mut n = 0;
    let set = c(r#"{"$set": {"seen": true}}"#);
    ok(unsafe {
        trunkdb_update(
            db.db,
            c("tasks").as_ptr(),
            null(),
            null(),
            set.as_ptr(),
            &mut n,
        )
    });
    assert_eq!(n, 2, "null filter: every document");
    assert_eq!(db.count("tasks", "seen = true"), 2);

    let mut id = null_mut();
    let mut inserted = false;
    let inc = c(r#"{"$inc": {"hits": 1}}"#);
    let page = c("page = :p");
    let params = c(r#"{"p": "/home"}"#);
    ok(unsafe {
        trunkdb_upsert(
            db.db,
            c("visits").as_ptr(),
            page.as_ptr(),
            params.as_ptr(),
            inc.as_ptr(),
            &mut id,
            &mut inserted,
        )
    });
    let made = take(id).unwrap();
    assert!(inserted);
    ok(unsafe {
        trunkdb_upsert(
            db.db,
            c("visits").as_ptr(),
            page.as_ptr(),
            params.as_ptr(),
            inc.as_ptr(),
            &mut id,
            &mut inserted,
        )
    });
    assert_eq!((take(id).unwrap(), inserted), (made, false));
    assert_eq!(db.find("visits", "", None)[0]["hits"], 2);

    let mut there = false;
    let whole = c(r#"{"title": "a2"}"#);
    ok(unsafe {
        trunkdb_replace(
            db.db,
            c("tasks").as_ptr(),
            c(&a).as_ptr(),
            whole.as_ptr(),
            &mut there,
        )
    });
    assert!(there);
    assert_eq!(
        db.find("tasks", "title = \"a2\"", None)[0]["seen"],
        Value::Null,
        "replaced whole"
    );
    ok(unsafe {
        trunkdb_replace(
            db.db,
            c("tasks").as_ptr(),
            c(none).as_ptr(),
            whole.as_ptr(),
            &mut there,
        )
    });
    assert!(!there, "none is inserted");

    ok(unsafe { trunkdb_delete(db.db, c("tasks").as_ptr(), c(&a).as_ptr(), &mut there) });
    assert!(there);
    ok(unsafe { trunkdb_delete(db.db, c("tasks").as_ptr(), c(&a).as_ptr(), &mut there) });
    assert!(!there);
    ok(unsafe { trunkdb_delete_many(db.db, c("tasks").as_ptr(), c("").as_ptr(), null(), &mut n) });
    assert_eq!(n, 1);

    let mut names = null_mut();
    ok(unsafe { trunkdb_collections(db.db, &mut names) });
    assert_eq!(json(names), serde_json::json!(["tasks", "visits"]));
    let mut dropped = false;
    ok(unsafe { trunkdb_drop_collection(db.db, c("visits").as_ptr(), &mut dropped) });
    assert!(dropped);
    ok(unsafe { trunkdb_drop_collection(db.db, c("visits").as_ptr(), &mut dropped) });
    assert!(!dropped);
}

/// Indexes made, listed and dropped; a unique one's refusal in the
/// details.
#[test]
fn indexes_and_what_they_refuse() {
    let db = Open::new();
    let mut created = false;
    let users = c("users");
    ok(unsafe {
        trunkdb_index_ensure(
            db.db,
            users.as_ptr(),
            c("email").as_ptr(),
            c(r#"{"unique": true}"#).as_ptr(),
            &mut created,
        )
    });
    assert!(created, "a bare name");
    let unique = c(r#"{"unique": true}"#);
    ok(unsafe {
        trunkdb_index_ensure(
            db.db,
            users.as_ptr(),
            c(r#""email""#).as_ptr(),
            unique.as_ptr(),
            &mut created,
        )
    });
    assert!(!created, "there already");
    let code = unsafe {
        trunkdb_index_ensure(
            db.db,
            users.as_ptr(),
            c("email").as_ptr(),
            null(),
            &mut created,
        )
    };
    let detail = fails(code, TRUNKDB_INVALID_ARGUMENT);
    assert_eq!(
        detail["io"], "InvalidInput",
        "refused as asked: another kind of index"
    );
    ok(unsafe {
        trunkdb_index_ensure(
            db.db,
            users.as_ptr(),
            c(r#"["team", "name"]"#).as_ptr(),
            c(r#"{"sparse": true}"#).as_ptr(),
            &mut created,
        )
    });
    assert!(created);
    let mut list = null_mut();
    ok(unsafe { trunkdb_indexes(db.db, users.as_ptr(), &mut list) });
    assert_eq!(
        json(list),
        serde_json::json!([
            {"fields": ["email"], "unique": true, "sparse": false},
            {"fields": ["team", "name"], "unique": false, "sparse": true},
        ])
    );

    let first = db.insert("users", r#"{"email": "a@x"}"#);
    let mut id = null_mut();
    let code = unsafe {
        trunkdb_insert(
            db.db,
            users.as_ptr(),
            c(r#"{"email": "a@x"}"#).as_ptr(),
            &mut id,
        )
    };
    let detail = fails(code, TRUNKDB_DUPLICATE_VALUE);
    assert!(id.is_null());
    assert_eq!(detail["kind"], "duplicate_value");
    assert_eq!(detail["collection"], "users");
    assert_eq!(detail["field"], "email");
    assert_eq!(detail["existing"], first.as_str());

    let mut dropped = false;
    ok(unsafe { trunkdb_index_drop(db.db, users.as_ptr(), c("email").as_ptr(), &mut dropped) });
    assert!(dropped);
    ok(unsafe { trunkdb_index_drop(db.db, users.as_ptr(), c("email").as_ptr(), &mut dropped) });
    assert!(!dropped);
    for bad in ["[1]", "{}", "", r#"{"a": 1}"#] {
        let code =
            unsafe { trunkdb_index_drop(db.db, users.as_ptr(), c(bad).as_ptr(), &mut dropped) };
        assert_eq!(fails(code, TRUNKDB_PARSE)["what"], "fields", "{bad}");
    }
}

/// An aggregate and its filter share the values, each taking its own;
/// a value neither has is refused. Explain says how a read ran.
#[test]
fn aggregates_and_explain() {
    let db = Open::new();
    for (team, hours) in [("red", 3), ("red", 5), ("blue", 2)] {
        db.insert(
            "work",
            &format!(r#"{{"team": "{team}", "hours": {hours}}}"#),
        );
    }
    let mut rows = null_mut();
    let run = |filter: &str, aggregate: &str, params: &str, rows: &mut *mut c_char| unsafe {
        trunkdb_aggregate(
            db.db,
            null(),
            c("work").as_ptr(),
            c(filter).as_ptr(),
            c(aggregate).as_ptr(),
            c(params).as_ptr(),
            rows,
        )
    };
    ok(run(
        "hours > :min",
        "sum(hours) as total group by team having total > :over",
        r#"{"min": 2, "over": 1}"#,
        &mut rows,
    ));
    assert_eq!(json(rows), serde_json::json!([{"team": "red", "total": 8}]));
    let detail = fails(
        run("", "count() as n", r#"{"typo": 1}"#, &mut rows),
        TRUNKDB_PARSE,
    );
    assert_eq!(detail["parameter"], "typo");
    assert!(rows.is_null());
    let detail = fails(run("", "count() as n by", "", &mut rows), TRUNKDB_PARSE);
    assert_eq!(detail["what"], "aggregate");

    let mut explained = null_mut();
    ok(unsafe {
        trunkdb_explain(
            db.db,
            null(),
            c("work").as_ptr(),
            c("team = \"red\"").as_ptr(),
            null(),
            &mut explained,
        )
    });
    let text = take(explained).unwrap();
    assert!(text.starts_with("plan: scan of every document\n"), "{text}");
    assert!(text.contains("documents matched: 2\n"), "{text}");
}

/// A batch as JSON lines: what each op wrote, ids, a dry run without
/// them, and the line a failure came from.
#[test]
fn batches_say_what_each_line_did() {
    let db = Open::new();
    let lines = c(
        r#"{"op": "insert", "collection": "log", "document": {"page": "/home"}}
{"op": "upsert", "collection": "visits", "filter": "page = :page", "update": {"$inc": {"hits": 1}}}

{"op": "require", "collection": "log", "filter": "page = :page"}"#,
    );
    let params = c(r#"{"page": "/home"}"#);
    let mut written = null_mut();
    ok(unsafe { trunkdb_batch(db.db, lines.as_ptr(), params.as_ptr(), true, &mut written) });
    let dry = json(written);
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["total"], 2);
    assert!(
        dry["ops"]
            .as_array()
            .unwrap()
            .iter()
            .all(|op| op.get("id").is_none()),
        "{dry}"
    );
    assert_eq!(db.count("log", ""), 0, "a dry run writes nothing");

    ok(unsafe { trunkdb_batch(db.db, lines.as_ptr(), params.as_ptr(), false, &mut written) });
    let done = json(written);
    let ops = done["ops"].as_array().unwrap();
    assert_eq!(
        (
            ops[0]["line"].clone(),
            ops[0]["op"].clone(),
            ops[0]["count"].clone()
        ),
        (1.into(), "insert".into(), 1.into())
    );
    assert!(ops[0]["id"].is_string());
    assert_eq!(
        (ops[1]["op"].clone(), ops[1]["inserted"].clone()),
        ("upsert".into(), true.into())
    );
    assert_eq!(ops[2]["line"], 4, "blank lines counted");
    assert_eq!(ops[2]["op"], "require");
    let visit = &db.find("visits", "", None)[0];
    assert_eq!(ops[1]["id"], visit["_id"]["$id"]);

    let failing = c(
        r#"{"op": "insert", "collection": "log", "document": {"n": 1}}
{"op": "require_none", "collection": "log", "filter": "page = \"/home\""}"#,
    );
    let code = unsafe { trunkdb_batch(db.db, failing.as_ptr(), null(), false, &mut written) };
    let detail = fails(code, TRUNKDB_PRECONDITION_FAILED);
    assert_eq!(
        (
            detail["line"].clone(),
            detail["op"].clone(),
            detail["matched"].clone()
        ),
        (2.into(), 1.into(), true.into())
    );
    assert!(written.is_null());
    assert_eq!(db.count("log", ""), 1, "nothing of it written");

    let bad = c(
        "{\"op\": \"delete\", \"collection\": \"log\", \"filter\": \"a = 1\"}\n{\"op\": \"delete\", \"collection\": \"log\", \"filter\": \"a = = 1\"}",
    );
    let detail = fails(
        unsafe { trunkdb_batch(db.db, bad.as_ptr(), null(), false, &mut written) },
        TRUNKDB_PARSE,
    );
    assert_eq!(
        (detail["line"].clone(), detail["offset"].clone()),
        (2.into(), 4.into())
    );
    assert!(
        message().starts_with("batch: line 2: filter:"),
        "{}",
        message()
    );
    let limited = c(r#"{"op": "require", "collection": "log", "filter": "limit 1"}"#);
    let detail = fails(
        unsafe { trunkdb_batch(db.db, limited.as_ptr(), null(), false, &mut written) },
        TRUNKDB_INVALID_ARGUMENT,
    );
    assert_eq!(detail["line"], 1);
    let unused = c(r#"{"p": 1}"#);
    let detail = fails(
        unsafe { trunkdb_batch(db.db, c("").as_ptr(), unused.as_ptr(), false, &mut written) },
        TRUNKDB_PARSE,
    );
    assert_eq!(detail["parameter"], "p");
    assert!(detail.get("line").is_none());
}

/// A snapshot reads as it was taken, by every read and a cursor; a
/// cursor hands out a document at a time, then null.
#[test]
fn snapshots_and_cursors() {
    let db = Open::new();
    let first = db.insert("t", r#"{"n": 1}"#);
    let mut snap = null_mut();
    ok(unsafe { trunkdb_snapshot_open(db.db, &mut snap) });
    db.insert("t", r#"{"n": 2}"#);
    let t = c("t");

    let mut n = 0;
    ok(unsafe { trunkdb_count(db.db, snap, t.as_ptr(), null(), null(), &mut n) });
    assert_eq!(n, 1, "as it was");
    assert_eq!(db.count("t", ""), 2, "and as it is");
    let mut found = null_mut();
    ok(unsafe { trunkdb_find(db.db, snap, t.as_ptr(), null(), null(), &mut found) });
    assert_eq!(ids(&json(found)), [first.as_str()]);

    let read_all = |snapshot: *const trunkdb_snapshot| {
        let mut cursor = null_mut();
        ok(unsafe {
            trunkdb_cursor_open(
                db.db,
                snapshot,
                t.as_ptr(),
                c("order by n").as_ptr(),
                null(),
                &mut cursor,
            )
        });
        let mut seen = Vec::new();
        loop {
            let mut doc = null_mut();
            ok(unsafe { trunkdb_cursor_next(cursor, &mut doc) });
            match take(doc) {
                Some(doc) => seen.push(
                    serde_json::from_str::<Value>(&doc).unwrap()["n"]
                        .as_i64()
                        .unwrap(),
                ),
                None => break,
            }
        }
        let mut doc = null_mut();
        ok(unsafe { trunkdb_cursor_next(cursor, &mut doc) });
        assert!(doc.is_null(), "still the end");
        ok(unsafe { trunkdb_cursor_close(cursor) });
        seen
    };
    assert_eq!(read_all(snap), [1]);
    assert_eq!(read_all(null()), [1, 2]);

    // One that holds the last commit keeps out a compaction that has
    // pages to free.
    for i in 0..300 {
        db.insert(
            "pad",
            &format!(r#"{{"i": {i}, "x": "{}"}}"#, "x".repeat(i * 13 % 900)),
        );
    }
    let mut gone = 0;
    ok(unsafe {
        trunkdb_delete_many(
            db.db,
            c("pad").as_ptr(),
            c("i >= 10").as_ptr(),
            null(),
            &mut gone,
        )
    });
    assert_eq!(gone, 290);
    ok(unsafe { trunkdb_snapshot_close(snap) });
    ok(unsafe { trunkdb_snapshot_open(db.db, &mut snap) });
    let mut report = null_mut();
    let code = unsafe { trunkdb_compact(db.db, &mut report) };
    fails(code, TRUNKDB_SNAPSHOT_OPEN);
    ok(unsafe { trunkdb_snapshot_close(snap) });
    ok(unsafe { trunkdb_compact(db.db, &mut report) });
    assert!(json(report)["pages_after"].is_u64());
}

/// The reports: check, stats, compact, an export and its import.
#[test]
fn reports_and_backups() {
    let db = Open::new();
    db.insert("t", r#"{"n": 1}"#);
    db.insert("u", r#"{"n": 2}"#);
    let mut report = null_mut();
    ok(unsafe { trunkdb_check(db.db, &mut report) });
    assert_eq!(
        json(report),
        serde_json::json!({"ok": true, "collections": 2, "documents": 2, "pages": json_pages(&db), "problems": []})
    );
    ok(unsafe { trunkdb_stats(db.db, &mut report) });
    let stats = json(report);
    assert_eq!(stats["file"]["written_by"], db::RELEASE.to_string());
    assert_eq!(stats["collections"][0]["name"], "t");
    assert_eq!(stats["collections"][0]["documents"], 1);
    assert!(stats["pages"]["data"].as_u64().unwrap() >= 2);
    ok(unsafe { trunkdb_checkpoint(db.db) });

    let out = db.path("backup.jsonl");
    let mut summary = null_mut();
    ok(unsafe { trunkdb_export(db.db, out.as_ptr(), &mut summary) });
    assert_eq!(
        json(summary),
        serde_json::json!({"collections": 2, "documents": 2, "ended": true})
    );
    let itself = db.path("app.trunkdb");
    let detail = fails(
        unsafe { trunkdb_export(db.db, itself.as_ptr(), &mut summary) },
        TRUNKDB_INVALID_ARGUMENT,
    );
    assert_eq!(detail["io"], "InvalidInput");
    assert!(
        message().contains("is the database or its log"),
        "{}",
        message()
    );

    let other = Open::new();
    ok(unsafe {
        trunkdb_import(
            other.db,
            out.as_ptr(),
            c(r#"{"max_line_bytes": 100000}"#).as_ptr(),
            &mut summary,
        )
    });
    assert_eq!(json(summary)["documents"], 2);
    assert_eq!(other.count("u", "n = 2"), 1);
    let missing = db.path("missing.jsonl");
    let detail = fails(
        unsafe { trunkdb_import(other.db, missing.as_ptr(), null(), &mut summary) },
        TRUNKDB_IO,
    );
    assert_eq!(detail["io"], "NotFound");
    assert!(summary.is_null());
}

fn json_pages(db: &Open) -> u64 {
    let mut report = null_mut();
    ok(unsafe { trunkdb_stats(db.db, &mut report) });
    json(report)["file"]["pages"].as_u64().unwrap()
}

/// Each text that doesn't read is `TRUNKDB_PARSE`, `what` naming it, and
/// a filter's place in it.
#[test]
fn texts_that_dont_read() {
    let db = Open::new();
    let t = c("t");
    let mut out = null_mut();
    let mut n = 7;
    let detail = fails(
        unsafe {
            trunkdb_count(
                db.db,
                null(),
                t.as_ptr(),
                c("a = = 1").as_ptr(),
                null(),
                &mut n,
            )
        },
        TRUNKDB_PARSE,
    );
    assert_eq!(
        (detail["what"].clone(), detail["offset"].clone()),
        ("filter".into(), 4.into())
    );
    assert_eq!(n, 0, "the out parameter reset");
    let detail = fails(
        unsafe {
            trunkdb_count(
                db.db,
                null(),
                t.as_ptr(),
                c("a = :v").as_ptr(),
                null(),
                &mut n,
            )
        },
        TRUNKDB_PARSE,
    );
    assert_eq!(
        (detail["parameter"].clone(), detail["offset"].clone()),
        ("v".into(), 4.into())
    );
    let detail = fails(
        unsafe {
            trunkdb_count(
                db.db,
                null(),
                t.as_ptr(),
                c("a = 1").as_ptr(),
                c(r#"{"v": 1}"#).as_ptr(),
                &mut n,
            )
        },
        TRUNKDB_PARSE,
    );
    assert_eq!(
        detail["parameter"], "v",
        "a value the filter has no parameter of"
    );
    for params in ["[1]", "{", "1"] {
        let code = unsafe {
            trunkdb_count(
                db.db,
                null(),
                t.as_ptr(),
                null(),
                c(params).as_ptr(),
                &mut n,
            )
        };
        assert_eq!(fails(code, TRUNKDB_PARSE)["what"], "params");
    }
    ok(unsafe { trunkdb_count(db.db, null(), t.as_ptr(), null(), c(" ").as_ptr(), &mut n) });

    let code = unsafe { trunkdb_insert(db.db, t.as_ptr(), c("{\"a\": ").as_ptr(), &mut out) };
    assert_eq!(fails(code, TRUNKDB_PARSE)["what"], "document");
    let code = unsafe { trunkdb_insert(db.db, t.as_ptr(), c("[1]").as_ptr(), &mut out) };
    fails(code, TRUNKDB_NOT_AN_OBJECT);
    let code = unsafe { trunkdb_get(db.db, null(), t.as_ptr(), c("not-an-id").as_ptr(), &mut out) };
    assert_eq!(fails(code, TRUNKDB_PARSE)["what"], "id");
    let code = unsafe {
        trunkdb_update(
            db.db,
            t.as_ptr(),
            null(),
            null(),
            c(r#"{"a": 1}"#).as_ptr(),
            &mut n,
        )
    };
    assert_eq!(fails(code, TRUNKDB_PARSE)["what"], "update");
    assert!(message().contains("`a` isn't an operator"), "{}", message());

    let mut other = null_mut();
    let path = db.path("other.trunkdb");
    let code = unsafe { trunkdb_open(path.as_ptr(), c(r#"{"cache": 1}"#).as_ptr(), &mut other) };
    assert_eq!(fails(code, TRUNKDB_PARSE)["what"], "options");
    let code = unsafe {
        trunkdb_open(
            path.as_ptr(),
            c(r#"{"cache_size": -1}"#).as_ptr(),
            &mut other,
        )
    };
    fails(code, TRUNKDB_PARSE);
    let code =
        unsafe { trunkdb_open(path.as_ptr(), c(r#"{"read_only": 1}"#).as_ptr(), &mut other) };
    fails(code, TRUNKDB_PARSE);
    assert!(other.is_null());
}

/// A value no parameter takes is refused by every call with a filter,
/// as `bind` refuses it: a typo in a name would otherwise go unseen.
#[test]
fn every_filter_refuses_an_unused_value() {
    let db = Open::new();
    db.insert("t", r#"{"a": 1}"#);
    let (t, filter, params, update) = (
        c("t"),
        c("a = :a"),
        c(r#"{"a": 1, "typo": 2}"#),
        c(r#"{"$set": {"b": 1}}"#),
    );
    let (t, f, p, u) = (
        t.as_ptr(),
        filter.as_ptr(),
        params.as_ptr(),
        update.as_ptr(),
    );
    let (mut text, mut n, mut flag) = (null_mut(), 0, false);
    let mut cursor = null_mut();
    let calls: Vec<(&str, i32)> = unsafe {
        vec![
            ("find", trunkdb_find(db.db, null(), t, f, p, &mut text)),
            (
                "find_one",
                trunkdb_find_one(db.db, null(), t, f, p, &mut text),
            ),
            ("count", trunkdb_count(db.db, null(), t, f, p, &mut n)),
            (
                "explain",
                trunkdb_explain(db.db, null(), t, f, p, &mut text),
            ),
            (
                "cursor_open",
                trunkdb_cursor_open(db.db, null(), t, f, p, &mut cursor),
            ),
            ("update", trunkdb_update(db.db, t, f, p, u, &mut n)),
            ("delete_many", trunkdb_delete_many(db.db, t, f, p, &mut n)),
            (
                "upsert",
                trunkdb_upsert(db.db, t, f, p, u, &mut text, &mut flag),
            ),
        ]
    };
    for (name, code) in calls {
        assert_eq!(code, TRUNKDB_PARSE, "{name}");
    }
    assert_eq!(detail()["parameter"], "typo");
    assert_eq!(db.count("t", "a = 1 and b = null"), 1, "nothing written");
}

/// The errors a database gives, through its codes and details.
#[test]
fn errors_of_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("junk.trunkdb");
    std::fs::write(
        &junk,
        b"this is not a database, not at all, it is just some text",
    )
    .unwrap();
    let mut db = null_mut();
    let code = unsafe { trunkdb_open(c(junk.to_str().unwrap()).as_ptr(), null(), &mut db) };
    assert_eq!(fails(code, TRUNKDB_FORMAT)["kind"], "format");
    assert!(db.is_null());

    let read_only = c(r#"{"read_only": true}"#);
    let missing = c(dir.path().join("none.trunkdb").to_str().unwrap());
    let code = unsafe { trunkdb_open(missing.as_ptr(), read_only.as_ptr(), &mut db) };
    assert_eq!(fails(code, TRUNKDB_IO)["io"], "NotFound");

    let path = c(dir.path().join("app.trunkdb").to_str().unwrap());
    let mut id = null_mut();
    ok(unsafe { trunkdb_open(path.as_ptr(), null(), &mut db) });
    ok(unsafe { trunkdb_insert(db, c("t").as_ptr(), c("{}").as_ptr(), &mut id) });
    let id = take(id).unwrap();
    let mut again = null_mut();
    let code = unsafe {
        trunkdb_insert(
            db,
            c("t").as_ptr(),
            c(&format!(r#"{{"_id": {{"$id": "{id}"}}}}"#)).as_ptr(),
            &mut again,
        )
    };
    let detail = fails(code, TRUNKDB_DUPLICATE_ID);
    assert_eq!(
        (detail["collection"].clone(), detail["id"].clone()),
        ("t".into(), id.into())
    );
    ok(unsafe { trunkdb_close(db) });

    ok(unsafe { trunkdb_open(path.as_ptr(), read_only.as_ptr(), &mut db) });
    let code = unsafe { trunkdb_insert(db, c("t").as_ptr(), c("{}").as_ptr(), &mut again) };
    assert_eq!(fails(code, TRUNKDB_READ_ONLY)["kind"], "read_only");
    ok(unsafe { trunkdb_close(db) });
}

/// Null where a value is needed, text that isn't UTF-8, a handle of
/// another kind, one closed twice: each an error, never a crash; null
/// to close or free, nothing.
#[test]
fn misuse_that_can_be_caught() {
    let db = Open::new();
    let t = c("t");
    let mut out: *mut c_char = std::ptr::dangling_mut();
    let mut n = 0;
    let code = unsafe { trunkdb_find(null(), null(), t.as_ptr(), null(), null(), &mut out) };
    assert!(
        fails(code, TRUNKDB_INVALID_ARGUMENT)["message"]
            .as_str()
            .unwrap()
            .contains("db is null")
    );
    assert!(out.is_null(), "reset before the failure");
    let code = unsafe { trunkdb_count(db.db, null(), null(), null(), null(), &mut n) };
    assert_eq!(message(), "collection is null");
    fails(code, TRUNKDB_INVALID_ARGUMENT);
    let code = unsafe { trunkdb_count(db.db, null(), t.as_ptr(), null(), null(), null_mut()) };
    assert_eq!(message(), "count is null");
    fails(code, TRUNKDB_INVALID_ARGUMENT);
    let code = unsafe { trunkdb_insert(db.db, t.as_ptr(), null(), &mut out) };
    assert_eq!(message(), "document is null");
    fails(code, TRUNKDB_INVALID_ARGUMENT);

    let bad = [b'a', 0xff, b'b', 0];
    let code = unsafe { trunkdb_count(db.db, null(), bad.as_ptr().cast(), null(), null(), &mut n) };
    fails(code, TRUNKDB_INVALID_ARGUMENT);
    assert!(
        message().contains("collection is not UTF-8"),
        "{}",
        message()
    );

    let mut cursor = null_mut();
    ok(unsafe { trunkdb_cursor_open(db.db, null(), t.as_ptr(), null(), null(), &mut cursor) });
    let as_db = cursor.cast::<trunkdb_db>();
    let code = unsafe { trunkdb_count(as_db, null(), t.as_ptr(), null(), null(), &mut n) };
    fails(code, TRUNKDB_INVALID_ARGUMENT);
    assert_eq!(message(), "db is not an open database handle");
    let code = unsafe { trunkdb_close(as_db) };
    fails(code, TRUNKDB_INVALID_ARGUMENT);
    let as_snapshot = cursor.cast::<trunkdb_snapshot>();
    let code = unsafe { trunkdb_count(db.db, as_snapshot, t.as_ptr(), null(), null(), &mut n) };
    fails(code, TRUNKDB_INVALID_ARGUMENT);
    assert_eq!(message(), "snapshot is not an open snapshot handle");
    ok(unsafe { trunkdb_cursor_close(cursor) });

    let mut snap = null_mut();
    ok(unsafe { trunkdb_snapshot_open(db.db, &mut snap) });
    ok(unsafe { trunkdb_snapshot_close(snap) });
    let code = unsafe { trunkdb_snapshot_close(snap) };
    fails(code, TRUNKDB_INVALID_ARGUMENT);

    ok(unsafe { trunkdb_close(null_mut()) });
    ok(unsafe { trunkdb_snapshot_close(null_mut()) });
    ok(unsafe { trunkdb_cursor_close(null_mut()) });
    unsafe { trunkdb_string_free(null_mut()) };
}

/// The last error stays until the thread's next failure, and is the
/// thread's own.
#[test]
fn the_last_error_is_the_threads() {
    let db = Open::new();
    let mut n = 0;
    let t = c("t");
    fails(
        unsafe { trunkdb_count(db.db, null(), t.as_ptr(), c("(").as_ptr(), null(), &mut n) },
        TRUNKDB_PARSE,
    );
    let kept = message();
    ok(unsafe { trunkdb_count(db.db, null(), t.as_ptr(), null(), null(), &mut n) });
    assert_eq!(message(), kept, "a success leaves it");
    let other = std::thread::spawn(|| {
        let text = unsafe { CStr::from_ptr(trunkdb_error_message()) };
        text.to_str().unwrap().to_string()
    });
    assert_eq!(other.join().unwrap(), "", "another thread's is its own");
}

/// A panic inside a call comes back as `TRUNKDB_PANIC` with its message.
#[test]
fn a_panic_stops_at_the_boundary() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let code = error::run(|| panic!("boom"));
    std::panic::set_hook(hook);
    let detail = fails(code, TRUNKDB_PANIC);
    assert_eq!(detail["kind"], "panic");
    assert_eq!(message(), "trunkdb panicked: boom");
}

/// A pointer moved to another thread, as C moves one.
struct Shared<T>(*mut T);
impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Shared<T> {}
unsafe impl<T> Send for Shared<T> {}
unsafe impl<T> Sync for Shared<T> {}

/// One database handle used by eight threads at once; one cursor by two,
/// each document handed out once.
#[test]
fn threads_share_a_handle() {
    let db = Open::new();
    let shared = Shared(db.db);
    let threads: Vec<_> = (0..8)
        .map(|n| {
            std::thread::spawn(move || {
                let shared = shared;
                for i in 0..25 {
                    let mut id = null_mut();
                    let doc = c(&format!(r#"{{"thread": {n}, "i": {i}}}"#));
                    ok(unsafe { trunkdb_insert(shared.0, c("t").as_ptr(), doc.as_ptr(), &mut id) });
                    take(id);
                    let mut count = 0;
                    ok(unsafe {
                        trunkdb_count(
                            shared.0,
                            null(),
                            c("t").as_ptr(),
                            null(),
                            null(),
                            &mut count,
                        )
                    });
                    assert!(count > 0);
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(db.count("t", ""), 200);

    let mut cursor = null_mut();
    ok(unsafe { trunkdb_cursor_open(db.db, null(), c("t").as_ptr(), null(), null(), &mut cursor) });
    let cursor = Shared(cursor);
    let readers: Vec<_> = (0..2)
        .map(|_| {
            std::thread::spawn(move || {
                let cursor = cursor;
                let mut seen = Vec::new();
                loop {
                    let mut doc = null_mut();
                    ok(unsafe { trunkdb_cursor_next(cursor.0, &mut doc) });
                    match take(doc) {
                        Some(doc) => seen.push(doc),
                        None => return seen,
                    }
                }
            })
        })
        .collect();
    let mut all: Vec<String> = readers
        .into_iter()
        .flat_map(|r| r.join().unwrap())
        .collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 200, "each once");
    ok(unsafe { trunkdb_cursor_close(cursor.0) });
}

/// `include/trunkdb.h` is what the code says: made by cbindgen and
/// compared; `TRUNKDB_WRITE_HEADER=1` writes it anew.
#[test]
fn header_is_current() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let config = cbindgen::Config::from_file(format!("{dir}/cbindgen.toml")).unwrap();
    let mut made = Vec::new();
    cbindgen::Builder::new()
        .with_crate(dir)
        .with_config(config)
        .generate()
        .expect("cbindgen reads the crate")
        .write(&mut made);
    let path = format!("{dir}/include/trunkdb.h");
    if std::env::var_os("TRUNKDB_WRITE_HEADER").is_some() {
        std::fs::write(&path, &made).unwrap();
    }
    let kept = std::fs::read(&path).unwrap_or_default();
    assert!(
        kept == made,
        "include/trunkdb.h isn't what the code says: TRUNKDB_WRITE_HEADER=1 cargo test -p trunkdb-ffi header"
    );
}
