# 210. A group field cut to a unit (`trunc.rs`, `aggregate.rs`, `query_text.rs`)

**Built 2026-11-23: step 1 of §209.** A date-time group field cut down
to the start of its unit, or to its day, read from the documents.
§209 says why and how; this says what was built, and where it differs.

## 210.1 What it is
- **`Trunc`,** in `trunkdb::query`: `Every(Duration)`, `Week`, `Month`,
  `Year`, `#[non_exhaustive]`, and `Trunc::every(duration)`.
- **`Aggregate::group_by_trunc(path, unit)`** and
  **`group_by_date(path)`**, each a group field beside `group_by`'s, in
  the order called.
- **The text:** `trunc(path, unit)` and `date(path)` in `group by`, the
  unit a duration as a window's (`15m`) or `week`, `month`, `year`, in
  any case; `Display` writes them back, a duration in its longest whole
  unit (`trunc(at, 24h)` is `trunc(at, 1d)`). A field named `trunc` or
  `date` is a path where no `(` follows.
- **The cut** (`trunc.rs`), as §209.2 says: counted in whole units from
  1970-01-01T00:00:00Z, rounded down, before 1970 too, on `i128`
  nanoseconds; `Week` from the Monday on or before; `Month` and `Year`
  from their first day; a `Date` cut by a whole number of days, `Week`,
  `Month` or `Year` and left as it is by less; any other value as it is.
  Where the start is before the earliest date-time there is, the earliest
  (`DateTime::EARLIEST`, `Date`'s first day): a cut never panics.

## 210.2 Where it runs
- **`Groups::add_one`** cuts each group field's value before it is
  grouped. Every way an aggregate is run goes through it: a collection,
  a snapshot's view, `Aggregate::apply`, a live aggregate run again in a
  commit.
- **A window per group** (§174) keeps each group's newest by the cut
  values too, so its groups are the rows'.
- **From documents only:** `needs_documents` is true for an aggregate
  with a cut, as for a `sum` or an `unwind`, so §107.5's walk of an
  index's entries is never offered it. A filter's range on the
  date-time is still read through an index (§209.6 is step 2).
- **The row** holds the cut value at the path, never a document's own
  value; a live aggregate knows its row by it.

## 210.3 Refused
- **A zero duration:** in the text when read (`a duration of zero cuts
  nothing`, under its place); through `Trunc::every(Duration::ZERO)`
  when the aggregate runs, as every aggregate's other mistakes are
  (`` `at` cut to a duration of zero ``), in `aggregate`, `apply` and
  `watch_aggregate` alike.
- **A parameter for the unit** (`trunc(at, :unit)`), a unit missing or
  unknown (`trunc(at, day)`), a number without a unit (`trunc(at, 5)`).
- **A path twice,** cut or not, `[*]`, a column named as the group
  field: as for any group field, nothing new.

## 210.4 Tests
- `trunc.rs`: each unit on one instant; before 1970 (a minute, a
  millisecond, a day, a week, a month); the ends of time and
  `Duration::MAX`; a date by whole days, part days, weeks, months,
  years and two days from 1970; a cut on every kind of value.
- `query_text.rs`: `trunc` and `date` read and written back, any case,
  backticked paths, fields named `trunc` and `date`; the mistakes and
  what is said of each; the random aggregates whose text reads back the
  same now cut some group fields, by `date`, `month` or any duration
  in nanoseconds.
- `tests/group_cuts.rs`: per minute, per hour beside a plain field, per
  day as a date and as midnight, values of other kinds, each the same
  through the collection, a snapshot, `apply` and the text read back;
  with an index on the field and one on `(server, at)`, no row from an
  entry; a window per group by minutes; a live aggregate's minute
  `Changed` and a new one `Entered`, its group the minute; the
  refusals in `aggregate`, `apply` and `watch_aggregate`.
- C# and Python: per hour and per day through the text, the rows as a
  record of `DateTime` or `DateOnly` and as `datetime` and `date`.

## 210.5 Where it differs from §209
- **`Trunc::every` doesn't refuse a zero duration itself** (§209.1 said
  it would): a constructor that can fail would be `Option` for every
  caller. The aggregate refuses it when it runs, as it refuses a path
  with `[*]`.
- **`explain_aggregate` shows nothing new:** an aggregate's plan never
  listed its group fields; the rows say what was grouped.

## 210.6 Mutations
Sixteen, each made alone and caught by the tests above: a duration's
cut toward zero rather than down, a date's too; a week from Sunday; a
month cut as a year; a date cut by part of a day; the latest
date-time where the start is before the earliest; dates not cut;
`date` leaving date-times; groups not cut; a window per group not cut;
an index's entries offered an aggregate with a cut; a zero duration
not refused; `week` written as `month`; `date(…)` written without its
name; a zero duration read from the text; the text losing its cuts.

## 210.7 Limits
§209.11's, and step 2 not built: through an index on the date-time, an
aggregate with a cut reads every match's document.
