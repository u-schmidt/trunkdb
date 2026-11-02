# 153. A grown cell made room for in place (`storage/slotted.rs`)

Updates were the batched write furthest behind (§48): 14k/s, against
SQLite's 15k/s, redb's 20k/s and sled's 24k/s. A profile of the
benchmark's updates alone: 100,000 tasks inserted, then rounds of the
benchmark's update, about 9,500 random tasks each with a new status, a
new `created` and a title 9 bytes longer, 1,000 a commit, every flush
skipped (`--cfg fuzzing`, §55.1), sampled for 15 s (11,896 samples):

| | share of the CPU |
|---|---:|
| the updates themselves (`apply_write_op`) | 52% |
| ↳ index entries out and in (`update_secondary_indexes`) | 22% |
| ↳ the document rewritten in its page (`update_record`) | 16% |
| &nbsp;&nbsp;↳ the page compacted (`compact_keeping`) | 13% |
| checkpoints | 16% |
| writing the WAL record | 13% |

With flushes, a batch of 1,000 took about 62 ms, 42 of them the CPU:
three times the CPU of 1,000 inserts. Counted with the same program:
**83–96% of updates compacted their page**, and 2–14% moved the
document to another one.

Inserts fill a data page until the next document doesn't fit (§75), so
its free space, between the slot directory and the cells, is a few
bytes up to one document's worth. A document 9 bytes longer didn't fit
where it was, and `update_cell` wrote it as a new cell: it gave up the
old one and needed the whole new length free, about 150 bytes, which a
full page doesn't have. So it compacted the page, which copied every
live cell into its own allocation, about 40 on a data page, and back.
The next update on the page did the same: the page's free space was
back to its few bytes.

The rest of an update's cost is the pages a batch writes: about 2,160
pages for 1,000 updates, against 810 for 1,000 inserts, as each update
changes its data page and the old and new leaves of two indexes, at
random places. That is the WAL's and the checkpoint's ground (§149,
§123), left for later: §149's range records would help updates less
than inserts, as most of their pages are a page's first change after a
checkpoint, which is logged whole.

## 153.1 The change
- **A cell grown in place:** where the page's free space has the bytes
  a cell grew by, the bytes from the start of the cells up to the
  cell's own move down by that much, the slots of the cells among them
  with them, and the cell takes its old place and the bytes freed
  below it. On a data page those are half the cells on average, moved
  by one `copy_within`, with nothing allocated; the slot directory is
  read once to move their slots. A cell's slot is still its identity,
  so nothing outside the page sees a difference.
- **Compaction in place:** the live cells sorted by offset, highest
  first, and each moved up against the one above it, so none is
  written over before it moves; one allocation for the list of slots,
  none per cell. A page whose cells share bytes, which only a damaged
  file holds (§54.2, found by `check`, §141), is compacted as before,
  every cell copied out whole first, so each keeps the bytes it is
  read with.
- **Where neither has room** the update compacts and places the cell,
  or returns `false` for its caller to move the document, as before.

No file format, no API, no change to what any call returns: the cells
of a page are where they were or elsewhere in it, as a compaction
always left them.

## 153.2 Measured
On the machine of §123, the build before the change (`286d2a5`) and
after, one after the other.

The update program above, three rounds of about 9,500 in each of three
runs:

| | before | after |
|---|---:|---:|
| updates, flushes skipped | 19.8–29.5k/s, median 24.3k/s | 17.5–32.8k/s, median 29.7k/s |
| updates, with flushes | 15.9–18.5k/s, median 17.5k/s | 18.1–20.8k/s, median 19.8k/s |

1.22× of the CPU, and 1.13× with flushes, where the disk's share is
the same. More than the 13% the profile gave the compaction: it also
freed and allocated about 40 small blocks an update, which the
profile's `malloc` and `free` lines held.

Inserts alone with flushes skipped (§150.2's program), unchanged:
66.3–72.4k/s before, 67.0–72.4k/s after.

The §48 benchmark, trunkdb alone, three runs of each (a first round,
on a busier machine, varied too much to read: one run had every number
slow):

| | before | after |
|---|---|---|
| update, 1000 per commit | 13k/s, 13k/s, 13k/s | 15k/s, 16k/s, 15k/s |
| insert, 1000 per commit | 41k/s, 38k/s, 40k/s | 40k/s, 41k/s, 39k/s |
| delete, 1000 per commit | 30k/s, 31k/s, 32k/s | 31k/s, 29k/s, 32k/s |
| compact | 0.65 s, 0.62 s, 0.63 s | 0.62 s, 0.63 s, 0.62 s |
| get by id | 1.7–1.9 µs | 1.6–1.8 µs |

- **Updates 1.15×,** every run after above every run before.
- **Inserts, deletes, compaction and reads unchanged:** their pages
  compact rarely, and a cell they write never grows.

The profile again, after: the document rewritten in its page 8% of
the CPU instead of 16%, its compaction 2% instead of 13%; the index
entries (25%), the checkpoints (17%) and the WAL record (14%) are now
most of what is left.

The full benchmark, all four stores, three runs: updates 15k/s against
SQLite's 14k/s, redb's 18k/s and sled's 23k/s; inserts 42k/s against
40k/s; the README's and DESIGN's tables are these runs.

## 153.3 Rejected
- **Room kept free on every data page** (PostgreSQL's `fillfactor`):
  updates in place without moving any cell, but a file larger for
  every workload, inserts alone too; a choice for the application,
  which no one has asked for.
- **Moving the cells above the grown one instead:** the cells are
  packed from the page's end down, so the ones below are those next to
  the free space; moving those above would need room above, which
  there never is.
- **Compaction into a second page buffer:** one copy of 8 KB, but an
  allocation and a copy back; in place moves each live byte once.

## 153.4 Tests
- `cells_grown_and_compacted_in_place_keep_every_cell`: 300 pages of
  random inserts, updates that shrink a cell, grow it by a little or by
  a lot, deletes and compactions, each page filled to the brim, against
  a model of every slot's bytes. After every step the cells are the
  model's, the layout checks, no two cells share a byte, the gap is
  zero after a compaction, and an update is refused exactly when the
  page lacks the room.
- `a_page_whose_cells_overlap_compacts_each_cell_whole`: a damaged page
  whose cells share bytes compacts with every cell keeping its bytes.
- The full test suite, clippy, the MSRV build (1.89).

## 153.5 Mutations
Each run against the library's tests:

- the cells below not moved down, their slots not moved, or the slot
  of the cell at the cells' very start left out: caught;
- the cells' start not moved, or the grown cell written where it was:
  caught;
- a cell grown with one byte less free than it needs: caught;
- compaction not keeping a moved cell's slot, leaving the cells' start
  where it was, taking tombstones for cells, or not zeroing the gap:
  caught;
- overlapping cells taken as apart: **survived at first**, as the test
  had one cell start inside another, which moving in place survives;
  caught once it had a cell stretched over the one above it with a
  third below, which moving in place writes over;
- compaction sorting the lowest cell first: **survives, and is
  equivalent.** The check for cells sharing bytes then fails on every
  page of two cells or more, and the page is compacted the old way,
  copied out whole: the same cells, slower.

Not run, as nothing a test sees changes, only the time: the grown cell
never made room for in place (it compacts, as before), and every page
compacted the old way.
