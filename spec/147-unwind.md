# 147. Unwind (`aggregate.rs`, `collection.rs`)

§107 grouped an array as one value, and refused `[*]` in an
aggregate's paths; §107.8 left grouping by array elements, MongoDB's
`$unwind`, as "a design of its own, if asked for". It is the standard
way document databases answer a question per element: MongoDB's
`$unwind`, PostgreSQL's `unnest` and `jsonb_array_elements`, SQLite's
`json_each`, ClickHouse's `arrayJoin`. Tasks per tag, sales per sku over
orders' items, readings per sensor of a batch.

```rust
let per_sku = Aggregate::new()
    .unwind("items")                 // one document per element of `items`
    .group_by("items.sku")           // `items` is the element now
    .sum("items.qty").named("sold")  // the qty of that same element
    .count().named("lines");
```

## 147.1 The API
- **`Aggregate::unwind(path)`:** each document the filter finds is one
  per element of the array at `path`, the element at `path` in place
  of the array. The groups, the columns and their conditions (§126)
  see the elements.
- **Whole elements:** `items.sku` and `items.qty` are fields of one
  element, so a sum per sku sums that sku's quantities. Paths with
  `[*]`, `group_by("items[*].sku")` beside `sum("items[*].qty")`,
  couldn't say which qty is whose.
- **Each element counts:** `["a", "a"]` is two documents in group `"a"`.
- **An empty array, `null` or a missing field:** the document is left
  out, MongoDB's default. A value that isn't an array is one element,
  itself, as MongoDB has it.
- **Called again,** each document made is unwound by the next path:
  `items`, then `items.parts`, goes into each element's parts;
  unrelated paths make every pair.
- **Before and after:** the filter picks documents before they are
  unwound, its sort, skip and limit too. A window (§139) keeps
  documents, so its path may not be the unwound one or inside it.
- **Refused** as the aggregate runs, `InvalidInput`, as every mistake
  in an aggregate is: an empty path, one with `[*]`, `_id`, a window
  inside an unwound path.
- **`first` and `last`** (§146) by id see the elements in their
  array's order: the first element of the first document, the last of
  the last.

`apply`, live aggregates and windows unwind alike: each runs the
aggregate on documents, which `Groups::add_document` unwinds.

## 147.2 Read from documents
An aggregate that unwinds reads its documents, the unwound array whole
among the fields it decodes; the two shortcuts of §107.5 are left:

- **The count of every match,** where an aggregate is counts alone:
  it counts documents, not elements.
- **Index entries:** a multikey index (§42) holds each value once per
  document, so `["a", "a"]` is one entry where the unwind counts two;
  an index on the array whole holds no arrays (§34), and hands over a
  `null` the unwind leaves out.

Reading per element from a multikey index is a step of its own (147.5).

## 147.3 Memory
Each document is cloned once per element, the clone holding only the
fields the aggregate decodes, and handed to the groups one at a time:
nothing more is kept than the groups were keeping.

## 147.4 Rejected
- **`[*]` in group and column paths**, `group_by("tags[*]")`: reads
  naturally for one array, but fields of one element can't be paired,
  and a document with two arrays would be crossed silently.
- **An option to keep documents without elements,** MongoDB's
  `preserveNullAndEmptyArrays`: until asked for. A `null` or missing
  array as a group of its own is `group_by` without `unwind`.
- **The element's index in its array,** MongoDB's
  `includeArrayIndex`: until asked for.
- **Unwinding in `find`:** documents handed out per element would be a
  read that isn't what is stored; it is an aggregate's step.

## 147.5 Next: from a multikey index
"Tasks per tag" on an index over `tags[*]` could be counted without
reading documents, as §107.5 does for plain fields. As the index keeps
a value once per document, only what doesn't depend on repeats could
come from it: `count_distinct`, `min`, `max`, groups that are all a
document has. `count` and `sum` would read the documents whose arrays
repeat a value, which an entry can't tell. Planned.

## 147.6 Tests
- `aggregate.rs`:
  - `unwind_makes_a_document_per_element`: sales per sku, fields of one
    element together; an empty array, `null` and a missing field left
    out, an object not in an array one element; a column's condition
    on the element; equal elements each counted, a plain value one;
  - `unwinds_follow_one_another`: `items`, then `items.parts` into each
    element; two unrelated paths making every pair; `first` and `last`
    by the element's place;
  - refused: an empty path, `[*]`, `_id`, a window on the unwound path
    and inside it; a window on a path that only starts the same, `rat`
    beside `r`, taken.
- `collection.rs`, `an_unwind_reads_documents_and_counts_elements`: a
  multikey index and one on the array whole, elements counted from
  documents, with a filter and without, as `apply` gives them; a
  collection without arrays, where an index holds every document and
  its entries would count a `null`.
- `tests/live_aggregates.rs`, `a_live_aggregate_unwinds`: elements
  added to an array change one row and make another.
- **The `queries` fuzz target** (§105) unwinds `tags` in one aggregate
  in six where it is among the fields, a path of `tags` then `tags.k`
  one time in two: plain values, arrays of them and of objects, empty
  arrays and `null`, each held against `apply` on the model, through
  every plan, live (§139) and windowed. The smoke run's 1,000 programs
  made 963 such aggregates, 2,000 of seed 7 2,037, every one as the
  model says. It draws more of each program, so a seed no longer gives
  the program it gave; the aggregates from entries of 1,000 programs
  went from 601 to 445, those that unwind reading documents.

## 147.7 Mutations
| Mutation | Caught by |
|---|---|
| Never unwound | a document per element, one another, reads documents, live |
| Empty, `null` or missing kept | a document per element, reads documents |
| A plain value left out | a document per element, reads documents |
| Equal elements counted once | a document per element, live |
| Only the first unwind | one another |
| The elements of a document sharing an order | one another |
| The array not decoded | reads documents |
| Unwound from index entries | reads documents |
| The count of every match with an unwind | reads documents |
| The unwind path not checked | refused |
| A window inside an unwind taken | refused |
| "Inside" without the dot: `rat` inside `r` | refused |

One survived at first: unwinding taken off for index entries went
unseen with arrays in the collection, as an index holds none, so its
entries never cover every document and the entries path declines. A
collection of plain values and a `null` catches it.
