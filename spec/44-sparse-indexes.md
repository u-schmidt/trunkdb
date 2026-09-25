# 44. Sparse indexes (`catalog.rs`, `collection.rs`, `query.rs`, `export.rs`)

Real and tested: an index with no entry for a null or missing value,
for a field few documents have. It is part of file format 8, which
compound indexes started (§43.5) and which hasn't been released yet.

```rust
use trunkdb::IndexOptions;

let sparse = IndexOptions { sparse: true, ..IndexOptions::default() };
members.ensure_index_with("nick", sparse)?;
members.find(Filter::new().eq("nick", "ada"))?;      // through the index
members.find(Filter::new().is_null("nick"))?;        // a scan: they aren't in it
members.find(Filter::new().is_not_null("nick").sort_asc("nick").limit(20))?; // in order
```

## 44.1 What it leaves out
Since §32.2 every document has an entry in every index: a missing field
is indexed as null, so `x == null` can use the index. For a field that
one document in a hundred has, that means 99 null entries for every
real one. A sparse index drops them:
- **On one field:** every null value. That covers a missing field, a
  stored null, and, on a path with `[*]`, a null element or an element
  without the field.
- **On several fields:** the document, if it's null in all of them. So
  `(nick, team)` holds every document that has a nick or a team.

Measured by the typed test: 1,000 members, one in ten with a nick. The
sparse index on `nick` has 100 entries, and a plain index on the same
data would have 1,000.

It drops null as well as missing, while MongoDB's sparse indexes skip
only missing fields and keep stored nulls. Here the two are the same
value to every query (§32.1), so keeping stored nulls would enable no
additional query, and `== null` still couldn't use the index. On the
typed path, `None` is stored as null too, so skipping only missing
fields would skip nothing a typed app writes.

## 44.2 API
- `ensure_index_with(fields, IndexOptions { unique, sparse })`.
  `ensure_index` and `ensure_unique_index` now call it.
  `IndexOptions` derives `Default`, so `..IndexOptions::default()` fills
  in the rest, as an object initializer on an options class does in C#.
- `sparse_indexes()` lists them, next to `unique_indexes()`.
- Asking for an index that exists with other options is an error, as
  unique against plain already was (§33.2): `already has a sparse index
  on "nick", not a plain one; drop it first`.
- Unique and sparse combine. A unique index already exempted nulls
  (§33.1, §43.4), so only the entries change, not the rule.

Rejected:
- **A method per combination** (`ensure_sparse_index`,
  `ensure_unique_sparse_index`). The count doubles with every new
  option, while an options struct grows without new methods.
- **A builder** (`IndexOptions::new().sparse()`). Two bools don't need
  one, and the struct literal reads the same.

## 44.3 The planner
A sparse index lacks exactly the documents that are null in it, so it
may only be used where the filter already rules those out.
`rules_out_null(op, value)` asks whether a comparison fails for null:
`== "ada"`, `> 3` and `!= null` do, while `== null`, `<= null` and
`!= 5` don't (a missing nick isn't 5).

- **Bounds on a one-field sparse index** come only from comparisons
  that rule out null. With `[*]` this matters per element: `tags[*] ==
  null AND tags[*] > 3` holds for `[null, 5]`, whose null element has no
  entry. The range read is the one for `> 3`, and it holds the 5.
- **A compound sparse index** is used only if a comparison on one of its
  fields in the same AND rules out null (`excludes_null`). A matching
  document then isn't null in all of its fields, so it has its entry.
  `a == null AND b == 2` may use `(a, b)`.
- **Reading in sort order** follows the same rule. The index on `nick`
  serves a sort by `nick` only together with `nick != null` or another
  comparison that rules out null. Without one, the documents with a null
  nick would be missing from the result instead of sorted first.

Only the comparisons in the same AND list count. One inside an OR
doesn't, because the other branch may match a null.

`x != null` doesn't become a range of its own, though "the whole sparse
index" sounds like one. A one-field index also leaves out arrays and
NaN (§34.1), which `!= null` matches. For a sort, the sorted read
already scans for those unordered values after the index (§34.2).

## 44.4 Catalog, export, format
- **Catalog:** kind 3 now means "any other index", laid out as `[u8
  flags][u8 count]` and the fields, with flag 1 for unique and flag 2
  for sparse. A plain one-field index keeps kind 1 or 2. §43 gave
  compound indexes kinds 3 and 4, without flags. That was replaced
  before format 8 was released, so no released file has kind 4, or
  kind 3 without flags.
- **Unknown flags** are corruption ("unknown index flags"), not ignored.
  An index read without a flag it depends on would give wrong answers.
- **Export:** `{"field": "nick", "sparse": true}`, with `"unique": true`
  too if it is unique, and `"fields"` for a compound index.
- **`trunkdb info`:** `nick (sparse)`, `(team, email) (unique, sparse)`.

Rejected: **format 9**. No release carries format 8 yet, so one format
covers both compound and sparse indexes, and an older build refuses a
file with either up front (§43.5).

## 44.5 Tests
- `query.rs`: when a sparse index bounds a filter and when it's read in
  sort order. Covered: `== null`, `<= null` and `>= null` on one field;
  the element case above; a compound index with a null-ruling
  comparison on either field; and a sort with and without `!= null`.
- `collection.rs`:
  - The randomized find-against-scan tests run a second time with
    sparse indexes: on one field (`v`), multikey (`tags[*]`,
    `items[*].n`), and compound (`(a, b)`, `(a, b, c)`, plus `b`
    alone). The random values and filters include nulls, `== null` and
    `!= null`, and all of that holds through inserts, updates that move
    documents, deletes, compaction and a reopen. The compound variant
    must run both index plans, both sorted plans and a scan.
  - The random unique test runs again on a unique sparse index.
  - Typed members, as measured above: 100 entries, not 1,000; 120 for
    `(nick, team)`, one for each document with either field; 33
    documents read for 33 matches; `is_null` as a scan; a sort through
    the index with `is_not_null`.
  - Other options refused, in every direction, with the message; the
    same options accepted again; a unique sparse index refusing a
    duplicate name.
- `catalog.rs`: the cell kind for each combination of options, a round
  trip of each, and refused unknown flags; sparse indexes created and
  reopened.
- `export.rs`: a round trip with a sparse index and a unique sparse
  compound one; a hand-written sparse entry; `"sparse": "yes"` refused.
- `tests/cli.rs`: `info` shows `(sparse)` and `(unique, sparse)`, and
  `export` writes `"sparse": true`.
- Checked by breaking it on purpose, fifteen ways. Each of these fails
  a test:
  - null values kept in a sparse index;
  - a sparse compound index leaving out a document null in any field,
    not all;
  - an existing index accepted when only `unique` matches;
  - bounds on a sparse index from comparisons that match null;
  - a sparse compound index used whatever the conditions;
  - `rules_out_null` inverted;
  - a null-ruling comparison on any field, not the index's, counted;
  - a sparse index read in sort order whatever the conditions;
  - a sparse one-field index written as a plain cell;
  - the sparse flag not written, or read from the unique bit;
  - unknown flags accepted;
  - `"sparse"` not exported, or not imported;
  - `(sparse)` missing from `trunkdb info`.

## 44.6 Limits
- **No partial indexes** (MongoDB's `partialFilterExpression`, SQL's
  `CREATE INDEX … WHERE`). Only nulls are left out, not whatever a
  filter describes.
- **`x == null` never uses a sparse index.** It scans.
- **A comparison inside an OR** doesn't make a sparse index usable,
  even when every branch rules out null.
