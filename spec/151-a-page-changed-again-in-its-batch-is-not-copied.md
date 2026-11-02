# 151. A page changed again in its batch is not copied (`storage/file.rs`, `storage/scratch.rs`, `index/btree.rs`, `data.rs`)

§150's profile gave 11% of a batched insert's CPU to `make_mut`
copying pages. §67 made a change copy a page once, not twice, but only
once per change: a page the batch changed already is held in its dirty
set (§19), a read of it shares those bytes (§64), and the next change
copies all of them before writing one cell, as the first change did.
In the benchmark's batches a page is changed about six times on
average, and each change copied the whole page.

Counted with the benchmark's inserts (§48), 100,000 documents, 1,000 a
commit, three indexes, every page change counted once:

| | page changes | of them copied |
|---|---:|---:|
| before | 497,769 | every one |
| after | 497,769 | 80,827 |

The 78,600 that still copy are a page's first change in its batch,
which has to: the page before it is the committed one, which the cache
and any snapshot read (§80). The other 2,227 are the header and pages
written whole.

## 151.1 The change
- **`PageStore::change_page(id, page, change)`:** `write_page` of
  `page` after `change(&mut page)`, for a page just read to change it.
  By default it is just that, for every store but `FileStore` and
  `ScratchStore`.
- **`FileStore`:** when the batch holds a copy of the page already,
  it drops that copy first, so `page` holds the only reference to its
  bytes and `change` writes them in place; the page goes back where
  the copy was. Nothing between can fail, so the batch is never
  without the page. Its `before`, the page as the batch found it (§80),
  was taken with its first change, as it was. A page the batch hasn't
  changed yet goes through `write_page` as before, copied once.
- **`ScratchStore`**, the file a compaction builds its image in
  (§108), the same for the pages it holds in memory: dropped, changed,
  written back.
- **Inserts and removes:** a B-tree insert into a leaf or a branch with
  room, a removal from a leaf, a document put into the current page or
  one from the free-space map (§75), and a document deleted from a page
  that keeps others.
- **`SlottedPage::fits(len)` and `fits_reusing_slot(len)`:** whether
  `insert_cell_at` or `insert_cell_reusing_slot` takes a cell of
  `len` bytes, said before it is tried, since `change_page` writes the
  page whatever `change` does. `insert_cell_at` uses `fits` itself.
  A page that doesn't fit is split or left as before.

No file format, no API, no change to what any call returns. The cell a
delete leaves is zeroed as before; a page left without documents is
freed as before.

## 151.2 Measured
On the machine of §123, the build before the change (`e672657`) and
after, one after the other.

Inserts alone with flushes skipped (§150.2's program), two rounds of
100,000 in each of three runs:

| | before | after |
|---|---:|---:|
| inserts | 49.4–56.8k/s, median 55.1k/s | 45.0–67.3k/s, median 66.7k/s |

1.21×: more than the 11% the profile gave the copies, as a copy also
moved the page's 4 KB through the cache and allocated and freed them.
The 45.0k/s was the first round after the build, once.

The §48 benchmark, trunkdb alone, three runs of each:

| | before | after |
|---|---|---|
| insert, 1000 per commit | 36k/s, 36k/s, 37k/s | 39k/s, 41k/s, 40k/s |
| update, 1000 per commit | 14k/s, 13k/s, 13k/s | 14k/s, 14k/s, 14k/s |
| delete, 1000 per commit | 28k/s, 27k/s, 27k/s | 27k/s, 30k/s, 31k/s |
| compact | 0.92 s, 0.78 s, 0.78 s | 0.74 s, 0.75 s, 0.73 s |

- **Inserts 1.11×** by the medians, deletes 1.11×, compaction 1.05×.
  The CPU is a part of a commit, the flush and the WAL's writes the
  rest (§149). An earlier round of three, before `ScratchStore` had
  the change, agreed: inserts 36k/s against 40k/s, deletes 28k/s
  against 30k/s; single commits 4.9–5.2 ms and `get` 1.9–2.4 µs
  either way.
- **Updates unchanged:** an update rewrites a cell that may move, so
  it isn't known beforehand whether it fits; it writes as before. A
  benchmark's update is one in about ten documents, so its pages are
  changed less often in a batch anyway.
- **Compaction a little:** most of its time is reading every document
  and writing the image out, not changing pages.
- **Reads unchanged**, as nothing they do changed.

The full benchmark, all four stores, three runs: inserts 38k/s against
SQLite's 40k/s on the same day, 0.95× (0.88× in §150.2), updates level,
deletes 29k/s against 19k/s; the README's and DESIGN's tables are these
runs.

## 151.3 Rejected
- **Taking the page out of the batch to change it, and putting it back
  with `write_page`:** two calls, between which the batch lacks the
  page. A read of it there would read the committed page, and an error
  there (an allocation for a split) would lose the change.
- **A closure given the store's own page, read inside it:** the B-tree
  reads the page to find where the key goes and whether it is a leaf
  before it knows it will change it; reading it again would check its
  layout a second time (§66).
- **Keeping the layout mark through a change** (the other 7% of §150's
  profile): next, measured on its own.

## 151.4 Tests
- `a_page_changed_again_in_its_batch_is_changed_in_place`: a page's
  first change in a batch copies it, the second changes that copy in
  place (the same bytes in memory); a page held by a reader is copied,
  and the reader keeps it as it was; a snapshot open before the batch
  reads the page as the batch found it; a rollback after two changes
  takes both back.
- `a_held_page_is_changed_in_place`: the same for `ScratchStore`, a
  page it holds changed in place, one a reader holds copied.
- `fits_says_what_the_insert_will_do`: on 200 pages filled to every
  level, with cells removed, tombstoned or both, for every length up
  to a quarter page, `fits` and `fits_reusing_slot` agree with what
  `insert_cell_at` and `insert_cell_reusing_slot` do.
- The full test suite, clippy, the MSRV build (1.89).

## 151.5 Mutations
Each run against the library's tests:

- the batch's copy kept while the page changes: caught (the bytes
  are not the same);
- the changed page not put back into the batch: caught;
- a page the batch hasn't changed taken as one it has, with no
  `before` taken: caught (the snapshot reads the change);
- the header page changed in place like any other: caught, after the
  test was given a change of the header to refuse;
- `fits` taking room only after compacting, or one byte short: caught;
- `fits_reusing_slot` counting a new slot for a reused one, or not
  looking for tombstones: caught, after the test tried lengths around
  each page's free and reclaimable bytes, not only up to a quarter
  page, which left both standing;
- a leaf or a branch taken as having room: caught;
- a page's last document deleted without freeing the page, a
  deletion's room or an insert's not noted in the free-space map:
  caught;
- the current data page taken as having room: caught;
- `ScratchStore` keeping its page while it changes: caught.
