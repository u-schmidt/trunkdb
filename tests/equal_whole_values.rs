//! `=` and `!=` on arrays, objects and binary (SPEC §124): compared by
//! their contents, through every way of reading, with and without
//! indexes, and in groups.

use trunkdb::query::{Aggregate, Condition, Filter};
use trunkdb::{Collection, Database, Document, IndexOptions};

fn doc(text: &str) -> Document {
    text.parse().unwrap()
}

/// The `name`s of `docs`, in their order.
fn names(docs: &[Document]) -> Vec<String> {
    docs.iter()
        .map(|d| match d {
            Document::Object(fields) => match fields.get("name") {
                Some(Document::String(name)) => name.clone(),
                other => panic!("no name: {other:?}"),
            },
            _ => panic!("not an object"),
        })
        .collect()
}

/// `a` holds exactly the values the table of §124 asks for; `b` holds
/// equal ones written differently; `c` other ones; `d` none.
fn things(dir: &tempfile::TempDir) -> (Database, Collection<Document>) {
    let db = Database::open(dir.path().join("t.trunkdb")).unwrap();
    let c = db.collection::<Document>("things");
    for text in [
        r#"{"name": "a", "n": 1, "meta": {"k": 1, "j": [1, 2]}, "tags": ["a", "b"], "bin": {"$binary": "AAEC"}, "items": [{"sku": "A1", "qty": 3}]}"#,
        r#"{"name": "b", "n": 2, "meta": {"j": [1.0, 2], "k": 1.0}, "tags": ["a", "b"], "bin": {"$binary": "AAEC"}, "items": [{"qty": 3, "sku": "A1"}, {"sku": "B2", "qty": 1}]}"#,
        r#"{"name": "c", "n": 3, "meta": {"k": 1}, "tags": ["b", "a"], "bin": {"$binary": "AQ=="}, "items": [{"sku": "A1"}]}"#,
        r#"{"name": "d", "n": 4, "meta": null}"#,
    ] {
        c.insert(doc(text)).unwrap();
    }
    (db, c)
}

