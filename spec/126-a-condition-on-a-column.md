# 126. A condition on a column (`aggregate.rs`, `collection.rs`)

§107's filter picks the documents an aggregate sees, for every column
at once. Some questions want a column to see fewer:
- per team, all tasks counted, and the mean only of those with
  `hours >= 4`;
- per team, in one pass, how many are open and how many took an hour or
  more, where a filter gives one count per query and a query per count
  reads the documents again each time.

A filter in front can't say either: it would leave the others out of
every column, and out of the count of all. §126 gives a column a
condition of its own: the column takes in only the group's documents
that meet it. Standard SQL has it as `FILTER (WHERE …)` after an
aggregate, and PostgreSQL, DuckDB and SQLite (since 3.30) run it. It was
the ROADMAP's "A condition on a column".

## 126.1 The API
```rust
let per_team = tasks.aggregate(
    // Which documents: tenant t1's tasks, for every column.
    Filter::new().eq("tenant", "t1"),
    &Aggregate::new()
        .group_by("team")
        // A column named "tasks": every task of the team.
        .count("tasks")
        // A column named "open_tasks": only the team's tasks whose
        // status is "open". The string names; the condition matches.
        .count("open_tasks").filter(Condition::eq("status", "open"))
        // A column named "long_tasks": only those with hours >= 1.
        .count("long_tasks").filter(Condition::gte("hours", 1))
        // A column named "mean_long_hours": the mean of `hours`, only
        // over those with hours >= 4.
        .avg("hours", "mean_long_hours").filter(Condition::gte("hours", 4)),
)?;
// [{"team": "a", "tasks": 12, "open_tasks": 3, "long_tasks": 9, "mean_long_hours": 6.5},
//  {"team": "b", "tasks": 4, "open_tasks": 0, "long_tasks": 0, "mean_long_hours": null}]
```

As LINQ writes it, `Tasks = g.Count()` and
`OpenTasks = g.Count(t => t.Status == "open")`: the column's name is the
property's name, the condition the predicate.

- **A column's `name` names, it doesn't match:** in
  `count("open_tasks")`, `"open_tasks"` is the key the number gets in
  the row, as SQL's `count(*) AS open_tasks`; it says nothing about
  which documents count. In `sum`, `min`, `max` and `avg` the path
  comes first and is read, the name second (§107.1). Samples and
  rustdoc say so at every column method, and samples pick names that
  don't look like values (`open_tasks`, not `open` beside
  `"status": "open"`).
- **`Aggregate::filter(condition)`**: a `Condition` for the column
  added last, as SQL writes `FILTER` after its aggregate; it filters
  that one column, not the aggregate, and its rustdoc says so first.
  Called again for the same column, the conditions are ANDed, as
  `having` ANDs its own; an OR is `Condition::any`.
- **Any column takes it:** `count`, `sum`, `min`, `max`, `avg`, and
  those added later (`count_distinct`, `first`, …) without a variant
  of their own. One rule, not a name per kind.
- **The condition is on the document**, not on the row: its paths are
  the stored fields', as in a filter, whatever the columns are named.
  `having` is the condition on the row (§107.2).
- **The meaning a filter gives it:** the same `Condition`, matched as
  `find` matches it: `null` and missing as §32 has them, `=` on arrays,
  objects and binary by contents (§124), `[*]`, `elem_match`, `_id`.
- **Called before any column,** `filter` has no column to go to: the
  aggregate is refused when it runs (126.2). Since §134 the condition
  is given on the column, before its name, `count().filter(c).named(n)`,
  and a `filter` before any column doesn't compile.

## 126.2 What a column with a condition holds
For each group, the column takes in exactly the group's documents that
meet its condition, and is what §107.2 says of that column over them:
- **`count`:** how many of the group's documents meet it; `0` where
  none does.
- **`sum`:** of the numbers among them; `0` where there's none.
- **`avg`:** their exact total over how many numbers it holds; `null`
  where there's none.
- **`min`, `max`:** among them, by the sort's order; equal extremes
  give the first document's by id among them; `null` where there's
  none.

