# 107. Aggregates: count, sum, min, max and avg by group (`aggregate.rs`, `collection.rs`, `query.rs`, `snapshot.rs`, `explain.rs`)

v0 left aggregates to application code (§5.1, §6): a `find`, then a
loop. Then that cost what the engine would have paid, since `count`
read every document too. It doesn't any more:
- `count` reads no document where an index decides the filter (§100);
- a walk decides a run of equal values once, and passes a leaf whole
  (§101, §104);
- a match can be handed out from its entry (§106.5).

Application code can't use any of that. "How many tasks per status" in
a loop reads 100,000 documents. An index on `(status, created)` holds
the answer in its entries, three runs of keys side by side.

§107 adds `Collection::aggregate`: the documents a filter finds, put in
groups by the values of some fields, and per group a count, a sum, a
minimum, a maximum or an average. SQL calls it `GROUP BY`, MongoDB
`$group`, LINQ `GroupBy`. No joins: one collection, as everywhere. It
was the ROADMAP's "Aggregates, reconsidered (§106.7)".

## 107.1 The API
```rust
let per_status = tasks.aggregate(
    Filter::new().eq("tenant", "t1"),
    &Aggregate::new()
        .group_by("status")
        .count("n")
        .avg("tries", "average_tries")
        .max("created", "newest"),
)?;
// [{"status": "Done", "n": 340, "average_tries": 4.5, "newest": ...},
//  {"status": "Queued", "n": 330, ...}, {"status": "Running", ...}]
```

- **`Collection::aggregate(filter, &Aggregate) -> Result<Vec<Document>>`**,
  on `Collection<T>` of any `T` and on `View<T>`. The rows are
  documents: a row isn't a `T`, and `Document::into_value::<Row>()`
  turns one into a struct (§13).
