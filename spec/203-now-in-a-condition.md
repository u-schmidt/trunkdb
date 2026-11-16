# 203. `now` in a condition (`query.rs`, `query_text.rs`, `collection/`, `aggregate.rs`, `explain.rs`, `datetime.rs`)

Step 1 of §202: a comparison's value may be the time a read takes,
moved by a duration. Reads, writes, batches, aggregates and the
command take it; live queries refuse it until step 2 reads them again
as time passes.

```rust
let silent = devices.find(Filter::new().older_than("last_seen", Duration::from_secs(600)))?;
```

```
$ trunkdb find app.trunkdb devices 'last_seen < now - 10m'
```

## 203.1 `Condition::AgainstNow` and `Offset`
- **`Condition::AgainstNow { field, op, offset }`,** a variant of its
  own, built by `Condition::against_now(field, op, offset)`, and the
  two common cases: `older_than(field, age)`, `field < now - age`, and
  `newer_than(field, age)`, `field >= now - age`. A date-time is in
  exactly one of the two; the instant exactly `age` ago is newer. As
  `Filter` methods too.
- **`Offset`:** `Offset::NONE`, `Offset::before(d)` and
  `Offset::after(d)`; `after` of zero is `NONE`, so `now + 0s` and
  `now` are one condition. `from(now)` is the time it stands for,
  stopping at the earliest and the latest date-time there is
  (`DateTime::back`, and a new `forward`).
- **A date-time only:** resolved, it is a comparison with a
  `DateTime`, which compares with date-times alone (§69). A missing
  field, or a number of seconds, is neither older nor newer.

## 203.2 Read once per read
- **`Filter::resolved`,** at the start of each read: where a condition
  compares with the clock, the time is read once from `SystemTime`, and
  every such condition becomes a comparison with a `DateTime`; the
  filter is borrowed unchanged where none does, so other filters pay a
  walk over their conditions and nothing else. From there the planner,
  the index ranges, the entries that decide a filter (§104) and the
  matchers see a date-time.
- **Where:** `find`, `find_one`, `count`, `cursor`, `explain`,
  `explain_run`; `aggregate` and `explain_aggregate`, with one time for
  the filter, the columns' conditions and `having`; `update_fields`,
  `delete_many`, `upsert`, `upsert_fields`; in a batch, each op that
  reads (`require`, `require_none`, `update_many`, `delete_many`,
  `upsert_fields`) when the batch runs it, under the writer's lock.
  Snapshots and views read through these.
- **In memory,** `Filter::matches`, `Filter::apply`,
  `Condition::matches` and `Aggregate::apply` resolve first too, with
  one time for the whole call. `Aggregate::apply` makes its columns'
  matchers before it reads the documents and `having`'s after: with a
  time read for each, a slow iterator saw the clock move between them,
  and a row could count a date-time as not yet past in a column and as
  past in `having` (found in review). A matcher made from a condition
  no one resolved reads the clock itself, a last resort nothing in the
  crate reaches.
- **`Filter::now_is(time)`,** public: the filter as it reads at
  `time`. A program that wants an answer it can repeat says the time
  itself; the tests do.
- **`Explained::now`:** the time the run compared with, `None` where
  no clock was read; `--explain` prints it as a line after the plan,
  `now: 2026-11-16T12:00:00.123456789Z`.

## 203.3 In the text
- **After a comparison:** `now`, `now - <duration>` or
  `now + <duration>`, the duration a number and a unit, as a window's
  (§157): `d`, `h`, `m`, `s`, `ms`, `us`, `ns`. Printed back in the
  longest whole unit: `now - 300s` prints as `now - 5m`.
- **Not reserved:** a field named `now` stays a field; `now` means the
  time only where a value stands.
- **No parameter for the duration:** `now - :age` is refused as "a
  duration after `now`". A program that needs one builds the condition
  in code.

## 203.4 Refused
- **`=`, `!=`, `contains`, `starts with` against `now`:** at the text,
  where it is read; in code, by the read, with an `InvalidInput` error,
  as `Condition::against_now` can't fail. A time held for a nanosecond
  is never what is meant.