What doesn't change:
- **The groups** come from the filter's matches alone. A group none of
  whose documents meets a column's condition is still a row, the column
  empty as above: team `b` in 126.1. That is the difference from a
  filter in front.
- **A row's group values** are its group's first document's by id, of
  all its documents, whether or not that one meets any condition.
- **Without `group_by`,** still one row, even for no matches.
- **The columns without a condition** see every document of the group.

**Refused** with `Error::Io` of kind `InvalidInput`, before anything is
read, as §107.2 refuses: a `filter` before any column. A condition that
`find` would take is taken.

## 126.3 Reading the documents
The general way (§107.3), for any condition:
- **The paths decoded** are the group's, the columns', and each
  condition's fields (`Filter::fields`, §99.2), in one `Selection`
  (§106.4). Still never the whole document, unless a condition needs it
  (a path starting at `[*]`).
- **Each condition is compiled once** into a `Matcher` (§102), as a
  filter's conditions are, when the groups are set up (`Groups::new`),
  and checked per match: `add_document` makes a list of which columns
  take the document, `takes`, one `bool` a column, and a column that
  doesn't passes it over. Not by hiding its value: a `count` has none
  and counts every document it's given.
- **`aggregate_selection`** in `collection.rs` adds each condition's
  fields to the group and column paths, the empty path (the whole
  document) where a condition starts at it.
- **A count of everything, no group** (§107.3's shortcut to `count`):
  unchanged where no column has a condition. Where one has, the
  general way, one pass for every column; 126.9 has the alternative.

## 126.4 From index entries
§107.5 aggregates from index entries where an index holds the
aggregate's paths and its entries decide the filter. A column's
condition joins that on the same terms:
1. **Its paths count as the aggregate's:** the index must hold them
   too, exactly as written, and the planner is offered only such
   indexes, as it is for the columns' paths. So per team, the mean of
   `hours >= 4` reads `(team, hours)`.
2. **Its entries must decide it,** as a filter's (`Covering::new` on a
   filter of the condition alone, §100.1); otherwise the general way.
   `holds`, the test of an index in `aggregate_by_entries`, asks
   `Covering::new` of each condition's own filter
   (`Aggregate::column_filters`), which says both: whether the index
   holds what it reads, and whether its entries decide it.
3. **Per entry, each condition is decided on the key** (`Decider`,
   §101.2, on its bytes where it can, §104): true, the column takes the
   entry's value; false, the column passes it over, while the other
   columns take it; undecided (a value cut in the key), the entry's
   document is read and every column and condition checked on it, as
   §107.5 reads an entry it can't decide.
4. **A leaf counted whole** (§107.5), where every column is a `count`:
   each condition is decided for the leaf at once (`decide_all`, as
   §101.3 does for the filter). True, its column adds the leaf's
   entries; false, nothing; undecided for any, the leaf is walked entry
   by entry; `add_counted` adds the leaf's entries to the columns whose
   condition holds for it. `decide_all` decides a leaf where its two
   ends agree on
   the bytes the condition reads, or where the condition accepts one
   interval of the index's keys (§104.4). So a condition on the group
   fields, or on fields before the last of them, is decided a leaf at
   a time: per team on `(team, status)`, all and the open ones, counts
   leaves without reading them. A range on a field after the group
   fields is neither: per status on `(status, created)`, all and those
   since Monday, is one interval of keys per status, not one for the
   index, and a leaf's ends differ in `created`. Such leaves are walked
   entry by entry, from their entries still, no document read. Deciding
   them whole is a follow-up (126.9).

