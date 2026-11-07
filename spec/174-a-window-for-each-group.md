# 174. A window for each group (`aggregate.rs`, `collection.rs`, `query_text.rs`)

The window of §139 counts back from the newest document of all. Where
groups report at rates far apart, that leaves out the slow ones: a
server reporting once an hour has nothing in the last five minutes of
a server reporting every second. And "the last ten readings of each
server" was not to be said at all: a filter's limit counts over every
group (§139.6). Now each group can have a window of its own, by count
or by time, SQL's `row_number() over (partition by server order by at
desc) <= 10`:

```rust
let recent = Aggregate::new()
    .group_by("server")
    .avg("load").named("mean")
    .newest_per_group("at", 10);            // each server's last ten
let quiet = Aggregate::new()
    .group_by("server")
    .max("load").named("peak")
    .window_per_group("at", Duration::from_secs(300)); // each one's last five minutes
```

In the text, `window at 10 per group` and `window at 5m per group`.

## 174.1 `newest_per_group(path, n)`
Of each group, the `n` documents with the largest values at `path`.

- **Any value a sort orders,** in the order a sort uses (§34.1):
  date-times, numbers, a sequence, text. A window by count has no
  arithmetic to do, so it needn't ask for a date-time.
- **Of equal values, the later id is newer.** Ids are the order
  documents were made in, as `first` and `last` take them (§146); `7`
  and `7.0` are equal, as a filter finds them.
- **`null` and missing are out,** as a window by time leaves out what
  isn't a date-time: a reading without a time isn't among the newest.
- **`n` of 0 keeps nothing,** so there is no group, and without
  `group_by` one row of none, as for no matches. More than a group has
  keeps them all.

## 174.2 `window_per_group(path, duration)`
Of each group, the documents whose `DateTime` at `path` is within
`duration` of that group's newest: §139.2's window, with a newest per
group.

- **Both ends in,** as in §139.2. A document whose value there isn't a
  `DateTime`, or is missing, is out; a group with none has no row.
- **A group gone silent keeps its last documents,** and its row, for
  ever: the reason §139.2 gave for one newest of all. It is what is
  asked for here; a program that wants silent groups to age out uses
  `window`, or both.

## 174.3 Beside the rest of an aggregate
- **One window per group,** as one sort: the later of
  `newest_per_group` and `window_per_group` replaces the earlier.
- **With `window`, the window of all comes first,** as a condition on
  the documents (§139.2), then each group's in what is left. On the
  same path the two commute: each keeps a group's newest documents,
  down to some value, and the shorter of two such runs is what both
  keep.
- **The filter picks first;** each group's window then counts among
  its matches. A filter on `path`, `at >= …`, bounds what is read.
- **Refused beside a filter's sort, skip or limit,** with
  `InvalidInput`, as `window` is: which first would be a rule to
  learn, and `apply`, given what the filter found, could do only one.
- **Groups are the aggregate's:** a document's group values read as
  `group_by` reads them, made canonical (§107.2), so `5` and `5.0` are
  one group and a missing value groups with `null`. Rows show the
  first document's values, by id, of those kept.
- **With `unwind`,** the window keeps documents before they are
  unwound (§147). Refused where a group field or `path` is inside an
  unwound array: a document would be in several groups, or ordered by
  several values. An array unwound that neither reads stays with its
  document, its elements kept or left out together.
- **Columns, `having`, the row sort, skip and limit** see only what the
  windows kept.
- **The path must name a field,** as a group's (§107.2): not empty,
  not with `[*]`, not `_id`.

## 174.4 How it runs
Every match is read, decoded on the paths the aggregate reads and
`path`; then each group's are kept, and only those go to the groups.

- **No index bounds it.** A group's newest can be anywhere in time, so
  reading through an index on `path` newest first could stop only once
  every group had its `n`, and a group with fewer would read to the
  end. Index entries never answer it (§107.5): the documents are read.
- **The matches are held in memory,** each only its read paths, until
  every group's window is known: a group's newest may come last.
- **Measured** on 100,000 readings of 100 servers, `avg` per server:
  121 ms with `newest_per_group("at", 10)` and with
  `window_per_group("at", 5 min)`, 116 ms without either, 0.5 ms with
  `window` of all, which reads through the index. With the filter
  `at >= the last hour`, `newest_per_group` took 4.3 ms.
- **Live** (§139), it runs whole at each commit as any aggregate does.
  A filter bounding `path` keeps that cheap where there are many
  readings.
- **`Aggregate::apply`** keeps the same: a document's `_id` orders it,
  documents without one after every id, in the order given.

## 174.5 Text
- **`window <path> <n> per group`** and **`window <path> <duration>
  per group`**: a number without a unit is a count, as a duration
  always has one. A parameter stands for the count (§163),
  `window at :n per group`, never for a duration.
- **Two `window` clauses,** one of all and one per group, in either
  order; `Display` writes the one of all first.
- **`window at 10` alone is refused,** expecting a unit or `per group`:
  the newest ten of all are the filter's sort and limit (§139.2).
- `per` is a word only there, as the aggregate's others (§157).

## 174.6 Rejected, and limits
- **A window by count of all,** `window at 10`: the filter's
  `sort_desc("at").limit(10)` says it already.
- **The newest of each group as documents, in `find`,** a filter's
  limit per group: asked, the user chose aggregates only. `last(…)
  .by(…)` gives the newest one of each in a row (§146).
- **Ties by a second field:** the id decides between equal values, as
  for `first` and `last`; a second field is one more thing to learn.
- **A window per group from the clock:** as §139.2 for the window of
  all, decided only if asked for.
- **Reading through an index, group by group:** see 174.4. An index on
  `(server, at)` could seek each server's newest; deferred until a
  measurement asks for it.
- A duration of `Duration::MAX` written as text doesn't read back: its
  nanoseconds pass 64 bits. So for `window` since §157; no window
  that long is needed, as `DateTime::back` stops at the earliest time.

## 174.7 Tests
- `tests/group_windows.rs`, each case run four ways — the database, a
  snapshot, `apply` over what the filter finds, and the aggregate's
  text read back: by count, the filter first, 0, more than a group
  has, no `group_by`; by time, with both ends, beside `window`, the
  one replacing the other, a nanosecond narrower; values of any kind
  in order, equal ones by id, `null` and missing out, no date-time for
  a window by time; groups `5` and `5.0` one, `[5]` and `[5.0]` one,
  missing with `null`; an index holding every field, and a count
  alone, still keeping the window;
  random readings of four servers, many at the same second, with and
  without an index, against a model sorting each server's by hand,
  for five counts and five durations; refusals beside a sort, skip and
  limit, for wrong paths, and inside an unwound array, and an unwind
  allowed beside it; the text read back, its mistakes, a parameter for
  the count; live, a silent server keeping its row where §139's
  window would drop it, and by count.
- `query_text.rs`: random aggregates, read back and run the same, now
  with a window per group, and `per` among the names that are words.
- The fuzz program (§105) gives one aggregate in four a window per
  group, by count or by time, checked against `apply` on the model's
  matches, live too, and refused beside a sort, skip or limit.

## 174.8 Fuzzed and mutated
- **Fuzzed** 15 minutes each, the targets this reaches: queries 3,003
  programs, filter_text 16.7 million. filter_text's first run stopped
  on its own harness: `a in :l or size(i) < :l` uses `:l` as a list
  and as a count, which no value is, and `bind` rightly refuses it;
  the harness now passes over such a text. The rerun found nothing.
  Nothing in storage changed, so the other targets weren't run.
- **Mutated,** twenty-eight ways by hand, each run against every test:
  the window by time's ends, its newest, its duration; by count, the
  order, ties, `null`, one more; group keys not canonical, or one for
  all; nothing kept, or all; the checks of the path and of unwound
  arrays; index entries or a count alone answering; the path not read;
  `apply` and the database without the window; the refusal beside a
  limit; ids lost; the text writing, reading, a parameter, two
  windows. Twenty-four failed a test at first. Three more do now: a
  group of arrays `[5]` and `[5.0]`, an index holding every field, and
  a count alone, each with the window kept. The one standing leaves
  the kept documents out of their order: the groups order them by id
  whatever order they come in, so no answer changes.
