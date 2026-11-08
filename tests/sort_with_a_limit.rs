//! A sort by fields with a limit, read in memory (SPEC §176): only the
//! best `skip + limit` kept as the matches are read. Every read and
//! write that sorts gives what sorting every match gives.

use trunkdb::query::{Filter, SortOrder, Update};
use trunkdb::{Collection, Database, DocId, Document};

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("sort.trunkdb")).unwrap();
    (dir, db)
}

struct Random(u64);

impl Random {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn id_of(doc: &Document) -> DocId {
    match doc.get("_id") {
        Some(Document::Id(id)) => *id,
        other => panic!("_id: {other:?}"),
    }
}

fn ids(docs: &[Document]) -> Vec<DocId> {
    docs.iter().map(id_of).collect()
}

/// A value of some kind for `a`: few, so many are equal, across the
/// kinds a sort orders (SPEC §34.1), `2` and `2.0` equal, and arrays and
/// objects, which it orders as equals after all the rest.
fn value(random: &mut Random) -> &'static str {
    const VALUES: [&str; 14] = [
        "null",
        "false",
        "true",
        "-1",
        "0",
        "2",
        "2.0",
        "2.5",
        r#""a""#,
        r#""b""#,
        r#"{"$date": "2026-11-08T00:00:00Z"}"#,
        "[1]",
        r#"{"x": 1}"#,
        "-0.0",
    ];
    VALUES[random.below(VALUES.len() as u64) as usize]
}

