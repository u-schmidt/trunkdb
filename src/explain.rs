//! What a query did, read out of `find` itself (SPEC §94): the plan,
//! the index reads with their bounds, how it sorted, and the counts.

use crate::query::{Condition, QueryPlan};
use std::cell::{Cell, RefCell};
use std::time::Duration;

/// How a query ran — see `Collection::explain_run`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Explained {
    /// What `explain` says for the same filter, on the same snapshot.
    pub plan: QueryPlan,
    /// The index reads, in the order they ran.
    pub reads: Vec<IndexRead>,
    /// How the results were put in order; `None` without a sort.
    pub sort: Option<SortWork>,
    /// The paths the filter selects (SPEC §106), as given; `None`
    /// without a selection: whole documents.
    pub select: Option<Vec<String>>,
    pub stats: ExecutionStats,
}

/// One read of an index.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct IndexRead {
    /// As `explain` and `indexes` name it: `_id` for the primary index,
    /// `(status, created)` for a compound one.
    pub index: String,
    /// The conditions the read is bounded by, as the index sees them
    /// (SPEC §94.2): all of them hold for every entry read. Empty: the
    /// whole index. Conditions not here are still checked, on every
    /// document read.
    pub bounds: Vec<Condition>,
    /// Read from the end: a descending sort through the index.
    pub backward: bool,
    /// Entries read from it. A lookup by id counts one, found or not.
    pub entries: u64,
    /// Its entries decide the filter (SPEC §100): every field the
    /// conditions name is in the index, or is `_id`. A document is then
    /// read only where an entry can't decide (a string cut to the key, a
    /// number past 2^53) and to be handed out.
    pub covering: bool,
}

/// How a sort was done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SortWork {
    /// How many sort keys, from the first, the index order gives (§47).
    pub from_index: usize,
    /// How many are left to memory, among documents equal in those.
    pub in_memory: usize,
}

/// What a query cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExecutionStats {
    /// Index entries read, every read together, the primary index's too.
    pub keys_examined: u64,
    /// Documents read: to be checked against the filter, and, where an
    /// index entry decided (SPEC §100), to be handed out.
    pub documents_examined: u64,
    /// The matches, decided by their documents or by their index entries;
    /// with a skip, the skipped too.
    pub documents_matched: u64,
    /// What `find` would have returned.
    pub documents_returned: u64,
    /// Of those, the ones built from their index entries, their
    /// documents never read (SPEC §106.5): where the entries decide the
    /// filter and hold every selected field.
    pub from_entries: u64,
    /// Wall-clock time of the whole call. It includes waiting for pages
    /// from disk, so a second run usually takes less (SPEC §94.4); the
    /// counts are the same every run on one snapshot.
    pub elapsed: Duration,
}

/// What `find` counts as it reads, on every read (SPEC §94.3), and
/// throws away unless `explain_run` asked. Shared (`&Trace`) so the
/// closures of one read can all count.
#[derive(Default)]
pub(crate) struct Trace {
    reads: RefCell<Vec<IndexRead>>,
    examined: Cell<u64>,
    matched: Cell<u64>,
    from_entries: Cell<u64>,
}

impl Trace {
    /// A read of `index` begins; returns its place, for `entries`.
    pub fn read(&self, index: String, bounds: Vec<Condition>, backward: bool) -> usize {
        let mut reads = self.reads.borrow_mut();
        reads.push(IndexRead {
            index,
            bounds,
            backward,
            entries: 0,
            covering: false,
        });
        reads.len() - 1
    }

    /// The read at `at` covers the filter (SPEC §100).
    pub fn covering(&self, at: usize) {
        self.reads.borrow_mut()[at].covering = true;
    }

    /// `n` more entries read by the read at `at`.
    pub fn entries(&self, at: usize, n: usize) {
        self.reads.borrow_mut()[at].entries += n as u64;
    }

    /// A document read, to be checked or handed out.
    pub fn examined(&self) {
        self.examined.set(self.examined.get() + 1);
    }

    /// A result built from its index entry, its document unread.
    pub fn answered_from_entry(&self) {
        self.from_entries.set(self.from_entries.get() + 1);
    }

    /// `n` documents checked that matched.
    pub fn matched(&self, n: usize) {
        self.matched.set(self.matched.get() + n as u64);
    }

