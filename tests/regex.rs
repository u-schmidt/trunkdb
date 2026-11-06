//! A regex in a filter (SPEC §169): `Filter::regex` and `matches` in
//! the text, a pattern with a fixed prefix read from an index as the
//! range `starts with` reads, any other checked on every document.
#![cfg(feature = "regex")]

use trunkdb::query::{BindError, Condition, Filter, Params, QueryPlan, Regex, Template};
use trunkdb::{Collection, Database, DocId, Document, Error};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("regex.trunkdb")).unwrap();
    (dir, db)
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap()
}

const TITLES: [&str; 12] = [
    "Fix login",
    "Fix",
    "Fixture cleanup",
    "fix typo",
    "FIX build",
    "A Fix later",
    "time-out in sync",
    "timeout",
    "",
    "Fix\0nul",
    "Écrire 2026",
    "Ünicode",
];

/// 600 tasks: `title` one of `TITLES`, a long one past a key's budget, a
/// number, null, missing or an array of titles; `team` "a" or "b".
/// Indexes on `title` and on `(team, title)`.
fn tasks(db: &Database) -> Collection<Document> {
    let tasks = db.collection::<Document>("tasks");
    let mut batch = db.batch();
    for n in 0..600usize {
        let team = ["a", "b"][n % 2];
        let mut doc: Document = format!(r#"{{"n": {n}, "team": "{team}"}}"#)
            .parse()
            .unwrap();
        let title = match n % 17 {
            12 => Some(Document::String(format!("Fix {}", "x".repeat(1100 + n)))),
            13 => Some(Document::Int(n as i64)),
            14 => Some(Document::Null),
            15 => None,
            16 => Some(Document::Array(vec![
                Document::String("Fix one".into()),
                Document::String("timeout".into()),
            ])),
            i => Some(Document::String(TITLES[i % TITLES.len()].into())),
        };
        if let (Document::Object(fields), Some(title)) = (&mut doc, title) {
            fields.insert("title".into(), title);
        }
        batch.insert(&tasks, doc).unwrap();
    }
    batch.commit().unwrap();
    tasks.ensure_index("title").unwrap();
    tasks.ensure_index(["team", "title"]).unwrap();
    tasks
}

fn ids(found: Vec<Document>) -> Vec<DocId> {
    let mut ids: Vec<DocId> = found
        .iter()
        .map(|doc| match doc.get("_id") {
            Some(Document::Id(id)) => *id,
            other => panic!("no id: {other:?}"),
        })
        .collect();
    ids.sort();
    ids
}

/// The ids whose title the `regex` crate itself matches, worked out
/// here; with `title[*]`, any element of an array, and nothing else.
fn expected(tasks: &Collection<Document>, path: &str, pattern: &str) -> Vec<DocId> {
    let regex = ::regex::Regex::new(pattern).unwrap();
    let all = tasks.find(Filter::new().no_index()).unwrap();
    let matching = all.into_iter().filter(|doc| {
        let values: Vec<&Document> = match (path, doc.get("title")) {
            ("title[*]", Some(Document::Array(items))) => items.iter().collect(),
            ("title[*]", _) => vec![],
            (_, Some(value)) => vec![value],
            (_, None) => vec![],
        };
        (values.iter()).any(|v| matches!(v, Document::String(s) if regex.is_match(s)))
    });
    ids(matching.collect())
}

const PATTERNS: [&str; 16] = [
    "^Fix",
    "^Fix$",
    "^Fix(ture)? ",
    "^Fixt?",
    "^Fi",
    "^Fix\\x00",
    "^É",
    "time-?out",
    "(?i)^fix",
    "(?i)FIX",
    "^$",
    "",
    "^Fix x{1200}",
    "^Fix|^A",
    "\\d{4}$",
    "^[FA]",
];

#[test]
fn it_finds_what_the_regex_crate_finds_through_an_index_and_without() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    for pattern in PATTERNS {
        let want = expected(&tasks, "title", pattern);
        let indexed = Filter::new().regex("title", re(pattern));
        let found = ids(tasks.find(indexed.clone()).unwrap());
        assert_eq!(found, want, "{pattern:?}");
        assert_eq!(
            tasks.count(indexed.clone()).unwrap(),
            want.len(),
            "{pattern:?}"
        );
        let scanned = indexed.clone().no_index();
        assert_eq!(ids(tasks.find(scanned).unwrap()), want, "{pattern:?}");
        // In memory, and as text read back.
        let all = tasks.find(Filter::new()).unwrap();
        assert_eq!(ids(indexed.apply(all)), want, "{pattern:?}");
        let text: Filter = indexed.to_string().parse().unwrap();
        assert_eq!(format!("{text:?}"), format!("{indexed:?}"), "{pattern:?}");
        // Through the compound index, with the team fixed.
        let team = Filter::new().eq("team", "b").regex("title", re(pattern));
        let in_team = ids(tasks.find(Filter::new().eq("team", "b")).unwrap());
        let want_b: Vec<DocId> = (want.iter())
            .filter(|id| in_team.contains(id))
            .copied()
            .collect();
        assert_eq!(ids(tasks.find(team).unwrap()), want_b, "{pattern:?}");
    }
    // Each element of an array with `[*]`; an array itself is no string.
    let want = expected(&tasks, "title[*]", "^time");
    assert_eq!(want.len(), 35);
    let elements = Filter::new().regex("title[*]", re("^time"));
    assert_eq!(ids(tasks.find(elements).unwrap()), want);
    let whole = Filter::new().regex("title", re("^Fix one"));
    assert!(tasks.find(whole).unwrap().is_empty());
}

