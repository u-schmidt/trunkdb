# 52. B-tree pages changed in place (`storage/slotted.rs`, `index/btree.rs`)

§48.4 put batched writes 4–7× behind the other stores and blamed page
work; §51.4 confirmed that flushes weren't it. This section is the
profile that the roadmap asked for first, and the fix it pointed at.

## 52.1 The profile
A scratch program inserting documents like the benchmark's, 1,000 per
batch with its three indexes, under macOS's built-in `sample` (a
release build with `debug = true`; no Xcode, no install). The rate
against the number of indexes already located it: 32,000 documents a
second with none, 15,500 with one, 7,500 with three. Each index
roughly halved it.

Of 16,765 samples over 300,000 documents:
- 70% in `insert_into`, the B-tree insert.
- About 48% of all samples in `malloc` and `free`, and 12% in
  `memmove`. The fsyncs were 15%.
- Inside the insert: collecting a page's cells into a `Vec` of owned
  keys, once in each branch on the way down (2,360 samples) and once in
  the leaf (2,198), then rebuilding the leaf from them (2,066).

So each insert copied every key on the page into its own allocation
(about 100 per leaf) and wrote a new page from them. It did that even
for branches, which change only when a child splits, which is about
one insert in a hundred.

## 52.2 The fix
Slot order is key order on a B-tree page, so an insert can move the slot
directory up by 4 bytes and write one cell:
- `SlottedPage::insert_cell_at(slot, cell)` compacts first if the bytes
  are there but scattered. If they aren't there at all, it returns
  `false` and leaves the page untouched.
- `SlottedPage::remove_cell_at(slot)` takes the slot out and zeroes
  the cell's bytes; the gap is reclaimed by the next compaction.
- **Leaf insert:** find the first key not below the new one, then
  insert in place. Only a full leaf is rebuilt from its entries, and
  that code stays as it was: the split by bytes (§28.2) and the
  in-order append (§41).
- **Branch insert:** route like `find_child`, without copying
  anything. If the child split, the separator goes in at the child's
  slot and the entry after it is rerouted to the new right half. That
  update is an in-place `update_cell`, since the new entry has the same
  length. Only a full branch is collected and split.
- **Leaf remove:** `remove_cell_at` instead of a tombstone.

**No format change.** The pages hold the same cells in the same order;
only the bytes between them differ. Pages written by older versions can
still hold tombstones where a key was removed. They stay harmless:
- `iter_cells` skips them;
- a compaction before an insert keeps every slot, trailing ones
  included, so the slot the insert counted to stays valid;
- a full leaf's rebuild drops them.

**Removed keys no longer linger.** A remove used to tombstone the slot,
and the key's bytes stayed on disk until the next insert into that leaf
rebuilt it. Index keys hold field values (§28.1), so zeroing them is
the same rule `compact` follows for documents (§20).

Rejected:
- **Binary search for the position.** Scanning about 100 cells with a
  `memcmp` each no longer shows in the profile; the allocations were
  the cost, not the comparisons.
- **A page kept decoded in memory between inserts.** That would be a
  second representation of every page to keep in step with its bytes,
  for work that is now a memmove of the slot directory.

## 52.3 Measured
The scratch program, 100,000 documents:

| indexes | before | after |
|---|---:|---:|
| none | 32k/s | 87k/s |
| one | 15.5k/s | 57k/s |
| three | 7.5k/s | 22k/s |

The benchmark, trunkdb alone, against the table in §51.5:

| | before | after |
|---|---:|---:|
| insert 100000, 1000 per commit | 7,518/s | 23k/s |
| update 9528, 1000 per commit | 6,187/s | 9,061/s |
| delete 10000, 1000 per commit | 14k/s | 15k/s |
| compact | about 7 s | 1.00 s |

Compaction gained the most for its size, because its rebuild is
ordinary B-tree inserts (§41). Reads are unchanged.

The full run, with the others (a slower run for all four than §51.5's;
the small figures move between runs):

| | trunkdb | SQLite | redb | sled |
|---|---:|---:|---:|---:|
| insert 100000, 1000 per commit | 20k/s | 35k/s | 37k/s | 28k/s |
| insert 1000, one per commit | 5502 µs | 4660 µs | 4952 µs | 9360 µs |
| get by id | 5.5 µs | 21.0 µs | 1.9 µs | 2.1 µs |
| find tenant == x (1000 docs) | 2.25 ms | 2.64 ms | 1.00 ms | 1.78 ms |
| status == x, oldest 20 | 48.5 µs | 23.8 µs | 16.8 µs | 21.7 µs |
| scan: tries > 7, unindexed | 156 ms | 49 ms | 49 ms | 71 ms |
| update 9528, 1000 per commit | 8917/s | 12k/s | 17k/s | 21k/s |
| delete 10000, 1000 per commit | 15k/s | 18k/s | 23k/s | 35k/s |
| compact | 1.02 s | 0.20 s | 0.43 s | — |

Batched inserts went from 5× behind SQLite and redb to under 2×.

## 52.4 What's left: the checkpoint
Profiled again, at 600,000 documents: page work is 29% of the samples,
the WAL (writing and flushing whole pages) 23%, and the checkpoint 48%.
A batch of 1,000 documents with `created` in random order changes more
than 1,000 distinct index leaves once the index is large. That crosses
`CHECKPOINT_PAGES` (§51) on nearly every commit, so each page is
written twice and flushed twice. Trying other thresholds in the scratch
program:

| `CHECKPOINT_PAGES` | 100,000 documents | 300,000 documents |
|---|---:|---:|
| 1,000 (now) | 22k/s | 13k/s |
| 4,000 | 30k/s | 22k/s |
| 16,000 | 33k/s | 20k/s |

A larger threshold lets a page rewritten by several batches be written
back once. It costs memory (4,000 pages is 32 MB of unwritten pages),
a larger WAL, and a longer checkpoint for the commit that crosses the
threshold. That trade is left open (ROADMAP.md) rather than decided with
this change.

## 52.5 Tests
- `storage/slotted.rs`:
  - `insert_cell_at` at the front, in between and at the end keeps
    slot order.
  - Room made by removals is used after a compaction. With no room,
    the page is left byte for byte as it was.
  - A trailing tombstone keeps its slot through that compaction, so an
    insert at the end still lands at the end.
  - `remove_cell_at` closes the gap, zeroes the cell and clears the old
    last directory entry.
- `index/btree.rs`:
  - 2,000 keys inserted out of order, then every seventh removed: none
    of the removed keys' bytes is on any page of the tree, and the scan
    holds exactly the rest.
  - A leaf with every third slot tombstoned, as older versions left it,
    takes 400 inserts through splits, and the scan is exactly the
    expected keys in order.
- The randomized test of §28 (inserts and removes of keys up to
  `MAX_KEY_LEN` against a `BTreeMap`) runs unchanged. It now covers
  holes, compaction and splits of pages changed in place.
- Checked by breaking it on purpose, thirteen ways. Each of these fails
  a test:
  - no compaction before an insert, or one that drops trailing
    tombstones;
  - a room check without the slot entry;
  - the slot directory not moved up, or not moved down;
  - a removed cell not zeroed, or the old last slot not cleared;
  - removal by tombstone;
  - a leaf position that is wrong, or at the front instead of the end;
  - a branch routing equal keys left;
  - the wrong slot rerouted, or the rightmost child not rerouted.
