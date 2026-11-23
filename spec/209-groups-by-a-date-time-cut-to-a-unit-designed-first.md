# 209. Groups by a date-time cut to a unit, designed first (`aggregate.rs`, `query_text.rs`, `collection/count.rs`, `datetime.rs`)

**Designed 2026-11-23.** Its four questions were answered the same
day, each as proposed (209.10). Step 1 was built the same day as
§210; where it changed what this says, §210.5 lists it. Step 2 is
open.

A date-time is stored to the nanosecond (§69), so `group by created`
makes a group of almost every document: readings a few milliseconds
apart are different values. An application that wants a count per
minute, per hour or per day has two ways today, and both are
workarounds:

- **A second field, written beside the first:** `day` set from
  `created` on every insert, and once with `update_many` for what is
  stored. The application keeps the two in step, and the unit is
  fixed when the data is written: per hour needs a third field.
- **The groups made by the application:** `find` the matches, cut each
  date-time in the program, then `Aggregate::apply`. Every match
  crosses into the program, and no index counts for it.

SQL has `date_trunc('minute', created)` and `date_bin`, MongoDB
`$dateTrunc`. This section gives an aggregate the same: a group field
that is a date-time cut down to the start of its minute, hour, day,
week, month or year, or to its date.

## 209.1 What it looks like
In the text of an aggregate (§157):

```text
count() as n group by trunc(created, 1m)              per minute
count() as n, avg(load) as mean group by server, trunc(at, 15m)
count() as n group by trunc(created, month)           per month
count() as n group by date(created)                   per day, as a date
```

In Rust:

```rust
use std::time::Duration;
use trunkdb::query::{Aggregate, Trunc};

Aggregate::new()
    .group_by("server")
    .group_by_trunc("at", Trunc::every(Duration::from_secs(15 * 60)))
    .count().named("n");

Aggregate::new().group_by_trunc("created", Trunc::Month).count().named("n");
Aggregate::new().group_by_date("created").count().named("n");
```

- **`trunc(path, unit)`:** the unit is a duration, written as a
  window's (`500ms`, `1s`, `15m`, `1h`, `1d`, §139), or `week`, `month`
  or `year`. `Trunc::every(Duration)` refuses a zero duration.
- **`date(path)`:** the date-time's day, as a `Date` (§173). Where
  `trunc(created, 1d)` gives midnight as a `DateTime`, `date(created)`
  gives the `Date`, which reads better in a row and compares with
  dates.
- **A group field like any other:** several in one aggregate, plain and
  cut ones mixed, in the order given; `having`, the row sort, live
  aggregates, windows, `unwind` and `explain_aggregate` take them as
  they take a plain one.
- **Aggregates cross the C ABI as text** (§194), so C#, Python and
  `trunkdb aggregate` have it with nothing new.

## 209.2 What a value is cut to
- **A duration:** the latest instant at or before the value that is a
  whole number of that duration after 1970-01-01T00:00:00Z. Counted in
  whole durations from the epoch, rounded down: `1969-12-31T23:59:30Z`
  cut to `1m` is `23:59:00`, not `1970-01-01T00:00:00Z`. A duration
  that doesn't divide a day (`7m`) is counted from the epoch all the
  same, so its buckets don't start at midnight.
- **`1d`:** midnight, UTC, of the value's day, since every day of a
  stored date-time has 86,400 seconds (§69.2).
- **`week`:** midnight, UTC, of the Monday on or before the value, ISO
  8601's week.
- **`month`, `year`:** midnight, UTC, of the first day of the value's
  month or year.
- **The result is the kind it came as:** a `DateTime` cut is a
  `DateTime`. A `Date` (§173) is cut by `1d` or longer to a `Date`, the
  first day of its week, month or year; by a duration that isn't a whole
  number of days it is left as it is.
- **Any other value is left as it is:** a number, a string, `null` or a
  missing field (which groups with `null`, §107) groups by its own
  value, so a document stored with a wrong type shows as a group of its
  own and isn't lost or merged into a bucket (209.10, question 3).
- **Its row:** the cut value, at the path, as a plain group field puts
  its value (§107.2). `trunc(created, 1h)` gives
  `{"created": 2026-11-23T14:00:00Z, "n": 412}`; `having` and the row
  sort name it `created` (209.10, question 2).

## 209.3 UTC only
A stored date-time is an instant in UTC with no zone (§69.1), so the
cut is in UTC: a day is a UTC day. In Germany that day starts at 01:00
in winter and at 02:00 in summer, local time.

- **A fixed offset** (`trunc(created, 1d, +01:00)`) is simple, but
  wrong for half of the year in a zone with summer time.
- **A zone by name** (`Europe/Berlin`) needs the tz database: a
  dependency, or a copy of the data that goes stale as zones change.
  §69.1 left `jiff` and the like for when an application needs them.

This section is UTC only; an offset or a zone is 209.10, question 4.

## 209.4 Refused
- **A path twice:** `group by created, trunc(created, 1h)` is refused as
  `group by a, a` is (§107), since the row would put two values at one
  path. Two cuts of one field (`trunc(at, 1h), trunc(at, 1d)`) too.
- **`[*]` or `_id`:** as for any group path.
- **A unit that doesn't read:** `trunc(created, 0s)`,
  `trunc(created, fortnight)`, under its place with a `^` (§112).
