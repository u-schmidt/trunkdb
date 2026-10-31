# 139. Live aggregates, and a window (`live.rs`, `aggregate.rs`, `collection.rs`, `database.rs`, `datetime.rs`)

Readings sent every few seconds, a server's load among them, ask what
a fence asks of a position (§138), of a mean over a window: "the mean
load of each server over the last five minutes", and an alert as it
passes 0.8. This step keeps an `Aggregate` (§107) as a subscription,
as §138 keeps a `Filter`, and gives an aggregate a window to keep the
last five minutes in. A limit is a `having`, and crossing it is a row
coming into the result or leaving it.

```rust
let busy = Aggregate::new()
    .group_by("server")
    .avg("load").named("mean")
    .window("at", Duration::from_secs(300))
    .having(Condition::gt("mean", 0.8));
let alerts = readings.watch_aggregate(Filter::new(), &busy)?;
for event in alerts {
    match event? {
        LiveRow::Entered { row, .. } => { /* over the limit */ }
        LiveRow::Left { group, row, .. } => { /* under it; row None if gone */ }
        LiveRow::Changed { row, .. } => { /* still over, a new mean */ }
        _ => {}
    }
}
```

## 139.1 Run again whole, inside each commit
After a batch is durable and published, still under the writer's lock,
each live aggregate on a collection the batch changed runs again,
whole, on the snapshot just published, after the live queries are
checked (§138.2). Every crossing is seen, in order, by a reader
however late: a server over the limit and under again in two commits
is `Entered`, then `Left`.

It runs for any change to its collection, not only to documents that
match: a reading that no longer matches changes the result too, and
its version before the commit isn't read. A commit to another
collection doesn't run it.

There is no incremental maintenance (DBSP, TimescaleDB's continuous
aggregates): five minutes of a reading every five seconds is 60
documents, read through an index on the time. Batches record their
changes while one is open, as for a subscriber (§135.4); only which
collections they touched is used.

## 139.2 `Aggregate::window(path, duration)`
The documents whose `DateTime` at `path` is within `duration` of the
newest one's, among those the filter finds. A filter can't say it:
`at >= now - 5 min` is fixed as the filter is made.

- **Counted back from the newest document, not from the clock.** Only a
  commit changes the result, and the same documents give the same
  answer. Counted back from the clock, documents age out with nothing
  written, and a timer would have to run the aggregate again: a second
  source of events, decided only if asked for.
- **Both ends in:** the newest, and one exactly `duration` before it,
  to the nanosecond. A `duration` reaching past the earliest time there
  is keeps every date-time (`DateTime::back` stops there).
- **One newest for the whole aggregate,** not one per group. A server
  gone silent ages out as the others report on, and leaves an alert.
  With a newest of its own, its last five minutes would stay for ever.
- **A `DateTime` only** (§69): a document whose value there is
  anything else, or missing, is out. Date-times compare with
  date-times alone (§69.2), so the newest is read as the first match of
  `path >= the earliest time` sorted newest first, through an index on
  `path` where there is one; then the window is a condition,
  `path >= newest - duration`, ANDed with the filter's, and an index
  bounds that read too. With no date-time there, nothing is in: no
  group, and without `group_by` one row of none.
- **Part of `Aggregate`, not of live use alone:** `aggregate`,
  `explain_aggregate`, a snapshot's `aggregate` and `Aggregate::apply`
  take it, so an aggregate answers the same run once or live.
  `apply` finds the newest among the documents it is given.
- **Refused beside a filter's sort, skip or limit,** with an
  `InvalidInput` error as `aggregate`'s other refusals. Those are a
  window by count ("the last ten readings",
  `Filter::new().sort_desc("at").limit(10)`, which needed nothing
  new). Both at once would need a rule for which comes first, and
  `apply`, given what the filter found, could keep only one: its
  answer and the database's would part.
- **Its path must name a field:** not empty, not with `[*]`, not
  `_id`, refused as a group's path is (§107.2).

## 139.3 What is reported
A row is known by its group values, made canonical as groups are
(§107.2), so `5` and `5.0` are one row. Against the result before:

| Before | Now | Reported |
|---|---|---|
| not in the result | in it | `Entered` |
| in it | not in it | `Left` |
| in it | in it, other values | `Changed` |
| in it | in it, the same values | nothing |

- **`LiveRow`** (`#[non_exhaustive]`, each variant too, §117):
  `Entered`, `Left` and `Changed`, each with `group`, the row's group
  values at their paths (`{}` without `group_by`), and `row`. `Left`'s
  `row` is the row as it is now where its group still has documents,
  failing the `having` or left out by the row sort's `skip` or
  `limit`; `None` where no document of the group is left. `group()`
  reads the group without a `match`.
- **Every row first,** as `Entered`, in the result's order.
- **The rows that left first,** in their order before, then those that
  came in or changed, in their order now.
- **`Changed` only where a value changed:** both rows are in hand, so
  they are compared, unlike a live query's `Changed` (§138.4). A float
  to the bit, an object's fields in any order, as objects compare by
  their contents (§124): a group's object shown with its fields in
  another order, as another document came first, is the same.
- **Rows as `Document`s,** as `aggregate` hands them out.

**Everything `aggregate` takes goes,** the filter's sort, skip and
limit and the rows': the aggregate runs again whole, so "the three
busiest servers" is live too. A server pushed out of the three is
`Left`, with its row, and the one taking its place `Entered`; a new
order within the three is no event. To report the rows that left with
their values, each run aggregates every group, then applies `having`,
the row sort, skip and limit, as the aggregate does at its end.

## 139.4 The API
- **`Collection::watch_aggregate(filter, &aggregate) ->
  Result<LiveAggregate>`**, on any collection, typed or not: rows are
  `Document`s either way. It takes the aggregate by reference, as
  `aggregate` does, and keeps a clone. Refused as `aggregate` refuses;
  it waits for a batch being committed to finish, as `subscribe` does.
- **`LiveAggregate`** reads as `LiveQuery` does: `recv`, `try_recv`,
  `recv_timeout`, each an `Option<Result<LiveRow>>`, and an
  `Iterator`; dropped, it ends.

The ROADMAP's plan had no `group` in `LiveRow`. A row gone has none to
show, and the application must still know which server's alert ended.

## 139.5 What ends one
As §138.5: a panic in a `Condition::custom`, in the filter or a
column's condition, ends that live aggregate with
`Error::ClosurePanicked`, the commit standing and the others going on;
a panic in the first run fails `watch_aggregate`. Any other error as it
runs again ends it with that error. Dropped, it is noticed at the next
commit to its collection. `drop_collection` ends nothing: the
aggregate runs again on no documents, so every row `Left` with `None`,
and without `group_by` the one row `Changed` to its values for none.

## 139.6 Rejected
- **Incremental maintenance** (139.1).
- **A window from the clock** (139.2).
- **One newest per group** (139.2).
- **A window with the filter's sort and limit,** in some order
  (139.2).
- **`Live` reused for rows:** it carries a `DocId`, which a row hasn't.
- **Typed rows:** `aggregate` hands out `Document`s; a typed row may
  come with a typed `aggregate`.

Later, if asked for: **a window for each group,** by count or by time,
"the last ten readings of each server", which neither the filter's
limit, counted over every group, nor one newest for the whole
aggregate gives. SQL's window functions:
`ROW_NUMBER() OVER (PARTITION BY server ORDER BY at DESC) <= 10`. It
matters where groups report at rates far apart.

## 139.7 Limits
- **Without a window, every commit to the collection runs the aggregate
  over every match,** under the writer's lock: 100,000 readings are
  100,000 read at each commit. A window, and an index on its path, are
  what make it cheap; so is a filter an index bounds.
- **A `group_by` of many groups** compares as many rows at each commit.
- **The newest is read first,** a second index read for each run of a
  windowed aggregate.
- As §138.7: events wait in memory without limit; a dropped
  `LiveAggregate` is noticed late; events are lost if the process
  stops; a read-only open reports the first rows and nothing after.