/// 200 documents: `a` of any kind or missing, `b` 0 to 4, `c` 0 to 99 or
/// missing, `n` their number.
fn fill(items: &Collection<Document>, random: &mut Random) {
    for n in 0..200 {
        let a = match random.below(8) {
            0 => String::new(),
            _ => format!(r#""a": {}, "#, value(random)),
        };
        let c = match random.below(5) {
            0 => String::new(),
            _ => format!(r#", "c": {}"#, random.below(100)),
        };
        let text = format!(r#"{{{a}"b": {}, "n": {n}{c}}}"#, random.below(5));
        items.insert(text.parse().unwrap()).unwrap();
    }
}

/// A random filter that sorts by one to three fields, with a skip and a
/// limit, and maybe a condition on `b`.
fn random_filter(random: &mut Random) -> Filter {
    let mut filter = match random.below(3) {
        0 => Filter::new(),
        1 => Filter::new().lt("b", random.below(5) as i64),
        _ => Filter::new().eq("b", random.below(5) as i64),
    };
    let fields = ["a", "b", "c", "_id", "n"];
    for i in 0..1 + random.below(3) {
        let field = fields[random.below(fields.len() as u64) as usize];
        let order = [SortOrder::Asc, SortOrder::Desc][random.below(2) as usize];
        filter = match i {
            0 => filter.sort_by(field, order),
            _ => filter.then_by(field, order),
        };
    }
    let skip = [0, 0, 1, 3, 50, 500][random.below(6) as usize];
    let limit = [0, 1, 2, 10, 37, 1000][random.below(6) as usize];
    filter.skip(skip).limit(limit)
}

/// Every read gives what `Filter::apply` gives on every document, which
/// sorts them all; and so do the writes that sort: an `update_fields`
/// marks those it finds, a `delete_many` deletes them.
#[test]
fn every_read_and_write_keeps_what_a_full_sort_keeps() {
    for setup in 0..3 {
        let (_dir, db) = open();
        let items = db.collection::<Document>("items");
        let mut random = Random(0x9e37_79b9_7f4a_7c15 + setup);
        fill(&items, &mut random);
        match setup {
            1 => drop(items.ensure_index("b").unwrap()), // the condition's range read
            2 => drop(items.ensure_index("c").unwrap()), // sorts by `c` walk it
            _ => {}
        }
        for round in 0..100 {
            let filter = random_filter(&mut random);
            let context = format!("setup {setup}, round {round}: {filter:?}");
            let all = items.find(Filter::new()).unwrap();
            let expected = ids(&filter.apply(all.clone()));

            assert_eq!(
                ids(&items.find(filter.clone()).unwrap()),
                expected,
                "{context}"
            );
            let snapshot = db.snapshot().unwrap();
            let view = snapshot.collection::<Document>("items");
            assert_eq!(
                ids(&view.find(filter.clone()).unwrap()),
                expected,
                "{context}"
            );
            let cursor: Vec<Document> = (items.cursor(filter.clone()).unwrap())
                .map(Result::unwrap)
                .collect();
            assert_eq!(ids(&cursor), expected, "{context}");
            let one = items.find_one(filter.clone()).unwrap();
            assert_eq!(
                one.map(|d| id_of(&d)),
                expected.first().copied(),
                "{context}"
            );
            let selected = items.find(filter.clone().select(["n"])).unwrap();
            assert_eq!(ids(&selected), expected, "{context}");

            match round % 10 {
                0 => {
                    let marked = items
                        .update_fields(filter.clone(), &Update::new().set("m", round as i64))
                        .unwrap();
                    assert_eq!(marked, expected.len(), "{context}");
                    let mut found = ids(&items.find(Filter::new().eq("m", round as i64)).unwrap());
                    let mut wanted = expected.clone();
                    found.sort();
                    wanted.sort();
                    assert_eq!(found, wanted, "{context}");
                }
                5 if items.count(Filter::new()).unwrap() > 100 => {
                    let deleted = items.delete_many(filter.clone()).unwrap();
                    assert_eq!(deleted, expected.len(), "{context}");
                    for id in &expected {
                        assert!(items.get(id).unwrap().is_none(), "{context}");
                    }
                    fill(&items, &mut random);
                }
                _ => {}
            }
        }
        assert!(db.check().unwrap().is_ok());
    }
}

/// Equal in every sort field, the matches go by id, ascending, both ways
/// round, however many the limit cuts off: what the keep sees last
/// can't push out an earlier equal one.
#[test]
fn equal_values_go_by_id_both_ways() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    let mut made = Vec::new();
    for n in 0..100 {
        let text = format!(r#"{{"v": {}, "n": {n}}}"#, n % 3);
        made.push(items.insert(text.parse().unwrap()).unwrap());
    }
    for order in [SortOrder::Asc, SortOrder::Desc] {
        for limit in [1, 5, 33, 34, 50] {
            let found = items
                .find(Filter::new().sort_by("v", order).limit(limit))
                .unwrap();
            let v = if order == SortOrder::Asc { 0 } else { 2 };
            let wanted: Vec<DocId> = (made.iter().enumerate())
                .filter(|(n, _)| n % 3 == v)
                .map(|(_, id)| *id)
                .take(limit)
                .collect();
            let found = ids(&found);
            assert_eq!(
                found[..wanted.len()],
                wanted[..],
                "{order:?}, limit {limit}"
            );
        }
    }
}

/// Ten of 100,000 by a field no index holds, against all of them by it:
/// `cargo test --release --test sort_with_a_limit -- --ignored --nocapture`.
#[test]
#[ignore]
fn measure_ten_of_a_hundred_thousand() {
    let (_dir, db) = open();
    let items = db.collection::<Document>("items");
    let mut random = Random(0x2545_f491_4f6c_dd1d);
    let mut batch = db.batch();
    for n in 0..100_000 {
        let text = format!(
            r#"{{"lat": {}, "lon": {}, "n": {n}, "name": "place {n}"}}"#,
            random.below(5_000_000) as f64 / 1e6,
            random.below(7_000_000) as f64 / 1e6
        );
        batch
            .insert(&items, text.parse::<Document>().unwrap())
            .unwrap();
    }
    batch.commit().unwrap();
    let time = |filter: Filter, runs: u32| {
        let mut found = 0;
        let started = std::time::Instant::now();
        for _ in 0..runs {
            found = items.find(filter.clone()).unwrap().len();
        }
        (started.elapsed() / runs, found)
    };
    for round in 0..2 {
        for (name, filter, runs) in [
            ("ten by lon", Filter::new().sort_asc("lon").limit(10), 20),
            (
                "ten by lon, then lat",
                Filter::new().sort_asc("lon").then_asc("lat").limit(10),
                20,
            ),
            (
                "1,000 by lon",
                Filter::new().sort_asc("lon").limit(1000),
                20,
            ),
            ("all by lon", Filter::new().sort_asc("lon"), 5),
            ("all, unsorted", Filter::new(), 5),
        ] {
            let (took, found) = time(filter, runs);
            println!("round {round}: {name}: {took:.1?}, {found} found");
        }
    }
}
