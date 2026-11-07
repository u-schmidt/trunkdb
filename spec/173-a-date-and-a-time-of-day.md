# 173. A date, and a time of day (`datetime.rs`, `document.rs`, `index/key.rs`, `query.rs`, `query_text.rs`, `json.rs`, `export.rs`, `serde_bridge.rs`, `serde_stored.rs`, `bin/trunkdb.rs`)

A `DateTime` is a moment (§69, §70). A birthday, a due date or the day
a report covers isn't one: it is a day in the calendar, the same day
wherever it is read. Opening hours and a daily alarm are times of day,
on no day in particular. Until now an application stored noon UTC of
the day (§69.1), or text, and a filter couldn't tell either from a
moment. Now `Document` has `Date` and `Time`, with types of their own.

```rust
use trunkdb::{Date, DateTime, Time};

#[derive(Serialize, Deserialize)]
struct Shift {
    day: Date,
    starts: Time,
    ends: Option<Time>,
}

shifts.ensure_index("day")?;
let week = Filter::new()
    .gte("day", Date::from_ymd(2026, 11, 2).unwrap())
    .lt("day", Date::from_ymd(2026, 11, 9).unwrap());
let today = DateTime::now().date(); // in UTC
```

In a filter's text: `day >= date("2026-11-02") and starts < time("07:00")`.

## 173.1 `Date` and `Time`
- **`Date`:** a day of the proleptic Gregorian calendar, as days since
  1970-01-01 in an `i64`. `Copy`, `Eq`, `Ord` in calendar order,
  `Hash`. Every day a `DateTime` falls on and no more, so
  `DateTime::date()` never fails: 292 billion years either side of
  1970. Made by `from_ymd(year, month, day)` and
  `from_days_since_epoch(days)`, both `None` for what isn't a day;
  `Date::UNIX_EPOCH`; read by `year()` (0 for 1 BC, as ISO 8601),
  `month()`, `day()` and `days_since_epoch()`, which is also how a
  program adds days. Text `2026-11-07`, a year past 9999 or before 0
  with a sign and more digits, as a `DateTime`'s (§69.4).
- **`Time`:** nanoseconds since midnight, below a day. `Time::MIDNIGHT`,
  `from_hms`, `from_hms_nano`, `from_nanos_since_midnight`; `hour()`,
  `minute()`, `second()`, `nanosecond()`, `nanos_since_midnight()`. Text
  `14:05:00`, with the fractional digits the nanoseconds need, up to
  nine, as in a `DateTime`'s. Read also as `14:05`. No zone and no
  leap second: `23:59:60` is refused.
- **Between them:** `DateTime::date()` and `DateTime::time()`, in UTC,
  and `Date::at(time)`, the moment back, `None` only at the two ends of
  `DateTime`'s range, where a day is cut short.
- **Parsing** returns `ParseDateTimeError`, as for a `DateTime`, its
  message saying which was expected: `"soon" is not a date (like
  "2026-09-27")`.
- **Serde:** each a newtype around its text, named so the serde bridge
  stores it as what it is (`$trunkdb::Date`, `$trunkdb::Time`), as
  `DateTime`'s is (§70). Read back, a `Date` or `Time` is its text, so
  a `String` field or a `serde_json::Value` gets `"2026-11-07"`, and a
  `Date` field reads a string that is one. Other formats see the text.

Named as C#'s `DateOnly` and `TimeOnly` mean, and as `jiff::civil::Date`
and `Time` are named.

## 173.2 Separate kinds
A date compares with dates and a time with times, never with a
date-time, a string or each other, as an id never equals its string
(§59.5) — the answer of the application asked:

- **`due < date("2026-11-07")` on a field of date-times matches
  nothing.** Comparing a date as midnight UTC would make `=` match only
  a moment exactly at midnight, and the day would be UTC's, not the
  reader's. A filter on date-times writes the moment:
  `datetime("2026-11-07T00:00:00Z")`.
- **`Kind::Date` and `Kind::Time`**, `date` and `time` in the text:
  `due is date`. `trunkdb fields` counts them under those names.
- **`update`'s `min` and `max`** take them, as any kind filters order.
  `inc` doesn't.
- **An aggregate's `min`, `max`, `first`, `last`** take them; `sum` and
  `avg` don't, as they don't take date-times.

