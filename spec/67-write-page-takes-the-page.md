# 67. `write_page` takes the page (`storage/mod.rs`, `storage/file.rs`, `storage/memory.rs`)

A change to a page was copied twice (§64.1): once by `make_mut`, on the
first change, and again by `write_page`, which took a `&[u8]` and made
its own `Page` of it for the batch's dirty set. Now `write_page` takes
the `Page` and keeps it.

```rust
let mut leaf = SlottedPage::from_bytes(store.read_page(id)?)?;  // shared
leaf.insert_cell_at(slot, &entry);                              // copied once
store.write_page(id, leaf.into_page())?;                        // kept as it is
```

## 67.1 What changed
- **`PageStore::write_page(id, Page)`.** Callers hand over
  `SlottedPage::into_page()`; the few that build bytes themselves (an
  overflow chain, a free page, the header) convert them with
  `Page::from`, one copy, as before.
- **`FileStore` keeps the page:** in the dirty set while staging; in
  the cache and the file outside a batch, where it now writes the file
  first and caches the page only if that worked.
- **`MemoryStore`**, the image a compaction builds (§41), holds `Page`s
  too: a read shares, a write keeps what it's given, and `replace_all`
  takes the pages as they are.
- **A B-tree root that splits** moves its entries to a new left page by
  writing the root's page there: shared now, not copied, since the root
  gets a new page right after.

Rejected:
- **Keeping `&[u8]` and adding `write_page_owned`:** two ways to write a
  page, one of them slower, and every caller would have to pick. There
  are two stores and no outside implementers since §60.
- **`impl Into<Page>` as the parameter:** `PageStore` is used as `dyn
  PageStore`, which can't have generic methods.

## 67.2 Measured
The main benchmark (§48), M1 Pro, this change against §66, run in
alternating order, four times each:

| | §66 | §67 |
|---|---|---|
| compact | 1.33–1.36 s | 1.17–1.21 s |
| insert 100,000, 1,000 per commit | 22k–24k/s | 23k–24k/s |
| update 9,528, 1,000 per commit | 9.3k–11k/s | 9.1k–11k/s |

Compaction is 11% faster: it builds a whole new file in a
`MemoryStore`, reading and writing every page. Batched writes are
within the noise: a batch's time goes to checkpoints and the WAL's
page images (ROADMAP.md), not to the copy. One commit is the flush,
about 5 ms.

## 67.3 Limits
- **The WAL still copies every changed page**, whole, into its record,
  and the checkpoint writes each through a buffer that appends the
  checksum. Both are I/O-bound.

## 67.4 Tests
- `file.rs`: a page written in a batch is read back as the same bytes
  in memory, and still after the checkpoint caches it; so is a page
  written outside a batch.
- `memory.rs`: a page written to the image is read back shared.
- Every other test writes pages through the new signature.
- Checked by breaking it on purpose, five ways. Each of these fails a
  test:
  - a staged write, or one outside a batch, copying the page;
  - the image copying on a write, or on a read;
  - a page shorter than a page accepted.
