# 126. A condition on a column (`aggregate.rs`, `collection.rs`)

**Draft: planned, not built.** What follows is the design; tests,
mutations and measurements are what the build has to show (126.7,
126.8), and the section is rewritten as built when it lands.

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
  Called again
  for the same column, the conditions are ANDed, as `having` ANDs its
  own; an OR is `Condition::or`.
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
  aggregate is refused when it runs (126.2).

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
  filter's conditions are, and checked per match for its column only.
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
   by entry. `decide_all` decides a leaf where its two ends agree on
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

## 126.7 Tests (to build)
- **The meaning** (`aggregate.rs`, on `Aggregate::apply`),
  `a_column_takes_only_what_its_condition_meets`: each kind with a
  condition, one met by some, none and all of a group's documents; a
  group no document of which meets it still a row; the row's group
  values from a document that meets no condition; two `filter`s ANDed;
  a condition on `null` and missing, on an array by contents, with
  `[*]` and `elem_match`; a condition on a field no other column or
  group names; without `group_by` and no matches.
- **Refused:** `filter` before any column, in
  `a_wrong_aggregate_is_refused`.
- **The two ways agree:** `aggregate_agrees` (§107.9) gives some
  columns a random condition, half the time on the index's fields, so
  the corpora of §106 and §107 run them through every plan against
  `Aggregate::apply` on `find`'s documents.
- **What reads documents**
  (`a_condition_on_a_column_reads_documents_only_where_entries_cannot_tell`),
  on §107.9's 3,000 tasks:
  - per tenant, all counted, mean `tries` where `tries >= 4`, on
    `(tenant, tries)`: 0 documents read;
  - per team, all and the open ones, on `(team, status)`: 0 read,
    leaves counted whole;
  - per status, all and those since a date, on `(status, created)`:
    0 read, every entry walked, no leaf counted whole (126.4);
  - a condition on `status` with only `(tenant, tries)`: the documents;
  - a condition on a long string cut in the key: those entries'
    documents only.
- **The `queries` target (§105):** its `aggregate` step gives columns a
  random condition from the generator a filter's conditions come from;
  a smoke run of 20,000 programs.

## 126.8 Mutations (to run)
Each against the unit tests and 300 smoke programs:
- the condition ignored for one kind;
- a column's condition applied to every column;
- a group made only of documents meeting some condition;
- the row's group values from the first document meeting a condition;
- an entry the key leaves undecided taken as meeting it;
- a leaf undecided by a condition counted whole;
- a condition's fields left out of the `Selection`;
- an index offered that doesn't hold a condition's paths.

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

## 126.10 Measurements (to take)
`bench/src/bin/aggregate.rs`, on the 100,000 tasks of §107.10, with
SQLite's `FILTER (WHERE …)` on the same indexes:
- per tenant, count and mean `tries` where `tries >= 4`, without an
  index and on `(tenant, tries)`;
- per status, all and those since a date, on `(status, created)`;
- three conditional counts in one aggregate, against three `count`
  queries.

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
  there it would read `count() filter (where hours >= 1) as long_tasks`, as
  SQL writes it.
