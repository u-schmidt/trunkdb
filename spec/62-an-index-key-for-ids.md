# 62. An index key for ids (`index/key.rs`, `query.rs`, `collection.rs`, `index/btree.rs`, `storage/file.rs`, `database.rs`)

Since §59, a struct can hold a reference to another document as a
`DocId` field, and a filter on it matches (§59.5). But ids had no index
key: an index on `owner` was created without complaint, held no entry
for any document whose `owner` is an id, and the planner never used it
for one, so `eq("owner", id)` read every document (§59.6). Now ids have
a key, and an index on a reference works like any other.

```rust
#[derive(Serialize, Deserialize)]
struct Car { owner: DocId, seats: i64 }

cars.ensure_index("owner")?;
let owned = cars.find(Filter::new().eq("owner", owner_id))?;   // reads its range
```

## 62.1 The key
A new type tag, `TAG_ID` (4), followed by the id's 16 bytes: fixed
length, so the encoding stays prefix-free (§43.2), and in the order
`Filter` compares ids in, their bytes (§59.5), which for UUIDv7 is the
order they were made in. So everything an index does for a string it
now does for an id:
- `eq`, `lt`, `lte`, `gt`, `gte` read a range of the index;
- a unique index refuses a second document with the same id in it;
- `drivers[*]` holds a document once per id in the array (§42);
- a compound index `(owner, seats)` finds one owner's cars in order of
  seats (§43.3), where an id used to be one "other" value (`TAG_OTHER`)
  like an array.

## 62.2 Sorting by an id
An index key's order has to be a sort's order (§34.1), or reading an
index in order would give something else than sorting in memory. Ids
used to sort among the values nothing orders: after everything, in
either direction, as equals. Now they sort **after strings, by when
they were made**, reversed for `Desc`; the values nothing orders
(arrays, objects, binary, NaN) still come last. That changes what a sort
on a field holding ids returns, which no application is likely to have
relied on: ids couldn't be put in a typed struct's field before §59,
in 0.12.0.

## 62.3 Older files: rebuilt at open, stamped 10
An index built before this has no entry for an id, in a one-field
index, or an "other" where a compound one now has the id. Read by this
build, it would answer wrong: `eq("owner", id)` would find nothing. So a
file must not be read with such an index, and `Database::open` makes
sure of it (`collection::key_ids_in_old_indexes`):
- **When:** the file says a format before 10, and has secondary
  indexes. Without any, nothing can be wrong.
- **What:** each collection with indexes is read once. An index that a
  document holds an id in (`holds_id`: in the indexed field, an element
  of it for `[*]`, or any field of a compound index) is emptied, keeping
  its root page, which is where the catalog finds it (`BTreeIndex::clear`),
  and filled again from the documents. The others stay as they are.
- **In one batch:** the one `open` already runs, so a crash in the middle
  leaves the file as it was, and an error rolls it back.
- **Stamped 10:** the header says 10 afterwards, even if no index needed
  rebuilding, so the check runs once. A file without secondary indexes
  isn't stamped at open; its first write stamps it, as every write does
  (§59.3).

**Why format 10, when no page changes shape.** A build before it knows
nothing of id keys. If it deleted a document whose `owner` is an id, it
would compute no key for it and leave the entry behind, pointing to a
document that no longer exists. A file with id keys has to be refused
by such a build, which is what the format version is for (§21.2): 0.12.0
reads 10 as newer than itself and says so. This build still opens 6, 7,
8 and 9, which are valid format-10 files once their indexes are checked.

Measured on 100,000 documents, release build, indexes on `seats` and
`owner`:

| First open of a format-9 file | |
|---|---|
| no document holds an id: checked, nothing rebuilt | 100 ms |
| `owner` holds ids: that index rebuilt | 490 ms |
| every open after that | 9 ms |

Rejected:
- **Rebuilding every index of an older file.** Simpler, but it writes
  every index into one batch, all of it held in memory until the
  commit (§51), for files that almost never need it: ids in fields only
  became usual with §59.
- **Export and import** as the way up, as for 4 and 5 (§30): nothing
  about the documents changes, and indexes are derived data.
- **A flag per index** in the catalog instead of the header's version:
  it would let an old index be rebuilt lazily, on first use, but an
  older build doesn't know the flag, so the version would have to
  change anyway to keep that build out.

## 62.4 Limits
- **A unique index an older file has two documents with the same id
  in** fails the open: ids never collided before, so the index held
  them both. `Database::open` returns `DuplicateValue`, and the file is
  left as it was. The way out is an older build (0.12.0): open the file
  with it, drop the index or change one of the documents, and open it
  with this build again.
- **A range on ids reads its bound too.** `lt("owner", id)` reads `id`'s
  own documents and drops them in the check, as every range does,
  because different strings or large numbers can share a key
  (`key::range_for`, §28.1). An id's key is exact, so its bound could
  be left out; nothing has needed it.
- **The first open of an older file is slower**, by a read of every
  document of every indexed collection (62.3). Opening such a file only
  to read it stamps it too; after that, 0.12.0 refuses it.

## 62.5 Tests
- `index/key.rs`: an id's key is its tag and 16 bytes, after every
  string, before "other", in the order ids compare; a compound key's
  parts split one off whole; `eq` and `lt` ranges hold the right ids
  and no string. Ids moved out of the list of values without a key.
- `query.rs`: ids sort after strings, by value, reversed for `Desc`, and
  before arrays, which come last both ways.
- `collection.rs`:
  - an index on a `DocId` field: an `eq` reads just its 30 of 300
    documents, a range its own plus its bound, a sort walks the index
    and reads 5 for a limit of 5, a compound `(owner, seats)` finds one
    owner's cars by seats reading 3, a unique index refuses the same id
    twice, `drivers[*]` finds the trips an id drove; the file checks out;
  - a file from before (indexes rebuilt the old way by a test helper,
    the header set back to 9): this build answers wrong before, and
    `check` sees it; opening it rebuilds exactly the two indexes with
    ids, keeps the index list's order, stamps 10, checks out, answers
    through both indexes, and the next open reads no document; indexes
    without ids are checked and stamped too; a file without secondary
    indexes stays 9 until its first write;
  - a unique index on an older file with a shared id fails the open,
    twice, and leaves the file's bytes as they were;
  - the random documents the planner tests compare against a scan now
    hold ids too, in every plan, sorted and not.
- `tests/cli.rs`: `trunkdb info` says format 10.
- Checked by breaking it on purpose, seventeen ways. Each of these fails
  a test, but one:
  - ids without a key, or with nulls' tag; an id part of a compound key
    split wrong; ids sorting as unordered, or unordered values with ids;
  - every open checking; a file without indexes stamped; no stamp;
  - no index, or every index, taken for stale; ids missed in a one-field
    or a compound index;
  - `clear` leaking pages, or keeping the old root; a rebuild without
    the unique check; format 9 refused.
  - The one that passes: the tag 0xFE instead of 4, which orders ids
    the same way, after strings and before "other".
