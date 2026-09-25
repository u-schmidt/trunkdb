# 43. Compound indexes (`index/key.rs`, `catalog.rs`, `query.rs`, `collection.rs`)

Real and tested: an index on several fields at once. It finds documents
by `Eq` on its first fields and a range on the next one, reads them in
the order of the field after the fixed ones, and as a unique index
forbids the same values in all of its fields at once. File format 8.

```rust
tasks.ensure_index(["status", "created"])?;
// Found by status and read in order of creation: 20 documents read.
tasks.find(Filter::new().eq("status", "Queued").sort_asc("created").limit(20))?;

users.ensure_unique_index(["tenant", "email"])?;   // an email once per tenant
tasks.drop_index(["status", "created"])?;
```

## 43.1 API and catalog
`ensure_index`, `ensure_unique_index` and `drop_index` take anything
that is `IndexFields`: a field (`"age"`, `"address.city"`, `"tags[*]"`),
or an array or slice of them. A one-field call works as before.
`indexes()` names a compound index by its fields in parentheses,
`"(status, created)"`, and so do `explain` and `DuplicateValue`.

`IndexMeta.field` became `fields: Vec<String>`. A one-field index keeps
its catalog cell (kinds 1 and 2). A compound index gets a new kind, 3:
`[u8 flags]` (unique, and since §44 sparse), `[u8 count]`, then each
field as `[u8 length][name]`. It first had kinds 3 and 4 for unique,
without flags; that changed before format 8 was released (§44.4).
Export writes a compound index as an array of its fields,
`["status", "created"]`, or as `{"fields": [...], "unique": true}`.

The rules `ensure_index` checks:
- at most 8 fields;
- no field twice;
- each a path as for one field (§31.4);
- none with `[*]`. A compound index holds one entry per document (§43.2);
  a multikey one holds one per element. MongoDB, too, allows at most one
  array per compound index. Here it's none, since the sorted read below
  needs one entry per document.
- A different order of the same fields is a different index: `(b, a)`
  sorts by `b` first.

Rejected:
- **Methods of their own** (`ensure_compound_index`), and a fourth for
  unique ones. A trait lets `ensure_index("age")` and
  `ensure_index(["status", "created"])` be the same call.
- **Named indexes**, as LiteDB and MongoDB have. The fields are the name
  here, as they were for one field.

## 43.2 Keys
A compound key is its values encoded one after another
(`key::encode_values`), then the document id. The encoding was already
prefix-free (§28.1), so keys sort by the first value, then the second,
and so on, whatever the id after them. `key::parts` splits a key back
into its values, reading each one's length from its tag.

Each value gets an even share of the key: a string is cut to
`(1024 − 16) / n − 3` bytes, 1005 for one field (as before) and 123 for
eight. `key::part_is_exact` knows the share, so a cut string is still
known to be cut (§34.2).

A compound index holds every document, unlike a one-field index, which
leaves out values no comparison can match (arrays, objects, NaN, ...).
If it left out a document whose `b` is an array, an `Eq` on `a` through
the index `(a, b)` would miss that document. Such values get a tag of
their own, `TAG_OTHER`, sorting after every other type, as they do in a
sort (§34.1). A missing field is null, as everywhere (§32).

## 43.3 The planner
- **Finding by value** (`compound_bounds`): an `Eq` on each of the first
  fields fixes a key prefix, and range comparisons on the next field
  narrow within it (`KeyRange::under`). Nothing on the first field means
  no bounds, since keys sort by it first.
- **Choosing an index:** bounds are ranked by kind as before (by value,
  then an OR, then a range), and among equal kinds by how many fields
  they narrow. So `status == "Queued" AND created > x` takes
  `(status, created)` over an index on `status` alone, while `status ==
  "Queued"` by itself keeps the one-field index, the first among
  equals.
- **Reading in sort order** (`index_order`): a compound index qualifies
  when the sort field follows fields that an `Eq` each fixes, as in
  `(status, created)` for `status == "Queued"` sorted by `created`. It's
  found by value and in order at once. The more fields fixed, the
  better. With fields fixed, the old rule of preferring another `Eq`
  index over reading in order doesn't apply: this read is already by
  value.
- **The sorted read** groups entries by the values up to the sort field.
  Within a group they're sorted by id, because fields after the sort
  field would otherwise order them (ties go in id order, §34.1). Groups
  whose sort value is `TAG_OTHER` go last in either direction, again as
  in §34.1. A compound index holds those documents, so the scan that
  finds them for a one-field index isn't needed, and would find them
  twice.

