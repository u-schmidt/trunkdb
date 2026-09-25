# 37. `delete_many` and dropping a collection (`collection.rs`, `database.rs`, `catalog.rs`, `data.rs`)

Real and tested:

```rust
runs.delete_many(Filter::new().lt("started_at", cutoff))?;              // -> how many
runs.delete_many(Filter::new().sort_asc("started_at").limit(100))?;   // the oldest 100
db.drop_collection("runs")?;                                          // -> false if there was none
```

## 37.1 `delete_many`: what `find` would return
It deletes exactly the documents `find(filter)` would return — sort and
limit included, so a retention rule like "the oldest 100" is one call —
and returns how many. It uses the same code as `find` (`find_in`) and
every plan with it: an index range, a union for an OR (§36.3), or a
walk in sort order that stops at the limit (§34.2).

Lookup and deletes run in one batch under one write lock, like `upsert`
(§29.3): all of them go or none, and no write can slip in between
finding a document and deleting it. The alternative a caller had —
`find_with_ids`, then a batch of deletes — could delete a document that
an update in between had made no longer match. A collection that doesn't
exist has nothing to delete and isn't created.

Rejected: ignoring `sort` and `limit`, as MongoDB's `deleteMany` does. A
filter meaning one thing to `find` and another to `delete_many` is a
trap; and the limited form is useful.

`find` now also filters under its read lock — before, it dropped the
lock before checking candidates. That's CPU work only, and sharing one
function is worth more.

## 37.2 Dropping a collection
`Database::drop_collection(name)` deletes the collection's documents,
indexes and catalog entries, and frees all their pages for reuse, in one
atomic batch. A collection owns:
- its data pages — a data page never holds two collections (§20), so
  they're all its own: every page one of its documents is on, plus its
  current data page, which stays even when empty (§20) and so may hold
  none of them;
- the overflow chains of its large documents (§26);
- its primary index tree and each secondary index tree (§28).

All of it is read before anything is freed, since freeing overwrites a
page (`data::free_collection_pages`, `BTreeIndex::free_all`). The
catalog drops the collection's cell and its index cells
(`Catalog::drop_collection`); catalog pages themselves stay, like after
`drop_index`.

Rejected: deleting the documents one by one (`delete_many` of
everything, then the empty collection). Each delete rewrites its page
and every index; dropping only reads the pages it frees. Rejected:
`Collection::drop()` instead — `drop` is what Rust calls a destructor,
and on a handle it reads like releasing the handle. LiteDB puts it on
the database too (`DropCollection`).

A `Collection` handle to a dropped collection stays usable: it only
holds a name, and the next write creates the collection again, empty.

## 37.3 Tests
- `dropping_a_collection_frees_every_page_it_had`: three collections
  interleaved in the file, the middle one with 300 documents of very
  different sizes, one in overflow pages, a plain and a unique index,
  and deletes that empty some pages — the current one included. After
  dropping it, filling a new collection with the same data must not grow
  the file by a single page; the other two must be unchanged, also after
  a reopen, and the new one must answer through its indexes. Checked by
  breaking it on purpose, four ways: not freeing the empty current page,
  the overflow chains, the secondary index trees, or the primary tree —
  each fails it. (The current page's case failed to fail at first: the
  test data left documents on it. The test now empties it.)
- `delete_many_deletes_what_find_would_return`: a range, the youngest
  five by sort and limit, an OR, nothing matching, a missing collection
  (not created); unique values free again afterwards, index queries
  consistent.
- `delete_many_matches_find_through_random_filters`: 40 rounds of
  inserts and a random nested filter, sometimes sorted and limited:
  each call deletes exactly what `find` returned just before; at the
  end the indexes agree with a scan on every plan (§36.4). Checked by
  making `delete_many` ignore the limit — both tests fail.

## 37.4 Limits
- A large `delete_many` or drop is one big batch: every changed page is
  staged in memory and written to the WAL (§19.9), like `ensure_index`.
- Freed pages are reused, but the file doesn't shrink (compaction,
  ROADMAP.md).
- No `update_many` yet — it came next (§38).
