# trunkdb — design

How trunkdb works today, one subsystem at a time. This file describes
the current state; the reasons, the rejected alternatives and the
measurements are in [the spec](spec/README.md), one numbered section per
step, linked from each part below. What's done and what's open is in
[ROADMAP.md](ROADMAP.md).

When this file and a spec section disagree, the newer spec section
describes the code. This file should be updated with every change that
alters what it says.

## 1. What it is

An embedded, single-file, schema-less document database: open a file,
get collections of documents, no tables, joins or schema migrations. It
takes the idea of [LiteDB](https://www.litedb.org/) outside .NET, as an
independent design rather than a port [§1](spec/01-motivation.md).

It's a learning project first and a personal tool second [§2](spec/02-feasibility-read.md). Two
real workloads shaped what it needs: a time-series workload of many
small documents queried by time, and a large-document desktop workload;
a small sync app replaced MongoDB with it [§5](spec/05-reference-workloads.md).

Deliberately not there: a cost-based planner, joins, aggregates,
several writers at once, several processes on one file, encryption and
schema validation [§6](spec/06-v0-feature-set.md).

## 2. Layers

Every layer sits behind a small trait, so a naive version could be
swapped for a real one without reshaping what's above it [§3](spec/03-architecture-layers-and-whats-real-vs-fakeable.md). All of
them are real now; `InMemoryIndex` remains, for tests.

| Layer | Trait | Implementation | Code |
|---|---|---|---|
| Pages | `PageStore` | `FileStore`: the file, checksums, the page cache, staged and committed pages | `storage/` |
| Documents | — | the `Document` enum, its byte encoding, the serde bridge, tagged JSON | `document.rs`, `serde_bridge.rs`, `json.rs`, `decode.rs` |
| Ids | `IdGenerator` | `UuidV7Generator` | `id.rs` |
| Indexes | `Index` | `BTreeIndex`, for the primary index and every secondary one | `index/` |
| Transactions | `TransactionManager` | `GlobalLockTxnManager`: one batch at a time, all or nothing | `txn/` |
| Durability | `Durability` | `WalDurability`: a page-image write-ahead log | `durability/` |
| Queries | — | `Filter`, `Condition`, `Sort`; a fixed-rule planner | `query.rs` |
| API | — | `Database`, `Collection<T>`, `Batch`, `Cursor` | `database.rs`, `collection.rs`, `batch.rs`, `cursor.rs` |

`PageStore` is the one seam that isn't meant to be faked: everything
above it works in pages, never in file offsets. Index and data code
take `&mut dyn PageStore` and `&mut Catalog` as plain parameters and
hold no handles of their own, so the database's lock is the one place
they come from [§8](spec/08-ownership-one-refcell-boundary-not-one-per-component.md) [§27](spec/27-a-thread-safe-cloneable-database-handle.md).

## 3. Files on disk

| File | What |
|---|---|
| `app.trunkdb` | the database: pages of 8 KB |
| `app.trunkdb.wal` | the write-ahead log: whole page images, one record per commit |

The database file holds an OS advisory lock (`flock`, `LockFileEx`)
while it's open, so a second `open` fails at once instead of reading a
file another process is writing [§21](spec/21-an-exclusive-file-lock-and-a-format-version.md). Within one process, a handle is
cloned, not opened twice.

Between checkpoints the newest pages are only in the WAL. The main file
is complete on its own after `db.checkpoint()`, and after the last
handle is dropped [§51](spec/51-one-flush-per-commit.md).

## 4. Pages

Every page is 8,192 bytes, and its last 4 bytes are a CRC-32C of the
page's id and the rest of its bytes [§7](spec/07-page-layout.md) [§40](spec/40-page-checksums.md). A read checks it, so a
damaged page is an error, not garbage; the checksum over the id also
catches a page written to the wrong place. Above `FileStore`, a page is
8,188 bytes: nothing else ever sees the checksum.

| Page | Layout |
|---|---|
| 0: header | magic `TRUNKDB1`, page size, page count, head of the free list, format version |
| 1: catalog | slotted: one cell per collection and per index |
| data | slotted: documents of one collection |
| index leaf, index branch | slotted: B-tree entries, in key order |
| overflow | one run of a large document's bytes, and the next page |
| free | the next free page |

**Slotted pages** have a 13-byte header (type, next page, slot count,
start of the cell area), then a directory of slots growing forward, 4
bytes each, then the cells packed backward from the end [§7](spec/07-page-layout.md) [§20.2](spec/20-several-documents-per-data-page.md).
A cell is named by its slot, so it can move within its page without
anything that points at it changing. On data pages a slot is an
identity and a deleted one is reused; on B-tree pages slot order is key
order, and a cell is inserted or removed in place by moving the
directory [§52](spec/52-b-tree-pages-changed-in-place.md). A page's layout is checked when it's read: a slot
outside the cell area is an error saying what is wrong, not a panic [§54](spec/54-checking-a-page-when-it-is-read.md).
The same holds for everything decoded from a page: bytes that run out,
a count past what's left, a link back to a page already seen or past
the file's end are `InvalidData` errors. Decoders read through
`decode.rs`, and B-tree pages through one function that checks every
cell is long enough for its entry [§55](spec/55-fuzzing.md).

**Freed pages** go onto a free list threaded through the pages
themselves, and allocation takes from it first [§7](spec/07-page-layout.md). The file doesn't
shrink until a compaction [§41](spec/41-compaction.md).

**The format version** is 9 [§21.2](spec/21-an-exclusive-file-lock-and-a-format-version.md) [§59](spec/59-a-struct-carries-its-own-id.md). A build opens its own format and
the older ones that are valid files of it (6, 7 and 8), and the first
page a batch writes stamps them with the current one; it refuses newer
ones, and older ones that need converting. Moving between formats is
export and import [§30](spec/30-export-and-import-as-json-lines.md). The history is in the comment on
`FORMAT_VERSION` in `storage/file.rs`.

## 5. Durability: WAL, commit and checkpoint

A write batch goes through five steps under the write lock
(`Database::transact`) [§19.3](spec/19-durability-take-two-a-page-image-wal.md) [§51](spec/51-one-flush-per-commit.md):

1. **Stage.** Every page write stays in memory, in the store's staging
   area.
2. **Apply** the batch. On any error, the staged pages and the catalog
   cache are dropped: nothing reached the file, so nothing needs
   undoing [§19.5](spec/19-durability-take-two-a-page-image-wal.md).
3. **Log** every changed page, whole, to the WAL as one record, and
   `fsync` (`F_FULLFSYNC` on macOS). This is the one flush a commit
   waits for.
4. **Commit.** The pages become the newest committed ones. Reads find
   them in memory; the main file isn't touched.
5. **Checkpoint**, once `checkpoint_pages` or more committed pages
   wait (default 1,000, an `OpenOptions` setting) [§53](spec/53-a-configurable-checkpoint-threshold.md): write them to
   the main file, `fsync`, truncate the WAL.

A crash before step 3 finishes leaves the state before the batch; a
crash after it leaves a complete WAL record, which the next `open`
writes back before anything reads the file. Page images are
idempotent, so writing one back twice is harmless [§19](spec/19-durability-take-two-a-page-image-wal.md). A WAL page
past the file's end, as the header before it has it, is refused before
anything is written [§55](spec/55-fuzzing.md). A checkpoint
that fails keeps its pages waiting, and the next one tries again.

Only if the WAL can't be written *and* can't be truncated is the
batch's fate unknown; the database then refuses every call with
`Error::Poisoned` until it's reopened, which recovers from the WAL
[§19](spec/19-durability-take-two-a-page-image-wal.md) [§27](spec/27-a-thread-safe-cloneable-database-handle.md).

Costs: memory for the waiting pages (8 MB at the default), and a WAL
that grows with every commit, which the next open after a crash reads
back whole [§53.3](spec/53-a-configurable-checkpoint-threshold.md). The first WAL logged operations, not pages. It was
replaced because a crash between the several page writes of one
operation, a B-tree split say, left a file that replaying the operation
couldn't repair; page images need no structure-specific recovery at all
[§16](spec/16-durability-a-real-write-ahead-log.md) [§19.1](spec/19-durability-take-two-a-page-image-wal.md).

## 6. Documents and data pages

**The value type** is `Document`: `Null`, `Bool`, `Int`, `Float`,
`String`, `Binary`, `Array`, `Object`, `Id` [§4.1](spec/04-key-decisions-and-rationale.md) [§11](spec/11-document-encoding.md). It's what
everything below the API works in, like LiteDB's `BsonValue`.
`Collection<T>` converts any `T: Serialize + DeserializeOwned` to and
from it through serde, and delegates to `Collection<Document>`: one
code path, not two [§13](spec/13-the-serde-bridge.md). Each stored object gets its id as an `_id`
field [§18](spec/18-merging-id-into-the-untyped-path.md), and a struct takes it with `#[serde(rename = "_id")] id:
Option<DocId>`: filled in on every read, used on insert if `Some`, made
if `None` [§59](spec/59-a-struct-carries-its-own-id.md). The id is stored once, in the cell: an object is
encoded without its `_id`, and every read puts the cell's id there,
first, so there's no second copy that could say something else. `DocId`
serializes as a marked newtype the bridge stores as `Document::Id`, and
other formats as a UUID string. `find_with_ids` [§23](spec/23-find-with-ids.md) is deprecated, to be
removed before 1.0.

**Nesting** is limited to 64 levels, counted as MongoDB counts (the
document is the first, each object or array inside adds one): deeper
writes are refused, so a damaged document can't take the decoder off
the end of the stack, and every export imports again [§56](spec/56-a-limit-on-nesting.md).

**Ids** are UUIDv7 by default: 16 bytes, starting with a timestamp, so
ids made in sequence land at the end of the primary index instead of
all over it [§4.2](spec/04-key-decisions-and-rationale.md).

**Data pages** hold one collection's documents, several per page, each
cell being `[flags][id][encoded document]` [§20](spec/20-several-documents-per-data-page.md). The id is stored in
the cell because nothing else maps a location back to its document, and
a scan or a rebuild needs that. A collection's next insert tries its
current data page first and allocates a new one when that's full; there
is no free-space map, so space freed in older pages is reused only
within them, or by a compaction.

**Large documents** keep only a pointer in their cell: the length and
the first of a chain of overflow pages [§26](spec/26-overflow-pages-and-u32-lengths.md). Lengths are `u32`.

**The catalog** (page 1) holds a cell per collection (its primary
index root, its current data page, its name) and one per index (its
fields, root and flags: unique, sparse) [§9](spec/09-catalog.md) [§28](spec/28-secondary-indexes.md) [§44](spec/44-sparse-indexes.md). It's read at
open and kept in memory; a batch that fails restores the copy it
started with.

## 7. Indexes

**One B-tree** implements every index [§10](spec/10-index.md). Keys are byte strings
compared byte by byte, so the tree knows nothing about documents
[§28.1](spec/28-secondary-indexes.md). Leaves are linked, so a range read walks along them, and a
read in sort order is a lazy walk, forward or backward, that stops when
the caller does [§49](spec/49-a-lazy-b-tree-walk.md). Inserts change a page in place, and only a full
page is rebuilt when it splits; a split is by bytes, since keys differ
in length [§28.2](spec/28-secondary-indexes.md) [§52](spec/52-b-tree-pages-changed-in-place.md). Keys arriving in order fill their leaves
instead of leaving them half empty [§41](spec/41-compaction.md). A key is at most a quarter
page, so every page holds at least four.

**The primary index** maps each id, 16 bytes, to its document's
location (page and slot).

**Secondary indexes** (`ensure_index`) map an encoded value followed by
the id, so every key is unique and removing a document names exactly
one entry [§28](spec/28-secondary-indexes.md). The value encoding sorts in the same order `Filter`
compares values in [§34.1](spec/34-sorting-through-an-index.md). They cover:

- **nested paths:** `address.city` [§31](spec/31-nested-field-paths.md);
- **null and missing fields**, held as null, so `x == null` finds both
  through the index [§32](spec/32-null-and-missing-fields.md);
- **unique:** a second equal value fails the whole batch with
  `DuplicateValue`; nulls never count [§33](spec/33-unique-indexes.md);
- **multikey paths with `[*]`:** `tags[*]` holds a document once per
  element [§42](spec/42-array-conditions-and-multikey-indexes.md);
- **compound:** `(status, created)` sorts by the first value, then the
  second, and so on; the encoding is prefix-free [§43](spec/43-compound-indexes.md);
- **sparse:** no entry for null or missing values, for fields few
  documents have [§44](spec/44-sparse-indexes.md).

An index is built and kept up to date in the same batch as the
documents it covers, so it can't disagree with them after a crash.
`Database::check` confirms it anyway [§39](spec/39-the-trunkdb-command-and-database-check.md).

## 8. Queries

A `Filter` holds a tree of conditions, a list of sort keys and a limit
[§35](spec/35-a-filter-builder.md) [§36](spec/36-or-not-and-nesting.md) [§47](spec/47-sorting-by-several-fields.md):

- **comparisons** (`eq`, `ne`, `lt`, `lte`, `gt`, `gte`) and a
  case-insensitive `contains` [§25](spec/25-op-contains-case-insensitive-substring-match.md);
- **`all`, `any_of` and `not`**, also as `&`, `|` and `!` [§36](spec/36-or-not-and-nesting.md);
- **`exists` and `missing`**, telling a stored null from an absent field;
  `size` for array lengths [§45](spec/45-exists-and-array-size.md);
- **conditions on array elements:** any element with `[*]` [§42](spec/42-array-conditions-and-multikey-indexes.md), or
  one element meeting several conditions with `elem_match` [§46](spec/46-conditions-on-one-element-elem-match.md).

Values of different types never compare equal or ordered, except
numbers: an `Int` and a `Float` compare by exact value [§34](spec/34-sorting-through-an-index.md).

**The planner** follows fixed rules, not costs [§28.4](spec/28-secondary-indexes.md). `explain` shows
what it picked (`QueryPlan`):

- **`Scan`:** read every document of the collection and check it.
- **`Index`:** read one index range, then check each document against
  the whole filter. An `Eq` beats an OR of indexed branches, which beats
  a range; bounds on more fields beat fewer [§43.3](spec/43-compound-indexes.md). `ne`, `contains`
  and `not` never use an index. A sparse index is used only when the
  filter rules nulls out [§44](spec/44-sparse-indexes.md).
- **`IndexUnion`:** one range per branch of an OR, each document read
  once [§36.3](spec/36-or-not-and-nesting.md).
- **`IndexOrder`:** with a sort and a limit, walk the index on the
  first sort key and stop at the limit [§34.2](spec/34-sorting-through-an-index.md) [§49](spec/49-a-lazy-b-tree-walk.md). A compound index
  whose leading fields an `Eq` fixes gives both the filter and the order
  (`status == "Queued"`, newest first, on `(status, created)`). Sort
  keys the index doesn't serve are sorted in memory within each run of
  equal served keys [§47](spec/47-sorting-by-several-fields.md).

Every document is checked against the whole filter after an index
narrows the set, so an index only ever decides how much is read, never
what matches.

## 9. Concurrency

`Database` is a cheap, cloneable, `Send + Sync` handle; every clone,
`Collection` and `Batch` shares one open database [§27](spec/27-a-thread-safe-cloneable-database-handle.md). The state sits
behind one `RwLock`:

- **reads** (`get`, `find`, `count`, `cursor`) share the read lock, so
  they could run in parallel; the page cache's mutex still serializes
  them (see below);
- **a write batch** holds the write lock from staging to its
  checkpoint, so a reader sees a batch entirely or not at all, and
  readers wait for it.

The page cache is behind its own mutex inside the store, so parallel
readers take turns only for the cache lookup itself [§50](spec/50-a-page-cache.md). There is
one writer at a time, and one process per file.

**Snapshot reads are deferred, not rejected** [§57](spec/57-concurrency.md). Readers that
don't wait for writers (as in SQLite's WAL mode, LiteDB 5 or redb) aren't
needed by anything measured yet, and they'd live below `PageStore`, so
features above it don't make them harder. To keep them possible, the
storage layer follows six rules. The one to know first: **a freed page
isn't reused while a reader could still need its old contents**. It
costs nothing today, since no reader overlaps a commit, but a
free-space map or any other change to allocation must keep it.