/// SPEC §169.3: a pattern beginning `^` and fixed text reads the range
/// of keys that begin with it, every key read is a document read, as
/// the entries don't decide a regex; any other pattern scans.
#[test]
fn a_fixed_prefix_reads_the_index_range_and_nothing_else_does() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let starting = |prefix: &str| {
        tasks
            .count(Filter::new().starts_with("title", prefix))
            .unwrap()
    };
    for (pattern, prefix) in [("^Fix", "Fix"), ("^Fixt?ure", "Fix"), ("^Fix$", "Fix")] {
        let filter = Filter::new().regex("title", re(pattern));
        assert!(
            matches!(tasks.explain(&filter).unwrap(), QueryPlan::Index { ref field, .. } if field == "title"),
            "{pattern}"
        );
        let explained = tasks.explain_run(filter.select(["_id"])).unwrap();
        let read = starting(prefix) as u64;
        assert_eq!(explained.stats.keys_examined, read, "{pattern}");
        assert_eq!(
            explained.stats.documents_examined, read,
            "{pattern}: checked on each"
        );
        let want = expected(&tasks, "title", pattern).len() as u64;
        assert_eq!(explained.stats.documents_returned, want, "{pattern}");
    }
    for pattern in [
        "Fix",
        "(?i)^fix",
        "^Fix|^A",
        "(?m)^Fix",
        "^[Ff]ix",
        "time-?out",
    ] {
        let filter = Filter::new().regex("title", re(pattern));
        assert_eq!(
            tasks.explain(&filter).unwrap(),
            QueryPlan::Scan,
            "{pattern}"
        );
    }
    // With the team fixed, the compound index's range of the team's
    // titles beginning with the prefix.
    let team = Filter::new().eq("team", "a").regex("title", re("^Fix"));
    let explained = tasks.explain_run(team).unwrap();
    let in_team = tasks
        .count(Filter::new().eq("team", "a").starts_with("title", "Fix"))
        .unwrap();
    assert_eq!(explained.stats.keys_examined, in_team as u64);
    // An OR of two, as the union of their ranges.
    let either = Filter::new().any_of([
        Condition::regex("title", re("^Fix ")),
        Condition::regex("title", re("^A ")),
    ]);
    assert!(matches!(
        tasks.explain(&either).unwrap(),
        QueryPlan::IndexUnion { .. }
    ));
    let want = expected(&tasks, "title", "^Fix |^A ");
    assert_eq!(ids(tasks.find(either).unwrap()), want);
    // Inside `has`, through an index on the elements.
    tasks.ensure_index("title[*]").unwrap();
    let has = Filter::new().elem_match("title", Condition::regex("", re("^time")));
    assert!(matches!(
        tasks.explain(&has).unwrap(),
        QueryPlan::Index { .. }
    ));
    assert_eq!(
        ids(tasks.find(has).unwrap()),
        expected(&tasks, "title[*]", "^time")
    );
}