- **A live query or aggregate against `now`,** anywhere in its filter,
  a column's condition or `having`: `Error::NotLive` with the reason "a
  comparison with `now`, not yet". Checked only at commits, its result
  would go stale between them, which is what the step was asked for
  not to do. Step 2 lifts it.

## 203.5 Changed from §202
- **The refusal in code is at the read,** not "when the filter is
  made" as §202.5 said: constructors return a `Condition`, not a
  `Result`.
- **`contains` and `starts with`** are refused too; §202 named `=`
  and `!=`.
- **`Filter::now_is` is public,** where §202.2 had the tests set the
  clock: it is the same thing, and a program can use it.
- **Live queries are refused** until step 2, which §202.9 left unsaid.

## 203.6 Rejected
- **Resolving inside `Scope`,** each read method taking the time: a
  cursor and `explain_run` call several, and would compare with
  several times.
- **A `Document` variant for "now":** values are stored; this one
  never is, and a new variant is a new file format.

## 203.7 Tests
- `tests/clock.rs`:
  - `older_than` and `newer_than` split the date-times; a missing time
    and a number in neither; `now` alone, and an hour after it;
  - every read takes it, the same answer through an index, which is
    read over the range alone (2 keys examined); `explain_run`'s `now`
    between the times read before and after, its line printed;
    `explain`'s plan an index's; a cursor, `find_one` sorted, a
    snapshot's view;
  - writes and batches: `require`, `require_none`, a batch's
    `delete_many`; a precondition that fails once none is silent;
    `update_fields`, `delete_many`;
  - aggregates: a column's condition, `having` on a `max` column, the
    filter, `explain_aggregate`'s `now`;
  - `now_is`: two hours ago none silent, in a day all four;
  - in memory: `matches`, `apply`, `Aggregate::apply`; inside `has`;
    on a path with `[*]`; one time for a slow `Aggregate::apply`, a
    document a second ahead (50 ms until CI stalled past it) counted
    by a column and refused by `having`;
  - the text read and printed, `now - 300s` as `now - 5m`, `now + 0s`
    as `now`, a field named `now`;
  - refused: `=`, `!=`, `contains`, a number without a unit, no
    duration, a parameter; in code, by `find`, `count`, `explain_run`,
    `delete_many` and `aggregate`;
  - live queries and aggregates refused, by the filter and by a
    column's condition; nested in `has`, a NOT and an OR too;
  - the edge, to the nanosecond, with `now_is`;
  - `Offset`.
- `forward_counts_forward_and_stops_at_the_latest` (`datetime.rs`).
- `find_takes_now` (`tests/cli.rs`): `find`, `count`, `--explain`'s
  `now:` line, `= now` refused.
- **The `queries` fuzz target** (§105): one leaf in fourteen compares a
  path with the clock, `<`, `<=`, `>` or `>=`, its edge 2.5 s before
  the epoch (between the pool's -5 s and 0), 1 s after it, a year
  before it, `now` or an hour after now. Each edge is at least 2.5 s
  from the nearest value, so the clock moving during a program changes
  no answer; the database and the model read it each for itself. A live
  query or aggregate against it is refused, and counted. 1,000 programs
  of seed 7 made 3,197 such comparisons and 697 live refusals, all as
  the model says.

## 203.8 Mutations
| Mutation | Caught by |
|---|---|
| No filter resolved before a read | every read…, what it refuses |
| `before` and `after` swapped | `Offset`, in memory, the edge |
| `older_than` as `<=` | the edge |
| `=` against `now` taken in code | what it refuses |
| `=` against `now` taken in the text | what it refuses |
| `+` and `-` printed swapped | the text read and printed |
| `now` inside `has` not seen | nested comparisons |
| A column's `now` not seen | live queries refused |
| The fallback matcher without the offset | in memory |
| No `now:` line from `--explain` | `find_takes_now` |
| `explain_run` without its `now` | every read…, nested comparisons |
| `forward` and `back` stopping at the wrong end | `forward…`, `back…` |
| A batch's precondition not resolved | what it refuses |
| The live refusal dropped | live queries refused, nested comparisons |
| `Aggregate::apply` with a time per matcher | a slow check in memory |

One survives, and can't be caught: a column's conditions left
unresolved. The matcher then reads the clock as it is made, in the
same read, and the answer is the same.