**Measured** [§58](spec/58-measuring-reader-waits.md) (`bench/`, `reader_wait`): a writer committing
once or ten times a second costs readers nothing measurable; one that
commits back to back makes them wait tens of milliseconds, up to half a
second. And readers don't really run in parallel yet: every page read
takes the page cache's mutex, so four readers are no faster than one.

## 10. The API

- `Database::open(path)`, or `open_with(path, OpenOptions)` with
  `cache_size` [§50](spec/50-a-page-cache.md) and `checkpoint_pages` [§53](spec/53-a-configurable-checkpoint-threshold.md).
- `db.collection::<T>(name)`: `insert`, `get`, `update`, `upsert`,
  `delete`, `find`, `find_one`, `count`, `cursor`, `explain` [§12](spec/12-wiring-collection-document.md) [§29](spec/29-api-rounding-out-find-one-count-upsert-cursor.md);
  `delete_many` and `update_many` with a closure [§37](spec/37-delete-many-and-dropping-a-collection.md) [§38](spec/38-update-many.md);
  `ensure_index` and `ensure_index_with` (`IndexOptions::new().unique()`,
  `.sparse()`), `indexes()` listing each index's fields and options
  [§28](spec/28-secondary-indexes.md) [§33](spec/33-unique-indexes.md) [§44](spec/44-sparse-indexes.md) [§60](spec/60-a-smaller-public-api.md).
