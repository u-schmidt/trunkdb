# 75. A free-space map (`free_space.rs`, `data.rs`, `catalog.rs`)

An insert went into its collection's current data page, or a new one
(§20.1). Space a delete or a shrinking update left in an older page came
back only when an update on that same page needed it, or by `compact`
(§41.5): a collection that deleted and inserted as much kept growing.
Now each collection has a map of its data pages with room left, and an
insert that doesn't fit the current page goes into the fullest of them
it fits in, before allocating a new one.

Wholly free pages already had a list: from the header, linked through
the pages themselves (§7), as in LiteDB. This is the other half of
LiteDB's design, its lists of data pages by how full they are, done
differently (75.4).

## 75.1 The map
`FreeSpace`, one per collection, holds each remembered page with its
exact free bytes, as `reclaimable_space` counts them: free space plus
the dead bytes of deleted and shrunk cells, all of it usable, since an
insert compacts the page first when it has to (§20.1). A page is 8 KB,
so a `u16` holds it.

- **`by_free: BTreeSet<(u16, PageId)>`**, sorted by free bytes, then
  page. `find(needed)` is `range((needed, 0)..).next()`: the first page
  with at least `needed` free, which is the fullest that fits. One
  lookup, O(log n), and the page it names always has the room, which
  categories of fullness can't promise.
- **`free_of: HashMap<PageId, u16>`**, each page's current free bytes,
  so that `set` and `remove` find a page's entry in `by_free` from its
  id alone. A delete or update arrives by document, not through the
  map, and doesn't know the entry's old value; with this, `set` removes
  whatever was there, and no page is ever in the map twice.
- **What an insert needs** is the cell and its slot entry,
  `SlottedPage::space_needed`, what `has_room_for` checks.

## 75.2 What tells it
Every write that changes a data page's room, in `data.rs`:
- **`place_cell`** tries the current page, then `find`, then allocates.
  A page from the map is noted again after the insert, with what it has
  left. A page that stops being current, because a new one was
  allocated, gets in with what it has left, if that's enough (75.3).
- **`write_or_free`**, where every delete and every update that moves a
  document away ends: a page it frees is removed; any other is noted.
- **`update_record`**, a document that stays on its page: noted.
- **The current page is never in the map**: inserts try it first.

`note_room` decides: the room, not the current page, and enough (75.3):
`set`; otherwise `remove`. A dropped collection's map goes with it
(§37); a compaction starts a new catalog, with empty maps, and fills
pages in order, leaving nothing to remember (§41).

## 75.3 Enough room: two thresholds
A page gets in with at least `MIN_ROOM`, a quarter of a page (2,048
bytes), and stays until less than `MIN_ROOM_TO_STAY`, 256 bytes, is
left. Most full pages keep a few hundred bytes nothing fits into; a
page gets in only with a real hole, from deletes or from being left
behind as current, so the map stays small.

One threshold for both was the first version, and the test through a
whole collection (75.6) found it wrong: a half-emptied page took inserts
only until it was back at a quarter free, then left the map, and 1,000
inserts after 1,000 deletes allocated 24 new data pages, though the
room was there. With the second threshold, none.

## 75.4 In memory, in the catalog
- **Learned from writes, empty at open.** No format change, nothing
  read at open, no page written to keep it. The limit: space freed in
  an earlier session comes back only by `compact`. A long-running
  application reuses what it frees itself.
- **In the `Catalog`**, the in-memory one, beside its collections and
  indexes, not in any page. `transact` clones the catalog before a
  batch and puts the clone back if it fails (§19.5), so the map rolls
  back with the pages. It has to: a map that kept a page a failed batch
  had allocated, and the rollback gave back to the free list, would
  send a later insert into a page by then reused, for an index or
  another collection. `Catalog::indexes_and_free_space` hands out a
  collection's indexes and its map at once, which update and delete
  need together.
- **§57.3:** space freed by a commit isn't reused while a reader could
  still need it. Today that's at once, since no reader overlaps a
  commit; with snapshots, the map would take a freed page, or a page's
  freed room, only once no snapshot from before is open.

Rejected:
- **LiteDB's lists**, five per collection by fullness, linked through
  the data pages: a link both ways in every data page, a format
  change, and a page moving between categories rewrites its neighbours
  and the collection's list heads, up to four pages, each a whole image
  in the WAL (§19), for one insert.
- **Rebuilt at open**, by reading every data page's header: open as
  slow as the file is large.
- **Persisted now**, as PostgreSQL's free-space map: one byte per data
  page, rounded down to 32 bytes, in map pages of their own, read at
  open into the exact map. One byte, not the exact two, so the map
  changes only when a page crosses a step, not with nearly every
  insert. The way to go if restarts leave too much behind (ROADMAP.md);
  1.0 allows it later, since an older 1.x build refuses a file using
  something it lacks.
- **A `BTreeMap<u16, PageId>`**: two pages with the same free bytes
  would share a key, and the second would overwrite the first.

## 75.5 Cost
- **The clone:** `transact` now copies the maps with the catalog, once
  per batch. For 10,000 remembered pages (80 MB of data pages with
  room) that's 0.13 ms (release build, Apple M1 Pro), against about
  5 ms for a commit of one document, most of it the flush. An undo log
  instead of the clone would make it free, if a workload ever shows it.
- **The benchmark of §48**, trunkdb only, 100,000 documents, before and
  after: every row within the noise of a rerun. It deletes last and
  inserts nothing after, so its file size doesn't change either.

## 75.6 Tests
- `free_space.rs`: the fullest page that fits, exactly enough counting;
  two pages with the same room both kept; a new value replacing the old.
- `slotted.rs`: `space_needed` is what `has_room_for` checks.
- `data.rs`: a delete's room remembered, the current page never, a
  freed page forgotten; an insert filling a remembered page before
  allocating, the current page staying current; a page that stops
  being current keeping its room; a page with only a little room left
  not getting in.
- `collection.rs`: 2,000 documents, every other one deleted, 1,000
  inserted: no new data page, only index pages for the new ids; a
  failed batch that freed and allocated pages leaves the map as it was;
  a dropped collection's map gone with it. And every random workload
  that ends in `assert_consistent` (§36, §42, §68 and others) now also
  holds each collection's map against its pages: each one of its data
  pages, not the current one, with exactly the room recorded, and the
  map's two halves agreeing.
- Checked by breaking it on purpose, thirteen ways, each failing a
  test: nothing remembered; the current page let in; one threshold for
  getting in and staying, either way; a freed page kept; a page an
  insert used, or an update changed in place, not noted again; the
  room of a page no longer current lost; the map never asked; the
  roomiest page taken instead of the fullest; the slot left out of
  `space_needed`; a dropped collection's map kept; `set` leaving the
  old entry.
