# 176. A sort with a limit kept small (`query.rs`, `collection.rs`)

A sort no index serves is made in memory: every match was gathered,
all of them sorted, and the first `skip + limit` handed out (§102.5).
Ten of 100,000 by a field without an index held 100,000 documents to
hand out ten. A sort by a key of the application's (§154) already kept
only the best as it read, and was faster for it (§154.4). Now a sort by
fields with a limit does the same. No API, file format or result
changed.

## 176.1 What changed
- **A sort by fields with a limit,** where neither an index walk in sort
  order (§34.2, §47) nor a merge by id (§167) serves it, goes through
  `find_by_key`, §154's keep: each match is ranked as it is read, by its
  sort fields' values, and when the list holds twice `skip + limit`, a
  selection cuts it back to the best. Memory stays at about twice
  `skip + limit` matches however many match.
- **`Ranking` without a key:** `Filter::kept_sort` builds the order that
  keep uses, the sort fields alone, every match ranked with the same
  key of 0. `Filter::ranking`, for a key sort, is as it was.
- **The same order:** by the sort fields as `sort_order` compares them
  (§34.1), then by id, ascending both ways round, as the sort it
  replaces put equal matches. The last match read can't push out an
  earlier equal one.
- **Everywhere `find_in` serves:** `find`, `find_one`, `cursor`, a
  snapshot's reads, a selection, and the writes that find by a sorted,
  limited filter (`update_fields`, `delete_many`, a batch's
  `update_many` and `delete_many`).
- **Without a limit,** every match is kept anyway, so the sort is made as
  before. `Filter::apply`, over what a program already holds, sorts
  them all as before too.

## 176.2 Measured
Release build, an M1 Pro; 100,000 places, `lat` and `lon` random, no
index; the mean of 20 runs (5 for all), two rounds within 3%:

| | before | now |
|---|---:|---:|
| ten by `lon` | 87 ms | 52 ms |
| ten by `lon`, then `lat` | 99 ms | 62 ms |
| 1,000 by `lon` | 91 ms | 54 ms |
| all by `lon`, no limit | 215 ms | 214 ms |
| all, unsorted | 103 ms | 102 ms |

A third less time with a limit: the matches are no longer all held
and sorted, about one comparison each instead of a full sort's. Only
the ones handed out were ever read in full, before as now. Every match
is still read and its sort fields decoded; an index on the sort field
is what makes it fast (§34.2).
`measure_ten_of_a_hundred_thousand` in `tests/sort_with_a_limit.rs`,
ignored, runs it.

## 176.3 Rejected
- **A heap of `skip + limit`:** each match compared against the worst
  kept, about log(limit) comparisons each. §154's selection does about
  one comparison a match, and one code path serves both sorts.
- **Keeping the best for a sort without a limit:** it keeps every match,
  as now.
- **The same in `Filter::apply`:** its items are in memory already; the
  sort of §102.5 moves out only the ones handed out.

## 176.4 Tests
- `tests/sort_with_a_limit.rs`:
  - a model: 200 documents whose `a` is of every kind a sort orders,
    `2` and `2.0` equal, arrays and objects, or missing; random sorts by
    one to three of `a`, `b`, `c`, `_id` and `n`, either way, skips and
    limits from 0 past the count, some with a condition on `b`; without
    an index, with one on `b` that the condition reads, and with one on
    `c` that a sort by it walks. Each of 100 rounds checks `find`, a
    snapshot's `find`, `cursor`, `find_one` and a selection against
    `Filter::apply` on every document; every tenth round marks the
    matches with `update_fields`, and another deletes them with
    `delete_many`, each against the same;
  - 100 documents of three values, each way round with five limits:
    equal values by id, ascending;
  - the measurement of 176.2, ignored.
- `collection.rs`: ten of 2,000 by an unindexed field hold 21 matches at
  most, ten after skipping five 31, a limit of 0 none, and no limit all
  2,000. Memory can't be seen from a test, so the read's trace counts
  the most matches a sort in memory held at once (`Trace::held`); it is
  for the tests, not in `Explained`.
- Every earlier test that reads a sort with a limit in memory runs
  through the keep now, the key sort's (§154) and the random reads of
  `collection.rs` among them.
- The `queries` fuzz program (§105) sorts its reads with skips and
  limits and checks each against the model: 1,000 programs of seed 11.

## 176.5 Fuzzed and mutated
- **Fuzzed:** the `queries` target, whose reads sort with skips and
  limits and are checked against the model: 1,000 random programs of
  seed 11, then 15 minutes guided, 3,005 programs. Nothing found.
  Nothing in storage changed, so the other targets weren't run.
- **Mutated,** ten ways by hand, each run against the library's, the
  key sort's and these tests: one fewer kept than `skip + limit`; cut
  back one short; equal matches by id the other way; every match left
  out without a key; the last sort field not compared; descending fields
  compared ascending; the skip not taken off what is kept; and the keep
  never used, every match gathered as before. Seven failed a test at
  once. The last one stood, as results don't change, until
  `Trace::held` and the test above it. Two stand: the keep used
  without a limit, which keeps every match as the sort it replaces
  does, and the key of 0 compared the other way round, which is equal
  either way.
