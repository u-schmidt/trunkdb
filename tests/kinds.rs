//! A test of a value's kind (SPEC §159): `hours is string`, the
//! documents whose field holds a value of one kind, where others hold
//! another.

use trunkdb::query::{Aggregate, Condition, Filter, Kind, QueryPlan};
use trunkdb::{Collection, Database, DateTime, DocId, Document};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("kinds.trunkdb")).unwrap();
    (dir, db)
}

/// One `hours` per entry, with the kinds it is of; `None`, missing.
fn values() -> Vec<(Option<Document>, &'static [&'static str])> {
    vec![
        (Some(Document::Int(4)), &["int", "number"]),
        (Some(Document::Float(2.5)), &["float", "number"]),
        (Some(Document::Float(f64::NAN)), &["float", "number"]),
        (Some(Document::String("4".into())), &["string"]),
        (Some(Document::String(String::new())), &["string"]),
        (Some(Document::Bool(false)), &["bool"]),
        (Some(Document::Binary(vec![1, 2])), &["binary"]),
        (
            Some(Document::Array(vec![Document::String("a".into())])),
            &["array"],
        ),
        (Some(Document::Array(vec![])), &["array"]),
        (Some("{\"h\": 1}".parse().unwrap()), &["object"]),
        (Some(Document::Id(DocId([7; 16]))), &["id"]),
        (
            Some(Document::DateTime(DateTime::from_unix(0, 0).unwrap())),
            &["datetime"],
        ),
        (
            Some(Document::Date("2026-11-07".parse().unwrap())),
            &["date"],
        ),
        (Some(Document::Time("14:05:00".parse().unwrap())), &["time"]),
        (Some(Document::Null), &[]),
        (None, &[]),
    ]
}

/// Two documents per value of `values`, `n` its place, `team` "a" or
/// "b"; `tags` an array of `hours` and a string. Indexes on `hours` and
/// `team`.
fn tasks(db: &Database) -> Collection<Document> {
    let tasks = db.collection::<Document>("tasks");
    for (n, (hours, _)) in values().into_iter().enumerate() {
        for team in ["a", "b"] {
            let mut doc: Document = format!(r#"{{"n": {n}, "team": "{team}"}}"#)
                .parse()
                .unwrap();
            if let (Document::Object(fields), Some(hours)) = (&mut doc, hours.clone()) {
                let tags = Document::Array(vec![hours.clone(), Document::String("t".into())]);
                fields.insert("hours".into(), hours);
                fields.insert("tags".into(), tags);
            }
            tasks.insert(doc).unwrap();
        }
    }
    tasks.ensure_index("hours").unwrap();
    tasks.ensure_index("team").unwrap();
    tasks
}

/// The `n`s of what `filter` finds, sorted, each once.
fn ns(tasks: &Collection<Document>, filter: Filter) -> Vec<i64> {
    let mut ns: Vec<i64> = (tasks.find(filter).unwrap().iter())
        .map(|doc| doc.get("n").and_then(Document::as_i64).unwrap())
        .collect();
    ns.sort();
    ns.dedup();
    ns
}

#[test]
fn each_kind_finds_the_values_of_that_kind() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let values = values();
    let all: Vec<i64> = (0..values.len() as i64).collect();
    for &kind in Kind::ALL {
        let want: Vec<i64> = (values.iter().enumerate())
            .filter(|(_, (_, kinds))| kinds.contains(&kind.name()))
            .map(|(n, _)| n as i64)
            .collect();
        assert!(!want.is_empty(), "{kind:?}");
        let filter = Filter::new().is_kind("hours", kind);
        assert_eq!(ns(&tasks, filter.clone()), want, "{kind:?}");
        assert_eq!(tasks.count(filter.clone()).unwrap(), 2 * want.len());
        // In memory, and as text read back.
        assert_eq!(filter.to_string(), format!("hours is {}", kind.name()));
        let text: Filter = filter.to_string().parse().unwrap();
        assert_eq!(ns(&tasks, text), want, "{kind:?}");
        let found = filter.apply(tasks.find(Filter::new()).unwrap());
        assert_eq!(found.len(), 2 * want.len(), "{kind:?}");
        // Its NOT, null and missing included.
        let not = Filter::new().and(!Condition::is_kind("hours", kind));
        assert_eq!(not.to_string(), format!("hours is not {}", kind.name()));
        let rest: Vec<i64> = all.iter().filter(|n| !want.contains(n)).copied().collect();
        assert_eq!(ns(&tasks, not.clone()), rest, "{kind:?}");
        let text: Filter = not.to_string().parse().unwrap();
        assert_eq!(ns(&tasks, text), rest, "{kind:?}");
        // With a condition an index serves.
        let in_team = Filter::new().eq("team", "b").is_kind("hours", kind);
        assert!(matches!(
            tasks.explain(&in_team).unwrap(),
            QueryPlan::Index { .. }
        ));
        assert_eq!(tasks.count(in_team).unwrap(), want.len(), "{kind:?}");
    }
}

