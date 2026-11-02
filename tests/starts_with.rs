//! `starts with` (SPEC §158): a string beginning with a prefix, case
//! counting, read from an index as the range of keys that begin with it.

use trunkdb::query::{Condition, Filter, Op, QueryPlan};
use trunkdb::{Collection, Database, DocId, Document};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("starts_with.trunkdb")).unwrap();
    (dir, db)
}

/// Titles that do and don't begin with "Fix", in either case, of every
/// length up to past a key's budget, and values that aren't strings.
const TITLES: [&str; 12] = [
    "Fix login",
    "Fix",
    "Fixture cleanup",
    "fix typo",
    "FIX build",
    "A Fix later",
    "Add export",
    "Fi",
    "",
    "Fix\0nul",
    "Écrire",
    "Ünicode",
];

/// 600 tasks: `title` one of `TITLES`, a long one, a number, null, missing
/// or an array of titles; `team` "a" or "b". Indexes on `title` and on
/// `(team, title)`.
fn tasks(db: &Database) -> Collection<Document> {
    let tasks = db.collection::<Document>("tasks");
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
                Document::String("other".into()),
            ])),
            i => Some(Document::String(TITLES[i % TITLES.len()].into())),
        };
        if let (Document::Object(fields), Some(title)) = (&mut doc, title) {
            fields.insert("title".into(), title);
        }
        tasks.insert(doc).unwrap();
    }
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

/// The ids whose title, worked out here, begins with `prefix`.
fn expected(tasks: &Collection<Document>, path: &str, prefix: &str) -> Vec<DocId> {
    let all = tasks.find(Filter::new().no_index()).unwrap();
    let mut ids: Vec<DocId> = all
        .iter()
        .filter(|doc| {
            let values: Vec<&Document> = match (path, doc.get("title")) {
                ("title[*]", Some(Document::Array(items))) => items.iter().collect(),
                ("title[*]", Some(other)) => vec![other],
                (_, Some(value)) => vec![value],
                (_, None) => vec![],
            };
            values
                .iter()
                .any(|v| matches!(v, Document::String(s) if s.starts_with(prefix)))
        })
        .map(|doc| match doc.get("_id") {
            Some(Document::Id(id)) => *id,
            _ => unreachable!(),
        })
        .collect();
    ids.sort();
    ids
}

#[test]
fn it_finds_what_a_scan_finds_through_an_index_and_without() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let long = format!("Fix {}", "x".repeat(1200));
    let prefixes = [
        "Fix", "fix", "Fi", "", "Fix ", "Fixture", "F", "\0", "Fix\0", "É",
    ];
    for prefix in prefixes.iter().copied().chain([long.as_str()]) {
        let want = expected(&tasks, "title", prefix);
        let indexed = Filter::new().starts_with("title", prefix);
        assert_eq!(
            ids(tasks.find(indexed.clone()).unwrap()),
            want,
            "{prefix:?}"
        );
        assert_eq!(
            tasks.count(indexed.clone()).unwrap(),
            want.len(),
            "{prefix:?}"
        );
        let scanned = indexed.clone().no_index();
        assert_eq!(ids(tasks.find(scanned).unwrap()), want, "{prefix:?}");
        // In memory, and as text read back.
        let all = tasks.find(Filter::new()).unwrap();
        assert_eq!(ids(indexed.apply(all)), want, "{prefix:?}");
        let text: Filter = indexed.to_string().parse().unwrap();
        assert_eq!(
            format!("{:?}", text),
            format!("{:?}", indexed),
            "{prefix:?}"
        );
        // Through the compound index, with the team fixed.
        let team = Filter::new().eq("team", "b").starts_with("title", prefix);
        let found = ids(tasks.find(team).unwrap());
        let in_team = ids(tasks.find(Filter::new().eq("team", "b")).unwrap());
        let want_b: Vec<DocId> = want
            .iter()
            .filter(|id| in_team.contains(id))
            .copied()
            .collect();
        assert_eq!(found, want_b, "{prefix:?}");
    }
    // Each element of an array, with `[*]`; the array itself without.
    let elements = Filter::new().starts_with("title[*]", "Fix o");
    let want = expected(&tasks, "title[*]", "Fix o");
    assert!(!want.is_empty());
    assert_eq!(ids(tasks.find(elements).unwrap()), want);
    let whole = Filter::new().starts_with("title", "Fix o");
    assert!(tasks.find(whole).unwrap().is_empty());
}

