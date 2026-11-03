//! Parameters in a filter's text (SPEC §163): `:name` where a value or a
//! count stands, read once as a `Template` and bound to values, each
//! bound text the same as the text with the values written out.

use trunkdb::query::{Aggregate, BindError, Condition, Filter, Params, Template};
use trunkdb::{Collection, Database, DateTime, DocId, Document};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("parameters.trunkdb")).unwrap();
    (dir, db)
}

fn template<T>(text: &str) -> Template<T>
where
    Template<T>: std::str::FromStr<Err = trunkdb::query::ParseFilterError>,
{
    text.parse().unwrap_or_else(|e| panic!("{text}: {e}"))
}

/// 60 tasks: `n`, a `status` of three, a `title`, `tags`, and some
/// with a `due` date-time or a `done` of null; an index on `status`.
fn tasks(db: &Database) -> Collection<Document> {
    let tasks = db.collection::<Document>("tasks");
    for n in 0..60i64 {
        let status = ["open", "done", "waiting"][n as usize % 3];
        let mut doc: Document = format!(
            r#"{{"n": {n}, "status": "{status}", "title": "Task {n}", "tags": ["t{}", "t{}"]}}"#,
            n % 4,
            n % 5
        )
        .parse()
        .unwrap();
        let Document::Object(map) = &mut doc else {
            unreachable!()
        };
        if n % 7 == 0 {
            map.insert("done".into(), Document::Null);
        }
        if n % 2 == 0 {
            let due = DateTime::from_unix(1_800_000_000 + n * 86_400, 0).unwrap();
            map.insert("due".into(), Document::DateTime(due));
        }
        tasks.insert(doc).unwrap();
    }
    tasks.ensure_index(["status"]).unwrap();
    tasks
}

fn found(tasks: &Collection<Document>, filter: Filter) -> Vec<String> {
    (tasks.find(filter).unwrap().iter())
        .map(|doc| doc.to_string())
        .collect()
}

/// Every place a parameter stands (§163.1): the template bound reads as
/// the text with the value written out, prints the same, and finds the
/// same.
#[test]
fn a_bound_template_is_its_text_with_the_values_written_out() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let due = DateTime::from_unix(1_800_000_000 + 10 * 86_400, 0).unwrap();
    let cases: [(&str, Params, &str); 15] = [
        (
            "status = :s",
            Params::new().set("s", "open"),
            r#"status = "open""#,
        ),
        (
            "status != :s",
            Params::new().set("s", "open"),
            r#"status != "open""#,
        ),
        ("n < :n", Params::new().set("n", 7), "n < 7"),
        ("n <= :n", Params::new().set("n", 7), "n <= 7"),
        (
            "n > :n and n >= :m",
            Params::new().set("n", 7).set("m", 8.5),
            "n > 7 and n >= 8.5",
        ),
        (
            "tags contains :t",
            Params::new().set("t", "t3"),
            r#"tags contains "t3""#,
        ),
        (
            "title starts with :p",
            Params::new().set("p", "Task 1"),
            r#"title starts with "Task 1""#,
        ),
        (
            "due < :d",
            Params::new().set("d", due),
            &format!(r#"due < date("{due}")"#),
        ),
        (
            "done = :x",
            Params::new().set("x", Document::Null),
            "done = null",
        ),
        (
            r#"status in ("waiting", :s)"#,
            Params::new().set("s", "open"),
            r#"status in ("waiting", "open")"#,
        ),
        (
            "status in :list",
            Params::new().set("list", Document::Array(vec!["open".into(), "done".into()])),
            r#"status in ("open", "done")"#,
        ),
        (
            "n not in :list",
            Params::new().set("list", Document::Array(vec![])),
            "n not in ()",
        ),
        (
            "size(tags) >= :n",
            Params::new().set("n", 2),
            "size(tags) >= 2",
        ),
        (
            "status = :s order by n desc limit :n offset :k",
            Params::new().set("s", "done").set("n", 3).set("k", 2),
            r#"status = "done" order by n desc limit 3 offset 2"#,
        ),
        (
            "tags has (@ = :t) and n > :t2",
            Params::new().set("t", "t1").set("t2", 30),
            r#"tags has (@ = "t1") and n > 30"#,
        ),
    ];
    for (text, params, written) in cases {
        let bound = template::<Filter>(text).bind(&params).unwrap();
        let plain: Filter = written.parse().unwrap();
        assert_eq!(bound.to_string(), plain.to_string(), "{text}");
        assert_eq!(found(&tasks, bound), found(&tasks, plain), "{text}");
    }
}