#[test]
fn an_array_is_an_array_and_its_elements_are_read_with_a_star() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let with_tags = (values().iter())
        .filter(|(hours, _)| hours.is_some())
        .count();
    // Every `tags` is an array; none is a string itself.
    assert_eq!(
        ns(&tasks, Filter::new().is_kind("tags", Kind::Array)).len(),
        with_tags
    );
    assert!(ns(&tasks, Filter::new().is_kind("tags", Kind::String)).is_empty());
    // Each has a string among its elements, and some a number.
    let strings = Filter::new().is_kind("tags[*]", Kind::String);
    assert_eq!(ns(&tasks, strings).len(), with_tags);
    let numbers = Filter::new().is_kind("tags[*]", Kind::Number);
    assert_eq!(ns(&tasks, numbers), [0, 1, 2]);
    let text: Filter = "tags[*] is number".parse().unwrap();
    assert_eq!(ns(&tasks, text), [0, 1, 2]);
    // An element that is an array is one, a level down.
    let nested = Filter::new().is_kind("tags[*]", Kind::Array);
    assert_eq!(ns(&tasks, nested), [7, 8]);
}

#[test]
fn the_odd_values_of_a_field_are_found_as_an_agent_wants_them() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    // "hours that aren't numbers", stored ones only.
    let odd: Filter = "exists(hours) and hours is not number and hours is not null"
        .parse()
        .unwrap();
    assert_eq!(ns(&tasks, odd), [3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
    // A stored null, where `is null` also finds missing fields.
    let null: Filter = "exists(hours) and hours is null".parse().unwrap();
    assert_eq!(ns(&tasks, null), [14]);
    // In an aggregate's column condition, which reads the same text.
    let aggregate: Aggregate = "count() filter (where hours is string) as strings, count() as n"
        .parse()
        .unwrap();
    let rows = tasks.aggregate(Filter::new(), &aggregate).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("strings"), Some(&Document::Int(4)));
    assert_eq!(rows[0].get("n"), Some(&Document::Int(32)));
}

#[test]
fn its_text_is_a_kind_after_is() {
    let any_case: Filter = "hours IS String and x Is Not NUMBER".parse().unwrap();
    assert_eq!(any_case.to_string(), "hours is string and x is not number");
    // The names are no reserved words: fields may have them.
    let named: Filter = "string is string and number = 1 and date is date"
        .parse()
        .unwrap();
    assert_eq!(
        named.to_string(),
        "string is string and number = 1 and date is date"
    );
    // `null` keeps its meaning; a word that's no kind is an error.
    let null: Filter = "hours is null".parse().unwrap();
    assert_eq!(null.to_string(), "hours is null");
    let error = "hours is strng".parse::<Filter>().unwrap_err().to_string();
    assert!(
        error.contains("expected `null` or a kind (bool, int, float, number, string"),
        "{error}"
    );
    assert!(error.contains("column 10"), "{error}");
    let error = "hours is not".parse::<Filter>().unwrap_err().to_string();
    assert!(error.contains("expected `null` or a kind"), "{error}");
    // A NOT of an AND of one prints as the kind's NOT, and reads back so.
    let wrapped = Filter::new().and(!Condition::All(vec![Condition::is_kind("h", Kind::Id)]));
    assert_eq!(wrapped.to_string(), "h is not id");
}

#[test]
fn the_names_are_trunkdb_fields_names() {
    let names: Vec<&str> = Kind::ALL.iter().map(|kind| kind.name()).collect();
    assert_eq!(
        names,
        [
            "bool", "int", "float", "number", "string", "binary", "array", "object", "id",
            "datetime", "date", "time"
        ]
    );
    // A slice, so a kind added later doesn't change its type (SPEC §161.1).
    let all: &'static [Kind] = Kind::ALL;
    assert_eq!(all.len(), names.len());
    // Named for the variant it tests; written `datetime` since dates
    // without a time took `date` (SPEC §173).
    let now = Document::DateTime(DateTime::from_unix(0, 0).unwrap());
    assert!(Kind::DateTime.of(&now) && Kind::DateTime.name() == "datetime");
    let day = Document::Date(trunkdb::Date::UNIX_EPOCH);
    assert!(Kind::Date.of(&day) && !Kind::DateTime.of(&day) && !Kind::Date.of(&now));
    assert!(Kind::Time.of(&Document::Time(trunkdb::Time::MIDNIGHT)) && !Kind::Time.of(&day));
    assert!(Kind::Number.of(&Document::Int(1)) && Kind::Number.of(&Document::Float(1.0)));
    assert!(!Kind::Number.of(&Document::String("1".into())));
    assert!(Kind::ALL.iter().all(|kind| !kind.of(&Document::Null)));
}