#[test]
fn case_counts_and_only_strings_match() {
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
        titles(Filter::new().starts_with("title", "Fix")),
        ["Fix", "Fix\0nul", "Fix login", "Fixture cleanup"]
    );
    assert_eq!(
        titles(Filter::new().starts_with("title", "fix")),
        ["fix typo"]
    );
    // A number is no string, nor a prefix of one.
    let number = Condition::compare("title", Op::StartsWith, 1);
    assert!(tasks.find(Filter::new().and(number)).unwrap().is_empty());
    let numbered = Filter::new().starts_with("n", "1");
    assert!(tasks.find(numbered).unwrap().is_empty());
    // Decided so on the compound index's entries, reading no document.
    let in_team = Filter::new()
        .eq("team", "a")
        .and(Condition::compare("title", Op::StartsWith, 1))
        .select(["_id"]);
    let explained = tasks.explain_run(in_team).unwrap();
    assert_eq!(explained.stats.documents_returned, 0);
    assert_eq!(explained.stats.documents_examined, 0);
    // `not` takes in what isn't a string, or is missing.
    let not_fix = Filter::new().and(!Condition::starts_with("title", "F"));
    let all = tasks.count(Filter::new()).unwrap();
    let fix = tasks
        .count(Filter::new().starts_with("title", "F"))
        .unwrap();
    assert_eq!(tasks.count(not_fix).unwrap(), all - fix);
}

#[test]
fn an_index_reads_only_the_keys_that_begin_with_the_prefix() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    let filter = Filter::new().starts_with("title", "Fix");
    assert!(matches!(
        tasks.explain(&filter).unwrap(),
        QueryPlan::Index { ref field, .. } if field == "title"
    ));
    let explained = tasks.explain_run(filter).unwrap();
    let found = explained.stats.documents_returned;
    assert_eq!(found as usize, expected(&tasks, "title", "Fix").len());
    // Every key read begins with "Fix", so every document read is found.
    assert_eq!(explained.stats.keys_examined, found);
    assert_eq!(explained.stats.documents_examined, found);

    // With the team fixed, the compound index's range is the team's
    // titles that begin with it.
    let team = Filter::new().eq("team", "a").starts_with("title", "Fix");
    let explained = tasks.explain_run(team).unwrap();
    assert!(explained.stats.documents_returned > 0);
    assert_eq!(
        explained.stats.keys_examined,
        explained.stats.documents_returned
    );
}

#[test]
fn the_entries_decide_it_without_reading_a_document() {
    let (_dir, db) = open();
    let tasks = tasks(&db);
    // Short titles decide on their keys; the long ones are cut in the
    // key, but begin with the whole prefix, which decides them too. Only
    // ids are asked for, which the entries hold.
    for prefix in ["Fix", "Fixt", "A", "Fix x"] {
        let explained = tasks
            .explain_run(Filter::new().starts_with("title", prefix).select(["_id"]))
            .unwrap();
        let want = expected(&tasks, "title", prefix).len() as u64;
        assert_eq!(explained.stats.documents_returned, want, "{prefix:?}");
        assert_eq!(explained.stats.documents_examined, 0, "{prefix:?}");
    }
}

#[test]
fn its_text_is_two_words_after_a_field() {
    let filter: Filter = r#"title starts with "Fix" and starts = 1"#.parse().unwrap();
    let built = Filter::new().starts_with("title", "Fix").eq("starts", 1);
    assert_eq!(format!("{:?}", filter), format!("{:?}", built));
    assert_eq!(
        filter.to_string(),
        r#"title starts with "Fix" and starts = 1"#
    );
    let any_case: Filter = r#"title STARTS With "a""#.parse().unwrap();
    assert_eq!(
        format!("{:?}", any_case),
        format!("{:?}", Filter::new().starts_with("title", "a"))
    );
    let field_named_starts: Filter = r#"starts starts with "a""#.parse().unwrap();
    let named = Filter::new().starts_with("starts", "a");
    assert_eq!(format!("{:?}", field_named_starts), format!("{:?}", named));
    let error = r#"title starts "a""#.parse::<Filter>().unwrap_err();
    // At `starts`, which isn't an operator without `with`.
    assert!(
        error
            .to_string()
            .contains("found `starts` at line 1 column 7"),
        "{error}"
    );
}