#[test]
fn case_counts_unless_the_pattern_says_and_only_strings_match() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let titles = |filter: Filter| -> Vec<String> {
        let mut titles: Vec<String> = (tasks.find(filter).unwrap().iter())
            .filter_map(|doc| doc.get("title")?.as_str().map(str::to_string))
            .filter(|title| title.len() < 100)
            .collect();
        titles.sort();
        titles.dedup();
        titles
    };
    assert_eq!(
        titles(Filter::new().regex("title", re("^fix"))),
        ["fix typo"]
    );
    assert_eq!(
        titles(Filter::new().regex("title", re("(?i)^fix "))),
        ["FIX build", "Fix login", "fix typo"]
    );
    assert_eq!(
        titles(Filter::new().regex("title", re("time-?out"))),
        ["time-out in sync", "timeout"]
    );
    // A number is no string: `n` holds numbers only.
    assert_eq!(tasks.count(Filter::new().regex("n", re("1"))).unwrap(), 0);
    assert_eq!(
        tasks.count(Filter::new().regex("nowhere", re(""))).unwrap(),
        0
    );
    // `not` takes in what isn't a string, or is missing.
    let all = tasks.count(Filter::new()).unwrap();
    let some = tasks.count(Filter::new().regex("title", re("^F"))).unwrap();
    let not = Filter::new().and(!Condition::regex("title", re("^F")));
    assert_eq!(tasks.count(not).unwrap(), all - some);
}

