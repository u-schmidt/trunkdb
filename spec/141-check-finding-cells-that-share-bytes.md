# 141. `check` finding cells that share bytes (`check.rs`, `storage/slotted.rs`)

A page's own check (§54) makes sure every live slot lies inside the
cell area. It doesn't compare slots with each other, so two cells
sharing bytes got through: one document's or entry's bytes read as
part of another's. §54.2 left it out of `from_bytes`, where it would
sort the cells on every read, and named `Database::check` as the place
that can afford it. This step puts it there. No file format changed.

## 141.1 What is checked
After the collections, `check` reads once more every page owned by
something whose pages hold slots: the catalog, each collection's
primary index and data pages, each secondary index. For each it sorts
the live cells by offset and compares each with the one before; until
two overlap, that one ends furthest, so one comparison each is enough.
Two that overlap are one problem for the page:

```text
page 5 ("people"'s documents): slots 14 and 15 share bytes
```

- **Touching is not sharing:** a cell that ends where the next begins
  is how a packed page looks.
- **Tombstones** (length 0) have no bytes, and are left out.
- **One problem a page,** the first pair in offset order: a page whose
  directory is wrong is worth one look, not a line per slot.
- **Damaged pages aren't read again:** a page is owned only once it
  was read, so one whose checksum fails (§40.4) has no owner, and
  nothing to compare.
- **The header, the free list and overflow pages** have no slots, and
  aren't read again.

A cell made one byte longer, into the next, on a data page: the
document still reads, as decoding stops where the document ends, so
before this step `check` called the file clean. On an index page it
mostly breaks the entry too, and the index's own problems come with it.

## 141.2 What it costs
Each slotted page is read once more, mostly from the cache, and one
small list of its cells sorted. On 100,000 documents with two
secondary indexes, 4,577 pages, `check` took 0.50–0.56 s after and
0.52–0.53 s before: within the difference between two runs. One page
at a time, so memory doesn't grow with the file (§109).

## 141.3 Rejected
- **In `from_bytes`** (§54.2): an allocation and a sort on every read.
- **Every pair reported:** see 141.1.
- **As the documents are read:** a data page is read for each of its
  documents; the last pass reads it once.

## 141.4 Tests
- `overlapping_cells_are_found_and_touching_ones_are_not`
  (`storage/slotted.rs`): packed cells touch; the same bytes; one byte
  into the next; one cell over two; inside another cell; where a
  deleted cell was; a tombstone pointing inside a cell.
- `cells_sharing_bytes_are_found` (`check.rs`): a cell one byte longer
  on a data page, a primary index's leaf, a secondary index's and the
  catalog's page, each in a file of its own and each passing §54's
  check; found by page, owner and slots, and on the data page the only
  problem.

## 141.5 Mutations
| Mutation | Caught by |
|---|---|
| The pass never run | `cells_sharing_bytes_are_found` |
| Touching taken as sharing | both tests, and every clean `check` |
| Tombstones taken in | `overlapping_cells_are_found…`, made stronger: a tombstone inside a cell |
| Compared with the first cell, not the one before | `overlapping_cells_are_found…` |
| The catalog not slotted | `cells_sharing_bytes_are_found` |
| An index not slotted | `cells_sharing_bytes_are_found` |
| A primary index not slotted | `cells_sharing_bytes_are_found` |
| Data pages not slotted | `cells_sharing_bytes_are_found` |

Two survived at first. Tombstones taken in went unseen while every
tombstone was written as offset 0, which no cell starts before: the
test's tombstone now points inside a cell. A damaged page read again
went unseen because none is ever owned; that skip was taken out.