/// What each filter finds, the same through every way of reading.
const CASES: &[(&str, &[&str])] = &[
    (r#"meta = {"k": 1, "j": [1, 2]}"#, &["a", "b"]),
    (r#"meta != {"k": 1, "j": [1, 2]}"#, &["c", "d"]),
    (r#"meta = {"k": 1}"#, &["c"]),
    (r#"tags = ["a", "b"]"#, &["a", "b"]),
    (r#"tags != ["a", "b"]"#, &["c", "d"]),
    (r#"tags = ["b", "a"]"#, &["c"]),
    (r#"bin = {"$binary": "AAEC"}"#, &["a", "b"]),
    (r#"bin != {"$binary": "AAEC"}"#, &["c", "d"]),
    (
        r#"meta in ({"k": 1}, {"j": [1, 2], "k": 1})"#,
        &["a", "b", "c"],
    ),
    (r#"meta not in ({"k": 1}, {"j": [1, 2], "k": 1})"#, &["d"]),
    (r#"not (meta = {"k": 1})"#, &["a", "b", "d"]),
    (r#"items has (@ = {"sku": "A1", "qty": 3})"#, &["a", "b"]),
    (r#"items[*] = {"sku": "A1"}"#, &["c"]),
    (r#"items[*] != {"sku": "A1"}"#, &["a", "b", "d"]),
    (r#"meta = {"k": 1} or n = 4"#, &["c", "d"]),
    // An array is a value, not its elements (§42); a value of another
    // kind is never equal.
    (r#"tags = "a""#, &[]),
    (r#"meta = ["k", 1]"#, &[]),
    (r#"meta.k = 1"#, &["a", "b", "c"]),
    // No order: none of these holds, for equal values neither.
    (r#"meta < {"k": 2}"#, &[]),
    (r#"meta <= {"k": 1}"#, &[]),
    (r#"tags >= ["a", "b"]"#, &[]),
    (r#"bin > {"$binary": "AA=="}"#, &[]),
];

fn check_every_way(c: &Collection<Document>, label: &str) {
    for (text, expected) in CASES {
        let condition: Condition = text.parse().unwrap();
        let filter = Filter::new().and(condition.clone()).sort_asc("name");
        let found = names(&c.find(filter.clone()).unwrap());
        assert_eq!(found, *expected, "{label}: find {text}");
        assert_eq!(
            c.count(filter.clone()).unwrap(),
            expected.len(),
            "{label}: count {text}"
        );
        let first = c.find_one(filter.clone()).unwrap();
        assert_eq!(
            first.map(|d| names(&[d])[0].clone()),
            expected.first().map(|s| s.to_string()),
            "{label}: find_one {text}"
        );
        let cursor: Vec<Document> = c
            .cursor(filter.clone())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(names(&cursor), *expected, "{label}: cursor {text}");
        let run = c.explain_run(filter.clone()).unwrap();
        assert_eq!(
            run.stats.documents_matched as usize,
            expected.len(),
            "{label}: explain {text}"
        );
        // The same text read as a whole filter, and in memory.
        let read: Filter = format!("{text} order by name").parse().unwrap();
        assert_eq!(
            names(&c.find(read).unwrap()),
            *expected,
            "{label}: text {text}"
        );
        let all = c.find(Filter::new().sort_asc("name")).unwrap();
        let in_memory: Vec<Document> = all.into_iter().filter(|d| condition.matches(d)).collect();
        assert_eq!(names(&in_memory), *expected, "{label}: matches {text}");
    }
}

#[test]
fn equal_arrays_objects_and_binary_are_found_by_a_scan() {
    let dir = tempfile::tempdir().unwrap();
    let (db, c) = things(&dir);
    check_every_way(&c, "scan");

    // A snapshot reads as the collection does.
    let snapshot = db.snapshot().unwrap();
    let view = snapshot.collection::<Document>("things");
    let filter = Filter::new()
        .and(r#"meta = {"j": [1, 2], "k": 1}"#.parse().unwrap())
        .sort_asc("name");
    assert_eq!(names(&view.find(filter).unwrap()), ["a", "b"]);
}

#[test]
fn every_index_finds_them_as_a_scan_does() {
    let dir = tempfile::tempdir().unwrap();
    let (_db, c) = things(&dir);
    for fields in [vec!["meta"], vec!["tags"], vec!["bin"], vec!["items[*]"]] {
        c.ensure_index(fields).unwrap();
    }
    // Compound indexes hold these values as their tag alone (§43): the
    // entries can't tell equal ones from others (§124.3).
    for fields in [["meta", "n"], ["tags", "n"], ["bin", "n"], ["n", "meta"]] {
        c.ensure_index(fields).unwrap();
    }
    check_every_way(&c, "indexed");
}

#[test]
fn a_compound_index_reads_the_document_only_where_its_entry_cant_tell() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.trunkdb")).unwrap();
    let c = db.collection::<Document>("things");
    for n in 0..200 {
        let meta = match n % 4 {
            0 => r#"{"k": 1}"#.to_string(),
            1 => r#"{"k": 2}"#.to_string(),
            _ => n.to_string(),
        };
        c.insert(doc(&format!(r#"{{"n": {n}, "meta": {meta}}}"#)))
            .unwrap();
    }
    c.ensure_index_with(["meta", "n"], IndexOptions::new())
        .unwrap();

    let equal = Filter::new().and(r#"meta = {"k": 1}"#.parse().unwrap());
    let run = c.explain_run(equal.clone()).unwrap();
    assert_eq!(run.stats.documents_matched, 50);
    assert_eq!(c.count(equal).unwrap(), 50);
    let not_equal = Filter::new().and(r#"meta != {"k": 1}"#.parse().unwrap());
    assert_eq!(c.count(not_equal).unwrap(), 150);

    // A single value: entries holding an object are decided on their
    // bytes, as never equal, without reading their documents.
    let single = Filter::new().eq("meta", 7);
    let run = c.explain_run(single).unwrap();
    assert_eq!(run.reads[0].index, "(meta, n)", "{run:?}");
    assert_eq!(run.stats.documents_matched, 1);
    assert!(run.stats.documents_examined <= 1, "{run:?}");
    // Against an object the same entries can't tell, and are read.
    let run = c
        .explain_run(Filter::new().and(r#"meta = {"k": 2}"#.parse().unwrap()))
        .unwrap();
    assert_eq!(run.stats.documents_matched, 50);

    // Read through an index whose entries otherwise decide (§104): a
    // range on `n`, `meta` its second value.
    c.ensure_index_with(["n", "meta"], IndexOptions::new())
        .unwrap();
    for (text, expected) in [
        (r#"n >= 0 and meta = {"k": 1}"#, 50),
        (r#"n >= 0 and meta != {"k": 1}"#, 150),
        (r#"n >= 100 and meta in ({"k": 1}, {"k": 2})"#, 50),
        (r#"n >= 0 and not (meta = {"k": 2})"#, 150),
    ] {
        let filter = Filter::new().and(text.parse().unwrap());
        let run = c.explain_run(filter.clone()).unwrap();
        assert_eq!(run.reads[0].index, "(n, meta)", "{text}: {run:?}");
        assert!(run.reads[0].covering, "{text}: entries decide the filter");
        assert_eq!(run.stats.documents_matched, expected, "{text}");
        assert_eq!(c.count(filter).unwrap() as u64, expected, "{text}: count");
    }
}

#[test]
fn a_sort_through_an_index_finds_them_in_its_look_for_unordered_values() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.trunkdb")).unwrap();
    let c = db.collection::<Document>("things");
    for n in 0..60 {
        let meta = if n % 3 == 0 {
            r#"{"k": 1}"#.to_string()
        } else {
            n.to_string()
        };
        c.insert(doc(&format!(r#"{{"n": {n}, "meta": {meta}}}"#)))
            .unwrap();
    }
    c.ensure_index("meta").unwrap();
    for limit in [1, 5, 20, 25] {
        let filter = Filter::new()
            .and(r#"meta = {"k": 1}"#.parse().unwrap())
            .sort_asc("meta")
            .limit(limit);
        assert_eq!(
            c.find(filter).unwrap().len(),
            limit.min(20),
            "limit {limit}"
        );
    }
    let unequal = Filter::new()
        .and(r#"meta != {"k": 1}"#.parse().unwrap())
        .sort_asc("meta")
        .limit(100);
    assert_eq!(c.find(unequal).unwrap().len(), 40);
}

#[test]
fn equal_values_are_one_group() {
    let dir = tempfile::tempdir().unwrap();
    let (_db, c) = things(&dir);
    for (field, groups) in [("meta", 3), ("tags", 3), ("bin", 3), ("items", 4)] {
        let rows = c
            .aggregate(
                Filter::new(),
                &Aggregate::new().group_by(field).count().named("n"),
            )
            .unwrap();
        assert_eq!(rows.len(), groups, "{field}: {rows:?}");
        // Each group's `=` finds what it counted (§107.2, §124.4).
        for row in &rows {
            let Document::Object(fields) = row else {
                panic!()
            };
            let value = fields.get(field).unwrap().clone();
            let Some(Document::Int(counted)) = fields.get("n") else {
                panic!()
            };
            let found = c.count(Filter::new().eq(field, value.clone())).unwrap();
            assert_eq!(found as i64, *counted, "{field} = {value}");
        }
    }
    // The row shows the first document's value by id: `a`'s, not `b`'s.
    let rows = c
        .aggregate(
            Filter::new().and(r#"name in ("a", "b")"#.parse().unwrap()),
            &Aggregate::new().group_by("meta").count().named("n"),
        )
        .unwrap();
    let Document::Object(fields) = &rows[0] else {
        panic!()
    };
    assert_eq!(
        fields.get("meta").unwrap().to_string(),
        r#"{"k":1,"j":[1,2]}"#
    );
}

#[test]
fn updates_add_to_set_and_pull_by_the_same_equality() {
    use trunkdb::query::Update;
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("t.trunkdb")).unwrap();
    let c = db.collection::<Document>("things");
    let id = c.insert(doc(r#"{"set": [{"a": 1, "b": [2]}]}"#)).unwrap();
    c.update_fields(
        Filter::new().id(id),
        &Update::new().add_to_set("set", doc(r#"{"b": [2.0], "a": 1}"#)),
    )
    .unwrap();
    assert_eq!(
        c.get(&id)
            .unwrap()
            .unwrap()
            .to_string()
            .matches("\"a\"")
            .count(),
        1
    );
    c.update_fields(
        Filter::new().id(id),
        &Update::new().pull("set", doc(r#"{"b": [2], "a": 1.0}"#)),
    )
    .unwrap();
    let found = c
        .find(Filter::new().and(r#"set = []"#.parse().unwrap()))
        .unwrap();
    assert_eq!(found.len(), 1);
}
