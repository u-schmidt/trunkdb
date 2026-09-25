# 41. Compaction (`compact.rs`, `storage/memory.rs`, `storage/file.rs`, `index/btree.rs`)

Real and tested: `Database::compact` rebuilds the database into as few
pages as it needs, and cuts the file to that length. `trunkdb compact`
does the same from the command line.

```rust
let compacted = db.compact()?;   // Compacted { pages_before: 1380, pages_after: 828 }
```

```text
$ trunkdb compact app.trunkdb
compacted: 1380 → 828 pages, 4.3 MB smaller
$ trunkdb compact app.trunkdb
already compact: 828 pages
```

## 41.1 Where the space went
Three kinds of waste built up, and none of them ever went back to the
file system:
- **Free pages.** Pages emptied by deletes, dropped collections and
  freed overflow chains are reused, but the file never got shorter.
- **Half-empty data pages.** Inserts only go to a collection's current
  page (§20.4), so space freed on other pages stays empty until their
  own documents grow into it.
- **Half-empty index leaves.** A full leaf split in the middle. Keys
  that arrive in order therefore left every leaf half full, and that is
  every primary index, since ids are UUIDv7 in time order. Deletes don't
  merge leaves either.

Measured on 20,000 documents with two indexes, after half of them were
deleted and a quarter grown: 1,380 pages, and 828 once compacted.

## 41.2 A rebuild, written as one batch
`compact` is one write batch (`transact`):
1. **Rebuild into memory** (`MemoryStore`), collection by collection,
   in name order. Documents are copied in id order, so data pages fill
   one after another and the primary index grows at its end. Each
   secondary index's keys are collected along the way, sorted, and then
   inserted in order.
2. **Stage the result as the whole file.** `FileStore::replace_all`
   stages it: pages 1 to n, a page count of n, an empty free list.
3. **Log, write back, cut.** The batch goes through the WAL like any
   other. `write_back` then cuts the file to the new page count.

Crash safety is the WAL's (§19). A crash before the record is complete
leaves the old file. A crash after it leaves the new one: `open` writes
it back, and `restore_pages` cuts the file too, since a crash can come
between a write-back and its cut.

Details:
- If the rebuild isn't smaller than the file, nothing is written.
- Documents keep their ids. Each collection's last rebuilt data page
  becomes its current one, so the next insert fills it.
- A unique index is checked as it's rebuilt: neighbors in sort order
  with the same value part are compared by value (as §33 does). A file
  whose unique index holds a duplicate, damage `check` would report,
  isn't compacted into another one: `DuplicateValue`, and nothing
  changes.
- `MemoryStore::free_page` is an error. A page freed in the image would
  reach the file with no owner, and nothing a rebuild runs frees one.

Rejected:
- **Moving pages in place**, as SQLite's incremental `auto_vacuum`
  does. Moving a page means rewriting every pointer to it: a branch
  entry, a leaf's sibling link, index entries for a data page, an
  overflow chain's links, catalog roots. That needs a map from each page
  to what points at it; SQLite keeps pointer-map pages for this. Too
  much machinery for what it gains here.
- **Rebuilding into a second file and renaming it over the first.** It
  wouldn't need memory for the image. But the rename happens outside the
  file lock (§21.1), and Windows can't replace a locked, open file at
  all. The old file's WAL would have to be dealt with too. As one batch,
  compaction keeps the lock, the file handle and the WAL.
- **Export and import (§30) into a new file.** That already works, as
  two commands. It has the rename's drawbacks, and it goes through JSON.
- **Compacting automatically**, on open or when free pages pass some
  share. The caller knows when a pause is acceptable, and `file_info`
  shows the free pages to decide by. It could come later as an option.
- **Merging half-empty leaves on delete** (a full B-tree rebalance).
  It only helps indexes: not data pages, not the file's length.