- **Batches:** `db.batch()` across collections, typed and untyped alike
  [§24](spec/24-typed-batches.md) [§60](spec/60-a-smaller-public-api.md); all or nothing.
- **Filters** only through `Filter`'s methods; `limit` takes an `Option`
  too [§60](spec/60-a-smaller-public-api.md).
- **Conversions:** `Document::from_value(&t)` and `doc.into_value::<T>()`.
- **Limits** sit on the types they limit: `Document::MAX_NESTING`, and
  on `Database` the rest; its documentation lists them all.
- **What's public** is only this: `Database`, `Collection`, `Batch`,
  `Cursor`, `Document`, `DocId`, the options, reports and errors, and
  `query`. Types a caller passes in are built with methods; types it
  gets back are `#[non_exhaustive]`, like `Error` and `Document` [§60](spec/60-a-smaller-public-api.md).
- **Upkeep:** `export`/`import` as JSON Lines [§30](spec/30-export-and-import-as-json-lines.md), `check` [§39](spec/39-the-trunkdb-command-and-database-check.md),
  `compact` [§41](spec/41-compaction.md), `checkpoint` [§51](spec/51-one-flush-per-commit.md), `drop_collection` [§37](spec/37-delete-many-and-dropping-a-collection.md).
- **Errors:** one `Error` enum. A failed batch is rolled back and says
  why: `DuplicateId`, `DuplicateValue`, `NotFound`, and so on.

