# 212. Groups by a cut, from index entries (`collection/count.rs`, `trunc.rs`, `aggregate.rs`)

**Built 2026-11-26: step 2 of §209**, as §211's measurement proposed.
An aggregate with a `trunc` or `date` group field is answered from an
index's entries where one holds every path it reads, as §107.5 answers
any other: no document read, a bucket counted a leaf at a time. The
rows are the same; nothing in the API, the text, the file format or the
ABI changed.

## 212.1 What changed
- **Offered the entries.** `Aggregate::needs_documents` no longer
  counts a cut: §107.5's walk of an index's entries takes an aggregate
  with one, with no filter on an index that holds every document, or a
  range the filter's plan reads.
- **An entry's group from its key:** each group value decoded from the
  key and cut, as `Groups::add_one` cuts a document's. Date-times and
  dates are exact in a key, so no document is read for a group value;
  a value of another kind is what it was, as a cut leaves it.
- **The last group by its cut** (§209.6): what tells whether an entry
  is in the last one's group is its key's bytes up to the last group
  field, as before, with a cut field's bytes replaced by its cut value
  encoded as a key encodes it (`group_bytes`). A field no group field
  takes as it is keeps its bytes.
- **A bucket told by its bytes.** Where the cut field is the last group
  field and the only one at its index field, the common case, `group by
  trunc(at, 1m)` or `group by server, trunc(at, 1h)`, the walk works
  out once per bucket where it ends in the key's order (`Cut::end`,
  encoded), and holds an entry in the bucket while its bytes before the
  cut field are the bucket's and the cut field's are below that end: one
  comparison, nothing decoded. Keys come in order, so an entry at or
  after one of the bucket and below its end is in it; a later kind is
  above any date-time's end, as a key's kind comes first (§34.1).
- **A bucket's end** (`Cut::end`): the first value of the same kind past
  the bucket, the start of the next minute, day, week, month or year,
  by the unit's own arithmetic; `None` past the latest time, and for a
  kind a cut leaves. It is checked as it is made: the value just before
  it must cut as the bucket's does, so, a cut keeping order, every value
  between does. An end wrong by a bug would only send entries the
  longer way.