/// A condition and an aggregate take parameters too, in `having` and a
/// column's `filter (where …)` and where their counts stand.
#[test]
fn conditions_and_aggregates_are_templates_too() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let condition = template::<Condition>("n >= :lo and n < :hi")
        .bind(&Params::new().set("lo", 10).set("hi", 20))
        .unwrap();
    assert_eq!(condition.to_string(), "n >= 10 and n < 20");

    let text = "count() as n, count() filter (where n > :big) as big group by status \
                having n > :min order by n desc limit :k";
    let bound = template::<Aggregate>(text)
        .bind(&Params::new().set("big", 40).set("min", 5).set("k", 2))
        .unwrap();
    let plain: Aggregate = "count() as n, count() filter (where n > 40) as big group by status \
                            having n > 5 order by n desc limit 2"
        .parse()
        .unwrap();
    assert_eq!(bound.to_string(), plain.to_string());
    let rows = |aggregate: &Aggregate| {
        (tasks.aggregate(Filter::new(), aggregate).unwrap().iter())
            .map(|row| row.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(rows(&bound), rows(&plain));
    assert_eq!(rows(&bound).len(), 2);
}

/// §163.2: a value is never read as text. Whatever a string holds, it
/// is compared as that string, and finds what `Filter::eq` with it finds.
#[test]
fn a_value_never_changes_what_the_text_means() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    tasks
        .insert(
            r#"{"title": "x\" or true or title = \"y", "n": -1}"#
                .parse()
                .unwrap(),
        )
        .unwrap();
    let by_title = template::<Filter>("title = :title");
    for sneaky in [
        r#"x" or true or title = "y"#,
        r#"" or true or title = ""#,
        ") or (true",
        ":title",
        "Task 1\" or n > 0 or title = \"",
        "",
    ] {
        let bound = by_title.bind(&Params::new().set("title", sneaky)).unwrap();
        let builder = Filter::new().eq("title", sneaky);
        let got = found(&tasks, bound);
        assert_eq!(got, found(&tasks, builder), "{sneaky}");
        assert!(got.len() <= 1, "{sneaky}: {got:?}");
    }
}

