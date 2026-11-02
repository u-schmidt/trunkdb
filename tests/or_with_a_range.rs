//! An `in` on a compound index's field, a range per value (SPEC §160):
//! `status in ("open", "late") and due < 50` reads, of an index on
//! `(status, due)`, each status's entries before 50, not all of them.

use trunkdb::query::{Condition, Filter, QueryPlan};
use trunkdb::{Collection, Database, DocId, Document};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("or_with_a_range.trunkdb")).unwrap();
    (dir, db)
}

const STATUSES: [&str; 4] = ["open", "late", "done", "cancelled"];

/// 800 tasks: `status` one of `STATUSES`, `due` 0 to 199, `prio` 0 to 2,
/// `team` "a" or "b".
fn made(db: &Database, indexes: &[&[&str]]) -> Collection<Document> {
    let tasks = db.collection::<Document>("tasks");
    for n in 0..800usize {
        let doc: Document = format!(
            r#"{{"n": {n}, "status": "{}", "due": {}, "prio": {}, "team": "{}"}}"#,
            STATUSES[n % 4],
            (n * 7) % 200,
            n % 3,
            ["a", "b"][n / 400],
        )
        .parse()
        .unwrap();
        tasks.insert(doc).unwrap();
    }
    for fields in indexes {
        tasks.ensure_index(fields.to_vec()).unwrap();
    }
    tasks
}

fn ids(found: Vec<Document>) -> Vec<DocId> {
    let mut ids: Vec<DocId> = (found.iter())
        .map(|doc| match doc.get("_id") {
            Some(Document::Id(id)) => *id,
            other => panic!("no id: {other:?}"),
        })
        .collect();
    ids.sort();
    ids
}

/// What `filter` finds, read through its plan and by a scan, which must
/// agree; and how many index keys the plan read.
fn run(tasks: &Collection<Document>, filter: Filter) -> (usize, u64) {
    let found = ids(tasks.find(filter.clone()).unwrap());
    let scanned = ids(tasks.find(filter.clone().no_index()).unwrap());
    assert_eq!(found, scanned, "{filter}");
    assert_eq!(
        tasks.count(filter.clone()).unwrap(),
        found.len(),
        "{filter}"
    );
    let explained = tasks.explain_run(filter).unwrap();
    (found.len(), explained.stats.keys_examined)
}

/// The keys `with_in`, an `in` of `values`, reads, and the keys each
/// `=` of them reads, written into `template` for `{}`, summed: equal,
/// as each value's range is the one `=` reads (SPEC §160).
fn reads_as_eq_would(tasks: &Collection<Document>, template: &str, values: &[&str]) {
    let list = (values.iter())
        .map(|v| format!("{v:?}"))
        .collect::<Vec<_>>();
    let with_in: Filter = template
        .replace("{}", &format!("in ({})", list.join(", ")))
        .parse()
        .unwrap();
    let (found, keys) = run(tasks, with_in.clone());
    let (mut eq_found, mut eq_keys) = (0, 0);
    for value in values {
        let with_eq: Filter = template
            .replace("{}", &format!("= {value:?}"))
            .parse()
            .unwrap();
        let (found, keys) = run(tasks, with_eq);
        eq_found += found;
        eq_keys += keys;
    }
    assert!(found > 0, "{with_in}");
    assert_eq!((found, keys), (eq_found, eq_keys), "{with_in}");
}

#[test]
fn each_value_of_an_in_reads_only_its_range() {
    let (_dir, db) = open();
    let tasks = made(&db, &[&["status", "due"]]);
    let filter: Filter = r#"status in ("open", "late") and due < 50"#.parse().unwrap();
    assert!(matches!(
        tasks.explain(&filter).unwrap(),
        QueryPlan::IndexUnion { .. }
    ));
    reads_as_eq_would(&tasks, "status {} and due < 50", &["open", "late"]);
    reads_as_eq_would(
        &tasks,
        "status {} and due >= 20 and due < 50",
        &["open", "late", "done"],
    );
    reads_as_eq_would(&tasks, "due > 150 and status {}", &["cancelled", "open"]);
    // An OR written out is an `in`.
    let written: Filter = r#"(status = "open" or status = "late") and due < 50"#
        .parse()
        .unwrap();
    let in_list: Filter = r#"status in ("open", "late") and due < 50"#.parse().unwrap();
    assert_eq!(run(&tasks, written), run(&tasks, in_list));
}