- Left to the application: hysteresis at the limit, a mean wobbling
  around 0.8, and readings sent late.

## 139.8 Tests
- `tests/live_aggregates.rs`:
  - a window: both ends in, to the nanosecond; values not date-times,
    and missing, out; through an index on its path the same; a window
    past all of time; each answer the database's, a snapshot's and
    `apply`'s alike;
  - one newest for the whole aggregate, among the filter's matches;
    none there, no group, and one row of none without `group_by`;
    refused beside a sort, a skip or a limit, once and live;
  - a window read through an index: at most 12 documents examined of
    500;
  - a window's path refused: empty, `[*]`, `_id`;
  - every row first, and the one row without `group_by`;
  - crossing a `having`: in, changed, out with its row, gone with none;
  - `Changed` only where a value changed: the same mean no event; `5`
    and `5.0` one row; an object's fields in another order no event;
  - a late reader: four commits, each crossing seen;
  - only a commit to its collection runs it again, counted through a
    `Condition::custom`;
  - the three busiest: one pushed out `Left`, one in `Entered`; the
    filter's limit, the mean of the last two;
  - a server gone silent ages out of the window;
  - a panic ends that one alone, and the next commit doesn't run it;
    the commit stands; a panic in the first run;
  - a dropped collection: every row gone, the one row without
    `group_by` changed, the query going on;
  - refused as `aggregate` refuses; a typed collection;
  - iterating ends with the database.
- `back_counts_back_through_seconds_and_stops_at_the_earliest`
  (`datetime.rs`).
- `a_live_aggregate_records_changes_until_it_is_dropped`
  (`database.rs`).
- **The `queries` fuzz target** (§105): one aggregate in three of its
  aggregate step has a window on one of its fields, of 0, 1 ns, 5 s,
  5 s and 1 ns, or past all of time (the pool holds date-times at 0 s,
  1 ns and -5 s), answering as `apply` does on the model, or refused
  where the filter has a sort, a skip or a limit. A live aggregate of
  the program's conditions, made with the live queries, at times with a
  `having` and a window: after every step each event is checked
  against what it reported (`Entered` a row not in it, `Left` and
  `Changed` one in it, `Changed` to other values), and what it reported
  is then exactly the model's result.
  The smoke run's 1,000 programs made 2,132 windows (425 refused) and
  16,387 live aggregate events; 2,000 of seed 7 made 4,188 (790
  refused) and 41,718, all as the model says. It draws more of each
  program, so a seed no longer gives the program it gave.
  Two failures of the target's model, not of trunkdb, were found on
  the way: rows keyed apart where a group's value went from `Int` to
  `Float` (one group, §107.2), and an object's fields in another order
  taken as a change, which trunkdb took as one too until 139.3 said
  otherwise.

## 139.9 Mutations
| Mutation | Caught by |
|---|---|
| The window's edge left out | a window, and through an index |
| The oldest taken for the newest | a window; one newest |
| The window ignored by a read | a window, one newest, through an index, a server gone silent |
| The newest of every document, not the matches | one newest |
| A window beside a limit taken | one newest |
| The window ignored by `apply` | a window; one newest |
| `back` stopping at the epoch | `back` |
| Rows that left looked up in the result only | crossing a `having`, a late reader, the three busiest |
| `Left` never with its row | crossing a `having`, a late reader, the three busiest |
| `Changed` for every row | `Changed` only where…, and three more |
| An object's fields compared in order | `Changed` only where… |
| Keys not canonical | `Changed` only where… |
| Run again on every commit | only a commit to its collection |
| Never run again | every live test but four |
| Changes not recorded for a live aggregate alone | every live test but four |
| A dropped `LiveAggregate` kept | records changes until dropped |
| A panic not ending it | a panic, made stronger: the next commit |
| The first rows not kept | every row first, and five more |
| `Left` without its group | a dropped collection, and three more |

A panic not ending it survived at first: the test read no further than
the error. The next commit now runs, and the ended aggregate reports
nothing more.
