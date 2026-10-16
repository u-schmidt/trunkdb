# 94. Explain with execution statistics (`explain.rs`, `query.rs`, `collection.rs`, `snapshot.rs`)

`explain` names the kind of plan, `QueryPlan::Index { field }`, and
that was all. It didn't say:

- which part of the index is read;
- whether the sort comes from the index or is done in memory;
- how much the query actually read.

Nor did it say whether a query that "uses an index" reads 20 documents
or 20,000. To find out, a program counted the results and guessed at
the rest.

`explain_run` runs the query as `find` does and reports what it did.

## 94.1 What it reports
```rust
// On Collection<T>, and on View<T> for the view's snapshot.
pub fn explain_run(&self, filter: Filter) -> crate::Result<Explained>;

pub struct Explained {
    pub plan: QueryPlan,          // what `explain` says, same snapshot
    pub reads: Vec<IndexRead>,    // in the order they ran
    pub sort: Option<SortWork>,   // None without a sort
    pub stats: ExecutionStats,
}
pub struct IndexRead {
    pub index: String,            // "_id", "age", "(status, created)"
    pub bounds: Vec<Condition>,   // 94.2; empty: the whole index
    pub backward: bool,           // a descending walk
    pub entries: u64,
}
pub struct SortWork { pub from_index: usize, pub in_memory: usize }
pub struct ExecutionStats {
    pub keys_examined: u64,       // every read's entries, the primary's too
    pub documents_examined: u64,  // read and checked
    pub documents_matched: u64,   // the skipped included
    pub documents_returned: u64,  // what `find` returns
    pub elapsed: Duration,
}
```
All four are `#[non_exhaustive]` and live in `query`, beside
`QueryPlan`. The documents are read and checked as `find` reads them,
then dropped. `Op` and `Condition` gained `PartialEq`, so bounds can be
compared.

How each plan reports itself:

| Plan | `reads` | `entries` |
|---|---|---|
| `Scan` | `_id`, no bounds | every document's |
| `ById` | `_id`, bounded by `_id == id`, or an OR of them for several | one per id looked up, found or not |
| `Index` | the index, bounded | the range's |
| `IndexUnion` | one per range, in order | each range's; a document in two counts in both |
| `IndexOrder` | the index, `backward` for a descending sort; then `_id`, unbounded, if the values it can't order had to be looked for (§34.1) | walked until the limit, and the entry that ended the last group |

A multikey index has an entry per element, so `keys_examined` can be
more than `documents_examined`. A document is examined once however
many entries lead to it, as `find` reads it once. The one exception is
the look for unordered values, which reads every document again.

A `limit(0)` through an index reads nothing at all: no reads, and
zeros.

## 94.2 Bounds as conditions
An index range is encoded bytes (`KeyRange`). They are not decoded back
to values: a long string is cut and a huge number rounded in the key
(§34.1). Instead `bounds` names the conditions the planner built the
range from, as the index sees them:

- `age >= 18`, `age < 65` on `age`: the comparisons on one field,
  intersected;
- `status == "open"`, `created > t` on `(status, created)`: the `Eq`s on
  its leading fields, then the comparisons on the next;
- on a multikey field only the one comparison that bounds it (§42.3),
  an `Eq`'s if there is one: `tags[*] > 0`, not `tags[*] < 100` too;
- an `elem_match` as the paths it is rewritten to (§46.3):
  `items[*].sku == "a1"`;
- in an `IndexOrder` walk, the `Eq`s that fix the leading fields and the
  comparisons on the sort field.

Every document found meets every bound of some read; the tests check
it on every planner corpus (94.6).
Conditions not in `bounds` are still checked on every document read.
`documents_examined` against `documents_matched` shows what that costs.

A range may be wider than its conditions. `age < 65` reads the 65s too,
since the range ends after the value (`key::range_for`), and the check
leaves them out (§28.3). `explain_run` shows it: the example in its
documentation examined 48 to return 47. (Since §100, where a read's
entries decide the filter, `IndexRead::covering` says so: the 65s are
left out on their entries, unread, and `documents_matched` can exceed
`documents_examined`.)

To get the conditions, the planner carries them beside each range:
`IndexRange { index, range, by }` replaces the `(index, range)` pair,
and `OrderedRead` has `by` too. Which plan the planner picks is
unchanged, and so is `explain`, which now shares `plan_for` with
`explain_run`.

## 94.3 One code path
`explain_run` is `find` with its counters read out. `find_in`,
`candidate_entries`, `read_candidates` and `read_in_index_order` take a
`&Trace`, where they record each read and count entries, documents
examined and matches. Every caller passes one: `find`, `find_one`,
`count`, `cursor`, `upsert`, `update_many`, `update_fields`,
`delete_many`. All but `explain_run` drop it.

The counters are `Cell`s and a `RefCell<Vec<IndexRead>>`, so the
closures of the walk in order can all count. Recording a read names the
index and moves the planner's conditions into it, which costs one small
allocation or two per read. Only the clock is `explain_run`'s own.

Measured with the benchmark (§48), 100,000 documents, trunkdb alone,
four alternating runs of the old and the new build, median:

| | before | after |
|---|---:|---:|
| get by id | 4.5 µs | 4.0 µs |
| find tenant == x (1000 docs) | 2.04 ms | 2.00 ms |
| find created in a range (1000 docs) | 2.23 ms | 2.09 ms |
| status == x, oldest 20 | 44.0 µs | 44.1 µs |
| status == x, newest 20 | 44.8 µs | 44.7 µs |
| scan: tries > 7, unindexed | 166.5 ms | 166.9 ms |

Within the runs' noise everywhere. A flag or generic parameter for
counting only when asked was not needed (94.5).

Like `find`:
- on one snapshot;
- `skip` and `limit` included: they change the plan (§34.2) and the
  cost (§72.3);
- the same errors.

A sorted `cursor` reads as `find` does. An unsorted one streams the same
entries and documents. A cursor doesn't report on itself.

## 94.4 The time
`elapsed` is the whole call, wall-clock, and includes waiting for pages
from disk. A second run usually takes less, from the page cache (§50).
The counts don't change between runs on one snapshot; the time can. The
tests check the counts, never the time.

The CLI has no query command, and §94 adds none.

## 94.5 Rejected
- **Results with the statistics** (`(Vec<T>, Explained)`): a caller that
  wants both runs `find` and `explain_run` on one `View`. The query
  then runs twice, but only when someone asks how it ran.
- **Fields on `QueryPlan`'s variants** (`Index { field, bounds }`):
  `QueryPlan` is `#[non_exhaustive]`, its variants aren't, and every
  pattern naming `Index { field }` would break.
- **Decoded key bounds** (`age: [18, 65)`): lossy for cut strings and
  rounded numbers, and the conditions say it in the caller's terms.
- **Counting only when asked:** two paths through the reading code, for
  a saving the benchmark doesn't show (94.3).
- **Counting only secondary entries**, as MongoDB counts a collection
  scan's keys as 0: the primary index's walk is real work, and `reads`
  says which index it was.
- **Per-stage timings:** a profiler's job.
- **Advice** ("an index on `x` would serve this", "the sparse index
  wasn't used because the filter allows null"): a second rule set beside
  the planner's. It could come later, as a field on `Explained`.

## 94.6 Tests
Every randomized planner test now goes through `explained`, which runs
`explain`, `explain_run` and `find` on the same filter. It checks:

- the same plan;
- `documents_returned` equal to `find`'s results;
- `documents_examined` equal to the test-only record counter
  (`RECORDS_READ`) for `find`;
- `keys_examined` equal to the reads' entries, and at least the
  documents examined;
- `documents_matched` within the documents examined, and equal to the
  matches without skip and limit, or for `IndexOrder` at least the
  first skip plus limit of them;
- the reads' indexes against the plan, and `backward` against the sort;
- bounds on every read of an `Index`, `IndexUnion` or `ById` plan;
- every document found within the bounds of some read;
- `sort` against the plan.

These are the tests that compare finds with scans for single, multikey,
`elem_match`, sorted (several keys, skip, limit), compound and sparse
compound indexes. Together they make thousands of random filters
through every plan.

`explain.rs`, on 100 documents, with exact numbers:
- a scan: one read of `_id`, 100 entries, 100 examined, 50 matched;
- by id: one id; three ids, one missing (3 entries, 2 examined);
- a range with a `Ne` beside it: two bounds, 22 entries (the 20s
  included), 19 returned;
- a union whose branches overlap: 10 entries, 8 examined, 6 matched;
- a compound prefix with a range after it;
- a multikey field: one bound of two, the `Eq` when there is one,
  200 entries for 100 documents;
- an `elem_match`: bounded by `items[*].sku == "a1"`;
- a sparse index: used for an `Eq`, not for `is_null`;
- sort:
  - none;
  - in memory;
  - without a limit;
  - from the index;
  - the first key from the index and the second in memory, with its
    groups read whole (6 examined for 5);
- a walk in order: 7 entries for 5 documents, `backward` for a
  descending sort, bounded by the sort field's range;
- the look for unordered values: reads `v`, then `_id`, 5 examined;
- skip: the skipped counted as matched, by scan and by walk;
- a `View`: the snapshot's documents and indexes, not ones made since;
- a missing collection: no reads, zeros.

The documentation's example runs as a doctest.

Twenty-three hand-made mutations, all caught:
- entries not counted on each of five paths (scan, by id, range, walk,
  the look for unordered values);
- a union's duplicate entries counted once;
- documents examined not counted, from candidates or on the walk;
- matches not counted on each of four paths (scan and ranges, the walk
  one by one, a group read whole, the unordered values);
- the skipped not counted as matched;
- `backward` inverted;
- `from_index` off by one;
- a `sort` for a filter without one;
- one id reported as an OR;
- the multikey bound from the wrong comparison, and from all of them;
- the compound range's, the walk's range's and the fixed prefix's
  conditions left out;
- `keys_examined` leaving out a read.

The two survivors of the first round, matches in a group read whole and
in the look for unordered values, led to the exact counts in the sort and
unordered tests.

Full suite, clippy `-D warnings` with and without fuzzing, fmt,
rustdoc, the fuzz and bench crates, Rust 1.89.

## 94.7 Limits
- The query runs in full: an `explain_run` of a scan over a large
  collection costs what the scan costs.
- No advice on indexes (94.5).
- `elapsed` depends on the cache (94.4).
