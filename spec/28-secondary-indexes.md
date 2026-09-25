# 28. Secondary indexes (`index/`, `catalog.rs`, `collection.rs`, `query.rs`)

Real and tested: a collection can have B-tree indexes on top-level
fields, and `find` reads through one when a condition allows.

```rust
pings.ensure_index("tst")?;   // true: built now; false: it existed
pings.find(filter)?;          // tst >= a AND tst <= b reads just that range
pings.explain(&filter)?;      // QueryPlan::Index { field: "tst" }
pings.indexes()?;             // ["tst"]
pings.drop_index("tst")?;     // frees its pages
```

`ensure_index` builds the index from every existing document in one
atomic batch, and creates the collection if needed, so an app can
declare its indexes at startup. From then on every insert, update and
delete keeps each index current, inside the same batch as the document
change. Indexes persist. Nested fields (`a.b`) came later (§31); there's
still no index on several fields at once.

## 28.1 Keys are byte strings
The B-tree no longer knows what it indexes. Its keys are byte strings
compared byte by byte (`[u8]`'s `Ord`), and `index/key.rs` builds them:
- **primary index**: the 16-byte `DocId` — so its leaf and branch cells
  are byte for byte what they were (`[key][location]`, `[key][child]`,
  the key's length implied by the cell's);
- **secondary index**: the encoded field value, then the `DocId`. The id
  makes every key unique even when thousands of documents share a
  value, and makes removing one document's entry an exact-key delete.
  The index stores the document's location, like the primary index, so
  a lookup reads the data page directly, not through the primary index.

The value encoding is built so byte order agrees with `Filter`'s
comparisons (`query::compare`):
- **numbers**: `Int` and `Float` share one type, as they do in `compare`
  (an `Int` compared with a `Float` converts to `f64`): the `f64`'s bits,
  big-endian, sign bit flipped for positives and all bits for negatives,
  the standard trick for making bytes sort numerically. `-0.0` becomes
  `0.0`; `NaN` isn't indexed.
- **strings**: UTF-8 bytes with `0x00` escaped as `0x00 0xFF` and
  `0x00 0x00` as terminator. `compare` orders strings by bytes, and the
  terminator makes the encoding prefix-free: without it, `"ab"` + id
  could sort after `"abc"` + id.
- **bools**: one byte.
- A type tag comes first, so each type is one contiguous stretch of the
  index — a range never crosses into another type, matching `compare`,
  which never orders values of different types.

Everything else (`Null`, arrays, objects, binary, ids, missing fields)
has no key: no `Eq`/`Lt`/`Lte`/`Gt`/`Gte` condition can match it, so an
index without those documents answers those conditions correctly.
(Since §32, `Null` and missing fields do have a key: `== null` matches
them.)

Rejected: a **typed key** (compare decoded `Document`s in the tree).
Every comparison would decode, and the B-tree would depend on the
document model; byte keys keep it a plain ordered map, which the
`InMemoryIndex` fake now literally is (a `BTreeMap<Vec<u8>, _>`), and
the tree tests check the real one against it.

## 28.2 Splitting by bytes, and why each half still fits
§10.4's guarantee rested on fixed-size entries: `n + 1` entries that
overflow a page holding `n` split into two halves of about `n / 2`,
each of which fits. With keys from 0 to 1024 bytes, "half the entries"
can be most of the bytes, so leaves and branches now split at the
**byte** midpoint: the first entry at which the running total reaches
half. The guarantee comes back through a size cap:

Let `C` be a page's room for cells and slots (8179 bytes; 8175 since
the page checksum, §40) and `m` the
largest entry. Keys are capped at `MAX_KEY_LEN = 1024` bytes, so
`m ≤ 1038 < C / 4`. An overflowing page holds `T` bytes with
`C < T ≤ C + m`. Splitting where the running total first reaches `T / 2`:
- left `< T / 2 + m ≤ (C + m) / 2 + m < 5C/8 + C/4 < C`;
- right `≤ T / 2 ≤ (C + m) / 2 < 5C/8 < C`.

Both halves also keep at least two entries, since two entries can't
reach `T / 2 > C / 2`. That matters for a branch split, which promotes
one entry and still needs one on each side.

*Since §41.3 a leaf split has a second rule: a key appended past the
rightmost leaf's last one moves to the new page alone. Both halves fit
trivially: the left one is the page as it was, the right one a single
entry.*

The cap costs nothing in practice: string values are **cut** to fit,
about 1000 bytes (§28.3), so no document is ever rejected for an
overlong indexed value.

## 28.3 Candidates are always rechecked
Several different values can share a key prefix:
- `Int`s past 2^53 round to the same `f64`;
- strings that agree for their first ~1000 bytes are cut to the same
  key.

So an index range is a **superset**: `find` reads the documents in it
and applies the whole filter to them, exactly as it does for a full
scan. The ranges are built for that (`key::range_for`):
- `Eq v`: every key starting with `v`'s encoding;
- `Gt`/`Gte v`: from `v`'s encoding to the end of its type;
- `Lt`/`Lte v`: from the start of its type through `v`'s encoding.

