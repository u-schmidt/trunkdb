# 211. Groups by a cut, measured (`bench/`)

§209.9 put a measurement before step 2: whether answering `trunc` and
`date` group fields from an index's entries (§209.6) is worth its code.
This is that measurement. Nothing in the library changed; the bench has
a new binary, `trunc`.

## 211.1 How
Apple M1 Pro (macOS), Rust 1.98.1, 2026-11-26, `main` after §210.
`cargo run --release --bin trunc`: a million readings, one a second
from 2026-01-01T00:00:00Z, about eleven and a half days; each a
date-time `at`, a device's name, two coordinates and a speed; an
index on `at`, nothing else. SQLite beside it, `at` as
an integer of seconds with an index, `GROUP BY at / 60`. Each case is
checked to give the same number of groups three ways before it is
timed. Three runs; the medians below, the runs within 5% of each other.

For each aggregate, `count() group by trunc(at, unit)`:
- **Today (step 1):** `Collection::aggregate`, as §210 runs it.
- **A count per bucket:** one `count` per bucket, `at` from its start
  to the next, each a range of the index counted from its entries, a
  leaf at a time where a leaf lies inside the range (§101, §104). Today's
  way to get the same rows from entries, a seek per bucket.
- **One count of the range:** a single `count` over the aggregate's
  whole range, no groups: what reading the entries costs at the least.

Step 2 makes one walk where a count per bucket makes one seek per
bucket, and groups as it goes where a count of the range doesn't, so
it should land between the two columns.

## 211.2 Results
A leaf of the index holds about 275 entries (100,000 readings: 363
pages, depth 3).

| aggregate | rows | today (step 1) | how | a count per bucket | one count of the range | SQLite |
|---|---:|---:|---|---:|---:|---:|
| per minute, the last hour | 61 | 2.04 ms | built from entries | 430 µs | 6.5 µs | 343 µs |
| per minute, the last day | 1,441 | 60.9 ms | built from entries | 9.83 ms | 85.9 µs | 12.7 ms |
| per hour, all, `at >= start` | 278 | 650 ms | built from entries | 4.42 ms | 1.45 ms | 146 ms |
| per hour, all, no filter | 278 | 744 ms | every document read | 4.40 ms | 1.35 ms | 146 ms |
| per day, all, `at >= start` | 12 | 592 ms | built from entries | 1.67 ms | 1.47 ms | 146 ms |
| per day, all, no filter | 12 | 663 ms | every document read | 1.56 ms | 1.44 ms | 146 ms |

## 211.3 What it shows
- **Step 1 doesn't read documents where the filter bounds `at`.** §210.2
  sends an aggregate with a cut to `find`, not to §107.5's walk of
  entries; but `find` with the aggregate's selection, `at` alone, an
  exact key, builds each match from its index entry and never reads
  the document (§106.5). Only an aggregate with no filter on `at`
  scans the documents. §210.7's limit, "reads every match's document",
  and CHANGELOG item 180's "read from documents" are true only of that
  case.
- **Documents aren't the cost.** Built from entries, a match costs about
  0.65 µs; read from its document, 0.74 µs. The time goes to the work
  per match, a document built for each and its group found by its
  value, not to reading pages.
- **Counts from entries are 6 to 400 times faster.** Per minute over
  the last day, 9.8 ms against 61 ms; per day over everything, 1.6 ms
  against 600 ms. Where step 1 is 4 to 6 times SQLite's time, counts
  from entries are level with SQLite per minute and 90 times faster per
  day.
- **A minute isn't a leaf.** §209.6 expected most leaves to hold one
  minute's entries; a leaf holds 275, four and a half minutes at one a
  second. Per minute, a leaf's ends nearly always cut to different
  buckets, so step 2 would walk its entries one by one, decoding and
  cutting each key: cheaper than step 1's match per entry, but no
  whole leaves. Per hour, about 12 of every 13 leaves lie inside one
  hour, and per day nearly all: there whole leaves do the work, as the
  count of the range shows, 1.4 ms for a million.
- **A live aggregate pays it at every commit** (§139): a count per
  minute over the last day, live beside §5.1's time-series workload,
  costs 61 ms each time a reading comes, a second apart.

## 211.4 Proposed
**Step 2 is worth building**, for the counts above and live aggregates
most: per hour and per day from whole leaves, near the count of the
range; per minute from a walk of keys, near or below the count per
bucket. It answers `count`, `min`, `max` and `count_distinct`, as
§107.5 does; `sum` and `avg` of another field still read documents.
Its section measures it with this binary, against these columns.

Seen on the way, not part of step 2: 0.65 µs a match is what every
aggregate with many matches pays where its columns aren't counted from
whole leaves, `group by device` too. Where it goes is for a profile,
when an aggregate asks.

## 211.5 Rejected
- **Counting bucket by bucket inside the library,** a seek per bucket
  as the middle column does: it needs each bucket's edges ahead, which
  a group field before the cut, or one per device, makes many; one walk
  finds them as it goes.
- **Readings a millisecond apart,** as §5.1's time-series workload
  stores them: only the entries per bucket change, a minute holding 60,000 instead
  of 60, which favours whole leaves; a second apart is the harder case
  for step 2.
