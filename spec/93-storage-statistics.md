# 93. Storage statistics (`stats.rs`, `data.rs`, `index/btree.rs`, `catalog.rs`, `database.rs`, `src/bin/trunkdb.rs`)

What a file holds is easy to ask: `collections`, `count`, `indexes`.
Where its space goes was not. `file_info` said how many pages there are
and how many are free, and that was all. Which collection is large, how
large an index is beside its documents, how much room lies unused inside
data pages, whether a `compact` would give much back, whether the open
recovered a crash: a program had to read every document itself, or run
`check` and read its prose.

`Database::stats()` answers it in one read.

## 93.1 What it reports
- `file`: what `file_info` says.
- `pages: PageUse`: the file's pages by what they hold: `header`,
  `catalog`, `free`, `data`, `overflow`, `index`, and `unaccounted` for
  pages nothing claimed. They add up to `file.pages`.
- `collections: Vec<CollectionStats>`, by name: `documents`,
  `data_pages` (the current one included), `overflow_pages`,
  `document_bytes` (the documents encoded, inline and in chains),
  `free_bytes` (room in its data pages, tombstones included), the
  `primary` index and its secondary `indexes`.
- Each `IndexStats`: `fields` (`["_id"]` for the primary), `unique`,
  `sparse`, `entries` (one per element in a multikey index, §42),
  `pages`, `depth` (1 for a lone leaf) and `key_bytes`.
- `wal_bytes`: the WAL file's length at the call, `None` without one.
- `recovered: Option<Recovered>`: what the open of this handle restored
  from the WAL, `pages` and whether `in_memory` (a read-only open, §88).
  `None` after a clean open.
- `compacted_pages_estimate()`: the pages a `compact` would leave
  (93.4).

All structs are `#[non_exhaustive]`.

## 93.2 How it reads, and what it costs
On one snapshot, like `check` and `export` (§80): reads and writes go on
beside it. The catalog keeps no counts, and §93 adds none (93.6), so
`stats` reads:

- every page of every index once: `BTreeIndex::pages` for pages, a walk
  down the first children for the depth, the forward walk (§49) for
  entries and key bytes. The primary index's entries are the documents'
  locations;
- every data page holding a document once, taking each cell's length
  and the page's `reclaimable_space`, decoding no document. Documents
  in id order lie mostly page after page, so the last page read is kept
  while its documents come;
- no overflow page: a chain's pages follow from its length, which the
  cell holds. `check` is what follows the links;
- the catalog's pages, and the free list's count from the header.

On 100,000 documents with two indexes, 35.6 MB, release build: `stats`
35 ms, `check` 220 ms, `export` 193 ms, `compact` 935 ms.

`wal_bytes` is the WAL file's metadata at the call. `recovered` is kept
by the open (`Recovered::of` the pending images, on `Shared`) and costs
nothing.

## 93.3 A damaged file
`stats` is not `check`. A page it has to read and can't, a checksum that
doesn't match or a page of the wrong kind, fails the call with `Io` of
kind `InvalidData` (damage, §92.3): numbers that silently left part of a
collection out would be worse than none. A page nothing claimed is
counted in `unaccounted` and the call succeeds: a leak doesn't make the
other numbers wrong. Saying what is wrong stays `check`'s job.

## 93.4 What a compaction would leave
`compact` (§41) copies every collection by name: its documents in id
order through `insert_record`, then each index from its sorted keys
through `insert`. Both fill only appended-to pages, which lose exactly a
slot and a cell per insert. So the file it builds follows from the
sizes alone, and `stats` works it out alongside the read, exactly,
without building anything:

- **Data pages** (`data::PackedBuild`): `place_cell`'s decisions over
  the cells' lengths, in id order. The current page if it has room,
  else the fullest page in the free-space map (§75) with room, else a
  new one. A page left behind with a quarter page of room goes into the
  map, as `note_room` decides. It keeps the room of the current page
  and of the pages in the map only. Overflow chains are the same pages
  again.
- **Indexes** (`index::SortedBuild`): `insert` over the keys' lengths,
  ascending. A full rightmost leaf stays full and the new key starts the
  next one (§41); a full branch splits by bytes and promotes its middle
  entry; a root split adds a level. It keeps one branch page's key
  lengths per level.