    pub fn explained(
        self,
        plan: QueryPlan,
        sort: Option<SortWork>,
        select: Option<Vec<String>>,
        returned: usize,
        elapsed: Duration,
    ) -> Explained {
        let reads = self.reads.into_inner();
        Explained {
            plan,
            stats: ExecutionStats {
                keys_examined: reads.iter().map(|read| read.entries).sum(),
                documents_examined: self.examined.get(),
                documents_matched: self.matched.get(),
                documents_returned: returned as u64,
                from_entries: self.from_entries.get(),
                elapsed,
            },
            reads,
            sort,
            select,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Filter;
    use crate::{Collection, Database, DocId, Document, IndexOptions};

    /// 100 people, `n` 0 to 99: `age` is `n % 50` (two of each), `group`
    /// `n % 2`, `tags` two different elements, one item, and a `nick` for
    /// the first ten.
    fn people(db: &Database) -> (Collection<Document>, Vec<DocId>) {
        let people = db.collection::<Document>("people");
        let ids = (0..100)
            .map(|n| {
                let nick = match n < 10 {
                    true => format!(r#", "nick": "x{n}""#),
                    false => String::new(),
                };
                let json = format!(
                    r#"{{"n": {n}, "age": {}, "group": {}, "tags": [{}, {}],
                        "items": [{{"sku": "a{}"}}]{nick}}}"#,
                    n % 50,
                    n % 2,
                    n % 3,
                    10 + n % 5,
                    n % 4,
                );
                people.insert(json.parse().unwrap()).unwrap()
            })
            .collect();
        (people, ids)
    }

    fn database() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("explain.trunkdb")).unwrap();
        (dir, db)
    }

    fn read(index: &str, bounds: Vec<Condition>, entries: u64) -> IndexRead {
        IndexRead {
            index: index.to_string(),
            bounds,
            backward: false,
            entries,
            covering: false,
        }
    }

    /// `read`, whose entries decide the filter (SPEC §100).
    fn covering(index: &str, bounds: Vec<Condition>, entries: u64) -> IndexRead {
        IndexRead {
            covering: true,
            ..read(index, bounds, entries)
        }
    }

    /// examined, matched, returned.
    fn counts(run: &Explained) -> (u64, u64, u64) {
        let stats = run.stats;
        (
            stats.documents_examined,
            stats.documents_matched,
            stats.documents_returned,
        )
    }

    #[test]
    fn a_scan_reads_every_entry_of_the_primary_index() {
        let (_dir, db) = database();
        let (people, _) = people(&db);
        let run = people.explain_run(Filter::new().eq("group", 0)).unwrap();
        assert_eq!(run.plan, QueryPlan::Scan);
        assert_eq!(run.reads, [read("_id", vec![], 100)]);
        assert_eq!(run.stats.keys_examined, 100);
        assert_eq!(counts(&run), (100, 50, 50));
        assert_eq!(run.sort, None);
    }

    #[test]
    fn lookups_by_id_count_one_entry_each_found_or_not() {
        let (_dir, db) = database();
        let (people, ids) = people(&db);
        let one = people.explain_run(Filter::new().id(ids[7])).unwrap();
        assert_eq!(one.plan, QueryPlan::ById);
        let by_id = vec![Condition::id(ids[7])];
        assert_eq!(one.reads, [covering("_id", by_id, 1)]);
        assert_eq!(counts(&one), (1, 1, 1));

        let missing = DocId([0xEE; 16]);
        let mut wanted = [ids[3], ids[1], missing];
        let three = Filter::new().any_of(wanted.iter().map(|id| Condition::id(*id)));
        let run = people.explain_run(three).unwrap();
        wanted.sort();
        let bounds = vec![Condition::any(wanted.iter().map(|id| Condition::id(*id)))];
        assert_eq!(run.reads, [covering("_id", bounds, 3)]);
        assert_eq!(counts(&run), (2, 2, 2));
    }

    #[test]
    fn a_range_is_bounded_by_the_comparisons_that_make_it() {
        let (_dir, db) = database();
        let (people, _) = people(&db);
        people.ensure_index("age").unwrap();
        let f = Filter::new().gte("age", 10).lt("age", 20).ne("n", 15);
        let run = people.explain_run(f).unwrap();
        assert_eq!(
            run.plan,
            QueryPlan::Index {
                field: "age".into()
            }
        );
        // `< 20` reads the 20s too; the check leaves them out (§28.3).
        let bounds = vec![Condition::gte("age", 10), Condition::lt("age", 20)];
        assert_eq!(run.reads, [read("age", bounds, 22)]);
        assert_eq!(counts(&run), (22, 19, 19));
    }

    #[test]
    fn a_union_reads_each_branch_and_each_document_once() {
        let (_dir, db) = database();
        let (people, _) = people(&db);
        people.ensure_index("age").unwrap();
        let f = Filter::new().any_of([Condition::eq("age", 1), Condition::lt("age", 3)]);
        let run = people.explain_run(f).unwrap();
        assert_eq!(
            run.plan,
            QueryPlan::IndexUnion {
                fields: vec!["age".into(), "age".into()]
            }
        );
        assert_eq!(
            run.reads,
            [
                covering("age", vec![Condition::eq("age", 1)], 2),
                covering("age", vec![Condition::lt("age", 3)], 8),
            ]
        );
        // The two of age 1 are in both ranges: read twice, checked once.
        // The entries decide (SPEC §100): only the matches are read.
        assert_eq!(run.stats.keys_examined, 10);
        assert_eq!(counts(&run), (6, 6, 6));
    }

    #[test]
    fn a_compound_index_is_bounded_by_its_prefix_and_the_range_after_it() {
        let (_dir, db) = database();
        let (people, _) = people(&db);
        people.ensure_index(["group", "age"]).unwrap();
        let f = Filter::new().gt("age", 40).eq("group", 0);
        let run = people.explain_run(f).unwrap();
        assert_eq!(
            run.plan,
            QueryPlan::Index {
                field: "(group, age)".into()
            }
        );
        // Fixed fields first, in the index's order.
        let bounds = vec![Condition::eq("group", 0), Condition::gt("age", 40)];
        assert_eq!(run.reads, [covering("(group, age)", bounds, 10)]);
        assert_eq!(counts(&run), (8, 8, 8));
    }

    #[test]
    fn a_multikey_index_is_bounded_by_one_comparison_and_has_more_entries_than_documents() {
        let (_dir, db) = database();
        let (people, _) = people(&db);
        people.ensure_index("tags[*]").unwrap();
        let both = Filter::new().gt("tags[*]", 0).lt("tags[*]", 100);
        let run = people.explain_run(both).unwrap();
        assert_eq!(run.reads[0].bounds, [Condition::gt("tags[*]", 0)]);
        assert_eq!(run.stats.keys_examined, 200);
        assert_eq!(counts(&run), (100, 100, 100));

        let with_eq = Filter::new().lt("tags[*]", 100).eq("tags[*]", 11);
        let run = people.explain_run(with_eq).unwrap();
        assert_eq!(
            run.reads,
            [read("tags[*]", vec![Condition::eq("tags[*]", 11)], 20)]
        );
        assert_eq!(counts(&run), (20, 20, 20));
    }

    #[test]
    fn an_elem_match_is_bounded_by_the_paths_it_rewrites_to() {
        let (_dir, db) = database();
        let (people, _) = people(&db);
        people.ensure_index("items[*].sku").unwrap();
        let f = Filter::new().elem_match("items", Condition::eq("sku", "a1"));
        let run = people.explain_run(f).unwrap();
        let bounds = vec![Condition::eq("items[*].sku", "a1")];
        assert_eq!(run.reads, [read("items[*].sku", bounds, 25)]);
        assert_eq!(counts(&run), (25, 25, 25));
    }

    #[test]
    fn a_sparse_index_serves_only_what_rules_out_null() {
        let (_dir, db) = database();
        let (people, _) = people(&db);
        people
            .ensure_index_with("nick", IndexOptions::new().sparse())
            .unwrap();
        let run = people.explain_run(Filter::new().eq("nick", "x3")).unwrap();
        assert_eq!(
            run.reads,
            [covering("nick", vec![Condition::eq("nick", "x3")], 1)]
        );
        let run = people.explain_run(Filter::new().is_null("nick")).unwrap();
        assert_eq!(run.plan, QueryPlan::Scan);
        assert_eq!(run.reads, [read("_id", vec![], 100)]);
        assert_eq!(counts(&run), (100, 90, 90));
    }

    #[test]
    fn the_sort_says_how_many_keys_the_index_gives() {
        let (_dir, db) = database();
        let (people, _) = people(&db);
        people.ensure_index("age").unwrap();
        let sort = |f: Filter| people.explain_run(f).unwrap().sort;
        let work = |from_index, in_memory| {
            Some(SortWork {
                from_index,
                in_memory,
            })
        };
        assert_eq!(sort(Filter::new()), None);
        assert_eq!(sort(Filter::new().sort_asc("n").limit(5)), work(0, 1));
        // Without a limit there's no walk in order (§34.2).
        assert_eq!(sort(Filter::new().sort_asc("age")), work(0, 1));
        assert_eq!(sort(Filter::new().sort_asc("age").limit(5)), work(1, 0));
        let two_keys = Filter::new().sort_asc("age").then_desc("n").limit(5);
        assert_eq!(sort(two_keys.clone()), work(1, 1));
        // Each age read whole, to sort its two by `n`: three ages for 5.
        assert_eq!(counts(&people.explain_run(two_keys).unwrap()), (6, 6, 5));
    }

    #[test]
    fn a_walk_in_order_stops_at_the_limit_and_says_its_direction() {
        let (_dir, db) = database();
        let (people, _) = people(&db);
        people.ensure_index("age").unwrap();
        let run = people
            .explain_run(Filter::new().sort_asc("age").limit(5))
            .unwrap();
        assert_eq!(
            run.plan,
            QueryPlan::IndexOrder {
                field: "age".into()
            }
        );
        assert_eq!(run.reads.len(), 1);
        assert_eq!(
            (run.reads[0].index.as_str(), run.reads[0].backward),
            ("age", false)
        );
        assert!(run.reads[0].bounds.is_empty());
        // Ages 0, 1 and 2, two each: ties come in id order, so each entry
        // is handed out as it's walked, and the walk stops at the fifth
        // (SPEC §101.5).
        assert_eq!(run.stats.keys_examined, 5);
        assert_eq!(counts(&run), (5, 5, 5));

        let f = Filter::new().gte("age", 45).sort_desc("age").limit(3);
        let run = people.explain_run(f).unwrap();
        assert!(run.reads[0].backward);
        assert_eq!(run.reads[0].bounds, [Condition::gte("age", 45)]);
        assert_eq!(counts(&run).2, 3);
    }

    #[test]
    fn values_the_index_cannot_order_are_looked_for_in_the_primary_index() {
        let (_dir, db) = database();
        let docs = db.collection::<Document>("docs");
        docs.ensure_index("v").unwrap();
        for json in [r#"{"v": 1}"#, r#"{"v": 2}"#, r#"{"v": [5]}"#] {
            docs.insert(json.parse().unwrap()).unwrap();
        }
        let run = docs
            .explain_run(Filter::new().sort_asc("v").limit(10))
            .unwrap();
        let names: Vec<&str> = run.reads.iter().map(|r| r.index.as_str()).collect();
        assert_eq!(names, ["v", "_id"]);
        assert_eq!(run.reads[0].entries, 2);
        assert_eq!(run.reads[1], read("_id", vec![], 3));
        // 1 and 2 from the walk, then all three again for the array.
        assert_eq!(counts(&run), (5, 3, 3));
    }

    #[test]
    fn the_skipped_count_as_matched() {
        let (_dir, db) = database();
        let (people, _) = people(&db);
        let f = Filter::new().eq("group", 0).skip(10).limit(5);
        assert_eq!(counts(&people.explain_run(f).unwrap()), (100, 50, 5));

        people.ensure_index("age").unwrap();
        // Passed over by their entries, unread (SPEC §100).
        let f = Filter::new().sort_asc("age").skip(10).limit(5);
        let run = people.explain_run(f).unwrap();
        assert_eq!(counts(&run), (5, 15, 5));
        assert!(run.stats.keys_examined < 100);
    }

    #[test]
    fn a_view_explains_its_snapshot() {
        let (_dir, db) = database();
        let (people, _) = people(&db);
        let snapshot = db.snapshot().unwrap();
        people.insert(r#"{"n": 100}"#.parse().unwrap()).unwrap();
        people.ensure_index("n").unwrap();
        let then = snapshot.collection::<Document>("people");
        let run = then.explain_run(Filter::new().eq("n", 100)).unwrap();
        assert_eq!(run.plan, QueryPlan::Scan, "the index came later");
        assert_eq!(counts(&run), (100, 0, 0));
        let now = people.explain_run(Filter::new().eq("n", 100)).unwrap();
        let bounds = vec![Condition::eq("n", 100)];
        assert_eq!(now.reads, [covering("n", bounds, 1)]);
    }

    #[test]
    fn a_missing_collection_reads_nothing() {
        let (_dir, db) = database();
        let none = db.collection::<Document>("none");
        let run = none.explain_run(Filter::new().eq("a", 1)).unwrap();
        assert_eq!(run.plan, QueryPlan::Scan);
        assert!(run.reads.is_empty());
        assert_eq!(run.stats.keys_examined, 0);
        assert_eq!(counts(&run), (0, 0, 0));
    }
}