/// SPEC §169.2: `field matches "pattern"`, `matches` a word after a
/// field as `contains` is, the pattern a JSON string, and back.
#[test]
fn its_text_is_matches_and_a_string() {
    let filter: Filter = r#"title matches "^Fix" and matches = 1"#.parse().unwrap();
    let built = Filter::new().regex("title", re("^Fix")).eq("matches", 1);
    assert_eq!(format!("{filter:?}"), format!("{built:?}"));
    assert_eq!(
        filter.to_string(),
        r#"title matches "^Fix" and matches = 1"#
    );
    let any_case: Filter = r#"title MATCHES "a""#.parse().unwrap();
    assert_eq!(
        format!("{any_case:?}"),
        format!("{:?}", Filter::new().regex("title", re("a")))
    );
    // Backslashes as JSON writes them, and read back the same.
    let digits = Filter::new().regex("code", re(r#"^\d+"x"$"#));
    assert_eq!(digits.to_string(), r#"code matches "^\\d+\"x\"$""#);
    let back: Filter = digits.to_string().parse().unwrap();
    assert_eq!(format!("{back:?}"), format!("{digits:?}"));

    // A pattern that isn't a regex is a mistake at its string.
    let error = r#"title matches "(unclosed""#.parse::<Filter>().unwrap_err();
    assert_eq!(error.offset(), 14, "{error}");
    assert!(error.to_string().contains("not a regex"), "{error}");
    let error = "title matches 5".parse::<Filter>().unwrap_err();
    assert!(error.to_string().contains("a regex is a string"), "{error}");
    let error = "title matches".parse::<Filter>().unwrap_err();
    assert!(error.to_string().contains("value"), "{error}");
}

/// SPEC §169.2: the pattern as a parameter, its value checked to be a
/// string that is a regex when it is bound.
#[test]
fn the_pattern_is_a_parameter_too() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let template: Template<Filter> = "title matches :p".parse().unwrap();
    assert_eq!(template.params(), ["p"]);
    let bound = template.bind(&Params::new().set("p", "^Fix")).unwrap();
    assert_eq!(
        format!("{bound:?}"),
        format!("{:?}", Filter::new().regex("title", re("^Fix")))
    );
    assert_eq!(
        ids(tasks.find(bound).unwrap()),
        expected(&tasks, "title", "^Fix")
    );
    // Never read as text: a pattern with a quote in it is a pattern.
    let quoted = template.bind(&Params::new().set("p", r#"" or 1"#)).unwrap();
    assert_eq!(tasks.count(quoted).unwrap(), 0);

    let refused = |value: Document| template.bind(&Params::new().set("p", value)).unwrap_err();
    let BindError::NotAPattern {
        name,
        offset,
        reason,
        ..
    } = refused("(".into())
    else {
        panic!("expected NotAPattern");
    };
    assert_eq!((name.as_str(), offset), ("p", 14));
    assert!(reason.starts_with("not a regex"), "{reason}");
    let error = refused(Document::Int(5));
    assert!(
        error.to_string().contains("its value must be a string"),
        "{error}"
    );
    assert_eq!(error.offset(), Some(14));
}

/// A regex where any filter goes: a live query, an aggregate's column,
/// a batch's precondition, an update by filter.
#[test]
fn a_regex_goes_wherever_a_filter_does() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let fixes = || Filter::new().regex("title", re("^Fix "));
    let n = tasks.count(fixes()).unwrap();

    let live = tasks.watch(fixes()).unwrap();
    let mut entered = 0;
    while let Some(Ok(_)) = live.try_recv() {
        entered += 1;
    }
    assert_eq!(entered, n);
    tasks
        .insert(r#"{"title": "Fix later"}"#.parse().unwrap())
        .unwrap();
    tasks
        .insert(r#"{"title": "Later fix"}"#.parse().unwrap())
        .unwrap();
    assert!(matches!(live.try_recv(), Some(Ok(_))));
    assert!(live.try_recv().is_none(), "only the one it matches");

    let column = "count() filter (where title matches \"^Fix \") as fixes group by team";
    let rows = tasks
        .aggregate(Filter::new(), &column.parse().unwrap())
        .unwrap();
    let total: i64 = rows
        .iter()
        .map(|row| row.get("fixes").and_then(Document::as_i64).unwrap())
        .sum();
    assert_eq!(total as usize, n + 1);

    let mut batch = db.batch();
    batch
        .require_none(&tasks, Filter::new().regex("title", re("^Done")))
        .unwrap();
    batch
        .update_many(
            &tasks,
            fixes(),
            &trunkdb::query::Update::new().set("fix", true),
        )
        .unwrap();
    assert_eq!(batch.commit().unwrap().counts(), [0, n + 1]);
    assert_eq!(tasks.count(Filter::new().eq("fix", true)).unwrap(), n + 1);

    let mut batch = db.batch();
    batch
        .require(&tasks, Filter::new().regex("title", re("^Done")))
        .unwrap();
    assert!(matches!(
        batch.commit(),
        Err(Error::PreconditionFailed { .. })
    ));
}

/// A regex with a fixed prefix rules out null and missing fields, so a
/// sparse index, which has no entry for them, serves it (SPEC §44).
#[test]
fn a_sparse_index_serves_a_fixed_prefix() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    for n in 0..200 {
        let doc = match n % 4 {
            0 => format!(r#"{{"n": {n}, "code": "A-{n}"}}"#),
            1 => format!(r#"{{"n": {n}, "code": "B-{n}"}}"#),
            2 => format!(r#"{{"n": {n}, "code": null}}"#),
            _ => format!(r#"{{"n": {n}}}"#),
        };
        items.insert(doc.parse().unwrap()).unwrap();
    }
    items
        .ensure_index_with("code", trunkdb::IndexOptions::new().sparse())
        .unwrap();
    let a = Filter::new().regex("code", re("^A-1"));
    assert!(matches!(
        items.explain(&a).unwrap(),
        QueryPlan::Index { .. }
    ));
    assert_eq!(items.count(a).unwrap(), 27); // A-12, A-16, A-100 to A-196
    let any = Filter::new().regex("code", re("-1"));
    assert_eq!(items.explain(&any).unwrap(), QueryPlan::Scan);
    // A sparse compound index too, the regex on its first field.
    items.drop_index("code").unwrap();
    let sparse = trunkdb::IndexOptions::new().sparse();
    items.ensure_index_with(["code", "n"], sparse).unwrap();
    let a = Filter::new().regex("code", re("^A-1"));
    assert!(matches!(
        items.explain(&a).unwrap(),
        QueryPlan::Index { .. }
    ));
    assert_eq!(items.count(a).unwrap(), 27);
}
