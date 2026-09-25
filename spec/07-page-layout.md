# 7. Page layout (`storage/`)

`FileStore` (implementing `PageStore`) and the generic `SlottedPage`
primitive are now real and tested — the first genuinely finished layer.

## 7.1 Page size: 8192 bytes
Matches LiteDB's own page size, deliberately — it keeps numbers comparable
to the reference implementation later. Also a better fit than the
originally-considered 4096 for a document DB specifically: documents are
whole JSON-ish objects (hundreds of bytes to a few KB), so fewer, larger
pages means less fragmentation across page boundaries; and once a real
B-tree exists, larger pages hold more keys per node, giving higher fanout
and a shallower tree.

## 7.2 Header page (page 0) and the free list
Page 0 is reserved and positionally identified (never tagged). It holds an
8-byte magic (`TRUNKDB1`, catches opening a corrupt or foreign file early),
`page_size` (validated against the build's `PAGE_SIZE` constant on open),
`page_count`, `free_list_head`, and — since §21.2 — a `u32` format
version.

Freed pages form an on-disk linked list — the same technique SQLite's
freelist trunk pages use: a freed page's own first bytes hold the next
free page's id (`0` doubles safely as "no free page," since page 0 is
never itself freeable). `allocate_page` pops the free-list head if one
exists, otherwise grows the file by one page. Content of a freshly
allocated page — whether reused from the free list or newly grown — is
unspecified until the caller writes to it; allocation does not zero reused
pages (matching how most page allocators behave).

`write_page`/`free_page` on the header page (id 0) are rejected through
the generic `PageStore` interface — the header is cached in memory by
`FileStore` and managed internally, so writing it through the generic path
would desync that cache from disk.

## 7.3 Page type tags and `SlottedPage`
Every page except the header carries a 1-byte type tag as its first byte
(`Free`, `Catalog`, `Data`, `IndexLeaf`, `IndexBranch`, `Overflow` —
the last added in §26) — cheap corruption detection (e.g.
`allocate_page` verifies a popped free-list page is actually tagged
`Free`), and lets `read_page`'s raw bytes be self-describing.

`SlottedPage` is one generic primitive — a slot directory growing forward
from a small header, cells packed backward from the end of the page, slots
identified by index rather than byte offset — that backs three different
page kinds:
- **Catalog** pages (§9, real and tested): cells are collection registry
  entries.
- **IndexLeaf** pages (§10, real and tested): cells are `(key,
  RecordLocation)` entries.
- **Data** pages (§11, §20, real and tested): cells are a serialized
  document prefixed with a flags byte and its own `DocId`, as many per
  page as fit.

This is the same layout PostgreSQL heap pages and SQLite B-tree pages use,
and it's *why* `RecordLocation` is `{ page, slot }` rather than `{ page,
offset }` — a cell can be relocated within its page later (a future
compaction pass) without invalidating anything that references it, since
only the slot directory entry changes.

## 7.4 Why a persisted index instead of rebuild-on-scan
Originally considered: keep the index purely in memory (as `InMemoryIndex`
already is) and rebuild it by scanning all data pages on `Database::open`.
Rejected in favor of persisting the index for real, because of what it
saves later: a real B-tree, when it replaces the naive linear index, would
otherwise have to invent its own page-persistence and catalog-wiring from
scratch. Building that plumbing once now — even though v0's index stays
algorithmically naive (a flat, linked chain of `IndexLeaf` pages, O(n)
lookup) — means the *leaf page format* doesn't change when real B-tree
branch nodes are added on top later. Concretely, what's fixed now vs. what
changes later:
- **Fixed now, unchanged later**: the `IndexLeaf` page format itself (via
  `SlottedPage`), the `Index` trait's signature (`insert`/`remove`/
  `lookup`/`scan` — a flat leaf chain and a real multi-level tree both
  implement the same four operations), and the catalog storing a single
  `root_page: PageId` per collection (today it always happens to point
  directly at a leaf page — a degenerate, height-1 tree; later it
  sometimes points at a branch page instead).
- **Added later, not yet built**: branch/internal node pages (`(key,
  child_page_pointer)` cells, using the same `SlottedPage` primitive with a
  different cell payload), split-on-overflow logic, and O(log n)
  navigation. Leaf pages stay linked to each other via `next_page` even
  after branch nodes exist — real B+trees (e.g. InnoDB) keep this, since it
  makes range scans fast without re-descending the tree.

One consequence: because the index is the persisted source of truth for
"which documents exist," a delete must be a real tombstone/removal on the
data page — not just dropped from the index — or a future rebuild-style
operation would resurrect it.

Update: the branch/internal node pages and split-on-overflow logic
anticipated above are now built — see §10.
