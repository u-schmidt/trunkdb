# 157. Aggregates as text (`query_text.rs`, `src/bin/trunkdb.rs`)

The third step of §155: an aggregate written as text, for the
`trunkdb` command and so for an AI agent, and for any program that
takes one from a person, as a filter has had text since §111. Before,
an agent counting per group read every document's field through a
pipe (`jq | sort | uniq -c`), and an aggregate was Rust alone.

```text
count() as n, avg(hours) as mean,
count() filter (where status = "open") as open,
last(status order by changed) as latest
group by team having n > 10 order by n desc limit 5
```

## 157.1 The language
SQL's select list, `GROUP BY` and what follows it, as a separate text
beside the filter's: `FromStr for Aggregate` and `Display`, as a
`Filter` has them. In this order, each part optional:

- `window <path> <duration>`: `Aggregate::window` (§139). A duration is
  a whole number and a unit, `ns`, `us`, `ms`, `s`, `m`, `h` or `d`;
  `Display` writes the longest unit it is a whole number of, `300s` as
  `5m`.
- `unwind <paths>`: `unwind` for each (§147).
- Columns, between commas, each a function, then `filter (where
  <condition>)` for §126's condition, then `as <name>`:
  `count()` (or `count(*)`), `count(distinct p)`, `sum(p)`, `min(p)`,
  `max(p)`, `avg(p)`, `first(p)`, `last(p)`; `first` and `last` take
  `order by <path>` inside, §146's `by`, as SQL's ordered aggregates
  write it (`array_agg(x ORDER BY y)`).
- `group by <paths>`, `having <condition>`, `order by <names>`,
  `limit <n>`, `offset <n>`: the rows' `having`, sort, skip and limit
  (§107).

The window and `unwind` come first as they act first, on the documents.
Conditions, paths and names are the filter's (§111.3), so backticks for
a name that would be read as a word.

## 157.2 Its words, and the filter's
§111 promised no new reserved words. The aggregate's words, `window`,
`unwind`, `group`, `by`, `having`, `as`, `filter`, `distinct`, and the
functions, are keywords only where the aggregate expects them, and the
filter's text doesn't change: `group by group` groups by a field named
`group`, `count() as having` names a column `having`. Where a word
could start a clause or be a path, it starts the clause, as `order`
does in a filter; such a field is written in backticks.

## 157.3 `trunkdb aggregate`
`trunkdb aggregate <file> <collection> <filter> <aggregate>
[--explain]`: the rows, one JSON line each, how many on stderr;
`--explain`, `explain_aggregate` printed as `find --explain` prints
`explain_run` (§113). The filter is one argument, `''` for none, as
for the writes (§156). A mistake in either text is printed with its
line and a `^`, naming which text, exit code 2; an aggregate the
library refuses (two columns of one name, §134) is exit code 1. It
opens the file read-only.

## 157.4 Rejected
- **One SQL statement, `select status, count(*) … where … group by
  …`:** a filter's `order by` and `limit` pick documents, an
  aggregate's pick rows; in one statement one of them needs a form of
  its own, and `select` already means a filter's selection (§106).
- **`count_distinct(p)` beside `count(distinct p)`:** two spellings of
  one column; SQL's is the one people know.
- **`last(p) by q`, as the builder writes it:** after the parentheses
  it reads as a clause of the aggregate.
- **Durations as `interval '5 minutes'`, or ISO 8601's `PT5M`:** longer
  to write, and nothing here needs months.

## 157.5 Tests
- `every_aggregate_reads_back_from_its_text`: 3,000 random aggregates
  of every part, paths and names drawn also from the aggregate's own
  words, the empty name, backticks and `[*]`: each one's text reads
  back to the same text, twice to the same aggregate, and gives the
  same rows over random documents as the one built.
- `an_aggregate_s_text_says_what_its_builders_say`: the doc's example
  against its builders, `count(*)`, `count(distinct …)`, any case, each
  unit, `Display`'s longest unit, `unwind` of two.
- `an_aggregate_s_mistakes_are_named`: seventeen texts that aren't an
  aggregate, each with what is said.
- `aggregate_prints_the_rows_of_its_text` (`tests/cli.rs`): rows by
  group and sorted, a column's filter, `unwind` with `having`, `last`
  by a field and `count(distinct …)`, `--explain`, a mistake in each
  text, a missing collection, an aggregate the library refuses.

## 157.6 Mutations
Each run against the library's text tests and `tests/cli.rs`'s
aggregate test: `distinct` not written; `count(*)` not read; `first`'s
`order by` not read; a column's filter dropped; a duration written in
the shortest unit; the shortest unit matched (`5ms` as `5m` and `s`);
a unit followed by letters taken; the window not kept; `limit` and
`offset` written the other way; a row sort's `desc` not written;
`unwind` not read: caught. `having`'s conditions kept as one `and`
instead of a list: survives, and is equivalent, the same rows and the
same text.