/// A value keeps its type: each kind of `Document` bound finds what
/// `Filter::eq` with the same value finds, and `"5"` isn't `5`.
#[test]
fn every_kind_of_value_binds_as_itself() {
    let (_dir, db) = open();
    let things = db.collection::<Document>("things");
    let id: DocId = "0192f0c4-7d7a-7c43-9e0b-1f6d1b8f0a11".parse().unwrap();
    let when = DateTime::from_unix(1_700_000_000, 5).unwrap();
    let values: Vec<Document> = vec![
        Document::Int(5),
        Document::String("5".into()),
        Document::Float(5.0),
        Document::Float(f64::NAN),
        Document::Bool(true),
        Document::Null,
        Document::Id(id),
        Document::DateTime(when),
        Document::Binary(vec![0, 1, 255]),
        Document::Array(vec![Document::Int(1), Document::String("a".into())]),
        r#"{"a": 1, "b": [true]}"#.parse().unwrap(),
    ];
    for (n, value) in values.iter().enumerate() {
        let mut doc: Document = format!(r#"{{"n": {n}}}"#).parse().unwrap();
        let Document::Object(map) = &mut doc else {
            unreachable!()
        };
        map.insert("v".into(), value.clone());
        things.insert(doc).unwrap();
    }
    let equal = template::<Filter>("v = :v");
    for value in &values {
        let bound = equal.bind(&Params::new().set("v", value.clone())).unwrap();
        let builder = Filter::new().eq("v", value.clone());
        assert_eq!(bound.to_string(), builder.to_string());
        assert_eq!(found(&things, bound), found(&things, builder), "{value}");
    }
    let five =
        |value: Document| found(&things, equal.bind(&Params::new().set("v", value)).unwrap()).len();
    // `5` and `5.0` are one number (§107); the string is the string.
    assert_eq!(five(Document::Int(5)), 2);
    assert_eq!(five(Document::String("5".into())), 1);
}

/// §163.4: each refusal, with the parameter and where it first stands.
#[test]
fn binding_refuses_what_doesnt_fit() {
    let t = template::<Filter>("a = :a and b in :list and c = :a limit :n");
    let fits = || {
        Params::new()
            .set("a", 1)
            .set("list", Document::Array(vec![]))
            .set("n", 3)
    };
    assert!(t.bind(&fits()).is_ok());

    let missing = t.bind(&Params::new().set("a", 1).set("n", 3)).unwrap_err();
    assert_eq!(
        missing,
        BindError::Missing {
            name: "list".into(),
            offset: 16
        }
    );
    assert_eq!(missing.to_string(), "no value for the parameter :list");
    assert_eq!((missing.name(), missing.offset()), ("list", Some(16)));

    // The first in the text, the one at 4, though `:a` stands twice.
    let none = t.bind(&Params::new()).unwrap_err();
    assert_eq!((none.name(), none.offset()), ("a", Some(4)));

    let unused = t.bind(&fits().set("b", 2)).unwrap_err();
    assert_eq!(unused, BindError::Unused { name: "b".into() });
    assert_eq!(unused.offset(), None);
    assert_eq!(
        unused.to_string(),
        "a value for :b, which the text has no parameter of"
    );

    let not_array = t.bind(&fits().set("list", "open")).unwrap_err();
    assert_eq!(
        not_array,
        BindError::NotAnArray {
            name: "list".into(),
            offset: 16
        }
    );
    for count in [
        Document::Int(-1),
        Document::Float(3.0),
        Document::String("3".into()),
    ] {
        let error = t.bind(&fits().set("n", count.clone())).unwrap_err();
        assert_eq!(
            error,
            BindError::NotACount {
                name: "n".into(),
                offset: 39
            },
            "{count}"
        );
    }
    let size = template::<Filter>("size(tags) > :n");
    assert!(size.bind(&Params::new().set("n", true)).is_err());
    assert!(size.bind(&Params::new().set("n", 0)).is_ok());
}

/// §163.3: the names in the order they first stand, each once; the text
/// printed as it was read; any number of binds; a text without
/// parameters a template too.
#[test]
fn a_template_reads_once_and_binds_again_and_again() {
    let text = "b = :b and (a = :a or b > :b)\norder by a limit :n";
    let t = template::<Filter>(text);
    assert_eq!(t.params(), ["b", "a", "n"]);
    assert_eq!(t.to_string(), text);
    assert_eq!(template::<Filter>(&t.to_string()), t);

    let copy = t.clone();
    let handle = std::thread::spawn(move || {
        (0..50)
            .map(|n| {
                let params = Params::new().set("a", n).set("b", "x").set("n", n);
                copy.bind(&params).unwrap().to_string()
            })
            .collect::<Vec<_>>()
    });
    let texts = handle.join().unwrap();
    assert_eq!(
        texts[7],
        r#"b = "x" and (a = 7 or b > "x") order by a limit 7"#
    );

    let none = template::<Filter>("status = \"open\" limit 5");
    assert!(none.params().is_empty());
    assert_eq!(
        none.bind(&Params::new()).unwrap().to_string(),
        r#"status = "open" limit 5"#
    );
    // Setting a name again replaces its value.
    let again = Params::new().set("s", 1).set("s", 2);
    let bound = template::<Filter>("a = :s").bind(&again).unwrap();
    assert_eq!(bound.to_string(), "a = 2");
}

/// `set_value` takes anything `Serialize`, as a document is made.
#[test]
fn a_value_of_the_application_s_own_type() {
    #[derive(serde::Serialize)]
    struct Owner {
        name: String,
        team: u8,
    }
    let owner = Owner {
        name: "Ada".into(),
        team: 3,
    };
    let params = Params::new().set_value("o", &owner).unwrap();
    let bound = template::<Filter>("owner = :o").bind(&params).unwrap();
    assert_eq!(bound.to_string(), r#"owner = {"name":"Ada","team":3}"#);
}

/// The planner sees a bound filter as the text written out: the same
/// plan through the index.
#[test]
fn a_bound_filter_is_planned_as_the_text_written_out() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let bound = template::<Filter>("status = :s and n > :n limit :k")
        .bind(&Params::new().set("s", "waiting").set("n", 10).set("k", 4))
        .unwrap();
    let plain: Filter = r#"status = "waiting" and n > 10 limit 4"#.parse().unwrap();
    assert_eq!(
        format!("{:?}", tasks.explain(&bound).unwrap()),
        format!("{:?}", tasks.explain(&plain).unwrap())
    );
}
