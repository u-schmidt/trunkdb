# 124. `=` and `!=` on arrays, objects and binary (`query.rs`, `aggregate.rs`, `update.rs`)

A filter compared single values only. For an array, an object or
binary data, `query::compare` answered "these don't compare" (`None`),
so `eq` never held and `ne` always did. Against 0.23.0, with three
documents, the first holding exactly the values asked for:

| Filter | Found | A reader expects |
|---|---|---|
| `meta = {"k": 1}` | nothing | the first |
| `meta != {"k": 1}` | **all three, the first too** | the other two |
| `tags = ["a", "b"]` | nothing | the first |
| `bin = {"$binary": "AAEC"}` | nothing | the first |
| `meta in ({"k": 1}, {"k": 2})` | nothing | the first two |
| `meta.k = 1`, `tags[*] = "a"` | the first | the first |

The `!=` row was the trap: a filter saying "not this value" picked the
document that held exactly this value. The builder and the text (§111)
took all of these without a word; §111.4 lists arrays and objects among
a filter's values.

The spec had said it in passing, never as a decision: §73 called these
"values … which no filter compares", and §59.6 listed binary as "the
same gap as ids had; nothing has asked for it". Found while TrunkDB
Admin put filters as text in front of a person (its §15).

## 124.1 What equal means
One equality, §73's, now `query::equal_values`, which `add_to_set` and
`pull` call too:
- **arrays** element by element, in order, of the same length;
- **objects** field by field, in any order, as `Map`'s `==` (§116);
- **binary** byte by byte;
- **inside them, single values as a filter's `eq`**: `[1]` equals
  `[1.0]`, NaN equals nothing, not even NaN; a missing field isn't a
  null one (`{}` isn't `{"a": null}`).

Not MongoDB's, which compares embedded documents field by field in
order: trunkdb's objects are equal in any order everywhere else, and a
JSON object has no order a person means.

## 124.2 Which comparisons
- **`=` and `!=`** compare arrays, objects and binary by 124.1, against
  a value of the same kind. An array never equals an object nor a
  single value: `tags = "a"` stays false (an array is a value, §42; its
  elements are `tags[*]`).
- With them **`in` and `not in`** (§115), **`not (… = …)`**, and **`@`
  inside `has`** (§46): `items has (@ = {"sku": "A1", "qty": 3})`.
  `items[*] = {"sku": "A1"}` compares each element.
- **`!=` against a missing field or null** holds, as for single values
  (§32).
- **`<`, `<=`, `>`, `>=` never hold** for these values, equal ones
  included: they ask for an order these values don't have (§34.1).

The change is in `value_matches`: where either side is an array, an
object or binary (`is_whole`), `Eq` is `equal_values`, `Ne` its
negation, the others false. `query::compare`, and with it sorting,
`query::equal` (unique indexes, §33) and `holds`, are as they were.

## 124.3 Indexes, and the bytes of entries
No single-field index holds these values (§34.1), so nothing bounds a
read by one: each filter above is planned as a scan, with or without an
index on its field. A sort walking an index finds documents with such a
sort value in its look for unordered values (§34.2), which checks the
whole filter.

One place decided on bytes and had to learn it. A compound index holds
every document (§43), these values as one shared tag, `TAG_OTHER`,
without their content. `key::compare_part` says such a part "doesn't
compare", and §104's `EntryMatcher` decided `=` false and `!=` true on
the entry alone. Now an `=` or `!=` with an array, object or binary
value against a `TAG_OTHER` part is undecided on the bytes, and the
document is read (`Covering` can't fill such a part either, so it falls
to the document as any inexact part does). Against a single value, and
for a single value against `TAG_OTHER`, the bytes still decide: never
equal. Without this, `n >= 0 and meta = {"k": 1}` read through an index
on `(n, meta)` would have missed every document it asks for, in that
plan alone.

No index holds these values whole (§42.2 rejected indexing an array as
one value): these filters cost a scan, as `contains` does.

## 124.4 Groups
§107.2 put values nothing orders into "one group per equal encoding":
`{"a": 1, "b": 2}` and `{"b": 2, "a": 1}` were two groups, `[1]` and
`[1.0]` two. Once a filter finds them equal, a group's documents by
`eq` would no longer have been the group's. Now the group key of an
array, object or binary value is its canonical form (`query::canonical`):
object fields by name, a `Float` with a whole value an `i64` holds as
that `Int`, at every depth; binary as it is. Values equal by 124.1 are
one group; the row shows its first document's value by id, as for `5`
and `5.0`. NaN keeps one group per bit pattern: no filter finds it.

## 124.5 Unique indexes: unchanged
A single-field unique index has no entries for these values and never
checked them; a compound one compares `TAG_OTHER` parts with
`query::equal`, which never finds them equal. Left so: a file written
before could hold two documents a stricter index would call duplicates,
with no way to open it as it is. `IndexOptions::unique` says these
values aren't checked. §33.1's "no duplicates means exactly
`find(field == v)` never returns two documents" holds for every value
an index checks; for these, `=` may now find two.

The `queries` fuzz target's model of unique indexes (§119) asked the
filter's `Eq` which writes collide. Its first run after the change
failed on exactly this: two equal objects in a unique multikey index,
written without refusal, which the model called a duplicate. The model
now leaves arrays, objects and binary out, as the index does.

## 124.6 What changes for a caller
Answers, nothing else: no file format, no WAL, no API. A filter with an
array, object or binary value compared by `=`, `!=`, `in` or `not in`
finds what it says, and aggregates merge groups equal by content. A
program that used `!=` with such a value got every document and now
gets fewer. The old-file fixtures (§120) read as they did. No flag
keeps the old answers: the old `!=` was wrong, not a choice anyone
made, and 1.0 promises that a query means the same in every 1.x, so it
changes before.

## 124.7 Rejected
- **Refusing these values in a filter** (an error, or a parse error in
  the text): honest about the gap, but it takes away a query people
  write and expect, and `Condition::eq` can't fail.
- **MongoDB's ordered object equality:** 124.1.
- **Ordering arrays and objects** for `<` and the rest, as MongoDB's
  BSON comparison does: nothing asks for it, and an order once chosen is
  API.
- **`tags = "a"` matching an element**, as MongoDB does: `[*]` says
  that, and §42 decided an array is a value.
- **Unique indexes checking these values:** 124.5.

## 124.8 Tests
`query.rs`, `whole_values_are_equal_by_their_contents`: equal pairs
(arrays, `[1]` and `[1.0]`, objects in either order, nested, nulls
inside, empty ones, binary, date-times inside), each both ways, with the
same canonical form, `Eq` holding and every other operator not;
unequal pairs (order of elements, lengths, `[1]` and `["1"]`, a missing
field against a null one, an array against an object and a single
value, other bytes, NaN inside, `2^53 + 1` against `2^53` inside), with
`Ne` holding.

`tests/equal_whole_values.rs`:
- 22 filters, through `find`, `count`, `find_one`, `cursor`,
  `explain_run`, the same text read as a whole filter, and
  `Condition::matches` in memory: `=`, `!=`, `in`, `not in`, `not`,
  `has (@ = …)`, `items[*] = {…}`, `!=` on `[*]`, an `or`, a value of
  another kind, a path into an object, and the four orderings finding
  nothing. Once on a scan, once with single-field, multikey and four
  compound indexes on the fields; and through a snapshot.
- A compound index on `(meta, n)`: `meta = 7` decided on the bytes,
  one document examined; an object read. One on `(n, meta)`, its
  entries deciding the filter (`covering`): `=`, `!=`, `in` and `not`
  with objects find 50, 150, 50 and 150 of 200.
- A sort through an index on `meta` with `=` an object, at limits 1,
  5, 20 and 25, and with `!=`: the look for unordered values finds
  exactly the equal ones.
- Groups by `meta`, `tags`, `bin` and `items`: equal values one group,
  each group's `=` counting what it counted, the row showing the first
  document's value.
- `add_to_set` and `pull` with objects written in another field order
  and `2.0` for `2`.

Fuzzing: the `queries` target's value pool gained `[1.0]`, `[1, 2]`,
`[2, 1]`, an object of two fields in both orders, a nested one with
`[1]` and with `[1.0]`, and a second binary value, so equal and unequal
ones meet in documents, filters and unique indexes. Its first
run of 1,000 programs found the uniqueness case of 124.5; after that,
1,000 programs (seed 124001) and 20,000 (seed 124124) ran clean.

**Mutations**, each caught: `=` on these values still never holding;
`!=` always holding; objects compared in order; `[1]` not equal to
`[1.0]`; `<=` and `>=` holding on equal ones; the bytes still deciding a
`TAG_OTHER` part against an object; the group key still by encoding;
the canonical form keeping whole floats; `add_to_set` comparing by
`Document`'s `==`.
