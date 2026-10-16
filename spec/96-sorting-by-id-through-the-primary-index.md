# 96. Sorting by `_id` through the primary index (`query.rs`, `collection.rs`, `index/key.rs`)

A sort with a limit walks a secondary index in order and stops at the
limit (§34.2). A sort by `_id` never did. The primary index holds every
document in id order, but `index_order` looked only at secondary
indexes. So `Filter::new().sort_asc("_id").limit(50)`, the first page
of any list in id order, read every document of the collection and
sorted them all in memory, to keep 50.

The admin tool's Explain (§94) showed it: its default list, sorted by
`_id`, read and sorted the whole collection for every page. On the
benchmark's 100,000 documents a page took 189 ms.

§96 walks the primary index for a sort whose first key is `_id`.

## 96.1 Why the order is the same
Since §95 every document is an object and carries its id as `_id`
(§59). Ids sort by their bytes (§62.2), and the primary index's key is
the id's 16 bytes (`key::primary`). So the primary index's order is the
order of `_id`, ascending forward and descending backward, for every
document of every file this build opens. Ids are unique, so no two
documents tie, and keys after `_id` have nothing to order.

## 96.2 When the walk is used
`Filter::index_order` takes the primary index for a sort whose first
key is `_id` (`Filter::id_order`), with the rules a secondary one has:

- a sort whose first key is `_id`, and a limit;
- not when the filter names ids (`ById` beats any walk, §61);
- not when an `Eq` on an indexed field, or an OR of indexed branches,
  finds the documents by value (§34.2: a few documents found by value
  beat walking in order). The check is shared with the secondary walk
  (`found_by_value`).

**`_id` comparisons with an id narrow the walk.** `gt("_id", last)`
reads only from `last` on, which makes keyset paging ("the 50 after the
last one shown") a walk too. The range is exact (`key::primary_range`):
every primary key is a whole id, so the first key after an id is that id
with a zero byte added, and `gt` leaves out its id where a secondary
range keeps the boundary for the check (§28.3). Several comparisons are
intersected. A comparison of `_id` with anything but an id doesn't
narrow, and neither does `ne`. Every document read is still checked
against the whole filter.

`read_in_index_order` does the walk, as for a secondary index.
`OrderedRead::index` is `None` for the primary index, and its root comes
from the collection; a collection that doesn't exist walks nothing. Most
of the machinery is idle:
- every key is one id, a group of its own and exact (`Spans::primary`);
- each document is handed out as soon as its key is read, without
  reading the next key to see its group end, so the walk reads exactly
  as many entries as documents;
- there are no unordered values to look for afterwards, and nothing to
  hold back on a descending walk.

## 96.3 How it reports itself
- **`explain`:** `QueryPlan::IndexOrder { field: "_id" }`. There is no
  new variant: it is the same plan, on the primary index.
- **`explain_run`:** one read of `_id`, bounded by the `_id` comparisons
  that narrowed it, `backward` for a descending sort, and
  `SortWork { from_index: <every key>, in_memory: 0 }`.

## 96.4 Rejected
- **A secondary index on `_id`:** refused since §33 and still
  pointless. The primary index already is one.
- **Walking without a limit:** as for secondary indexes (§34.2),
  without a limit every match is read anyway, and the sort in memory
  costs little beside that.
- **A new plan variant:** the walk is the one `IndexOrder` names; only
  the index differs, and `field` names it.

## 96.5 Tests
- **The randomized sorted-find corpus (§34, §47, §72):**
  - sorts by `_id` too, first and after `v` or `w`, both directions,
    with skip and limit;
  - with one or two `_id` comparisons against ids that exist and ids
    that don't, `ne`, and comparisons with a number;
  - `find` equals the sort in memory every time, and `explained` (§94)
    checks plan and counts; `IndexOrder { field: "_id" }` must run.
- **Exact counts** (`a_sort_by_id_walks_the_primary_index`, 1,000
  documents):
  - the first 20, ascending and descending: 20 entries and 20
    documents read, one unbounded read, backward for descending;
  - `gt("_id", x)`: the 10 after `x`, 10 read, bounded by it;
  - `lte` backward, and `gte` with `lt`: exactly the ids in the stretch;
  - page 3 of 50: 150 read, the first 100 skipped;
  - keys after `_id`: all served by the index, none in memory;
  - another condition: read up to its third match;
  - `gt("_id", 5)` and `ne`: no bounds, the walk unnarrowed;
  - a collection that doesn't exist: nothing.
- **Precedence** (`finding_by_id_or_by_value_beats_the_walk_by_id`):
  `ById`, an `Eq` on an indexed field and an OR of indexed branches
  beat the walk; a range on a secondary index doesn't; without a limit,
  a scan. An OR of two ids reads those two, sorted in memory: the walk,
  which such an OR can't narrow, would read them all.
- **`key::primary_range`:** each comparison holds exactly its ids, the
  boundary in or out as it says.

Benchmark (§48), "first 50 by id", 100,000 documents:

| | before | after |
|---|---:|---:|
| trunkdb | 189 ms | 80 µs |

SQLite takes 36 µs, redb 25 µs and sled 29 µs, which read their table
by key the same way.

Nine hand-made mutations, all caught:
- the walk forward for a descending sort;
- an `_id` comparison with a non-id narrowing the walk;
- the walk taken while ids are named;
- the walk taken without a limit;
- `from_index` reporting 1 where every key is served;
- the walk not giving way to documents found by value;
- reading the next key before handing out a document;
- `gt` keeping its id, `lte` leaving it out.

"The walk taken while ids are named" survived the first run: `explain`
still said `ById`, and an `eq` range read the same one document. The
OR of two ids above catches it.

Full suite, clippy `-D warnings`, fmt, rustdoc, the bench crate.

## 96.6 Limits
- Without a limit, a sort by `_id` still reads every match.
- Only top-level `_id` comparisons narrow; one inside an AND group or
  an OR is only checked.
