# 33. Unique indexes (`collection.rs`, `catalog.rs`, `storage/file.rs`, `export.rs`)

Real and tested: an index can also be a constraint — no two documents
may have equal values in its field.

```rust
users.ensure_unique_index("email")?;   // like LiteDB's EnsureIndex(x => x.Email, true)
users.insert(ada)?;                     // ok
users.insert(also_ada)?;                // Err(Error::DuplicateValue { collection, field, id, existing })
users.unique_indexes()?;                // ["email"]
```

A unique index is an ordinary secondary index (§28) plus a check: it
answers the same queries the same way, and `indexes()` lists it too.

## 33.1 What counts as a duplicate
Two values are duplicates if a filter's `Eq` says they're equal
(`query::equal`): `1` and `1.0` collide, `0.0` and `-0.0` too; `"a"` and
`"A"` don't. So "no duplicates" means exactly "`find(field == v)` never
returns two documents" — the index and the filter agree on what equal
means.

**Null and missing values are exempt**: any number of documents may lack
the field or hold null. That's what SQL's `UNIQUE` does (Postgres,
SQLite; SQL Server allows one `NULL`). MongoDB counts missing as a
duplicate null, so an optional field there needs a partial index as
well. Here an optional field like `email` can be unique as it is.
Values no condition can match as equal — arrays, objects, binary, ids,
`NaN` — aren't in the index (§28.1) and aren't checked either.

Keys alone can't decide it. Different values can share a key's value
part: `Int`s beyond 2^53 that round to one `f64`, strings cut to the
key budget (§28.1). So `check_unique` reads every document under the
same value part and compares the real values — normally there are none,
so the check costs one B-tree range lookup.

## 33.2 When it's checked
Right before a unique index gets a new entry, in
`update_secondary_indexes`: on every insert, and on every update that
changes the field's value. An update that keeps its value (even if the
document moves, §20.3) isn't checked. A failure is
`Error::DuplicateValue`, naming both documents, and like any failed op
it rolls back the whole batch (§17.3, §22.2).

The check runs per op, against the state after the batch's earlier ops.
So a batch that swaps two documents' values fails, even though the end
state would be fine — MongoDB behaves the same. The workaround is three
ops through a temporary value. Rejected: checking once at the end of
the batch. It would need every changed key collected across the batch
and a second pass — for a case that barely comes up.

`ensure_unique_index` over existing documents checks each one as it
goes into the new index: the first duplicate fails, names two documents
that share a value, and nothing is created.

Asking for the other kind of index on a field that has one is an error
(`InvalidInput`), not a match and not a conversion — `ensure_*` means
"make sure this exists as declared", and a declaration that disagrees
with the file is a bug to surface. Dropping and recreating it is one
line. LiteDB also refuses a different definition for an existing index.

## 33.3 Catalog: a new cell kind
A unique index is a catalog cell of kind `2`, laid out like kind `1`
(§28.5). Rejected: a flags byte in the index cell. That changes the
layout of every existing index cell; a new kind leaves them alone.

## 33.4 Format 5, and reading format 4 as it is
An older build would read a kind-`2` cell as corruption ("unknown
catalog entry kind 2 — file may be corrupt") — wrong, and alarming. So
the format version is now 5, and a format-5 file is refused by older
builds with the clear "newer than this build" error.

But a format-4 file needs no migration: it *is* a valid format-5 file,
one without unique indexes. So this build opens format 4 as it is
(`COMPATIBLE_OLDER_FORMATS`). The header gets stamped 5 on its next
write — every header write stamps the current version, and the header
is written whenever a page is allocated or freed. Creating a unique
index always allocates its root page, so the batch that adds the first
kind-`2` cell also writes version 5, atomically. A file that uses
something new always says so; a file that doesn't may keep saying 4,
which is true. The first time a format change doesn't need export and
import (§21.2).

## 33.5 Export and import
`$indexes` lists a unique index as an object instead of a string:

```text
{"$collection":"users","$indexes":["age",{"field":"email","unique":true}]}
```

Files without unique indexes are unchanged, so the export format stays
version 1. An older build reading the object form stops with an error
naming the line. On import, the indexes are built after the
collection's documents (§30.3), so a file whose data breaks a unique
index fails after its documents are in — `Error::DuplicateValue`,
naming the two, and without that index.

## 33.6 Tests
- `a_unique_index_refuses_exactly_what_a_scan_finds_taken`: 300 random
  single-op batches (insert, update, delete; values of every type, with
  frequent duplicates, large ints that share a key, long strings cut to
  the same key, nulls, missing fields, documents that grow and move).
  Each must fail exactly when a scan finds another document with an
  equal non-null value, and a failed one must change nothing; at the end
  the index must still agree with a scan.
- The rules one by one: case-sensitive strings, `1` vs `1.0`, `0.0` vs
  `-0.0`, same key but different values, `NaN` and arrays unchecked,
  nulls and missing fields exempt; a batch whose second insert fails
  loses its first; an update onto a taken value; keeping a value while
  moving; freeing a value by delete; all of it again after a reopen.
- `ensure_unique_index` over duplicates creates nothing and names them;
  the other kind on an indexed field is refused.
- A format-4 file opens, stays 4 through a write that allocates nothing,
  and becomes 5 in the batch that creates its first unique index; the
  same for the header alone (`storage/file.rs`). Format 3 is still
  refused.
- Export and import keep the unique flag; a hand-written file declares
  one; a duplicate in the data fails the import; malformed `$indexes`
  entries name their line; the catalog keeps the kind across a reopen
  and drops a kind-`2` cell.
- Checked by breaking it on purpose, three ways: duplicates decided by
  key instead of value, nulls not exempt, no check on writes (only when
  building). Each fails two or three of these tests.

## 33.7 Limits
- One field per unique index; no compound uniqueness (`(tenant, email)`).
  *Done in §43.4.*
- A value swap inside one batch fails (§33.2).
- Case-insensitive uniqueness would need a case-folded key; it isn't
  there. An app that wants it stores a folded copy and indexes that.