## 41.3 Index leaves fill when keys come in order
A leaf split had one rule: split in the middle, by bytes (§28.2). Now,
when the new key goes past the end of the rightmost leaf, only it moves
to the new page, and the old page stays full. SQLite ("quick balance")
and Postgres (rightmost-page splits) do the same. Only at the right edge:
in-order keys never come back to a leaf once they've moved past it,
while in the middle of a tree later keys land on both sides.

It serves two purposes:
- **The rebuild:** its sorted keys now fill every leaf but the last.
- **Everyday inserts:** ids are UUIDv7 in time order, so the primary
  index always grows at its right edge. Its leaves used to end up half
  full, and now end up full. The same goes for secondary indexes on
  values that only grow, such as timestamps.

Measured on the same workload as §41.1:

| | after the inserts | compacted | the same documents inserted fresh |
|---|---|---|---|
| middle split only | 1,451 pages | 956 | 922 |
| with the append split | 1,380 | 828 | 886 |

The cost: a full leaf that later gets a key in its middle splits right
away, for instance when several processes create ids slightly out of
order. Branch pages still split in the middle; they're a few percent of
an index.

## 41.4 Tests
- `compact.rs`, all on a churned database: three collections plus a
  dropped one, one of them emptied; unique, nested and mostly-missing
  indexed fields; overflow documents; deletes, and documents grown and
  shrunk.
  - Compacting keeps every document with its id, every index and unique
    flag, and what indexed queries find; the file is at least 30%
    smaller, `check` is clean, and writes go on as before (a unique
    index still refusing a duplicate). The same holds after reopening,
    with the file exactly the page count long.
  - The result is no larger than the same documents inserted into a new
    file in id order.
  - A compact file is left alone, byte for byte, and a write-back set
    up to fail is never reached.
  - A secondary index whose values arrived in random order comes out
    with every leaf full but the last.
  - An empty database compacts to its header and catalog: 2 pages.
  - A duplicate in a unique index stops the compaction; the file is
    unchanged.
  - A write-back that fails twice partway poisons the database. The
    next open completes the compaction from the WAL: cut to the new
    length, everything still there.
  - The next small document goes into the room on the last data page.
  - Ids stay what they were.
- `storage/`:
  - `MemoryStore`'s ids, bounds and refusals;
  - `replace_all` makes the pages the whole file and cuts it, and
    rolled back it changes nothing;
  - `restore_pages` cuts the file to its page count, also when it's
    just one page too long.
- `index/btree.rs`: keys in order fill every leaf but the last; in
  reverse order they still split in the middle.
- Every randomized test (§39.3) now also compacts whenever it checks
  the file, compares every document before and after, and goes on
  writing to the compacted file.
- `tests/cli.rs`: `trunkdb compact` makes a file more than ten times
  smaller; a second run says "already compact" and leaves the file as
  it is; `check` passes.
- Checked by breaking it on purpose, fourteen ways. Each of these fails
  a test:
  - the append split turned off, or applied wherever the new key goes;
  - no cut in `write_back` or `restore_pages`, or a cut one page late;
  - the free list kept by `replace_all`;
  - a compact file rewritten anyway;
  - the rebuilt catalog not taken over;
  - no current data page set;
  - no unique check, one that counts nulls, or one that remembers
    nothing;
  - the index keys not sorted.

  Three of them passed at first and got tests of their own: the cut one
  page late, the rewrite of a compact file, and the unsorted keys.

  One passes and stays: the append split on any leaf, not just the
  rightmost. Both rules gave the same 170 pages on an index over a
  five-valued status field. A key appended to one value's range is
  rarely its leaf's last, because the next value's keys follow on the
  same page. The rightmost leaf is where keys in order are certain to
  keep coming.

## 41.5 Limits
- **Memory:** the whole new file, twice (the image and its WAL record),
  and the WAL grows to that size while it runs. That's fine for files
  of megabytes to a few hundred; a file of gigabytes would want the WAL
  record streamed.
- **The write lock** is held throughout: 0.4 s for the 828-page file
  above (release build).
- **Only when asked for.** Half-empty data pages build up again between
  compactions; a free-space map (§20.1) is still open.