- **`trunc` and `date` outside `group by`:** a column's path, a filter
  and `order by` take paths only. A filter has ranges for it
  (`created >= datetime("2026-11-01")`).

## 209.5 From documents (step 1)
`Groups::add_one` reads each group field's value as now, then cuts it.
Nothing else in the grouping changes:

- **Groups are kept by their cut values,** in the order a sort uses
  (§34.1), so rows come in time order.
- **A row's value is its bucket's start,** never the first document's
  own value: §107's "a row shows its first document's values, by id" is
  about plain fields.
- **Live aggregates** (§139) know a row by its group values, here the
  bucket's start: a new minute's first reading is a row `Entered`, a
  minute leaving a window a row `Left`.
- **`Aggregate::apply`** cuts the same way.
- **`Display` writes it back** as it reads: `trunc(created, 15m)`,
  `trunc(created, month)`, `date(created)`; a duration in its longest
  whole unit, as a window's is.

## 209.6 From index entries (step 2)
Cutting keeps order: a later date-time is never in an earlier bucket.
So an index on `created`, or one that starts with the group fields,
still gives each group's entries one after the other, and §107.5 can
answer from entries:

- **The group of an entry:** its date-time decoded from the key, then
  cut. Date-times and dates are exact in a key (§106), so no document is
  read for a group value.
- **The last group:** where §107.5 compares an entry's key bytes up to
  its group values with the last entry's, a cut field is compared by
  its cut value instead.
- **A whole leaf counted at once** (§107.5): where the cut field is the
  last of the group fields in the index's order, the fields before it
  agree byte for byte at the leaf's two ends, and both ends cut to the
  same bucket. Every key between is then a date-time between the two
  (a key's type comes first in its order, §34.1), so its cut is the same
  bucket. Per minute over readings every second, about sixty entries
  a group, most leaves are one minute's. Where a cut field comes before
  another group field, (`trunc(at, 1h), status` on `(at, status)`), a
  leaf's ends don't decide what lies between; such leaves are walked
  entry by entry.
- **A measurement** on a million readings, a count per minute and per
  day through an index on the date-time, against the same from the
  documents, decides whether step 2 is worth its code (209.9).

## 209.7 What it doesn't change
- **The file format:** an aggregate isn't stored.
- **Aggregates without a cut:** read, planned and run as before.
- **The ABI version:** aggregates are text across it.
- **Filters:** nothing new; a range on the date-time already picks a
  day or a month.

## 209.8 Rejected
- **Expressions in `group by`,** SQL's `group by extract(…) * 2 + …`:
  a language of expressions for one need. The cut is one function over
  one field, and keeps order, which an index can use (209.6); an
  expression in general doesn't.
- **A closure as a group key,** as `sort_by_key` has one (§154): it
  would reach every need, but never through an index, and has no text,
  so C#, Python and the command couldn't use it. Deferred until a
  grouping that isn't a cut is asked for.
- **A field trunkdb keeps beside the first,** a generated column
  (`day` computed from `created` on every write): it needs a format
  change to store how it's computed, and its unit is fixed when it's
  declared. A cut at read time, through an index on the field itself,
  does both units with one index.
- **Rounding to the nearest unit** rather than down: a bucket would
  then start half a unit before its name, which no one reading
  "14:00" expects.

## 209.9 Steps, a section each
| Step | What | Seen from outside |
|---|---|---|
| 1 | `trunc` and `date` in `group by`: `Trunc`, `group_by_trunc`, `group_by_date`, the text and `Display`, the cut (209.2) on documents, `apply`, live aggregates, `explain_aggregate`; the refusals (209.4); the GUIDE, the agent skill, a C# and a Python test through the text | per minute, per day in every binding and the command |
| 2 | From index entries: the cut on a decoded key, the last group by its cut value, whole leaves counted (209.6); measured | the same rows, faster |

## 209.10 The questions, decided
Each was decided on 2026-11-23 as proposed.

1. **The names:** `trunc(path, unit)` and `date(path)` in the text,
   `Trunc`, `group_by_trunc` and `group_by_date` in Rust (proposed), as
   SQL's `date_trunc`? Or `bucket(path, 15m)`, closer to what it makes
   but further from SQL?
2. **The row's field:** the path, `created` holding the bucket's start
   (proposed)? Or a name of the application's, `trunc(created, 1h) as
   hour`, which would let `created` and its hour be grouped together
   (209.4) at the cost of a name to give each time?
3. **A value that isn't a date-time:** grouped by itself, so it shows
   (proposed)? Or grouped with `null`, as a missing field is?
4. **The time zone:** UTC only for now (proposed), an offset or a zone
   by name when an application needs a local day? Or a fixed offset in
   step 1, knowing it is wrong across summer time?

## 209.11 Limits
- **UTC days** (209.3).
- **Durations counted from 1970,** not from an origin of the
  application's, as Postgres' `date_bin` takes one: 15-minute buckets
  start at :00, :15, :30, :45; a 7-minute one doesn't start at
  midnight.
- **Cuts only:** an hour of the day across all days ("which hour is
  busiest") or a weekday is an extract, not a cut; deferred until asked.
- **Date-times and dates only:** numbers into buckets of ten, a
  histogram, are a cut too, deferred until asked; 209.2's rule for
  other values would leave room for it.