The run of a group (§107.5's last group remembered) is unchanged: the
conditions are decided per entry within it.

## 126.5 What explain_aggregate says
Nothing new: `documents_examined` counts the documents read, for
conditions too; `from_entries` the matches aggregated without one. A
condition's decisions aren't counted on their own.

## 126.6 What §107 and earlier give, and what stays
- **`Condition`, `Matcher`, `Covering` and `Decider`** as filters use
  them; no second meaning of a condition.
- **`Aggregate::apply`** gives the meaning in memory, conditions
  included, so the model the tests check against is the one `find`'s
  tests use.
- **No new file format, no new index kind.**
- **`find`, `count`, writes, and aggregates without a condition** don't
  change.

## 126.7 Tests
- **The meaning** (`aggregate.rs`, on `Aggregate::apply`),
  `a_column_takes_only_what_its_condition_meets`, on five documents:
  `count`, `sum`, `avg`, `min` and `max` each with a condition met by
  some of a group's documents and by none; a group none of whose
  documents meets any still a row, `0` for `count` and `sum`, `null`
  for the others; the row's group value `5.0` from its first document,
  which meets no condition, where the ones that do hold `5`; two
  `filter`s on one column ANDed beside a column with one; `is null` on
  a missing and a `null` field; `tags[*] = "x"` on arrays; conditions
  on fields no group or other column names; without `group_by` and no
  documents, one row of zeros.
- **Refused:** `filter` before any column, in
  `a_wrong_aggregate_is_refused`.
- **The two ways agree:** `aggregate_agrees` (§107.9) gives each column
  up to two conditions, three times in five: a comparison with any
  operator on a path of its pool, so half the time on the index's
  fields, against the value of a document found or a random one; now
  and then negated, an OR of two, or an `exists`, which no entry
  decides. The corpora of §106 and §107 run them through every plan
  against `Aggregate::apply` on `find`'s documents, and §100's still
  aggregates more than a quarter of its matches from entries.
- **What reads documents**
  (`a_condition_on_a_column_reads_documents_only_where_entries_cannot_tell`),
  on §107.9's 3,000 tasks, with `(status, created)`, `(tenant, tries)`
  and `(tenant, status)`:
  - per tenant, all counted and the mean `tries` where `tries >= 4`:
    `IndexOrder` on `(tenant, tries)`, 0 documents read, 3,000 from
    entries;
  - per tenant, all and the running ones: `IndexOrder` on
    `(tenant, status)`, 0 read, more than five leaves counted whole;
  - per status, all and those since a date: `IndexOrder` on
    `(status, created)`, 0 read, 3,000 from entries, no leaf counted
    whole (126.4);
  - per tenant, the mean `tries` of the done ones: no index holds
    `tenant`, `tries` and `status`, so a `Scan`, 3,000 read;
  - on 20 notes with `(tenant, note)`, five of them 1,201 characters
    long, a count of those equal to one such note: 5 read, the long
    ones whose keys are cut, 15 from entries.
- **The `queries` target (§105):** its `aggregate` step gives each
  column a condition half the time, from the generator a filter's
  conditions come from. 20,000 smoke programs (seed 126126, `--cfg
  fuzzing`, 2,465 s on a machine busy with the mutations) found
  nothing.

## 126.8 Mutations
Each against the library's tests and 300 smoke programs (`--cfg
fuzzing`):

| Mutation | Unit tests | Smoke |
|---|---|---|
| the condition ignored for `min` | caught | survived |
| a column's condition applied to every column | caught | caught |
| a group made only of documents meeting every condition | caught | caught |
| the row's group values from a document meeting the conditions | caught | survived |
| an entry the key leaves undecided taken as meeting it | caught | survived |
| a leaf undecided by a condition counted whole | caught | survived |
| a condition's fields left out of the `Selection` | caught | caught |
| an index offered that doesn't hold a condition's paths | caught | caught |

The four smoke survivors are each caught by one test written for it:
the meaning (`min`, the row's value) and what reads documents (a cut
key, a leaf undecided). The random corpora and programs miss the last
two: they rarely make a key cut inside a condition's value, or a leaf
of counts whose ends differ in a condition's field.

## 126.9 Rejected
- **A function per kind** (ClickHouse's `countIf`, `avgIf`; BigQuery's
  `COUNTIF`; Spark's `count_if`): five names now, two for every kind
  added later.
- **`count(condition)`:** in SQL `COUNT(x)` counts where `x` isn't
  null, so `count(hours >= 1)` counts every document with `hours`, true
  and false alike. A different meaning under a known name.
- **MongoDB's way**, a `$cond` turning the others into `null`, which
  `$avg` passes over: an expression language, rejected in §107.8, and
  it can't count (a `$sum` of 1 and 0 does).
- **A closure** (`.filter(|doc| …)`): no index decides it, no text
  prints it, no fuzz model compares it.
- **A `Filter` for a condition:** its sort, skip, limit and select mean
  nothing for a column. `Condition` is the part that does.
- **The condition as an argument of each column**
  (`.avg("hours", "mean", Some(c))`): every call pays for the rare
  case.
- **Another name:** `only` or `when` read well too, but `filter` is
  what SQL calls it and what `Iterator::filter` does, keep those that
  meet it. Since the document filter is an argument of `aggregate`,
  not a method of `Aggregate`, the two don't meet in code.
- **A leaf decided whole for a range after the group fields**
  (126.4), in this step: where a leaf's ends agree on their group
  values, a condition on the next field of the index accepts one
  interval of the keys under that prefix, so both ends meeting it means
  every entry between does. A new case in `decide_all`, with its own
  tests and mutations; left as a follow-up (ROADMAP) until 126.10's
  per-status count since a date shows the entry-by-entry walk costs
  enough.
- **Counts of everything as one `count` per column** (§100, §101):
  with an index for each, `filter ∧ condition` per column could be
  cheaper than one pass; without, it reads the documents once per
  column. One pass first; per-column counts if a workload shows the
  difference.

## 126.10 Measurements
`bench/src/bin/aggregate.rs`, on the 100,000 tasks of §107.10, with
SQLite's `FILTER (WHERE …)` on the same indexes. Release build, Apple
M1 Pro, two runs, on a machine kept busy by other programs (a load of
20 to 30): the times are wider apart than §107.10's.

| Query | trunkdb | How | SQLite |
|---|---:|---|---:|
| per tenant, all and the mean tries of `tries >= 4`, no index | 141–180 ms | `Scan`, 100,000 read | — |
| the same, on `(tenant, tries)` | 14.8–17.1 ms | `IndexOrder`, 0 read, every entry | 5.67–5.95 ms |
| per status, all and those since a time, on `(status, created)` | 16.2–25.6 ms | `IndexOrder`, 0 read, every entry | 5.69–6.66 ms |
| three counts in one aggregate, no group | 105–109 ms | `Scan`, 100,000 read | 11.8–13.7 ms |
| the same as three `count`s | 64.5–68.2 ms | three index reads | — |

- **A condition costs about what a column does:** per tenant on
  `(tenant, tries)`, 14.8–17.1 ms with the condition against 13.8–14.1
  ms for §107's count and average without one, in the same runs;
  2.6–2.9× SQLite's, as there.
- **Without conditions nothing changed,** once the entries' walk kept
  one list of each column's decisions from entry to entry, and none
  where no column has a condition: the first build made a list per
  entry, and §107's aggregates from entries took 30–48% longer (one
  tenant's average 192–199 µs against 134–135). The last commit's
  benchmark and this one run one after the other: 139–148 µs against
  137–144, 13.8–14.1 ms against 13.0–17.2 per tenant.
- **Since a time, entry by entry** (126.4): 16–26 ms, against 0.7 ms
  for §107's per status with its leaves counted whole. Deciding the
  leaves whole for a range (126.9, ROADMAP) would close most of that.
- **Three counts in one pass** read every document, 105–109 ms; three
  `count`s through their indexes take 65–68 ms, two of the three
  conditions served by an index. SQLite reads its row columns in
  11.8–13.7 ms, 8× faster than the one pass, 5× than the three counts. Counts of everything as a `count` per column
  (126.9) would pay here.

## 126.11 Limits
- **Conditions are on documents,** paths only, as filters are: no
  expressions.
- **A condition on a field the index lacks** reads the documents, even
  where the index holds everything else.
- **A range on a field after the group fields** walks the index entry
  by entry: no leaf is counted whole for it (126.4).
- **One pass for a count of everything** with conditions, where §107.3
  counts without reading.
- **Not in the text form yet:** aggregates as text are open (§111.11);
  there it would read `count() filter (where hours >= 1) as long_tasks`,
  as SQL writes it.