- **`Aggregate`**, a builder beside `Filter` in `query`:
  - `group_by(path)`, more than once for several fields; without it,
    one group of every match;
  - `count(name)`, and `sum(path, name)`, `min(path, name)`,
    `max(path, name)`, `avg(path, name)`: each names its column. The
    path comes first, as SQL writes `sum(tries) AS total`, so
    `.sum("tries", "total")` reads the same way (the draft put the
    name first, after MongoDB's `{total: {$sum: "$tries"}}`);
  - `having(condition)`: a `Condition` on the rows, by column name,
    SQL's `HAVING`;
  - `sort_asc(column)`, `sort_desc(column)`, then `then_asc` and
    `then_desc` for further keys, as on `Filter`; `skip(n)`,
    `limit(n)`: on the rows;
  - `apply(documents)`: the aggregate over documents in memory, as
    `Filter::apply` is the filter's meaning (107.9).
- **The filter picks the documents, whole:** conditions, sort, skip and
  limit, as `find` would. So "the average of the last 100 tasks" is
  `Filter::new().sort_desc("created").limit(100)` and an `avg`. The
  filter's `select` doesn't matter: the aggregate reads what it needs.
- **The plan:** `Collection::explain_aggregate(filter, &aggregate)`,
  as `explain_run` for `find`: the plan, the reads, the counts, and the
  groups (107.6).

## 107.2 What a row holds
- **The group's values,** at their paths: `group_by("address.city")`
  gives `{"address": {"city": ...}}`, as a selection would (§106.2).
  Several group fields are nested and ordered as `select` orders them.
- **Then each column,** in the order the builder added them.
- **No `_id`:** a row is no stored document.

**Groups:**
- **A missing field groups as `null`,** with the nulls, as conditions
  and sorts treat it (§32). SQL and MongoDB do the same.
- **Values a filter finds equal are one group:** `5` and `5.0` are one,
  `"a"` and `"A"` two. Values no comparison orders (arrays, objects,
  binary, NaN) are one group per equal encoding: `[1, 2]` is a group,
  `[2, 1]` another, and every NaN of one bit pattern one.
- **The value a row shows** is its group's first document's, by id: of
  `5` (inserted first) and `5.0`, the row says `5`.
- **An array is a value, not its elements:** `group_by("tags")` groups
  documents with equal arrays. Grouping by each element (MongoDB's
  `$unwind`) is left for later (107.8).

**Columns:**
- **`count`:** the documents in the group.
- **`sum`:** of the values that are numbers; others (missing, null,
  strings, ...) are passed over, as SQL passes over `NULL`. An `Int`
  when every value summed is one and the total fits in `i64`, a `Float`
  otherwise; `0` (an `Int`) when nothing is summed. The total is exact
  until it is rounded once at the end (107.4), so it doesn't depend on
  the order the documents are read in, and a zero is `0.0`, never
  `-0.0`.
- **`avg`:** that exact total divided by how many numbers it holds, a
  `Float`; `null` if none.
- **`min`, `max`:** by the order a sort uses (§34.1), across types:
  `null` and missing are passed over, and values no comparison orders
  count only where nothing else is there, as they sort last. `null` if
  there's no value. Equal extremes (`5` and `5.0`) give the first
  document's by id.
- **No group, no row;** without `group_by`, one row even for no
  matches: `{"n": 0, "total": 0, "average": null}`, as SQL's
  `SELECT count(*)` gives a row.

**Rows** come in the order of their group values, ascending, as a sort
by the group fields would put them, unless the row sort says otherwise;
then ties keep that order. `having` runs before the sort, `skip` and
`limit` after it.

**Refused** with `Error::Io` of kind `InvalidInput`, before anything is
read, as `drop_index("_id")` is: two columns of one name; a column
whose name is a group field's first step, or the first step of one; an
empty name; a path with `[*]` to group by or aggregate (107.8); a
`having` or a row sort naming no column and no group field.

## 107.3 Reading the documents
The general way, for any filter and any aggregate:
- The filter runs as `find`'s does, on its plan (§28.4): its
  candidates, checked on the fields they name (§99.2), sorted, skipped
  and limited.
- **Each match is decoded on the fields the aggregate needs:** the
  group and column paths are a `Selection` (§106.4), decoded in one
  walk. Never whole.
- **Groups are kept in memory**, each with its columns' running state,
  found through a map ordered by the group values (107.2's order).
  Memory grows with the groups, not the matches.
- `count` alone with no group is `count` itself (§100, §101), factored
  out as `count_matches`.

## 107.4 Exact sums
A float sum depends on the order it adds in: summed in index order and
in id order, the same numbers can give results apart in the last bit.
The two ways of reading (107.3, 107.5) would then disagree, and so
would §105's model.

So a sum is kept exact:
- **Integers** in an `i128`, while every value is an `Int`. It
  overflows only past 2^64 values of `i64::MAX`.
- **Floats** in a fixed-point integer of 34 64-bit limbs, two's
  complement, in units of 2^-1074, the smallest subnormal. From there
  to past `f64::MAX` (2^1024) is 2,098 bits, and the rest is room for
  carries and the sign: every finite `f64` is an integer of such units,
  so adding one is exact, and adding in any order gives one total.
- **The total** is rounded once to `f64` at the end, to nearest, ties to
  even, as IEEE rounds. A zero is `0.0`.
- **NaN and infinities** are kept apart and follow IEEE: one NaN makes
  the sum NaN, and `+inf` and `-inf` together make it NaN.

The draft planned Shewchuk's partials (Python's `math.fsum`). They are
exact only while no partial overflows: `[1e308, 1e308, -1e308]` gives
`inf` in that order and `1e308` in another. The wide integer has no
such case, and costs about the same: a shift and a few limb additions
per value.

## 107.5 From index entries
Where an index holds the aggregate's fields, its entries hold most of
the answer. A read of index entries (§100) aggregates them without
their documents when:
1. **its entries decide the filter** (`Covering`, §100.1), for the
   entries they decide; an entry they can't decide reads its document;
2. **every group path and column path is one of the index's fields**,
   exactly as written (§106.5), and the index isn't multikey;
3. **the filter has no sort, skip, limit or ids**, and no column is a
   `sum`;
4. **the read holds every match.** With conditions: exactly one range,
   of an index that holds every path and covers the filter; the planner
   is offered only such indexes, so `tenant == x` averaging `tries`
   reads `(tenant, tries)` rather than the `tenant` index `find` would
   take. Without conditions, only an index that holds every document
   does: a compound index (§43.2), or a single-field one whose entries
   are as many as the documents, counted leaf by leaf (§101.4);
   otherwise a single-field index leaves out what no comparison matches
   (§28.1), which may be summed or grouped. One whose leading fields
   are the group fields is preferred.

Then, for an entry whose values are exact (`key::part_is_exact`):

| Column | From the entries | A document read |
|---|---|---|
| group values | a bool, a string that fits, an id, a date-time, `null` | one per group, for a number: `5` or `5.0` (107.2) |
| `count` | always | never |
| `avg` | numbers below 2^53, exact as floats; non-numbers passed over by their tag | for a value that isn't exact |
| `min`, `max` | every entry of the group compared, in key order | the one extreme document, if its value is a number |
| `sum` | not: its type (`Int` or `Float`) isn't in the key | every document with a number |

A number in a key is its value only: the key doesn't say whether it was
stored as `5` or `5.0`. Such a value is kept with where its document
is, and read when the rows are made, only if it is shown: a group's
value from its first document, an extreme. An entry whose values
aren't exact (a cut string, a number past 2^53) reads its document and
is grouped as 107.3 groups it.

**Runs of a group.** Entries come in key order, so where the group
fields lead the index a group's entries come one after another.
- **The last group is remembered:** an entry whose key bytes up to its
  last group value are the last entry's goes to that group without its
  values decoded or the map searched. Equal bytes are equal values; a
  group cut into runs, as `(tenant, tries)` grouped by `tries` cuts it,
  is found in the map at each run.
- **A leaf counted whole:** where every column is a `count`, a leaf
  whose two ends share their bytes through the last group value is in
  one group, and is counted without being read, as `count` does
  (§101.3). "Tasks per status" on `(status, created)` reads three runs
  of entries and no document.

The draft planned to finish each group as the walk leaves it, holding
one group's state, and to seek a group's ends for `min` and `max`. As
built, groups stay in the map until the end, and `min`/`max` compare
every entry of the group: both are left as follow-ups (ROADMAP).

**Planner.** The plan is `find`'s, a `Scan` or an index read, unless
the entries can answer: then, without conditions, `IndexOrder` on the
index walked; with them, `Index` on the index read.

## 107.6 What explain_aggregate says
An `Explained` (§94) for the aggregate's read:
- the plan, the reads and their bounds, as for `find`;
- `documents_examined`: documents read, for matching or for values;
- `from_entries` (§106.6): matches aggregated from their entries alone,
  those of a leaf counted whole included;
- **`groups`**, new, in `ExecutionStats`: how many groups there were,
  before `having`;
- `documents_returned`: the rows.

## 107.7 What §106 and earlier give, and what stays
- **Paths** are parsed once per read (§102); the group and column
  paths are a `Selection` (§106.4).
- **Entry values** come from `key::decode_part_into` and `Covering`
  (§100), the leaf counts from `whole_leaf` (§101).
- **No new file format, no new index kind.** An index on `(status,
  created)` serves both "the queued, newest first" and "how many per
  status".
- **Writes, `find` and `count`** don't change.

## 107.8 Rejected
- **Aggregates on `Filter`** (`Filter::new().group_by(..)`, §106.7's
  first sketch): a filter that returns rows instead of documents would
  make every read method's result depend on it. A second argument keeps
  `find` what it is, and lets the filter pick the documents whole,
  sort and limit included.
- **An expression language** (`sum(price * qty)`, `$cond`): a second
  query language. Columns are paths, and computing is left to
  application code on the rows.
- **Grouping by array elements** (`$unwind`): what a multikey index
  does for conditions (§42), but its rows count a document once per
  element; a design of its own, if asked for.
- **`count_distinct`, `first`, `last`, `push`, percentiles:** when a
  workload asks. `count` of a path, SQL's `COUNT(col)`, is `count`
  with `having` or a `sum` of a 0/1 field; left out for now.
- **Type bits in keys** for `sum` from entries: an `Int`/`Float` bit
  and a missing mark in every key, file format 13 and every index
  rebuilt (§106.8). `count`, `avg`, `min` and `max`, the common ones,
  don't need it. Weighed again if `sum` over an index turns out to
  matter.
- **A float sum in reading order:** results that differ by plan
  (107.4).
- **Keeping running totals in the catalog** (materialized aggregates):
  a write to the catalog in every batch, for one query shape. The
  ROADMAP's kept total for `count` of everything is the same idea for
  the cheapest case, still open.
- **Typed rows** (`aggregate::<Row>`): a row is not a stored document,
  and `into_value` is one call.
- **Joins:** still not planned (§6).
- **The trunkdb command and the admin** don't get aggregates in §107.

## 107.9 Tests
- **The meaning** (`aggregate.rs`, on `Aggregate::apply`):
  - `groups_are_values_a_filter_finds_equal`: `5` and `5.0`, missing
    and `null`, arrays and NaN, the value a row shows, nested group
    paths;
  - `columns_count_sum_average_and_find_extremes`: sums of `Int`s,
    mixed, overflowing `i64`, empty, of `-0.0`, NaN and infinities;
    `avg` of none; `min`/`max` across types, of none, of equal
    extremes; one row without `group_by`;
  - `rows_are_picked_sorted_and_cut`: `having`, row sorts, skip and
    limit;
  - `a_wrong_aggregate_is_refused`: each refusal of 107.2.
- **Exact sums** (`sums_are_exact_in_any_order`): 300 lists of up to 40
  random floats, 53-bit mantissas at exponents from -40 to 40, each
  summed in its order, shuffled and reversed, give one result to the
  bit. Known totals, as `math.fsum` gives them: ten `0.1` make `1.0`;
  `[1e100, 1, -1e100, 1e-100]` makes `1.0`; two smallest subnormals
  make the next; `[MAX, MAX, -MAX]` makes `MAX`, where partials
  overflow; `-0.0 + -0.0` is `0.0`; `1 + 2^-53` is a tie, kept at `1`,
  and a bit past it rounds up; integral floats sum to the exact
  integer.
- **The two ways agree:** the randomized corpora §106 runs selections
  on (sorted finds through every index, §47, §72, §96; compound and
  sparse compound through every kind of write, §43, §44; §100's,
  §101's and §103's) run each filter again with a random
  aggregate (`aggregate_agrees`), half the time on one index's fields:
  `aggregate` and a `View`'s must give what `Aggregate::apply` gives on
  `find`'s documents. On §100's corpus
  (`aggregates_from_entries_agree_with_the_documents`) more than a
  quarter of the matches must come from entries.
- **What reads documents**
  (`an_aggregate_reads_documents_only_where_entries_cannot_tell`), on
  3,000 tasks:
  - per status: `IndexOrder` on `(status, created)`, 0 documents read,
    3,000 from entries, more than five leaves counted whole;
  - the newest per status (a date-time): 0 read;
  - per tenant, average and fewest tries (`tenant >= "t5"`): `Index`
    on `(tenant, tries)`, five documents read for five groups, one for
    each fewest;
  - grouped by `tries`: 7 read, one per group, on `tries`;
  - `sum` for one tenant: 300 read, 0 from entries;
  - with a `tenant` index beside `(tenant, tries)`, one tenant's
    average: `(tenant, tries)`, 0 read, 300 from entries;
  - an array of tries inserted: `tries` no longer holds every document,
    so `(tenant, tries)` is walked, 8 read for 8 groups; that index
    dropped, a `Scan`;
  - a sorted, limited filter: from its documents.
- **The `queries` target (§105)** gained an `aggregate` step: a random
  aggregate, half the time on one existing index's fields, on a
  focused query, half the time without its sort and limit, against the
  model's `Aggregate::apply`. A skip or limit without a total order
  picks no defined set of documents, so such a query is given `_id` as
  its last sort key first (`total()`), as `delete_many` is; an early run
  found that as a false alarm. 20,000 smoke programs (seed 107107,
  `--cfg fuzzing`, 1,118 s) found nothing.
- **Mutations**, each run against the unit tests and 300 smoke
  programs:

  | Mutation | Unit tests | Smoke |
  |---|---|---|
  | a number from a key taken as stored | caught | caught |
  | `sum` from entries | caught | caught |
  | a single-field index taken as holding every document | caught | caught |
  | a leaf's group compared one value short | caught | survived |
  | leaves counted whole for any column | caught | survived |
  | a missing field its own group | caught | caught |
  | `min` taking a null | caught | caught |
  | a group's value from its last document | caught | survived |
  | a carry lost in the wide sum | caught | survived |
  | `max` reversing the unordered ones too | caught | survived |
  | low bits of a float dropped | caught | caught |
  | the last group's first document not replaced by an earlier one | caught | — |
  | any entry taken into the last group | caught | — |

  A float sum in reading order, planned as a mutation, isn't one any
  more: no reading order exists in the wide sum.

## 107.10 Measurements
`bench/src/bin/aggregate.rs`, on the benchmark's 100,000 tasks (§48),
`plain` without an index, `tasks` with the comparison's indexes and
`(tenant, tries)`; SQLite beside it with `GROUP BY` on its columns and
the same indexes. Release build, Apple M1 Pro, two runs:

| Query | trunkdb | How | SQLite |
|---|---:|---|---:|
| per status, a `find` and a loop | 75.7–77.7 ms | typed, every document | — |
| per status, no index | 94.1–95.2 ms | `Scan`, 100,000 read | — |
| per status, on `(status, created)` | 673–691 µs | `IndexOrder`, 0 read, leaves counted whole | 4.57–4.61 ms |
| per tenant, count and average tries, no index | 117–119 ms | `Scan`, 100,000 read | — |
| the same, on `(tenant, tries)` | 13.2–13.7 ms | `IndexOrder`, 0 read, every entry | 5.41 ms |
| one tenant's sum of tries (1,000) | 851–887 µs | `Index` on `tenant`, 1,000 read | 32.9–35.3 µs |
| one tenant's average tries (1,000) | 135–141 µs | `Index` on `(tenant, tries)`, 0 read | 34.1–37.1 µs |
| newest per status, on `(status, created)` | 14.1–14.5 ms | `IndexOrder`, 3 read, every entry | 6.55–7.03 ms |

- **Per status from the index** is under a millisecond, as the draft
  expected, and about 140× faster than the scan; SQLite's `GROUP BY` on
  its index walks every entry and takes 6.7× as long.
- **Where every entry is compared** (an average, a newest), about 135 ns
  an entry, 2.5× SQLite's time. The first runs took 600 ns: each entry
  decoded its group values and searched the map. Remembering the last
  group (107.5) took the average per tenant from 59.6 ms to 13.3 ms and
  the newest per status from 45.6 ms to 14.2 ms.
- **The planner's own index:** one tenant's average read the 1,000
  documents through `tenant` (972 µs) until the entries' read was
  offered only indexes holding every path; now 140 µs.
- **`sum` reads its documents:** 870 ns a document, 25× SQLite's sum
  over its row columns. The cost of 107.8's choice, until type bits in
  keys are weighed.
- **The application loop** (a typed `find`, then counting) is faster
  than the aggregate over every document, 76 against 95 ms: it decodes
  one known struct, the aggregate a `Selection` into `Document`s. It
  matters only where no index helps.

## 107.11 Limits
- **Groups in memory**, one per distinct value: grouping 100,000
  documents by `_id` holds 100,000 groups.
- **`sum` reads documents** even where an index holds its numbers
  (107.5).
- **`min` and `max` compare every entry** of a group, not its ends.
- **One collection, paths only,** no expressions, no `$unwind`.
- **Rows aren't streamed:** `aggregate` returns them all, as a sorted
  `find` does.
