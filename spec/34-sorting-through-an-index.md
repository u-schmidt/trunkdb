# 34. Sorting through an index (`query.rs`, `collection.rs`, `index/key.rs`)

Real and tested: a `find` with a `sort` and a `limit` on an indexed
field reads the index in order and stops after `limit` matches — the
documents after that are never read.

```rust
runs.ensure_index("started_at")?;
runs.find(Filter {
    sort: Some(Sort { field: "started_at".into(), order: SortOrder::Desc }),
    limit: Some(20),
    ..Filter::default()
})?;   // reads 20 documents, not every run; explain: IndexOrder { field: "started_at" }
runs.find_one(newest_first)?;   // reads one
```

## 34.1 One sort order: the index's
Reading an index gives its keys' order. For that to be a way of
sorting rather than a different result, in-memory sorting had to use
the same order, and before this it had none worth copying: values that
don't compare (a number and a string, a null and anything) counted as
equal. That isn't a total order — `1 = null = 0`, yet `1 > 0` — and
Rust's `sort_by` may panic on a comparison that isn't one, or return an
order that depends on the input's. Now `query::sort_order` is total:

- null (and missing, §32) before bools before numbers before strings —
  the index's type tags (§28.1, §32.2) — each kind by value, all of it
  reversed for `Desc`;
- values nothing orders (arrays, objects, binary, ids, NaN) after all of
  those in **both** directions, as equals — no index holds them;
- equal values in id order, on every plan: ids are UUIDv7, so that's
  insertion order. The in-memory path sorts its candidates by id before
  its stable sort; an index keeps equal values in id order anyway.

Nulls come first ascending, like SQLite and MongoDB (Postgres puts them
last). Rejected: nulls last in both directions — the index has them at
the low end, so a descending walk would have to find them separately.

Numbers now compare by exact value, `Int` against `Float` too. Before,
an `Int` was cast to `f64`, which rounds beyond 2^53: `2^53 + 1` equaled
the float `2^53` but was greater than the int `2^53` — no total order
survives that. This also changes filters, for numbers beyond 2^53 only:
`Int(2^53 + 1) == Float(2^53)` is now false. Index keys stay valid:
rounding keeps order, so `a < b` still implies `key(a) <= key(b)`.

## 34.2 When and how the index is read in order
`Filter::index_order` picks it when the filter has a `sort` and a
`limit`, the sort field has an index, and no `Eq` condition could be
answered by another index — a few documents found by value beat a walk
in order. Range conditions on the sort field itself narrow the walk.
`explain` says `IndexOrder { field }`. Rule-based like §28.4: a very
selective range condition on another field loses to the walk.

`read_in_index_order` walks the index's range, grouping entries by
their key's value part:
- a group whose values are all equal (`key::is_exact` — everything but
  numbers beyond 2^53 and strings cut to the key budget) is already in
  id order; documents are read one by one, and reading stops at the
  limit;
- the rare group that isn't is read whole and sorted exactly;
- for `Desc`, the groups are walked in reverse, each still in id order.

If the index runs out before the limit, the values no index holds may
still be missing: they sort last. A scan then finds them — unless a
range condition on the sort field is there, which none of them can match.

`find_one` asks for `limit: 1`, so a sorted `find_one` reads one entry's
document instead of every match. A sorted `cursor` runs `find` (§29.4),
so it benefits the same way.

Rejected: using the index without a limit. Every match is read either
way, plus a possible scan for unordered values; sorting in memory costs
little next to the reads. Rejected for now: a lazy B-tree walk. `range`
still collects the range's entries (keys and locations, no documents),
a few hundred per leaf page — the documents are what's expensive. A
lazy walk, with backward leaf links for `Desc`, is a later step.
(§49 added it, without the links, after §48 measured the cost.)

## 34.3 Tests
- `sorting_through_an_index_matches_sorting_in_memory`: every kind of
  value (huge numbers sharing a key, strings cut to one key, nulls,
  missing fields, arrays, NaN) through inserts, updates that move
  documents, deletes, and a reopen. 300 random sorted filters, each
  compared element by element — same documents, same order — with a
  full scan sorted in memory. Sorts by two indexed fields, with and
  without limits, with range, `Eq`, `Ne` and unindexed conditions, so
  every plan runs (`IndexOrder` on either field, `Index`, `Scan`).
- `a_limit_stops_the_reading_when_an_index_gives_the_order`: with a
  test-only counter of documents read (`data::RECORDS_READ`): 1000 read
  without an index, 5 with it for `limit 5`, 1 for a sorted `find_one`,
  11 for a range of ten (the bound is included, §28.4) — and an array
  value sorted after everything else.
- `sort_orders_every_kind_of_value_totally`: the order itself, both
  directions, with ties keeping input order.
- `ints_and_floats_compare_by_exact_value`, and which keys are exact.
- Checked by breaking it on purpose, four ways: every group treated as
  exact; `Desc` reversing entries instead of groups (ties in reverse id
  order); no scan for unordered values; no id pre-sort in memory. Each
  fails a test.

## 34.4 Limits
- One sort field; no index for a sort on several.
- `range` collects a range's entries before the walk (§34.2).
- Sorting by `_id` gives id order in both directions: ids are among the
  values nothing orders (§34.1), so they all tie.
- `count` ignores `sort`, as before.