Measured by the typed test: "the oldest 20 queued tasks" out of 3,000,
with indexes on `status` and `created` alone, reads every queued task
(about a thousand); with `(status, created)` it reads 20.

## 43.4 Unique across all fields
A unique compound index forbids two documents that are equal (as `Eq`
sees it) in every one of its fields: `(tenant, email)` lets an email
repeat across tenants, not within one. A document with a null or
missing value in any of the fields is exempt, as in SQL. Values no
comparison finds equal (arrays, NaN, ...) never collide.

Every unique check now works on tuples, one value per field
(`unique_tuples`): a one-field index has a 1-tuple per value (per
element, §42.2). So `check_unique`, `check` and `compact` handle both
kinds of index the same way.

Rejected: **nulls colliding**, as MongoDB does (a second document
without `email` in a unique `(tenant, email)` is refused there). Here a
one-field unique index already exempts null (§33.1), and the compound
one follows it.

## 43.5 File format 8
A 0.7.0 build would stop at a catalog cell of kind 3 with "unknown
catalog entry kind". That's a refusal, not a misreading, but only once
it reaches the cell. So the format is 8, and 0.7.0 says so up front. A
format-6 or -7 file opens as it is and is stamped 8 on its next page
allocation (§33.4).

## 43.6 Tests
- `index/key.rs`: compound keys sort field by field, whatever the id,
  and split back into their parts; unordered values share one encoding
  and sort last; eight long strings still fit a key, each counted as
  cut; a range under a prefix stays within it.
- `query.rs`: which index the planner picks — the one-field index as the
  first among equals, the compound one when it narrows more, none when
  the first field is free, all three fields of `(a, b, c)`. Which index
  is read in sort order, and the range read.
- `collection.rs`:
  - A randomized test on `(a, b)` and `(a, b, c)` over documents with
    every kind of `b` and an `a` that is sometimes missing, null or an
    array. 400 random filters with conditions on all three fields,
    sorts in both directions and limits must return exactly what
    sorting a scan in memory returns. That holds through inserts,
    updates that move documents, deletes and a reopen, and the compound
    plans must actually run. A limit without a sort only promises some
    matching documents, as before.
  - The typed tasks test: 20 documents read instead of about a thousand,
    the same 20 a scan finds; newest first within a range on `created`.
  - Unique `(tenant, email)`: the same pair refused, the same email in
    another tenant fine, `1` and `1.0` colliding, nulls and missing
    fields exempt, an update freeing a pair, and a build over a
    duplicate refused.
  - Strings longer than their share of a key (800 bytes in a two-field
    key): `Eq`, all four ranges and the sorted read find them. Queries
    are cut the same way as keys.
  - The paths `ensure_index` refuses; eight fields; another order being
    another index; unique against non-unique; drop; export and import.
- `catalog.rs`: compound cells, unique and not, written, reopened and
  dropped.
- Checked by breaking it on purpose, sixteen ways. Each of these fails
  a test:
  - bounds ranked by kind alone;
  - no bounds from `Eq` alone;
  - fixed fields with a gap between them;
  - sort order read with a field before the sort field left free;
  - compound indexes never considered;
  - a range on a compound index cut with one field's budget;
  - the scan for unordered values run on a compound index too;
  - unordered groups left in place when descending;
  - groups not sorted by id;
  - grouping by the whole key;
  - nulls not exempt from uniqueness;
  - `[*]` or a repeated field allowed;
  - unordered values dropped from a key;
  - strings cut with one field's budget;
  - a unique compound index exported as `field`.

  One passed at first, the range cut with one field's budget. The
  random filters rarely paired an `Eq` on the first field with a range
  over a long string. The long-strings test above covers it now.

## 43.7 Limits
- **No `[*]` in a compound index.**
- **No sort by several fields** (`sort_by(a).then_by(b)`): `Filter` still
  sorts by one field. (Added in §47, which the index serves.)
- **An OR or a NOT never uses a compound index's second field.** Each
  branch of an OR is bounded by itself (§36.3), so `(a = 1 AND b = 2) OR
  (a = 3)` does use one, branch by branch.
- **Rule-based choice**, as before (§28.4): the more fields narrowed,
  the better, whatever the data.