- **Whole leaves** (§209.6): a leaf is counted whole, where every column
  is a `count`, if its ends have the same group bytes and, where they
  differ, the first value that differs is the last group field's, a cut
  field's, of one kind at both ends. Then every key between is of that
  kind (a key's kind comes first) and between the two, so it cuts to the
  same bucket. A cut field before another group field, `trunc(at, 1h),
  server` on `(at, server)`, leaves its leaves walked: the ends' hour
  says nothing of the servers between them.
- **A row's value read from a document** where its entry didn't hold
  it as stored (a number) is cut as the match's was.

## 212.2 Measured
`cargo run --release --bin trunc` (§211.1), the same machine and data,
2026-11-26, medians of three runs; the trunkdb column is now step 2.

| aggregate | rows | step 1 (§211) | step 2 | a count per bucket | one count of the range | SQLite |
|---|---:|---:|---:|---:|---:|---:|
| per minute, the last hour | 61 | 2.04 ms | 491 µs | 440 µs | 6.7 µs | 352 µs |
| per minute, the last day | 1,441 | 60.9 ms | 12.0 ms | 10.3 ms | 92.3 µs | 12.9 ms |
| per hour, all, `at >= start` | 278 | 650 ms | 14.1 ms | 4.57 ms | 1.56 ms | 151 ms |
| per hour, all, no filter | 278 | 744 ms | 16.4 ms | 4.85 ms | 1.64 ms | 153 ms |
| per day, all, `at >= start` | 12 | 592 ms | 4.92 ms | 1.74 ms | 1.46 ms | 140 ms |
| per day, all, no filter | 12 | 663 ms | 7.34 ms | 1.58 ms | 1.40 ms | 141 ms |

- **4 to 120 times step 1:** per minute over a day, the live case of
  §211.3, 12 ms where it was 61, level with SQLite; per hour and per
  day over a million, 5 to 16 ms where they were 0.6 to 0.7 s, 10 to
  30 times faster than SQLite.
- **The first version decoded and cut every entry,** and encoded its
  cut to compare: 20 ms per minute over a day, 22 ms per hour over
  everything. Telling a bucket by its bytes took them to 12 and 14.
- **What is left is the entries walked one by one.** A leaf holds 276
  entries, four and a half minutes: per minute every leaf is walked,
  per hour one in thirteen, the one an hour starts in, and those are
  77,000 of the million. An entry costs about 0.14 µs on that path, most
  of it what every aggregate from entries pays: its key split into a
  list of values, its column values made into another, each a small
  allocation. The count per bucket shows the floor a seek per bucket
  gives; reaching it means walking a leaf part by part, as §172 does for
  a count, which is left for a measurement that asks.
- **Without a filter,** an aggregate first counts the index's entries
  to know it holds every document (§107.5, rule 4): about 2.4 ms, the
  difference between the rows with and without one. A filter on the field skips
  it.

## 212.3 Rejected
- **Counting bucket by bucket with a seek each:** see §211.5.
- **Comparing each entry's cut value, not bytes:** what the first
  version did, through its encoding; the bytes against the bucket's end
  need nothing decoded.
- **A fast way for several cut fields, or one before another group
  field:** they take the general way, every entry's group bytes; the
  case is rare, and still from entries.

## 212.4 Tests
- `collection/tests/cuts.rs`, each answer held against
  `Aggregate::apply` over the documents the filter finds: per hour over
  20,000 readings from entries, no document read, more than 40 leaves
  counted whole; per minute from a time on; per day as a date from a
  range, leaves counted whole; `min` and `max` of the cut field beside
  it, as stored, not cut; a cut field before a plain one on `(at,
  server)`, no leaf counted whole, and after one on `(server, at)`, its
  leaves counted; a leaf whose ends are a date-time and a date of the
  same day with binary between, `group by date(at)`, four rows; random
  readings of every kind at the field on four indexes, random cuts,
  group fields, columns and filters, twelve databases.
- `trunc.rs`: a bucket's end, for ten units and cuts on date-times and
  dates, before 1970, at a month's and a year's end, on 29 February:
  the end of the same kind, cut differently from the value, the value
  before it cut the same (the check in `end`); `None` for other kinds
  and at the latest time.
- `tests/group_cuts.rs`: with an index on the field and one on
  `(server, at)`, the rows now from entries, no document read.
- **The `queries` fuzz target** cuts one group field in four, by a
  second, a minute, a month, or to its day, which puts the pool's
  date-times of 1970-01-01 in one group with its date. 2,000 random
  programs passed on the code as built, 2,785 group fields cut among
  them, and 5,300 on the first version (212.2); the summary counts the
  cut group fields.

## 212.5 Mutations
Fourteen made; twelve caught: a cut field not the last allowed a whole
leaf; ends of different kinds allowed one; the first difference
ignored; group bytes not cut; a whole leaf's group not cut; an entry's
group not cut; cuts sent to documents again; a bucket that ignores the
values before the cut field; a bucket's end included in it; an end a
unit late; a week's end a day late; December's end in February.

Two not caught, both making no difference to an answer:
- **A bucket's end not checked:** the check guards against a wrong
  end, and the ends are right, which the unit test holds unit by unit.
- **A bucket kept past an entry read from its document:** the bucket
  holds only while the last group came from a key, which that entry
  ends. The line was taken out.

## 212.6 Limits
- **A `sum`, `first`, `last`, an `unwind` or a window per group** still
  read documents, cut or not (§107.5, §146, §147, §174).
- **Per minute, the leaves are walked:** at a reading a second a minute
  is less than a leaf. At a reading a millisecond apart, a minute is
  217 leaves, nearly all counted whole.
- §209.11's limits stand: UTC only, a zone left for when it is asked.
