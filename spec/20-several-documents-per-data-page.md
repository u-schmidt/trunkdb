# 20. Several documents per data page (`data.rs`, `storage/slotted.rs`, `catalog.rs`)

Real and tested: `Data` pages hold as many documents as fit, replacing
§11.4's one-document-per-page layout. 1000 small documents (a few
fields each) now take a 20-page file in total, index included, instead
of over 1000 pages (`small_documents_are_packed_into_few_pages`); on
the sync workload's data (§5.3, ~900 B per document) that's about eight
documents per page instead of one.

## 20.1 Where inserts go: a current data page per collection
`CollectionMeta` gained `current_data_page` (`0` = none yet) — the page a
collection's next insert tries first. If the document fits there (after
compacting the page, if needed), it goes there; otherwise a fresh page
is allocated and becomes current. The catalog cell grew to `[u64
index_root][u64 current_data_page][name]`; `Catalog::set_current_data_page`
rewrites it in place (same length) — once per filled page, not per
insert.

`data.rs` stays catalog-agnostic: `insert_record`/`update_record` take
the current page as `&mut PageId` and update it when they allocate;
`apply_write_op` persists a change through the catalog. The catalog
cache changing mid-batch is covered by §19.5's snapshot/restore already.

Considered and rejected:
- **In memory only** (forget the current page at `open`): simpler, no
  catalog change, but every open would abandon a partly filled page — for
  an app that opens the file once per run, a steady leak.
- **A free-space map** (which pages have how much room, like Postgres'
  FSM): would also reuse the holes in older pages, but it's a new
  on-disk structure to keep consistent. Not needed until deletes are
  common; see §20.4.

One collection per page, never mixed: a scan of one collection then
touches only its own pages, and dropping a collection (§37.2) frees
whole pages.

## 20.2 `SlottedPage`: compaction, slot reuse, in-place update
Three new operations, all keeping a cell's slot number — the reason
`RecordLocation` is `{ page, slot }` (§7.3) finally pays off:
- `compact` repacks the live cells against the page end, so the dead
  bytes of deleted or shrunk cells become free space; tombstoned slots
  at the end of the directory are dropped (nothing references them).
- `insert_cell_reusing_slot` fills a tombstoned slot before growing the
  directory, compacting first if the space exists but is fragmented.
  Only for `Data` pages: B-tree pages rebuild themselves in key order
  and rely on slot order, which reuse would break — so plain
  `insert_cell` is unchanged.
- `update_cell` replaces a cell's bytes: in place if they didn't grow,
  otherwise by compacting around them; `false` (page untouched) if they
  don't fit even then.

A bug the unit tests caught: `update_cell` and `insert_cell_reusing_slot`
compact while their target slot is a tombstone, and `compact` trimmed it
as a trailing tombstone — the cell was then written to a slot beyond
`slot_count`, invisible. The internal `compact_keeping(min_slots)` keeps
it.

## 20.3 Updates can move a document
§11.4's "`loc.page` never changes on update" no longer holds: a document
that grows past what its page can hold, even compacted, is deleted from
its page and inserted like a new one (usually into the current page).
`update_record` returns the new location, and `apply_write_op` re-points
the index (`remove` + `insert`; the `Index` trait needs no new method).
All staged in one batch, so a crash can't separate the move from the
index change.

A forwarding pointer at the old location (Postgres' HOT chains, MySQL's
row migration) was considered: it spares the index update, but every
later read of the moved document pays an extra page read, and a moved
document can move again. With one index, updating it is cheap and
simpler.

## 20.4 Deletes: tombstone, free the page once empty
A delete tombstones the cell. A page whose last document goes is freed
(back to the free list, for any page type) — unless it's the current
page, which the next insert will use anyway. Known limitation: a partly
emptied page that isn't current only gets its space back through
updates of its own documents; inserts don't look there (no free-space
map, §20.1). Churn-heavy workloads can leave pages half empty until a
future vacuum. *`Database::compact` (§41) packs them.*

## 20.5 Room for overflow pages
Every data cell now starts with a flags byte: `[u8 flags][16-byte
DocId][document]`. `0` means the whole document is in the cell — the
only kind written at the time. `1` was reserved for overflow, since in use (§26): the
cell will hold `[u32 total length][u64 first Overflow page]` and as much
of the document as fits. Reading it today is an `InvalidData` error, not
a misread. The page type tag `Overflow = 6` is reserved alongside it, so
adding overflow pages needs no format migration. *Done in §26 — without
the "as much as fits" part (§26.3). It did need a format version bump
after all, for §26.1's wider lengths.*

A flags byte rather than an always-present 8-byte pointer field: one
byte per document instead of nine, and room for other per-record
variants later (e.g. compression).

## 20.6 A file-format change without migration
Data cells and catalog cells both changed, so files written by 0.1.0
can't be read — they either fail to decode or decode as garbage. No
real data exists yet (that's why this is in Block A); the format
version added right after (§21.2) makes such files a clear error, and
every later format change too.

## 20.7 Test changes
`a_crash_mid_b_tree_split_loses_nothing` (§19.8) found its splitting
insert by "the dirty set grew by more than one page", which packing
broke (a plain insert now usually adds no page, but a new data page adds
two). It now counts dirty `IndexLeaf` pages. It also has to restore the
catalog cache after its trial rollback, like `write_batch` does, since
trial inserts can move `current_data_page`.