- **Catalog** (`Catalog::rebuilt_pages`): the cells in the order
  `compact` creates them, each on the first page with room, as
  `append_cell` puts them.
- Plus the header. Never more than the file has now, as `compact`
  reports a file it doesn't make smaller.

The tests hold it against `compact`'s `pages_after` on six shapes of
file, and it is the same, page for page; on the 100,000 documents above
it said 3,967 and `compact` made 3,967. Should `compact` or
`insert_record` change how they pack, these tests are where it shows.

## 93.5 The command
`trunkdb info` prints, under the format line, the pages by kind, what a
`compact` would leave, the WAL's size and any recovery; and under each
collection's line, its documents' bytes, data and overflow pages, free
room and index pages. The lines it printed before are unchanged. Where
the numbers can't be read, it says so in one line, points to `trunkdb
check`, and prints the rest as before.

## 93.6 Rejected
- **Counters in the catalog**, kept by every commit: `stats` would cost
  nothing, but every commit would pay, the catalog's format would
  change, and the numbers could drift from the pages and need their own
  check. Reading is cheap enough (93.2).
- **Only in `CheckReport`**: `check` costs six times as much, and the
  numbers are wanted more often than a check.
- **A dry-run `compact`** for the size: it builds the new image in
  memory, 1.2× the new file (§90, §91.7). The simulation gives the same
  number in a few pages of memory.
- **Counts from the free-space map (§75)**: empty after an open, it
  knows only the pages writes have touched.
- **A cheaper call that skips the data pages**: not needed at these
  costs; possible later.
- **Per-document sizes** (largest documents, a histogram): a query's job
  more than a summary's; possible later.

## 93.7 Tests
`stats.rs`, on six shapes: documents of every size with a plain, a
unique, a sparse, a compound, a nested and a multikey index, deletes,
shrinking updates and a dropped collection; long keys in an index three
levels deep; overflowing documents only; empty collections with indexes;
sizes that leave pages a quarter empty, so the packing goes back to them
through the map; 300 collections with long and short names, some
dropped, so the catalog spans pages with gaps.
- The estimate is `compact`'s `pages_after`, and for the compacted file
  its own size.
- Every number against a reading of its own: documents against `count`
  and the primary index's entries; `document_bytes` against the
  documents re-encoded; data and overflow pages against
  `data::collection_pages`, which `check` uses; each secondary index's
  entries and key bytes against `index_keys` of every document;
  `PageUse` adding up to the file, `unaccounted` 0 where `check` passes.
- Options and depth of each index; an index on an array of tags has more
  entries than documents; empty indexes are one page deep.
- Deleting every other document of a range leaves the data pages and
  adds their room to `free_bytes`; deleting a whole range frees pages.
- A leaked page is `unaccounted` 1; a damaged data page fails with
  `InvalidData`.
- Read-only: the same numbers, the file unchanged, `wal_bytes` `None`
  without a WAL.

`database.rs`: after each of three crash points (§88.4), `recovered` is
the WAL's page images, in memory for a read-only open, then written
back by the open that writes, and `None` at the open after it.

`tests/cli.rs`: `info` prints the new lines, the old ones unchanged.

Mutations, each caught: chain pages not counted; the current data page
left out; free room without the tombstones; depth off by one; `recovered`
on a clean open; a read-only recovery not in memory; the estimate
without overflow pages; a leaf split at the middle; a branch split
without promoting; no free-space map in the packing; the catalog only
on its last page; free pages left out of the sum; the secondary indexes
left out of the estimate. One survives: a page that stays in the map
below `MIN_ROOM_TO_STAY`, which changes the count only when a cell small
enough for that last room comes; none of the shapes has one.

## 93.8 Limits
- A read of every used page per call: the cost grows with the file.
- The estimate holds for the packing `compact` and `insert_record` do
  now; a change to either changes it, and its tests say so.
- No per-document sizes (93.6).
- An index on a field that holds arrays, without `[*]`, has no entry for
  them (§42.1): `stats` reports what the index has, not what a caller
  may have meant.