## 173.3 Stored, keyed, sorted
- **In a document:** tag 15, the days zigzag-encoded as a varint, and
  tag 16, the nanoseconds as a varint (§170's varints), in both
  encodings: a day of this century takes four bytes with its tag, a
  time of day at most eight. A decoder refuses a date past `DateTime`'s days and a
  time of a day or more as damage (§55).
- **In an index key:** tag 7, the days big-endian with the sign bit
  flipped, and tag 8, the nanoseconds big-endian: byte order is their
  order. 9 bytes each, so a compound key's parts split them.
- **In a sort:** after binary, dates before times, before the values
  nothing orders (§34.1). Next to date-times would read better, but
  binary's tag 6 would have to move, and every index holding binary be
  rebuilt (§166) for an order across kinds no one sorts by.
- **File format 15.** A build before it can't decode the tags, so it
  refuses the file. A file of 14 opens as it is, holding neither, and
  is stamped 15 by the first write; no index needs rebuilding, as no
  older file can hold a date.

## 173.4 Text
- **In a filter:** `date("2026-11-07")`, `time("14:05")`, and
  `datetime("2026-11-07T14:05:00Z")` for a moment, which is how
  `Display` writes one now.
- **`date("…")` with a time in it is still a date-time,** as it was
  before `datetime`: a date-time's text always has a time, and a
  date's never, so the text says which, and every filter written so
  far reads as it did. `Kind::DateTime`'s name changes, `date` to
  `datetime`, as `date` now names dates: **`hours is date` finds dates
  now, not date-times** — the one change in what a filter means.
- **In JSON:** `{"$date": "2026-11-07"}`, the text again saying which,
  and `{"$time": "14:05:00"}`. An object that is `{"$time": …}` is
  written inside `$object`, as for any tag.
- **Export format 4.** A build before it reads `$time` as an object, so
  exports say version 4, and that build refuses them. An import reads
  1 to 3 as before: `$time` in them is an object, as it was.
- **`--param`** takes `datetime("…")`, `date("…")` and `time("…")`, as
  the text does.

Rejected:
- **Keeping `date` for date-times, and `dateonly`, `timeonly`** for the
  new kinds: nothing would change meaning, but `date` would go on
  naming a moment, and the names would be longer than SQL's `DATE`,
  `TIME` and `TIMESTAMP` everywhere they're written. Asked, the user
  chose `datetime`, `date` and `time`.
- **A date as midnight UTC, comparing with date-times:** see 173.2.
- **`$dateonly` in JSON:** `$date`'s text already says which; one tag
  fewer for others to know.
- **A date in an `i32` of days**, five million years: smaller, but then
  `DateTime::date()` could fail.

## 173.5 Limits
- **No arithmetic** but `days_since_epoch`: no "add a month", no
  durations between dates. A program has `jiff` or `chrono` for that.
- **No time zones,** as for date-times (§69.1): "today" is UTC's
  unless the program says otherwise.
- **`chrono`'s `NaiveDate` and `NaiveTime`** and `jiff`'s `civil`
  types are stored as strings, as their `DateTime`s are (§69.5); a
  `Date` or `Time` field reads its own type only, or a string.

## 173.6 Tests
- `datetime.rs`: dates written and read in ISO 8601, years 0, -1 and
  10000, every 37th day over eight hundred years, the first and last
  day and none past them, twelve malformed texts; times to the
  nanosecond, `HH:MM`, thirteen malformed texts, out-of-range parts;
  a date-time as its date at its time, at both ends of the range.
- `document.rs`: both round trip in both encodings and through a skip;
  one past its range, and one cut short, are damage.
- `index/key.rs`: keys in their order across the sign and where
  little-endian would reorder, after binary, dates before times, read
  back whole; a compound key split; a key past the range read back as
  nothing.
- `query.rs`: comparisons in order; no equality with a date-time, a
  string, an int or each other; the sort's order.
- `json.rs`: `$date` with and without a time, `$time`, an object that
  looks like `$time`, an export before version 4 keeping it, five bad
  values.
- `serde_bridge.rs`: a struct with `Date`, `Time` and `Option<Time>`
  fields stored as such and read back; read as a `String` and a
  `serde_json::Value`; a string read into a `Date`; another kind
  refused; `serde_json` sees the text.
- `query_text.rs`: the literals and the tags as the same filter, an
  `in` of times, four bad literals, fields named `date` and `time`.
- `tests/dates.rs`: a collection of shifts, a range of days and of
  times read through their indexes in order, kinds by name, `max` in an
  update and refused across kinds, an aggregate's `min` and `max`, the
  same after a reopen and after an export imported, which writes
  version 4; an export of version 3 keeping a `$time` object.
- `tests/kinds.rs`: each kind finds its values, `datetime` among them;
  `tests/cli.rs`: `--param` with each literal, `fields` saying
  `datetime`; format 15 and export version 4 where tests name them.
- The random values every randomized index, filter, update, export
  and stored-read test draws from now include dates and times; the
  fuzz program's pool (§105.3) and the fuzz base (§55.1) too, the base
  with an index on dates. Two randomized tests took new seeds: their
  counts of refusals and failures vary from seed to seed far more than
  between the old values and the new.

## 173.7 Fuzzed and mutated
- **Fuzzed,** each target 15 minutes on the base with dates and times
  (§55): open_file 14,648 runs, open_raw 33,968, queries 3,003, import
  17,019, recover_wal 6,642, filter_text 12.7 million. Nothing found.
- **Mutated,** forty ways by hand, each run against every test: the
  range of dates and of times, each end of it; calendar arithmetic
  (`div_euclid` and `rem_euclid` for days before 1970, `at` adding the
  time); `HH:MM` read for a time and not for a date-time; the texts'
  fractions and signed years; both encodings and the skip; keys
  without the sign flip, little-endian, split wrong, decoded without
  the flip; dates not comparing, equal to their midnight, sorted as
  times; `Kind::Date` taking date-times and `Kind::DateTime` named
  `date`; `$time` known to older exports, the export still version 3;
  `$date` and `date(…)` without a date alone; the serde newtype and a
  typed read missing dates; the format not bumped; `max` refusing
  dates; `--param` and `fields` with dates. Thirty-nine fail a test.
  A year of 9999 printed with a sign was the last to need one: years
  9999 and 10000 are now both written in the test. The one standing
  zigzag-encodes a date with `>> 62` instead of `>> 63`, which gives
  the same bits for every day there is: the days fit in 47 bits.
