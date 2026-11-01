# 146. `count_distinct`, `first` and `last` (`aggregate.rs`, `collection.rs`)

§107.8 left three more columns until a workload asked. The time-series
workload (§5.1) does: each van's latest position, kept as one
aggregate, live too (§139), rather than a query per van; and how many
different vans reported in the last hour.

```rust
let latest = Aggregate::new()
    .group_by("van")
    .last("pos").by("at").named("where")   // the position of the newest `at`
    .max("at").named("seen")
    .count_distinct("driver").named("drivers");
```

## 146.1 The columns
- **`count_distinct(path)`:** how many different values the group's
  documents have at `path`, SQL's `count(distinct path)`. Different as
  a filter's `=` tells them apart (§124): `5` and `5.0` one value,
  arrays and objects by their contents, fields in any order, a string's
  case counted. `null` and missing aren't counted, as SQL's. A group
  without values is `0`.
- **`first(path)`, `last(path)`:** the value at `path` of the group's
  first or last document. By id, the order documents were made in,
  which is also the order a row's group values come from (§107.2).
  Where that document has no value there, the column is `null`: the
  document is the group's end, whatever it holds. A `filter`
  (§126), `exists("pos")`, takes the last document that has one.
- **`by(path)`,** on `first` and `last` only: the documents ordered by
  the value at `path`, in the order a sort uses (§34.1), and by id
  where that is equal. A missing value orders as `null`, first: with
  `last` it never wins over a document that has one. On any other
  column it is refused as the aggregate runs, `InvalidInput`, as
  every other mistake in an aggregate is (§107.2).

Each is added and named as the others are (§134), and takes a
`filter` before its name.

## 146.2 `by`, not id alone
A tracker that was offline sends its pings late, in a burst: the
newest document by id holds an old position. §138.7 left the order of
time to the application. `by("at")` puts that order in the column, so
the latest position is that of the latest time, whatever order the
pings arrived in. By id remains the default: what an aggregate shows
of a group is its first document's by id already, and for documents
written as they happen it is the order of time.

## 146.3 From documents and from entries
- **`first` and `last` read their documents,** as `sum` does: the
  §107.5 path from index entries is left. It hands out a value with
  its document's place in id order, but not a second field's value for
  `by`; a `first` by id from entries would be one case more for a
  column whose groups are mostly read whole anyway.
- **`count_distinct` is served by the entries** where an index holds
  its path, as `min` and `max` are: an entry hands out a value only
  where its key holds it whole, and a match whose key cut a long
  string is read from its document (§107.5). Two strings that differ
  only past the cut count twice. A number in a key is its value alone,
  `5` for `5.0`, which `count_distinct` counts as one anyway.

## 146.4 Memory
`count_distinct` keeps each different value's canonical encoding, per
group: memory for every different value, as SQL's hash or sort of
them does. `first` and `last` keep one value each.

## 146.5 Rejected
- **`first` and `last` by the filter's sort,** as MongoDB's `$first`
  follows the `$sort` before it: a filter's sort, skip and limit pick
  the documents (§107.2), and their order isn't otherwise one the
  aggregate sees. A column's own `by` says it where it is used.
- **`last` passing over a missing value by itself:** SQL's
  `last_value` doesn't, nor MongoDB's `$last`; a `filter` does it, in
  the way every column passes over documents.
- **`arg_max`-style columns** (ClickHouse's `argMax(pos, at)`): the
  same as `last("pos").by("at")`, under a name fewer know.
- **`count_distinct` from documents only:** at first, on the reasoning
  that keys cut long strings. A mutation (146.7) showed the entries
  path already reads such a match's document.

## 146.6 Tests
- `aggregate.rs`:
  - `count_distinct`: `1` and `1.0`, `[1]` and `[1.0]`, an object's
    fields in either order, each one value; `null` and missing not
    counted; a string's case counted; with a `filter`; `0` without
    matches;
  - `first` and `last` by id, a missing value `null`, a `filter`
    passing it over; `null` without matches;
  - `by`: a time earlier than the newest written last; a missing value
    first; equal values by id;
  - refused: `by` on `count` and on `max`, a `by` path with `[*]`.
- `collection.rs`, `distinct_counts_and_ends_read_their_documents`: an
  index holding every path; `count_distinct` of long strings that
  differ past the cut, `last` by id and by a field only `by` reads,
  as `apply` gives them; `first` and `last` alone read their
  documents, `count_distinct` of short values is from entries.
- `aggregates_from_entries_agree_with_the_documents` takes
  `count_distinct` among its columns: NaN, `-0.0`, `1 << 60`, long
  strings, arrays and booleans, from entries and from documents alike.
- **The `queries` fuzz target** (§105) makes one column in ten one of
  the three, `first` and `last` by a field one time in two; every
  aggregate is held against `apply` on the model, through each plan,
  live (§139) and windowed. More would cost the entries path: with one
  in five, the aggregates from entries fell by nearly half; with one in
  ten, from 686 to 601 in 1,000 programs. The smoke run's 1,000
  programs made 1,376 such columns, 2,000 of seed 7 2,750, every one
  as the model says.
  It draws more of each program, so a seed no longer gives the program
  it gave.

## 146.7 Mutations
| Mutation | Caught by |
|---|---|
| `first` from entries | reads their documents |
| `last` from entries | reads their documents |
| `count_distinct` only from documents | reads their documents |
| `count_distinct` counting `null` | counts values a filter tells apart |
| Values not canonical, `1` and `1.0` two | counts values a filter tells apart |
| `first` taking the last | the ends by id, by a field |
| `by` ignored | by a field, reads their documents |
| Ties not by id | the ends by id, by a field, reads their documents |
| The column's condition ignored | the ends by id |
| `first` and `last` never kept | the ends by id, by a field, reads their documents |
| `by` allowed on any column | refused |
| `by` not checked for `[*]` | refused |
| `by` not decoded from the document | reads their documents |
| A missing value kept from an earlier document | the ends by id |
| A missing `by` ordered last | by a field |

Three survived at first. `count_distinct` taken off the entries path
went unseen, and wasn't wrong: the entries were exact after all, so
it went back on (146.3). `first` and `last` put on it went unseen while
one aggregate held both, and while `first` read long strings, which
the entries path reads from documents anyway: each alone, of short
values, now.
