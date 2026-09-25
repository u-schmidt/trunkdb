# 54. Checking a page when it is read (`storage/slotted.rs`)

A page's checksum (§40) proves that its bytes are the ones that were
written, not that they make sense. `SlottedPage::from_bytes` checked only
the page's size and its type byte, so a page with a wrong slot got
through, and the first `get_cell` on it panicked with an index out of
range: the host application crashed where it should have got an error.
`from_bytes` now checks the layout, and returns `InvalidData` naming
what is wrong. No file or WAL format changed. The 18 places that call
it already pass the error on.

## 54.1 What is checked
In this order, since each check relies on the one before:
1. **The type is one of the slotted ones:** `Catalog`, `Data`,
   `IndexLeaf`, `IndexBranch` (`PageType::is_slotted`). `Free`,
   `Overflow` and the header have other layouts; read as slots, their
   bytes would give a slot count and a `data_start` that mean nothing, and
   the error would say "corrupt" where "wrong kind of page" is the
   truth. A page of zeros has type byte 0, the header's, and is
   rejected here now.
2. **The directory doesn't run into the cells:** `data_start` is at least
   where the directory ends, `13 + slot_count × 4`. Equal is a full page
   and allowed. This also bounds `slot_count`, which comes from a `u16`.
3. **Every live slot lies inside the cell area:** its offset is at least
   `data_start`, and offset plus length is at most `USABLE_PAGE_SIZE`.
   A tombstone (length 0) has no cell and is skipped. The sums are made
   in `usize`, where two `u16` values can't overflow.

The slots are each checked on their own, against the same two limits.
They are not compared with each other: cells aren't in slot order (a
B-tree page's slot order is its key order, §52; a data page's cells sit
wherever they were placed, and compaction moves them).

## 54.2 What is not checked: overlapping cells
Two live cells that share bytes pass every check above. They can't make
`get_cell` panic, but one document's bytes would be read as part of
another. Finding them means sorting the live cells by offset on every
read: an allocation and O(n log n) on the path that §52's profile showed
to be allocation-bound, for damage that a checksum already catches when
it comes from the disk. Rejected for `from_bytes`. It would fit in
`Database::check` (§39), which can afford to be slow; listed as open in
ROADMAP.md. The doc comment on `from_bytes` says so.

## 54.3 What it costs
`from_bytes` runs on every B-tree page of every read, so the benchmark
of §48 was run before and after on the same machine, twice each, at 100,000
documents. Inserts, updates, deletes and the unindexed scan were within
the difference between two runs of the same code (for example 18k/s
against 17–18k/s for inserts), so the check stays eager. If a later
change made it show, the fallback is to check a slot in `get_cell`, one
at a time as it is read.

## 54.4 Tests
- In `storage/slotted.rs`, each damages one field of a valid page and
  expects `InvalidData` with the reason in the message:
  - a slot with its offset past the page, and one that starts inside
    the page and ends past it (`(8000, 200)`);
  - a slot reaching into the directory;
  - a cell starting before `data_start`, with a `data_start` that is
    itself legal: the only page that only this check can reject;
  - a `Header` page;
  - and, in the other direction, a page with a cell at `(8000, 100)`
    and a page with a tombstone both load.
- The other tests, which read pages of every kind, run unchanged: the
  check rejects none of what the code itself writes. That includes
  `survives_bytes_roundtrip`, whose first cell ends exactly at the
  page's end.
- Checked by breaking it on purpose, nine ways. Each of these fails a
  test:
  - no type check;
  - no directory check, or one that rejects an exactly full page;
  - no upper bound, or one that rejects a cell ending at the page end;
  - the lower bound one byte too strict, or the loop missing the
    last slot;
  - tombstones not skipped;
  - **no lower bound at all, which failed nothing** until the test for a
    cell before `data_start` was added. The test of a slot reaching into
    the directory doesn't reach it: `data_start` is inside the directory
    there too, and the directory check rejects the page first.
- Only the wider suite, not a test of its own, notices the directory
  check that rejects a full page (7 tests) and tombstones not being
  skipped, until the tombstone test was added.

## 54.5 Limits
- Overlapping cells (§54.2).
- The `Overflow` pages of §26 have their own layout and check (`data.rs`);
  this covers only the slotted ones.
- A page can still be wrong in what its cells mean: a B-tree page with
  its keys out of order, or a cell that doesn't decode. That is what
  `check` (§39) looks for.