Bounds include `v`'s own encoding even for the strict ops, since a
different value may share it; the recheck drops the extras. The
encoding only has to be monotone (`a < b` ⇒ `key(a) ≤ key(b)`), not
strictly — which is what makes cutting strings and rounding big `Int`s
safe.

Rejected: rejecting long values (an error on insert, as the old
`too_large` did for documents) or hashing them (loses order, so no
range queries).

## 28.4 Choosing an index: a fixed rule
`Filter::index_range` picks:
1. the first indexed field with an `Eq` condition, otherwise the first
   with any range condition;
2. then intersects **all** of that field's range conditions, so
   `tst >= a AND tst <= b` reads just `a..=b`, the time-series
   workload's range query (§5.1).

`Ne` and `Contains` never use an index: neither is a range. Conditions
on other fields are still applied, in the recheck. There's no cost
model — no statistics, no choosing between two indexes by selectivity.
With one or two indexes per collection that's the right amount of
planner. `explain` exposes the choice, for tests and for users.

Without a `sort`, `find`'s order is now unspecified: by `_id` for a
scan, by index key for an index range. §10.6 had already noted it wasn't
promised.

## 28.5 Catalog: a kind byte, and index cells
Indexes are catalog cells of their own, not a list inside the
collection's cell. Creating or dropping one appends or tombstones a
cell; the collection's cell keeps its fixed length, so
`set_current_data_page` still rewrites it in place.
- collection cell: `[u8 kind = 0][u64 index_root][u64 current_data_page][name]`
- index cell: `[u8 kind = 1][u64 root][u8 name length][collection name][field name]`
  (kind `2`, same layout: a unique index, §33.3)

Field names are capped at 255 bytes like collection names (§22.3), so
an index cell always fits. `CollectionMeta` stays a small `Copy` value;
the indexes live beside it in `Catalog` (`indexes(collection) ->
&[IndexMeta]`). An index cell whose collection doesn't exist is reported
as corruption at `load`. This changed the catalog cell format, so the
format version is now 3 (§21.2).

## 28.6 Writes keep indexes current
`apply_write_op` handles every op the same way through one function,
`update_secondary_indexes(old, new)`, where each side is a
`(document, location)` or nothing: insert is `(none, new)`, delete is
`(old, none)`. For each index it computes the key before and after,
and does nothing if both key and location are unchanged. Otherwise it
removes the old entry and inserts the new one. The location matters:
a document that moves (§20.3) needs its index entries re-pointed even
when the value didn't change. An update or delete reads the old
document first, but only when the collection has secondary indexes.
*Since §42.2 each side is a set of keys: a multikey index holds one
entry per element.*

Building, maintaining and dropping all run through the batch protocol,
so a failed batch rolls back its index entries with everything else. To
share that protocol, `write_batch`'s body became `Database::transact`,
which takes a closure over `(&mut Catalog, &mut FileStore)`;
`write_batch`, `ensure_index` and `drop_index` all use it.
`BTreeIndex::free_all` reads the whole tree before freeing any page of
it, as `free_chain` does (§26.5).

## 28.7 Tests
- `variable_length_keys_match_a_btreemap` (`btree.rs`): 3000 random
  inserts and removes of keys from 0 to 1024 bytes over a four-letter
  alphabet, including `0x00` and `0xFF`, so long shared prefixes and
  uneven splits are common. Every lookup, the full scan and 400 random
  ranges must match `InMemoryIndex`.
- `indexed_finds_match_full_scans_through_every_kind_of_write`
  (`collection.rs`): random documents cover every value type, duplicate
  values, `Int`s past 2^53, strings that differ only past the cut,
  `NaN`, `-0.0` and missing fields. Padding of very different sizes
  makes updates move documents. The index is built over existing
  documents, then maintained through inserts, updates (half of which
  keep the indexed value, so only the move touches it) and deletes.
  Then 300 random one- and two-condition filters must find through the
  index exactly what checking every document finds — before and after
  a reopen. About 90 % of the filters use the index, and about 60 %
  match something.
- **Checked by breaking the code on purpose:** three bugs were
  introduced one at a time and each made that test fail — not removing
  old keys, making `Lt` bounds exclusive, and not re-pointing a moved
  document's entries. The last one first went unnoticed, which is why
  the padding and the value-keeping updates exist.
- Plus: idempotent `ensure_index`/`drop_index`, a dropped index's pages
  being reused, declaring an index before the collection exists,
  rejecting `_id` and overlong field names, a failed batch leaving no
  index entries, and the time-series range query (§15) with an index.

## 28.8 Cost and limits
- Each index costs one B-tree insert per insert, and a remove + insert
  per update that changes the value or moves the document. Updates and
  deletes also read the old document.
- `ensure_index` on a large collection is one big batch: every index
  page is staged in memory and written to the WAL (§19.9).
- One field per index (top-level at first; dotted paths since §31);
  no compound, unique, or sparse/partial
  options; no index-ordered `sort` (results are still sorted in
  memory — until §34). Each is a natural next step, none is needed by the reference workloads yet.