#[test]
fn an_eq_before_it_and_a_range_after_narrow_every_value() {
    let (_dir, db) = open();
    let tasks = made(&db, &[&["team", "status", "due"]]);
    reads_as_eq_would(
        &tasks,
        r#"team = "b" and status {} and due < 100"#,
        &["open", "done"],
    );
    reads_as_eq_would(&tasks, r#"team = "a" and status {}"#, &["late", "open"]);
    // The `in` first, `=`s and a range after it.
    reads_as_eq_would(
        &tasks,
        r#"team {} and status = "open" and due < 50"#,
        &["a", "b"],
    );
    // The `=`s after the `in` count: three fields beat the two of an
    // index made before it.
    let (_dir, db) = open();
    let tasks = made(&db, &[&["status", "due"][..], &["team", "status", "due"]]);
    let three: Filter = r#"team in ("a", "b") and status = "open" and due < 50"#
        .parse()
        .unwrap();
    assert!(matches!(
        tasks.explain(&three).unwrap(),
        QueryPlan::IndexUnion { fields, .. } if fields[0] == "(team, status, due)"
    ));
    let (_dir, db) = open();
    let tasks = made(&db, &[&["team", "status", "due"]]);
    // Each range says what bounds it, its value among them.
    let filter: Filter = r#"team = "b" and status in ("open", "done") and due < 100"#
        .parse()
        .unwrap();
    let explained = tasks.explain_run(filter).unwrap();
    let bounds: Vec<String> = (explained.reads.iter())
        .map(|read| {
            let all = Filter::new().and(Condition::All(read.bounds.clone()));
            format!("{}: {all}", read.index)
        })
        .collect();
    assert_eq!(
        bounds,
        [
            r#"(team, status, due): team = "b" and status = "open" and due < 100"#,
            r#"(team, status, due): team = "b" and status = "done" and due < 100"#,
        ]
    );
    // `team` not fixed: the `in` is on no first field after `=`s.
    let loose: Filter = r#"status in ("open", "late") and due < 50"#.parse().unwrap();
    assert!(matches!(tasks.explain(&loose).unwrap(), QueryPlan::Scan));
}

#[test]
fn where_no_branch_gains_the_plan_stays() {
    // Indexes on one field each: a branch planned with `due < 50` reads
    // `status`'s value or `due`'s range, no narrower than before, so the
    // OR is read as before, one range per status.
    let (_dir, db) = open();
    let tasks = made(&db, &[&["status"][..], &["due"]]);
    let filter: Filter = r#"status in ("open", "late") and due < 50"#.parse().unwrap();
    let before = tasks.explain(&filter).unwrap();
    assert!(
        matches!(&before, QueryPlan::IndexUnion { fields, .. } if fields == &["status", "status"]),
        "{before:?}"
    );
    let (found, keys) = run(&tasks, filter);
    assert!(found > 0);
    assert_eq!(keys, 400);

    // Only `due` indexed: its range, read once, not once per branch.
    let (_dir, db) = open();
    let tasks = made(&db, &[&["due"]]);
    let filter: Filter = r#"status in ("open", "late") and due < 50"#.parse().unwrap();
    assert!(matches!(
        tasks.explain(&filter).unwrap(),
        QueryPlan::Index { ref field, .. } if field == "due"
    ));
    let alone: Filter = "due < 50".parse().unwrap();
    assert_eq!(run(&tasks, filter).1, run(&tasks, alone).1);

    // An `in` on a field the index has elsewhere, or not at all, is no
    // value of the field after its `=`s.
    let (_dir, db) = open();
    let tasks = made(&db, &[&["status", "due"][..], &["team"]]);
    for text in [
        r#"prio in (0, 1) and due < 50"#,
        r#"due in (0, 28, 56) and status = "open""#,
        r#"due in (0, 7) and status in ("open", "late")"#,
    ] {
        let filter: Filter = text.parse().unwrap();
        assert!(run(&tasks, filter).0 > 0, "{text}");
    }
    // An `in` alone on the first field is the OR it is, chosen as before:
    // between two ORs, the first.
    let two_ors: Filter = r#"team in ("a", "b") and status in ("open", "late")"#
        .parse()
        .unwrap();
    assert!(
        matches!(tasks.explain(&two_ors).unwrap(), QueryPlan::IndexUnion { fields, .. } if fields == ["team", "team"])
    );

    // Beside an `=` on another index, the index the same query with
    // `=` for the `in` reads: bounds on more fields beat fewer (§43.3).
    let (_dir, db) = open();
    let tasks = made(&db, &[&["status", "due"][..], &["n"]]);
    let with_in: Filter = r#"n = 4 and status in ("open", "late") and due < 50"#
        .parse()
        .unwrap();
    let with_eq: Filter = r#"n = 4 and status = "open" and due < 50"#.parse().unwrap();
    let plan = |filter: &Filter| match tasks.explain(filter).unwrap() {
        QueryPlan::Index { field, .. } => field,
        QueryPlan::IndexUnion { fields, .. } => fields[0].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(plan(&with_in), plan(&with_eq));
    assert_eq!(run(&tasks, with_in).0, 1);
}

#[test]
fn random_ors_beside_ranges_find_what_a_scan_finds() {
    let (_dir, db) = open();
    let tasks = made(&db, &[&["status", "due"][..], &["prio", "due"], &["team"]]);
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut below = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    for _ in 0..300 {
        let mut conditions = Vec::new();
        if below(4) != 0 {
            let statuses: Vec<Document> = (0..below(4))
                .map(|_| Document::String(STATUSES[below(5) % 4].into()))
                .collect();
            conditions.push(Condition::is_in("status", statuses));
        }
        if below(2) == 0 {
            let prios: Vec<Document> = (0..1 + below(3))
                .map(|_| Document::Int(below(4) as i64))
                .collect();
            conditions.push(Condition::is_in("prio", prios));
        }
        for _ in 0..below(3) {
            let at = below(220) as i64 - 10;
            conditions.push(match below(4) {
                0 => Condition::lt("due", at),
                1 => Condition::gte("due", at),
                2 => Condition::eq("due", at),
                _ => Condition::lte("due", at),
            });
        }
        if below(3) == 0 {
            conditions.push(Condition::eq("team", ["a", "b"][below(2)]));
        }
        if below(4) == 0 {
            conditions.push(!Condition::eq("prio", 1));
        }
        let mut filter = Filter::new();
        for condition in conditions {
            filter = filter.and(condition);
        }
        if below(4) == 0 {
            filter = filter.sort_desc("due").limit(below(20));
        }
        run(&tasks, filter);
    }
}