## 11. Tools

- **`trunkdb` command:** `info`, `check`, `compact`, `export`, `import`
  [§39](spec/39-the-trunkdb-command-and-database-check.md).
- **`Database::check`** reads the whole file and reports what doesn't
  add up: damaged pages, pages neither used nor free, index entries
  without their document and the other way round [§39](spec/39-the-trunkdb-command-and-database-check.md) [§40](spec/40-page-checksums.md).
- **Compaction** rebuilds the collections into a fresh image in memory,
  with full pages, logs it through the WAL like any batch and truncates
  the file [§41](spec/41-compaction.md). The whole new image is held in memory while it runs.
- **Export and import** write and read JSON Lines, with the types JSON
  lacks kept by tags (`{"$id": ...}`, `{"$binary": ...}`), so an export
  is a readable backup and the path between file formats [§30](spec/30-export-and-import-as-json-lines.md).

## 12. Performance

`bench/` runs one workload against trunkdb, SQLite, redb and sled, with
the same documents and indexes and a durable commit everywhere, and
checks that all four give the same answers [§48](spec/48-benchmarks-against-sqlite-redb-and-sled.md). The README has the
current table. Each gap had a measured cause, fixed one at a time:

| What | Before | Fix |
|---|---|---|
| "Oldest 20" by status | 19 ms | a lazy walk that stops at the limit [§49](spec/49-a-lazy-b-tree-walk.md) |
| Lookup by id | 32 µs | a page cache, 256 MiB by default, CLOCK eviction [§50](spec/50-a-page-cache.md) |
| One commit | 16 ms | one flush per commit; write-back at checkpoints [§51](spec/51-one-flush-per-commit.md) |
| Batched inserts | 7.5k/s | B-tree pages changed in place [§52](spec/52-b-tree-pages-changed-in-place.md) |

Still behind: full scans (decoding), a copy per page read, and whole
8 KB page images in the WAL for every changed page.

## 13. How it's tested

- **Unit and integration tests** for every section, most against a real
  file. Where an algorithm has a simple model (the B-tree against a
  `BTreeMap`, the planner against a scan), randomized tests compare the
  two.
- **Crash tests** inject a failure after any number of page writes, in
  the WAL, the write-back or the checkpoint, and check that the next
  open has the state before or after the batch, and nothing in between
  [§19](spec/19-durability-take-two-a-page-image-wal.md) [§51](spec/51-one-flush-per-commit.md).
- **Fuzzing** (`fuzz/`): damaged databases, WALs and exports, millions
  of them, where anything but a panic or a hang is a correct answer. Each
  crash it found became a test in the library [§55](spec/55-fuzzing.md).
- **Mutation checks:** each section breaks its own code on purpose in a
  handful of ways and lists them; every one has to fail a test.
- **CI** runs the tests on Linux, macOS and Windows, clippy with
  warnings as errors, fmt, the minimum Rust version (1.89), a short
  benchmark run, and keeps the fuzz crate compiling (it doesn't fuzz).
